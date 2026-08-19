use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

use axum::{
    Json,
    extract::{Extension, Query, State, rejection::QueryRejection},
};
use provider_auth::AuthenticatedSession;
use provider_core::{
    AccountAuthState, ProviderAccountSummary, ProviderQuotaSupport, ProviderQuotaView, QuotaAmount,
    QuotaUnit,
};
use provider_usage::{
    CostTotals, MAX_QUERY_RANGE, OpsAccountMetrics, OpsModelMetrics, TimeRange, TimeRangeError,
    TokenTotals, system_clock_ms,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    ManagementState,
    shared::{ApiError, data, query_request, require_super_admin},
};

const DEFAULT_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
const LOW_QUOTA_PERCENT: f64 = 20.0;

pub(super) async fn overview(
    State(state): State<ManagementState>,
    Extension(session): Extension<AuthenticatedSession>,
    params: Result<Query<OpsRangeParams>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    require_super_admin(&session)?;
    let params = query_request(params)?;
    let range = params.range()?;
    let all_accounts = state.manager.list_all_accounts().await?;
    let groups = group_options(&all_accounts);
    let group = params.group()?;
    let accounts = filter_accounts(all_accounts, group.as_deref());
    let account_ids = account_ids(&accounts);
    let include_unattributed_zero_dispatch = group.is_none();
    let usage = state.usage.as_ref().ok_or_else(ApiError::internal)?;
    let metrics = usage
        .query
        .ops_overview(&account_ids, range, include_unattributed_zero_dispatch)
        .await
        .map_err(|_| ApiError::internal())?;
    let total_range = retained_range()?;
    let total_metrics = usage
        .query
        .ops_total_tokens(&account_ids, total_range)
        .await
        .map_err(|_| ApiError::internal())?;

    Ok(data(json!({
        "from_ms": range.from_ms,
        "to_ms": range.to_ms,
        "requests": metrics.requests,
        "successes": metrics.successes,
        "failures": metrics.failures,
        "success_rate": success_rate(metrics.successes, metrics.failures),
        "tokens": tokens_json(&metrics.tokens),
        "total_tokens": tokens_json(&total_metrics),
        "cost": cost_json(&metrics.cost),
        "avg_response_ms": metrics.avg_response_ms,
        "ttft_p50_ms": metrics.ttft_p50_ms,
        "ttft_p95_ms": metrics.ttft_p95_ms,
        "accounts": account_counts(&accounts),
        "failure_layers": failure_layers_json(&metrics.failure_layers),
        "groups": groups,
    })))
}

