use provider_usage::{
    QuotaEstimateCompleteness, QuotaLimitEstimatePoint, TimeRange, UsageRepositoryError, UsdAtoms,
    recombine_atoms,
};
use sqlx::{AssertSqlSafe, Row, sqlite::SqliteRow};

use crate::{SqliteUsageRepository, usage::usage_error};

impl SqliteUsageRepository {
    pub(crate) async fn load_provider_quota_estimates(
        &self,
        account_ids: &[String],
        range: TimeRange,
    ) -> Result<Vec<QuotaLimitEstimatePoint>, UsageRepositoryError> {
        if account_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; account_ids.len()].join(", ");
        let sql = format!(
            r#"
            WITH ranked AS (
                SELECT
                    o.account_id, o.credential_revision, o.credential_identity_revision,
                    o.group_key, o.metric_key,
                    o.metric_position, o.period_kind, o.starts_at_ms, o.ends_at_ms,
                    o.duration_seconds, o.observed_at_ms, o.used_hundredths,
                    ROW_NUMBER() OVER (
                        PARTITION BY
                            o.account_id, o.group_key, o.metric_key,
                            o.starts_at_ms, o.ends_at_ms
                        ORDER BY o.observed_at_ms DESC, o.credential_revision DESC
                    ) AS observation_rank
                FROM provider_quota_window_observations AS o
                INNER JOIN provider_credentials AS c
                    ON c.account_id = o.account_id
                   AND c.quota_identity_revision = o.credential_identity_revision
                WHERE o.account_id IN ({placeholders})
                  AND o.used_hundredths >= 500
                  AND o.starts_at_ms >= ?
                  AND o.ends_at_ms <= ?
            ), latest AS (
                SELECT * FROM ranked WHERE observation_rank = 1
            )
            SELECT
                latest.account_id, latest.group_key, latest.metric_key,
                latest.metric_position, latest.period_kind, latest.duration_seconds,
                latest.starts_at_ms, latest.ends_at_ms, latest.observed_at_ms,
                latest.used_hundredths,
                COUNT(a.id) AS dispatched_attempts,
                COALESCE(SUM(CASE WHEN a.cost_atoms IS NOT NULL THEN 1 ELSE 0 END), 0)
                    AS priced_attempts,
                COALESCE(SUM(CASE
                    WHEN a.cost_status = 'complete_for_observed_catalog_components' THEN 1
                    ELSE 0 END), 0) AS complete_attempts,
                COALESCE(SUM(CASE WHEN a.cost_atoms IS NOT NULL
                    THEN a.cost_atoms / 1000000 ELSE 0 END), 0) AS cost_high,
                COALESCE(SUM(CASE WHEN a.cost_atoms IS NOT NULL
                    THEN a.cost_atoms % 1000000 ELSE 0 END), 0) AS cost_low
            FROM latest
            LEFT JOIN usage_attempts AS a
                ON a.account_id = latest.account_id
               AND a.credential_identity_revision = latest.credential_identity_revision
               AND a.dispatch_evidence <> 'not_invoked'
               AND a.completed_at_ms >= latest.starts_at_ms
               AND a.completed_at_ms < latest.observed_at_ms + 1000
            GROUP BY
                latest.account_id, latest.credential_revision,
                latest.credential_identity_revision, latest.group_key,
                latest.metric_key, latest.metric_position, latest.period_kind,
                latest.starts_at_ms, latest.ends_at_ms, latest.duration_seconds,
                latest.observed_at_ms, latest.used_hundredths
            ORDER BY latest.ends_at_ms, latest.metric_position, latest.metric_key
            "#,
        );
        let mut query = sqlx::query(AssertSqlSafe(sql));
        for account_id in account_ids {
            query = query.bind(account_id);
        }
        let rows = query
            .bind(range.from_ms)
            .bind(range.to_ms)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| usage_error("failed to read provider quota estimates", error))?;
        rows.iter().filter_map(quota_estimate_point).collect()
    }
}

fn quota_estimate_point(
    row: &SqliteRow,
) -> Option<Result<QuotaLimitEstimatePoint, UsageRepositoryError>> {
    let result = (|| {
        let dispatched_attempts = count(row, "dispatched_attempts")?;
        let priced_attempts = count(row, "priced_attempts")?;
        let complete_attempts = count(row, "complete_attempts")?;
        let used_hundredths = count(row, "used_hundredths")?;
        if dispatched_attempts == 0 || priced_attempts == 0 || used_hundredths < 500 {
            return Ok(None);
        }
        let cost_high: i64 = row
            .try_get("cost_high")
            .map_err(|error| usage_error("failed to read quota estimate cost", error))?;
        let cost_low: i64 = row
            .try_get("cost_low")
            .map_err(|error| usage_error("failed to read quota estimate cost", error))?;
        let observed_cost = recombine_atoms(cost_high, cost_low);
        if observed_cost.as_atoms() <= 0 {
            return Ok(None);
        }
        let estimated_atoms = observed_cost
            .as_atoms()
            .checked_mul(10_000)
            .and_then(|value| value.checked_add(i128::from(used_hundredths / 2)))
            .and_then(|value| value.checked_div(i128::from(used_hundredths)))
            .ok_or_else(|| UsageRepositoryError::new("quota estimate overflowed"))?;
        let metric_position = row
            .try_get::<i64, _>("metric_position")
            .ok()
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| UsageRepositoryError::new("stored quota metric position is invalid"))?;
        Ok(Some(QuotaLimitEstimatePoint {
            account_id: text(row, "account_id", "account")?,
            group_key: text(row, "group_key", "group")?,
            metric_key: text(row, "metric_key", "metric")?,
            metric_position,
            period_kind: text(row, "period_kind", "period")?,
            duration_seconds: row
                .try_get("duration_seconds")
                .map_err(|error| usage_error("failed to read quota estimate duration", error))?,
            window_start_ms: timestamp(row, "starts_at_ms")?,
            window_end_ms: timestamp(row, "ends_at_ms")?,
            observed_at_ms: row
                .try_get("observed_at_ms")
                .map_err(|error| usage_error("failed to read quota estimate observation", error))?,
            used_hundredths,
            observed_cost,
            estimated_limit_cost: UsdAtoms::from_atoms(estimated_atoms),
            completeness: if complete_attempts == dispatched_attempts {
                QuotaEstimateCompleteness::Complete
            } else {
                QuotaEstimateCompleteness::LowerBound
            },
            priced_attempts,
            dispatched_attempts,
        }))
    })();
    match result {
        Ok(Some(point)) => Some(Ok(point)),
        Ok(None) => None,
        Err(error) => Some(Err(error)),
    }
}

fn count(row: &SqliteRow, column: &str) -> Result<u64, UsageRepositoryError> {
    let value: i64 = row
        .try_get(column)
        .map_err(|error| usage_error("failed to read quota estimate count", error))?;
    u64::try_from(value).map_err(|_| UsageRepositoryError::new("quota estimate count is invalid"))
}

fn text(row: &SqliteRow, column: &str, label: &str) -> Result<String, UsageRepositoryError> {
    row.try_get(column)
        .map_err(|error| usage_error(&format!("failed to read quota estimate {label}"), error))
}

fn timestamp(row: &SqliteRow, column: &str) -> Result<i64, UsageRepositoryError> {
    row.try_get(column)
        .map_err(|error| usage_error("failed to read quota estimate window", error))
}
