//! Cross-owner operational aggregates for the super-admin dashboard.
//!
//! This module deliberately does not reuse the owner-scoped usage SQL. The
//! management layer supplies the account IDs the caller may see, and every
//! aggregate below is then built from final attempts for those accounts.
//! Percentiles use nearest-rank selection over the complete retained window:
//! `ceil(p * n) - 1`. There is no sampling in V1, so the retention window is
//! the only bound on the sample set.

use std::collections::BTreeMap;

use async_trait::async_trait;
use provider_usage::{
    CostTotals, OpsAccountMetrics, OpsFailureLayers, OpsModelMetrics, OpsOverview,
    OpsProviderMetrics, OpsQuery, OpsSeries, TimeRange, TokenTotals, UsageRepositoryError,
    UsdAtoms,
};
use sqlx::{AssertSqlSafe, Row, sqlite::SqliteRow};

use crate::{SqliteUsageRepository, usage::usage_error};

const HOUR_MS: i64 = 60 * 60 * 1000;
const TOP_MODELS: usize = 10;

#[derive(Debug)]
struct OpsFact {
    logical_status: String,
    completed_at_ms: i64,
    account_id: String,
    configured_model: Option<String>,
    dispatch_evidence: String,
    ttft_ms: Option<u64>,
    duration_ms: Option<u64>,
    cache_read_input_tokens: u64,
    effective_input_tokens: u64,
    output_tokens: u64,
    cost_atoms: Option<i128>,
}

#[derive(Default)]
struct MetricAccumulator {
    requests: u64,
    successes: u64,
    failures: u64,
    ttft_ms: Vec<u64>,
    duration_ms: Vec<u64>,
    cache_read_input_tokens: u64,
    effective_input_tokens: u64,
    output_tokens: u64,
    cost_atoms: Option<i128>,
    complete_cost_samples: u64,
}

#[async_trait]
impl OpsQuery for SqliteUsageRepository {
    async fn ops_overview(
        &self,
        account_ids: &[String],
        range: TimeRange,
        include_unattributed_zero_dispatch: bool,
    ) -> Result<OpsOverview, UsageRepositoryError> {
        let facts = self.ops_facts(account_ids, range).await?;
        let mut aggregate = MetricAccumulator::default();
        for fact in &facts {
            observe_fact(&mut aggregate, fact);
        }

        let zero_dispatch = self
            .zero_dispatch_failures(account_ids, range, include_unattributed_zero_dispatch)
            .await?;
        Ok(OpsOverview {
            requests: aggregate.requests,
            successes: aggregate.successes,
            tokens: TokenTotals {
                cache_read_input: aggregate.cache_read_input_tokens,
                effective_input: aggregate.effective_input_tokens,
                output: aggregate.output_tokens,
            },
            cost: cost_totals(&aggregate),
            avg_response_ms: average(&aggregate.duration_ms),
            failures: aggregate.failures,
            ttft_p50_ms: percentile(&mut aggregate.ttft_ms, 50),
            ttft_p95_ms: percentile(&mut aggregate.ttft_ms, 95),
            failure_layers: OpsFailureLayers {
                upstream_failed_requests: aggregate.failures,
                zero_dispatch_logical_failures: zero_dispatch,
            },
        })
    }

