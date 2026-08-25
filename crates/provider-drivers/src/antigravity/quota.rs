use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use provider_core::{
    BoundedBodyError, ProviderKind, ProviderQuotaError, ProviderQuotaErrorKind,
    ProviderQuotaSnapshot, QuotaAmount, QuotaGroup, QuotaGroupAudience, QuotaGroupScope,
    QuotaMetric, QuotaMetricKind, QuotaPeriod, QuotaPeriodKind, QuotaScalar, QuotaUnit,
    collect_bounded_body,
};
use secrecy::ExposeSecret;
use serde_json::{Map, Value};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::{contract::API_BASE_URL, credentials::AntigravityCredentials, version};

const MAX_RESPONSE_SIZE: usize = 256 * 1024;
const QUOTA_TIMEOUT: Duration = Duration::from_secs(15);
const FETCH_AVAILABLE_MODELS_PATH: &str = "/v1internal:fetchAvailableModels";
const RETRIEVE_USER_QUOTA_PATH: &str = "/v1internal:retrieveUserQuota";

#[derive(Clone)]
pub(crate) struct AntigravityQuotaClient {
    http: reqwest::Client,
    base_url: String,
}

#[derive(Clone)]
struct RawQuota {
    model_id: String,
    label: Option<String>,
    remaining_fraction: Option<f64>,
    reset_time: Option<Value>,
}

impl AntigravityQuotaClient {
    pub(crate) fn new() -> Self {
        Self::with_base_url(API_BASE_URL)
    }

    pub(crate) fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .http1_only()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    pub(crate) async fn fetch(
        &self,
        account_id: &str,
        credentials: &AntigravityCredentials,
    ) -> Result<ProviderQuotaSnapshot, ProviderQuotaError> {
        let model_payload = self
            .fetch_payload(
                FETCH_AVAILABLE_MODELS_PATH,
                project_request_body(credentials.project_id()),
                credentials,
            )
            .await;
        let model_quotas = match model_payload {
            Ok(payload) => parse_available_model_quotas(&payload)?,
            Err(error) if error.upstream_status() == Some(403) => {
                let payload = self
                    .fetch_payload(
                        RETRIEVE_USER_QUOTA_PATH,
                        project_request_body(credentials.project_id()),
                        credentials,
                    )
                    .await?;
                parse_quota_buckets(&payload)?
            }
            Err(error) => return Err(error),
        };

        let verified_quotas = if should_verify_with_quota_endpoint(&model_quotas) {
            let payload = self
                .fetch_payload(
                    RETRIEVE_USER_QUOTA_PATH,
                    project_request_body(credentials.project_id()),
                    credentials,
                )
                .await?;
            let quotas = parse_quota_buckets(&payload)?;
            if !has_quota_fraction_data(&quotas) {
                return Err(invalid_response(
                    "Antigravity quota verification did not contain remainingFraction",
                ));
            }
            quotas
        } else {
            model_quotas
        };

        normalize_quota(account_id, verified_quotas)
    }

    async fn fetch_payload(
        &self,
        path: &str,
        request_body: Value,
        credentials: &AntigravityCredentials,
    ) -> Result<Value, ProviderQuotaError> {
        let body = serde_json::to_vec(&request_body)
            .map_err(|_| upstream_error("failed to encode Antigravity quota request"))?;
        let response = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .timeout(QUOTA_TIMEOUT)
            .header(reqwest::header::ACCEPT, "*/*")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::USER_AGENT, version::user_agent())
            .bearer_auth(credentials.access_token().expose_secret())
            .body(body)
            .send()
            .await
            .map_err(|_| upstream_error("Antigravity quota request failed"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(response, status).await);
        }
        let body = collect_bounded_body(response.bytes_stream(), MAX_RESPONSE_SIZE)
            .await
            .map_err(|error| match error {
                BoundedBodyError::Read(_) => {
                    upstream_error("failed to read Antigravity quota response")
                }
                BoundedBodyError::TooLarge => ProviderQuotaError::new(
                    ProviderQuotaErrorKind::InvalidResponse,
                    "Antigravity quota response was too large",
                ),
            })?;
        serde_json::from_slice::<Value>(&body).map_err(|error| {
            ProviderQuotaError::new(
                ProviderQuotaErrorKind::InvalidResponse,
                format!("Antigravity quota returned invalid JSON: {error}"),
            )
        })
    }
}

