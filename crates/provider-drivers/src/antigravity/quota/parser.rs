use std::collections::BTreeMap;

use provider_core::{
    ProviderKind, ProviderQuotaError, ProviderQuotaSnapshot, QuotaAmount, QuotaGroup,
    QuotaGroupAudience, QuotaGroupScope, QuotaMetric, QuotaMetricKind, QuotaPeriod,
    QuotaPeriodKind, QuotaScalar, QuotaUnit,
};
use serde_json::{Map, Value};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::error::{invalid_response, unix_timestamp};

pub(super) struct RawQuota {
    model_id: String,
    label: Option<String>,
    remaining_fraction: Option<f64>,
    reset_time: Option<Value>,
}
pub(super) fn parse_available_model_quotas(
    payload: &Value,
) -> Result<Vec<RawQuota>, ProviderQuotaError> {
    let models = payload
        .get("models")
        .and_then(Value::as_object)
        .or_else(|| {
            payload
                .get("response")
                .and_then(Value::as_object)
                .and_then(|response| response.get("models"))
                .and_then(Value::as_object)
        })
        .ok_or_else(|| {
            invalid_response("Antigravity model quota response did not contain models")
        })?;
    let quotas = models
        .iter()
        .filter_map(|(model_id, model)| {
            let model = model.as_object()?;
            let quota = model
                .get("quotaInfo")
                .or_else(|| model.get("quota_info"))
                .and_then(Value::as_object)?;
            Some(RawQuota {
                model_id: model_id.to_owned(),
                label: object_string(model, "displayName")
                    .or_else(|| object_string(model, "label")),
                remaining_fraction: quota_remaining_fraction(quota),
                reset_time: object_value(quota, "resetTime")
                    .or_else(|| object_value(quota, "reset_time")),
            })
        })
        .collect::<Vec<_>>();
    if quotas.is_empty() {
        return Err(invalid_response(
            "Antigravity model quota response did not contain quotaInfo",
        ));
    }
    Ok(quotas)
}

pub(super) fn normalize_quota(
    account_id: &str,
    quotas: Vec<RawQuota>,
) -> Result<ProviderQuotaSnapshot, ProviderQuotaError> {
    let mut selected = BTreeMap::<String, RawQuota>::new();
    for quota in quotas {
        let Some(remaining_fraction) = quota.remaining_fraction else {
            continue;
        };
        let Some(family) = quota_family(&quota) else {
            continue;
        };
        if selected
            .get(family)
            .is_none_or(|existing| should_prefer(&quota, existing, remaining_fraction))
        {
            selected.insert(family.to_owned(), quota);
        }
    }
    if selected.is_empty() {
        return Err(invalid_response(
            "Antigravity quota response did not contain supported remainingFraction data",
        ));
    }

    let metrics = ["gemini", "claude_gpt"]
        .into_iter()
        .filter_map(|family| selected.get(family).map(|quota| (family, quota)))
        .map(|(family, quota)| normalize_metric(family, quota))
        .collect::<Vec<_>>();
    if metrics.is_empty() {
        return Err(invalid_response(
            "Antigravity quota response did not contain Gemini or Claude/GPT data",
        ));
    }

    let mut attributes = BTreeMap::new();
    attributes.insert(
        "display_name".to_owned(),
        QuotaScalar::Text("Antigravity".to_owned()),
    );
    Ok(ProviderQuotaSnapshot {
        account_id: account_id.to_owned(),
        provider: ProviderKind::Antigravity,
        fetched_at: unix_timestamp(),
        last_observed_at: None,
        groups: vec![QuotaGroup {
            key: "antigravity".to_owned(),
            scope: QuotaGroupScope::Aggregate,
            audience: QuotaGroupAudience::Shared,
            attributes,
            metrics,
        }],
        warnings: Vec::new(),
    })
}

fn should_prefer(candidate: &RawQuota, existing: &RawQuota, candidate_remaining: f64) -> bool {
    let existing_remaining = existing.remaining_fraction.unwrap_or(f64::INFINITY);
    if candidate_remaining != existing_remaining {
        return candidate_remaining < existing_remaining;
    }
    candidate.reset_time.is_some() && existing.reset_time.is_none()
}

fn quota_family(quota: &RawQuota) -> Option<&'static str> {
    let model = format!(
        "{} {}",
        quota.model_id.to_ascii_lowercase(),
        quota
            .label
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
    );
    if model.contains("gemini") || model.contains("flash") {
        Some("gemini")
    } else if model.contains("claude") || model.contains("gpt") {
        Some("claude_gpt")
    } else {
        None
    }
}

fn normalize_metric(family: &str, quota: &RawQuota) -> QuotaMetric {
    let remaining = normalize_fraction(quota.remaining_fraction.unwrap_or_default()) * 100.0;
    let period = quota
        .reset_time
        .as_ref()
        .and_then(parse_timestamp)
        .map(|ends_at| QuotaPeriod {
            kind: QuotaPeriodKind::Unknown,
            starts_at: None,
            ends_at: Some(ends_at),
            duration_seconds: None,
        });
    QuotaMetric {
        key: family.to_owned(),
        kind: QuotaMetricKind::Usage,
        unit: QuotaUnit::Percent,
        used: Some(QuotaAmount::Decimal(100.0 - remaining)),
        remaining: Some(QuotaAmount::Decimal(remaining)),
        limit: Some(QuotaAmount::Decimal(100.0)),
        period,
        breakdown: Vec::new(),
    }
}

fn quota_remaining_fraction(object: &Map<String, Value>) -> Option<f64> {
    object
        .get("remainingFraction")
        .or_else(|| object.get("remaining_fraction"))
        .and_then(value_f64)
        .or_else(|| object.get("remaining").and_then(remaining_oneof_fraction))
}

fn remaining_oneof_fraction(value: &Value) -> Option<f64> {
    value
        .get("remainingFraction")
        .and_then(value_f64)
        .or_else(|| {
            (value.get("case").and_then(Value::as_str) == Some("remainingFraction"))
                .then(|| value.get("value").and_then(value_f64))
                .flatten()
        })
        .or_else(|| value_f64(value))
}

fn value_f64(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| {
        value
            .as_str()
            .and_then(|text| text.trim().parse::<f64>().ok())
    })
}

fn normalize_fraction(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn object_string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn object_value(object: &Map<String, Value>, key: &str) -> Option<Value> {
    object.get(key).filter(|value| !value.is_null()).cloned()
}

fn parse_timestamp(value: &Value) -> Option<i64> {
    let timestamp = match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|value| value as i64)),
        Value::String(text) => text.parse::<i64>().ok().or_else(|| {
            OffsetDateTime::parse(text.trim(), &Rfc3339)
                .ok()
                .map(|value| value.unix_timestamp())
        }),
        Value::Object(object) => object
            .get("seconds")
            .and_then(value_f64)
            .map(|value| value as i64),
        _ => None,
    }?;
    if timestamp.unsigned_abs() > 10_000_000_000 {
        timestamp.checked_div(1000)
    } else {
        Some(timestamp)
    }
}