    async fn ops_providers(
        &self,
        account_ids: &[String],
        range: TimeRange,
        include_unattributed_zero_dispatch: bool,
    ) -> Result<OpsProviderMetrics, UsageRepositoryError> {
        let facts = self.ops_facts(account_ids, range).await?;
        let zero_dispatch = self
            .zero_dispatch_failures(account_ids, range, include_unattributed_zero_dispatch)
            .await?;
        let mut accounts = BTreeMap::<String, MetricAccumulator>::new();
        let mut models = BTreeMap::<String, MetricAccumulator>::new();
        let mut series = empty_series(range);
        let mut upstream_failures: u64 = 0;

        for fact in &facts {
            if !is_confirmed_dispatch(fact) {
                continue;
            }

            let account = accounts.entry(fact.account_id.clone()).or_default();
            observe_fact(account, fact);

            let model = fact
                .configured_model
                .as_deref()
                .filter(|model| !model.is_empty())
                .unwrap_or("unknown")
                .to_owned();
            let model_metrics = models.entry(model).or_default();
            observe_fact(model_metrics, fact);

            if is_health_request(fact) {
                let index = series_index(fact.completed_at_ms, range);
                if let Some(index) = index {
                    series.requests[index] = series.requests[index].saturating_add(1);
                    if fact.logical_status == "failed" {
                        series.failures[index] = series.failures[index].saturating_add(1);
                    }
                }
                if fact.logical_status == "failed" {
                    upstream_failures = upstream_failures.saturating_add(1);
                }
            }
        }

        let accounts = accounts
            .into_iter()
            .map(|(account_id, mut metrics)| OpsAccountMetrics {
                account_id,
                requests: metrics.requests,
                successes: metrics.successes,
                failures: metrics.failures,
                ttft_p50_ms: percentile(&mut metrics.ttft_ms, 50),
                ttft_p95_ms: percentile(&mut metrics.ttft_ms, 95),
                duration_p95_ms: percentile(&mut metrics.duration_ms, 95),
            })
            .collect();

        let mut models = models
            .into_iter()
            .map(|(model, mut metrics)| OpsModelMetrics {
                model,
                requests: metrics.requests,
                successes: metrics.successes,
                failures: metrics.failures,
                ttft_p50_ms: percentile(&mut metrics.ttft_ms, 50),
                effective_input_tokens: metrics.effective_input_tokens,
                output_tokens: metrics.output_tokens,
            })
            .collect::<Vec<_>>();
        models.sort_by(|left, right| {
            right
                .requests
                .cmp(&left.requests)
                .then_with(|| right.failures.cmp(&left.failures))
                .then_with(|| left.model.cmp(&right.model))
        });
        models.truncate(TOP_MODELS);

        Ok(OpsProviderMetrics {
            accounts,
            models,
            series,
            failure_layers: OpsFailureLayers {
                upstream_failed_requests: upstream_failures,
                zero_dispatch_logical_failures: zero_dispatch,
            },
        })
    }

    async fn ops_total_tokens(
        &self,
        account_ids: &[String],
        range: TimeRange,
    ) -> Result<TokenTotals, UsageRepositoryError> {
        if account_ids.is_empty() {
            return Ok(TokenTotals::default());
        }

        let placeholders = vec!["?"; account_ids.len()].join(", ");
        let sql = format!(
            r#"
            SELECT
                COALESCE(SUM(a.cache_read_input_tokens), 0) AS cache_read_input,
                COALESCE(SUM(a.effective_input_tokens), 0) AS effective_input,
                COALESCE(SUM(a.output_tokens), 0) AS output
            FROM usage_logical_requests AS l
            INNER JOIN usage_attempts AS a
                ON a.id = l.final_attempt_id
            WHERE l.logical_status <> 'in_progress'
              AND a.account_id IN ({placeholders})
              AND a.dispatch_evidence <> 'not_invoked'
              AND l.completed_at_ms >= ?
              AND l.completed_at_ms < ?
            "#,
        );
        let mut query = sqlx::query(AssertSqlSafe(sql));
        for account_id in account_ids {
            query = query.bind(account_id);
        }
        let row = query
            .bind(range.from_ms)
            .bind(range.to_ms)
            .fetch_one(&self.pool)
            .await
            .map_err(|error| usage_error("failed to read operations token totals", error))?;

        Ok(TokenTotals {
            cache_read_input: aggregate_token_count(&row, "cache_read_input")?,
            effective_input: aggregate_token_count(&row, "effective_input")?,
            output: aggregate_token_count(&row, "output")?,
        })
    }
}

