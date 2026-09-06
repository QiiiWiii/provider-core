use provider_core::{
    AccountId, AccountRepositoryError, ProviderQuotaObservation, QuotaAmount, QuotaMetric,
    QuotaMetricKind, QuotaPeriodKind, QuotaUnit,
};

use super::{SqliteAccountRepository, database_integer, repository_error};

impl SqliteAccountRepository {
    pub(super) async fn store_provider_quota_observation(
        &self,
        account_id: &AccountId,
        credential_identity_revision: u64,
        observation: &ProviderQuotaObservation,
    ) -> Result<(), AccountRepositoryError> {
        let revision = database_integer(observation.credential_revision, "credential revision")?;
        let identity_revision =
            database_integer(credential_identity_revision, "credential identity revision")?;
        let observed_at_ms = observation
            .observed_at
            .checked_mul(1000)
            .ok_or_else(|| AccountRepositoryError::new("quota observation time overflowed"))?;
        let mut transaction = self.write.begin().await.map_err(|error| {
            repository_error("failed to start quota observation transaction", error)
        })?;
        for group in &observation.groups {
            for (position, metric) in group.metrics.iter().enumerate() {
                let Some(row) = quota_observation_row(metric, observed_at_ms) else {
                    continue;
                };
                sqlx::query(
                    r#"
                    INSERT INTO provider_quota_window_observations (
                        account_id, credential_revision, credential_identity_revision,
                        observed_at_ms, group_key,
                        metric_key, metric_position, used_hundredths, period_kind,
                        starts_at_ms, ends_at_ms, duration_seconds
                    )
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                    ON CONFLICT DO NOTHING
                    "#,
                )
                .bind(account_id.as_str())
                .bind(revision)
                .bind(identity_revision)
                .bind(observed_at_ms)
                .bind(&group.key)
                .bind(&metric.key)
                .bind(
                    i64::try_from(position).map_err(|_| {
                        AccountRepositoryError::new("quota metric position overflowed")
                    })?,
                )
                .bind(row.used_hundredths)
                .bind(row.period_kind)
                .bind(row.starts_at_ms)
                .bind(row.ends_at_ms)
                .bind(row.duration_seconds)
                .execute(&mut *transaction)
                .await
                .map_err(|error| {
                    repository_error("failed to record quota window observation", error)
                })?;
            }
        }
        transaction.commit().await.map_err(|error| {
            repository_error("failed to commit quota observation transaction", error)
        })?;
        Ok(())
    }
}

struct QuotaObservationRow {
    used_hundredths: i64,
    period_kind: &'static str,
    starts_at_ms: i64,
    ends_at_ms: i64,
    duration_seconds: Option<i64>,
}

fn quota_observation_row(metric: &QuotaMetric, observed_at_ms: i64) -> Option<QuotaObservationRow> {
    if metric.kind != QuotaMetricKind::Usage || metric.unit != QuotaUnit::Percent {
        return None;
    }
    let used = match metric.used.as_ref()? {
        QuotaAmount::Integer(value) => *value as f64,
        QuotaAmount::Decimal(value) => *value,
        QuotaAmount::DecimalString(value) => value.parse().ok()?,
    };
    if !used.is_finite() || !(0.0..=100.0).contains(&used) {
        return None;
    }
    let used_hundredths = (used * 100.0).round() as i64;
    let period = metric.period.as_ref()?;
    let period_kind = match period.kind {
        QuotaPeriodKind::Weekly => "weekly",
        QuotaPeriodKind::Monthly => "monthly",
        QuotaPeriodKind::Rolling => "rolling",
        QuotaPeriodKind::Unknown => return None,
    };
    let ends_at_ms = period.ends_at?.checked_mul(1000)?;
    let starts_at_ms = period
        .starts_at
        .or_else(|| period.ends_at?.checked_sub(period.duration_seconds?))?
        .checked_mul(1000)?;
    if ends_at_ms <= starts_at_ms || observed_at_ms < starts_at_ms || observed_at_ms > ends_at_ms {
        return None;
    }
    Some(QuotaObservationRow {
        used_hundredths,
        period_kind,
        starts_at_ms,
        ends_at_ms,
        duration_seconds: period.duration_seconds,
    })
}