fn project_request_body(project_id: &str) -> Value {
    let project_id = project_id.trim();
    if project_id.is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::json!({"project": project_id})
    }
}

fn parse_available_model_quotas(payload: &Value) -> Result<Vec<RawQuota>, ProviderQuotaError> {
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

fn parse_quota_buckets(payload: &Value) -> Result<Vec<RawQuota>, ProviderQuotaError> {
    let buckets = payload
        .get("buckets")
        .or_else(|| {
            payload
                .get("response")
                .and_then(|value| value.get("buckets"))
        })
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_response("Antigravity quota response did not contain buckets"))?;
    Ok(buckets
        .iter()
        .filter_map(|bucket| {
            let bucket = bucket.as_object()?;
            let model_id =
                object_string(bucket, "modelId").or_else(|| object_string(bucket, "model_id"))?;
            Some(RawQuota {
                model_id,
                label: None,
                remaining_fraction: quota_remaining_fraction(bucket),
                reset_time: object_value(bucket, "resetTime")
                    .or_else(|| object_value(bucket, "reset_time")),
            })
        })
        .collect())
}

fn normalize_quota(
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

fn should_verify_with_quota_endpoint(quotas: &[RawQuota]) -> bool {
    !quotas.is_empty()
        && quotas.iter().all(|quota| {
            quota
                .remaining_fraction
                .is_some_and(|remaining| remaining >= 0.999)
        })
}

fn has_quota_fraction_data(quotas: &[RawQuota]) -> bool {
    quotas
        .iter()
        .any(|quota| quota.remaining_fraction.is_some())
}

async fn status_error(
    response: reqwest::Response,
    status: reqwest::StatusCode,
) -> ProviderQuotaError {
    let body = collect_bounded_body(response.bytes_stream(), MAX_RESPONSE_SIZE)
        .await
        .unwrap_or_default();
    let message = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| value.pointer("/error/message").and_then(Value::as_str))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| {
            std::str::from_utf8(&body)
                .ok()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| format!("Antigravity quota returned HTTP {status}"));
    let kind = match status.as_u16() {
        401 | 403 => ProviderQuotaErrorKind::Authentication,
        429 => ProviderQuotaErrorKind::RateLimited,
        _ => ProviderQuotaErrorKind::Upstream,
    };
    ProviderQuotaError::new(kind, message).with_upstream_status(status.as_u16())
}

fn invalid_response(message: impl Into<String>) -> ProviderQuotaError {
    ProviderQuotaError::new(ProviderQuotaErrorKind::InvalidResponse, message)
}