pub(super) async fn providers(
    State(state): State<ManagementState>,
    Extension(session): Extension<AuthenticatedSession>,
    params: Result<Query<OpsRangeParams>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    require_super_admin(&session)?;
    let params = query_request(params)?;
    let range = params.range()?;
    let all_accounts = state.manager.list_all_accounts().await?;
    let groups = group_options(&all_accounts);
    let group = params.group()?;
    let mut accounts = filter_accounts(all_accounts, group.as_deref());
    let account_ids = account_ids(&accounts);
    let include_unattributed_zero_dispatch = group.is_none();
    let usage = state.usage.as_ref().ok_or_else(ApiError::internal)?;
    let metrics = usage
        .query
        .ops_providers(&account_ids, range, include_unattributed_zero_dispatch)
        .await
        .map_err(|_| ApiError::internal())?;
    let metrics_by_account = metrics
        .accounts
        .iter()
        .map(|metrics| (metrics.account_id.as_str(), metrics))
        .collect::<HashMap<_, _>>();
    accounts.sort_by(|left, right| {
        let left_metrics = metrics_by_account.get(left.id.as_str()).copied();
        let right_metrics = metrics_by_account.get(right.id.as_str()).copied();
        account_requests(right_metrics)
            .cmp(&account_requests(left_metrics))
            .then_with(|| compare_failure_rate(right_metrics, left_metrics))
            .then_with(|| left.id.as_str().cmp(right.id.as_str()))
    });

    let now = unix_timestamp();
    let mut account_values = Vec::with_capacity(accounts.len());
    for account in &accounts {
        let account_metrics = metrics_by_account.get(account.id.as_str()).copied();
        let quota = state
            .manager
            .cached_quota(session.user.id.as_str(), account, now)
            .await;
        account_values.push(account_json(account, account_metrics, &quota));
    }

    Ok(data(json!({
        "from_ms": range.from_ms,
        "to_ms": range.to_ms,
        "accounts": account_values,
        "groups": groups,
        "models": metrics.models.iter().map(model_json).collect::<Vec<_>>(),
        "series": series_json(&metrics.series),
        "failure_layers": failure_layers_json(&metrics.failure_layers),
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OpsRangeParams {
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    group: Option<String>,
}

impl OpsRangeParams {
    fn range(&self) -> Result<TimeRange, ApiError> {
        let to_ms = self.to_ms.unwrap_or_else(system_clock_ms);
        let from_ms = self.from_ms.unwrap_or(to_ms - DEFAULT_WINDOW_MS);
        TimeRange::new(from_ms, to_ms).map_err(|error| match error {
            TimeRangeError::Empty => ApiError::invalid_request("to_ms must be after from_ms"),
            TimeRangeError::TooWide => {
                ApiError::invalid_request("range is wider than usage is retained for")
            }
        })
    }

    fn group(&self) -> Result<Option<String>, ApiError> {
        self.group
            .clone()
            .map(|group| {
                let group = group.trim().to_owned();
                if group.is_empty() {
                    return Err(ApiError::invalid_request("group must not be empty"));
                }
                Ok(group)
            })
            .transpose()
    }
}

fn filter_accounts(
    accounts: Vec<ProviderAccountSummary>,
    group: Option<&str>,
) -> Vec<ProviderAccountSummary> {
    accounts
        .into_iter()
        .filter(|account| group.is_none_or(|group| account.group_label == group))
        .collect()
}

fn group_options(accounts: &[ProviderAccountSummary]) -> Vec<String> {
    accounts
        .iter()
        .map(|account| account.group_label.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn account_ids(accounts: &[ProviderAccountSummary]) -> Vec<String> {
    accounts
        .iter()
        .map(|account| account.id.as_str().to_owned())
        .collect()
}

fn retained_range() -> Result<TimeRange, ApiError> {
    let to_ms = system_clock_ms();
    let span = i64::try_from(MAX_QUERY_RANGE.as_millis()).map_err(|_| ApiError::internal())?;
    TimeRange::new(to_ms.saturating_sub(span), to_ms).map_err(|_| ApiError::internal())
}

fn account_counts(accounts: &[ProviderAccountSummary]) -> Value {
    let enabled = accounts.iter().filter(|account| account.enabled).count();
    let active = accounts
        .iter()
        .filter(|account| account.enabled && account.auth_state == AccountAuthState::Active)
        .count();
    let reauth_required = accounts
        .iter()
        .filter(|account| account.enabled && account.auth_state == AccountAuthState::ReauthRequired)
        .count();
    json!({
        "total": accounts.len(),
        "enabled": enabled,
        "active": active,
        "reauth_required": reauth_required,
        "disabled": accounts.len().saturating_sub(enabled),
    })
}

fn account_json(
    account: &ProviderAccountSummary,
    metrics: Option<&OpsAccountMetrics>,
    quota: &ProviderQuotaView,
) -> Value {
    let requests = account_requests(metrics);
    let successes = metrics.map_or(0, |metrics| metrics.successes);
    let failures = metrics.map_or(0, |metrics| metrics.failures);
    json!({
        "account_id": account.id.as_str(),
        "provider": account.provider.as_str(),
        "label": account.label,
        "group_label": account.group_label,
        "visibility": account.visibility.as_str(),
        "enabled": account.enabled,
        "auth_state": account.auth_state.as_str(),
        "requests": requests,
        "successes": successes,
        "failures": failures,
        "success_rate": success_rate(successes, failures),
        "ttft_p50_ms": metrics.and_then(|metrics| metrics.ttft_p50_ms),
        "ttft_p95_ms": metrics.and_then(|metrics| metrics.ttft_p95_ms),
        "duration_p95_ms": metrics.and_then(|metrics| metrics.duration_p95_ms),
        "quota": quota_json(quota),
    })
}

fn model_json(metrics: &OpsModelMetrics) -> Value {
    json!({
        "model": metrics.model,
        "requests": metrics.requests,
        "successes": metrics.successes,
        "failures": metrics.failures,
        "success_rate": success_rate(metrics.successes, metrics.failures),
        "tokens": {
            "effective_input": metrics.effective_input_tokens,
            "output": metrics.output_tokens,
        },
        "ttft_p50_ms": metrics.ttft_p50_ms,
    })
}

fn series_json(series: &provider_usage::OpsSeries) -> Value {
    json!({
        "bucket_ms": series.bucket_ms,
        "buckets": series.buckets,
        "requests": series.requests,
        "failures": series.failures,
    })
}

fn failure_layers_json(layers: &provider_usage::OpsFailureLayers) -> Value {
    json!({
        "upstream_failed_requests": layers.upstream_failed_requests,
        "zero_dispatch_logical_failures": layers.zero_dispatch_logical_failures,
    })
}

fn tokens_json(tokens: &TokenTotals) -> Value {
    json!({
        "cache_read_input": tokens.cache_read_input,
        "effective_input": tokens.effective_input,
        "output": tokens.output,
        "total": tokens.effective_input.saturating_add(tokens.output),
    })
}

fn cost_json(cost: &CostTotals) -> Value {
    json!({
        "usd": cost.atoms.map(|atoms| atoms.to_decimal_string()),
    })
}

fn quota_json(quota: &ProviderQuotaView) -> Value {
    if quota.support == ProviderQuotaSupport::Unsupported {
        return json!({
            "summary": "unsupported",
            "tightest_remaining_percent": null,
            "fetched_at_ms": null,
        });
    }

    let Some(snapshot) = quota.snapshot.as_ref() else {
        return json!({
            "summary": if quota.last_error.is_some() { "unavailable" } else { "unknown" },
            "tightest_remaining_percent": null,
            "fetched_at_ms": null,
        });
    };

    let tightest = snapshot
        .groups
        .iter()
        .flat_map(|group| group.metrics.iter())
        .filter(|metric| metric.unit == QuotaUnit::Percent)
        .filter_map(|metric| metric.remaining.as_ref().and_then(quota_amount_number))
        .reduce(f64::min)
        .filter(|value| value.is_finite());
    let summary = match tightest {
        None => "unknown",
        Some(value) if value <= 0.0 => "exhausted",
        Some(value) if value <= LOW_QUOTA_PERCENT => "low",
        Some(_) => "ok",
    };

    json!({
        "summary": summary,
        "tightest_remaining_percent": tightest,
        "fetched_at_ms": snapshot.fetched_at.checked_mul(1000),
    })
}

fn quota_amount_number(amount: &QuotaAmount) -> Option<f64> {
    let value = match amount {
        QuotaAmount::Integer(value) => *value as f64,
        QuotaAmount::Decimal(value) => *value,
        QuotaAmount::DecimalString(value) => value.parse::<f64>().ok()?,
    };
    value.is_finite().then_some(value)
}

fn success_rate(successes: u64, failures: u64) -> Option<f64> {
    let total = successes.saturating_add(failures);
    (total > 0).then(|| successes as f64 / total as f64)
}

fn account_requests(metrics: Option<&OpsAccountMetrics>) -> u64 {
    metrics.map_or(0, |metrics| metrics.requests)
}

fn compare_failure_rate(
    left: Option<&OpsAccountMetrics>,
    right: Option<&OpsAccountMetrics>,
) -> Ordering {
    let left_requests = account_requests(left);
    let right_requests = account_requests(right);
    match (left_requests, right_requests) {
        (0, 0) => Ordering::Equal,
        (0, _) => Ordering::Less,
        (_, 0) => Ordering::Greater,
        _ => (left.map_or(0, |metrics| metrics.failures) as u128 * right_requests as u128)
            .cmp(&(right.map_or(0, |metrics| metrics.failures) as u128 * left_requests as u128)),
    }
}

fn unix_timestamp() -> i64 {
    system_clock_ms() / 1000
}