impl SqliteUsageRepository {
    async fn ops_facts(
        &self,
        account_ids: &[String],
        range: TimeRange,
    ) -> Result<Vec<OpsFact>, UsageRepositoryError> {
        if account_ids.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders = vec!["?"; account_ids.len()].join(", ");
        let sql = format!(
            r#"
            SELECT
                l.logical_status,
                l.completed_at_ms,
                a.account_id,
                a.configured_model,
                a.dispatch_evidence,
                a.started_at_ms AS attempt_started_at_ms,
                a.first_token_at_ms,
                a.completed_at_ms AS attempt_completed_at_ms,
                a.cache_read_input_tokens,
                a.effective_input_tokens,
                a.output_tokens,
                a.cost_status,
                a.cost_atoms
            FROM usage_logical_requests AS l
            INNER JOIN usage_attempts AS a
                ON a.id = l.final_attempt_id
            WHERE l.logical_status <> 'in_progress'
              AND a.account_id IN ({placeholders})
              AND l.completed_at_ms >= ?
              AND l.completed_at_ms < ?
            ORDER BY l.completed_at_ms, l.request_id
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
            .map_err(|error| usage_error("failed to read operations usage facts", error))?;

        rows.iter().map(ops_fact).collect()
    }

    async fn zero_dispatch_failures(
        &self,
        account_ids: &[String],
        range: TimeRange,
        include_unattributed: bool,
    ) -> Result<u64, UsageRepositoryError> {
        let account_scope = if account_ids.is_empty() {
            "0".to_owned()
        } else {
            let placeholders = vec!["?"; account_ids.len()].join(", ");
            format!(
                "EXISTS (SELECT 1 FROM usage_attempts AS candidate WHERE candidate.logical_request_id = l.request_id AND candidate.account_id IN ({placeholders}))"
            )
        };
        let unattributed_scope = if include_unattributed {
            "OR NOT EXISTS (SELECT 1 FROM usage_attempts AS candidate WHERE candidate.logical_request_id = l.request_id)"
        } else {
            ""
        };
        let sql = format!(
            r#"
            SELECT COUNT(*) AS failures
            FROM usage_logical_requests AS l
            WHERE l.logical_status = 'failed'
              AND l.completed_at_ms >= ?
              AND l.completed_at_ms < ?
              AND NOT EXISTS (
                  SELECT 1
                  FROM usage_attempts AS dispatched
                  WHERE dispatched.logical_request_id = l.request_id
                    AND dispatched.dispatch_evidence <> 'not_invoked'
              )
              AND ({account_scope} {unattributed_scope})
            "#,
        );
        let mut query = sqlx::query(AssertSqlSafe(sql));
        query = query.bind(range.from_ms).bind(range.to_ms);
        for account_id in account_ids {
            query = query.bind(account_id);
        }
        let row = query
            .fetch_one(&self.pool)
            .await
            .map_err(|error| usage_error("failed to read zero-dispatch failures", error))?;

        let failures: i64 = row
            .try_get("failures")
            .map_err(|error| usage_error("failed to read zero-dispatch failure count", error))?;
        u64::try_from(failures)
            .map_err(|_| UsageRepositoryError::new("zero-dispatch failure count is negative"))
    }
}

fn aggregate_token_count(row: &SqliteRow, column: &str) -> Result<u64, UsageRepositoryError> {
    let value: i64 = row
        .try_get(column)
        .map_err(|error| usage_error("failed to read operations token total", error))?;
    u64::try_from(value).map_err(|_| {
        UsageRepositoryError::new(format!("operations token total {column} is negative"))
    })
}

fn ops_fact(row: &SqliteRow) -> Result<OpsFact, UsageRepositoryError> {
    let started_at_ms: i64 = row
        .try_get("attempt_started_at_ms")
        .map_err(|error| usage_error("failed to read operations attempt start", error))?;
    let first_token_at_ms: Option<i64> = row
        .try_get("first_token_at_ms")
        .map_err(|error| usage_error("failed to read operations first token", error))?;
    let completed_at_ms: i64 = row
        .try_get("attempt_completed_at_ms")
        .map_err(|error| usage_error("failed to read operations attempt completion", error))?;
    let cache_read_input_tokens = non_negative_count(row, "cache_read_input_tokens")?;
    let effective_input_tokens = non_negative_count(row, "effective_input_tokens")?;
    let output_tokens = non_negative_count(row, "output_tokens")?;
    let cost_status: String = row
        .try_get("cost_status")
        .map_err(|error| usage_error("failed to read operations cost status", error))?;
    let cost_atoms = if cost_status == "complete_for_observed_catalog_components" {
        let atoms: Option<i64> = row
            .try_get("cost_atoms")
            .map_err(|error| usage_error("failed to read operations cost", error))?;
        atoms.map(i128::from)
    } else {
        None
    };

    Ok(OpsFact {
        logical_status: row
            .try_get("logical_status")
            .map_err(|error| usage_error("failed to read operations logical status", error))?,
        completed_at_ms: row
            .try_get("completed_at_ms")
            .map_err(|error| usage_error("failed to read operations logical completion", error))?,
        account_id: row
            .try_get("account_id")
            .map_err(|error| usage_error("failed to read operations account", error))?,
        configured_model: row
            .try_get("configured_model")
            .map_err(|error| usage_error("failed to read operations configured model", error))?,
        dispatch_evidence: row
            .try_get("dispatch_evidence")
            .map_err(|error| usage_error("failed to read operations dispatch evidence", error))?,
        ttft_ms: first_token_at_ms
            .and_then(|first| first.checked_sub(started_at_ms))
            .and_then(|duration| u64::try_from(duration).ok()),
        duration_ms: completed_at_ms
            .checked_sub(started_at_ms)
            .and_then(|duration| u64::try_from(duration).ok()),
        cache_read_input_tokens,
        effective_input_tokens,
        output_tokens,
        cost_atoms,
    })
}

fn non_negative_count(row: &SqliteRow, column: &str) -> Result<u64, UsageRepositoryError> {
    let value: Option<i64> = row
        .try_get(column)
        .map_err(|error| usage_error("failed to read operations token count", error))?;
    value.map_or(Ok(0), |value| {
        u64::try_from(value).map_err(|_| {
            UsageRepositoryError::new(format!("operations token count {column} is negative"))
        })
    })
}

fn is_confirmed_dispatch(fact: &OpsFact) -> bool {
    fact.dispatch_evidence != "not_invoked"
}

fn is_health_request(fact: &OpsFact) -> bool {
    is_confirmed_dispatch(fact) && matches!(fact.logical_status.as_str(), "succeeded" | "failed")
}

fn observe_fact(metrics: &mut MetricAccumulator, fact: &OpsFact) {
    if is_confirmed_dispatch(fact) {
        metrics.cache_read_input_tokens = metrics
            .cache_read_input_tokens
            .saturating_add(fact.cache_read_input_tokens);
        metrics.effective_input_tokens = metrics
            .effective_input_tokens
            .saturating_add(fact.effective_input_tokens);
        metrics.output_tokens = metrics.output_tokens.saturating_add(fact.output_tokens);
        if let Some(cost_atoms) = fact.cost_atoms {
            metrics.complete_cost_samples = metrics.complete_cost_samples.saturating_add(1);
            metrics.cost_atoms = if metrics.complete_cost_samples == 1 {
                Some(cost_atoms)
            } else {
                metrics
                    .cost_atoms
                    .and_then(|total| total.checked_add(cost_atoms))
            };
        }
    }

    if is_health_request(fact) {
        metrics.requests = metrics.requests.saturating_add(1);
        if fact.logical_status == "succeeded" {
            metrics.successes = metrics.successes.saturating_add(1);
        } else {
            metrics.failures = metrics.failures.saturating_add(1);
        }
    }

    if is_confirmed_dispatch(fact) {
        if let Some(ttft_ms) = fact.ttft_ms {
            metrics.ttft_ms.push(ttft_ms);
        }
        if let Some(duration_ms) = fact.duration_ms {
            metrics.duration_ms.push(duration_ms);
        }
    }
}

fn cost_totals(metrics: &MetricAccumulator) -> CostTotals {
    CostTotals {
        atoms: (metrics.complete_cost_samples > 0)
            .then(|| metrics.cost_atoms.map(UsdAtoms::from_atoms))
            .flatten(),
    }
}

fn average(values: &[u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let sum = values
        .iter()
        .fold(0_u128, |sum, value| sum.saturating_add(u128::from(*value)));
    u64::try_from(sum / values.len() as u128).ok()
}

fn percentile(values: &mut [u64], percentile: u64) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let rank = ((values.len() as u128 * percentile as u128).saturating_add(99) / 100) as usize;
    values
        .get(rank.saturating_sub(1).min(values.len() - 1))
        .copied()
}

fn empty_series(range: TimeRange) -> OpsSeries {
    let mut buckets = Vec::new();
    let first_bucket = range.from_ms.div_euclid(HOUR_MS) * HOUR_MS;
    let mut bucket = first_bucket;
    while bucket < range.to_ms {
        buckets.push(bucket);
        bucket = bucket.saturating_add(HOUR_MS);
    }
    let length = buckets.len();
    OpsSeries {
        bucket_ms: HOUR_MS,
        buckets,
        requests: vec![0; length],
        failures: vec![0; length],
    }
}

fn series_index(completed_at_ms: i64, range: TimeRange) -> Option<usize> {
    let first_bucket = range.from_ms.div_euclid(HOUR_MS) * HOUR_MS;
    let bucket = completed_at_ms.div_euclid(HOUR_MS) * HOUR_MS;
    usize::try_from(bucket.checked_sub(first_bucket)? / HOUR_MS).ok()
}