fn upstream_error(message: impl Into<String>) -> ProviderQuotaError {
    ProviderQuotaError::new(ProviderQuotaErrorKind::Upstream, message)
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Router,
        body::to_bytes,
        extract::{Request, State},
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::post,
    };
    use secrecy::SecretString;
    use tokio::net::TcpListener;

    use super::*;

    #[derive(Clone, Default)]
    struct CapturedRequest {
        body: Arc<Mutex<Option<Value>>>,
        headers: Arc<Mutex<HeaderMap>>,
    }

    async fn spawn(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(axum::serve(listener, router).into_future());
        format!("http://{address}")
    }

    fn credentials() -> AntigravityCredentials {
        AntigravityCredentials::from_json(&SecretString::from(
            serde_json::json!({
                "type": "antigravity",
                "access_token": "quota-token",
                "refresh_token": "refresh-token",
                "project_id": "project-1"
            })
            .to_string(),
        ))
        .expect("credentials")
    }

    #[tokio::test]
    async fn fetches_and_aggregates_model_quotas() {
        let captured = CapturedRequest::default();
        let state = captured.clone();
        let app = Router::new()
            .route(
                FETCH_AVAILABLE_MODELS_PATH,
                post(
                    move |State(state): State<CapturedRequest>,
                          headers: HeaderMap,
                          request: Request| async move {
                        let body = to_bytes(request.into_body(), 1 << 20)
                            .await
                            .expect("body");
                        *state.body.lock().expect("body lock") =
                            Some(serde_json::from_slice(&body).expect("request JSON"));
                        *state.headers.lock().expect("headers lock") = headers;
                        (
                            StatusCode::OK,
                            r#"{"models":{"gemini-3-pro":{"displayName":"Gemini Pro","quotaInfo":{"remainingFraction":0.75}},"gemini-3-flash":{"quotaInfo":{"remainingFraction":0.5}},"claude-sonnet-4":{"quotaInfo":{"remainingFraction":0.9}},"gpt-oss-120b":{"quotaInfo":{"remainingFraction":0.8}}}}"#,
                        )
                            .into_response()
                    },
                ),
            )
            .with_state(state);
        let client = AntigravityQuotaClient::with_base_url(spawn(app).await);

        let snapshot = client
            .fetch("account-1", &credentials())
            .await
            .expect("quota snapshot");

        assert_eq!(snapshot.groups.len(), 1);
        assert_eq!(snapshot.groups[0].scope, QuotaGroupScope::Aggregate);
        assert_eq!(snapshot.groups[0].metrics.len(), 2);
        assert_eq!(snapshot.groups[0].metrics[0].key, "gemini");
        assert_eq!(
            snapshot.groups[0].metrics[0].remaining,
            Some(QuotaAmount::Decimal(50.0))
        );
        assert_eq!(snapshot.groups[0].metrics[1].key, "claude_gpt");
        assert_eq!(
            snapshot.groups[0].metrics[1].remaining,
            Some(QuotaAmount::Decimal(80.0))
        );
        let body = captured
            .body
            .lock()
            .expect("body lock")
            .clone()
            .expect("body");
        assert_eq!(body, serde_json::json!({"project":"project-1"}));
        let headers = captured.headers.lock().expect("headers lock");
        assert_eq!(
            headers
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer quota-token")
        );
    }

    #[tokio::test]
    async fn verifies_all_available_models_with_quota_buckets() {
        let app = Router::new()
            .route(
                FETCH_AVAILABLE_MODELS_PATH,
                post(|| async {
                    (
                        StatusCode::OK,
                        r#"{"models":{"gemini-3-pro":{"quotaInfo":{"remainingFraction":1}},"claude-sonnet-4":{"quotaInfo":{"remainingFraction":1}}}}"#,
                    )
                }),
            )
            .route(
                RETRIEVE_USER_QUOTA_PATH,
                post(|| async {
                    (
                        StatusCode::OK,
                        r#"{"buckets":[{"modelId":"gemini-3-pro","remainingFraction":0.6},{"modelId":"claude-sonnet-4","remainingFraction":0.8}]}"#,
                    )
                }),
            );
        let client = AntigravityQuotaClient::with_base_url(spawn(app).await);
        let snapshot = client
            .fetch("account-1", &credentials())
            .await
            .expect("quota snapshot");

        assert_eq!(
            snapshot.groups[0].metrics[0].remaining,
            Some(QuotaAmount::Decimal(60.0))
        );
        assert_eq!(
            snapshot.groups[0].metrics[1].remaining,
            Some(QuotaAmount::Decimal(80.0))
        );
    }

    #[tokio::test]
    async fn falls_back_to_quota_buckets_when_model_discovery_is_forbidden() {
        let app = Router::new()
            .route(
                FETCH_AVAILABLE_MODELS_PATH,
                post(|| async { (StatusCode::FORBIDDEN, "forbidden") }),
            )
            .route(
                RETRIEVE_USER_QUOTA_PATH,
                post(|| async {
                    (
                        StatusCode::OK,
                        r#"{"buckets":[{"modelId":"gemini-3-pro","remainingFraction":0.4}]}"#,
                    )
                }),
            );
        let client = AntigravityQuotaClient::with_base_url(spawn(app).await);
        let snapshot = client
            .fetch("account-1", &credentials())
            .await
            .expect("quota snapshot");

        assert_eq!(snapshot.groups[0].metrics.len(), 1);
        assert_eq!(
            snapshot.groups[0].metrics[0].remaining,
            Some(QuotaAmount::Decimal(40.0))
        );
    }

    #[test]
    fn accepts_proto_remaining_oneof_and_reset_time() {
        let quotas = parse_quota_buckets(&serde_json::json!({
            "buckets": [{
                "modelId": "claude-sonnet-4",
                "remaining": {"case": "remainingFraction", "value": 0.25},
                "resetTime": "2026-01-01T00:00:00Z"
            }]
        }))
        .expect("buckets");
        let snapshot = normalize_quota("account-1", quotas).expect("snapshot");
        assert_eq!(
            snapshot.groups[0].metrics[0]
                .period
                .as_ref()
                .map(|period| period.kind),
            Some(QuotaPeriodKind::Unknown)
        );
        assert_eq!(
            snapshot.groups[0].metrics[0].remaining,
            Some(QuotaAmount::Decimal(25.0))
        );
    }
}
