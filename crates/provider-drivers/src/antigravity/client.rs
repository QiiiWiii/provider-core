use std::time::Duration;

use futures_util::TryStreamExt;
use provider_core::{
    ProviderError, ProviderErrorKind, ProviderFailoverReason, ProviderRequest, ProviderStream,
    collect_bounded_body, parse_provider_retry_after,
};
use secrecy::ExposeSecret;

use super::{
    contract::{API_BASE_URL, API_VERSION, DAILY_API_BASE_URL},
    credentials::AntigravityCredentials,
    request::{prepare_count_tokens_request, prepare_request},
    response::translate_stream,
    version,
};

const MAX_ERROR_RESPONSE_SIZE: usize = 256 * 1024;
const RESPONSE_HEADERS_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(crate) struct AntigravityClient {
    http: reqwest::Client,
    base_urls: Vec<String>,
    dynamic_version: bool,
}

impl Default for AntigravityClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AntigravityClient {
    pub(crate) fn new() -> Self {
        Self::with_base_urls_and_version(
            [DAILY_API_BASE_URL.to_owned(), API_BASE_URL.to_owned()],
            true,
        )
    }

    pub(crate) fn with_base_url(base_url: impl Into<String>) -> Self {
        Self::with_base_urls([base_url.into()])
    }

    fn with_base_urls<const N: usize>(base_urls: [String; N]) -> Self {
        Self::with_base_urls_and_version(base_urls, false)
    }

    fn with_base_urls_and_version<const N: usize>(
        base_urls: [String; N],
        dynamic_version: bool,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .http1_only()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base_urls: base_urls
                .into_iter()
                .map(|value| value.trim_end_matches('/').to_owned())
                .filter(|value| !value.is_empty())
                .collect(),
            dynamic_version,
        }
    }

    fn user_agent(&self) -> String {
        if self.dynamic_version {
            version::user_agent()
        } else {
            version::fallback_user_agent().to_owned()
        }
    }

    pub(crate) async fn execute_stream(
        &self,
        credentials: &AntigravityCredentials,
        request: &ProviderRequest,
    ) -> Result<ProviderStream, ProviderError> {
        let payload = prepare_request(request, credentials.project_id())?;
        let user_agent = self.user_agent();
        let mut last_error = None;
        for (index, base_url) in self.base_urls.iter().enumerate() {
            let url = format!("{base_url}/{API_VERSION}:streamGenerateContent?alt=sse");
            let response = tokio::time::timeout(
                RESPONSE_HEADERS_TIMEOUT,
                self.http
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .header(reqwest::header::ACCEPT, "text/event-stream")
                    .header(reqwest::header::USER_AGENT, &user_agent)
                    .bearer_auth(credentials.access_token().expose_secret())
                    .body(payload.clone())
                    .send(),
            )
            .await;
            let response = match response {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    let provider_error = ProviderError::new(
                        ProviderErrorKind::Upstream,
                        format!("Antigravity upstream request failed: {error}"),
                    );
                    if error.is_connect() && index + 1 < self.base_urls.len() {
                        last_error = Some(provider_error);
                        continue;
                    }
                    return Err(if error.is_connect() {
                        provider_error
                            .with_failover_reason(ProviderFailoverReason::PreconnectFailure)
                    } else {
                        provider_error
                    });
                }
                Err(_) => {
                    let error = ProviderError::new(
                        ProviderErrorKind::Upstream,
                        "Antigravity upstream response headers timed out",
                    )
                    .with_failover_reason(ProviderFailoverReason::CapacityExhausted);
                    if index + 1 < self.base_urls.len() {
                        last_error = Some(error);
                        continue;
                    }
                    return Err(error);
                }
            };
            let status = response.status();
            if !status.is_success() {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_provider_retry_after);
                let error = status_error(response, status).await;
                if index + 1 < self.base_urls.len() && should_try_fallback(status, &error) {
                    last_error = Some(error);
                    continue;
                }
                return Err(match retry_after {
                    Some(value) if error.retry_after().is_none() => error.with_retry_after(value),
                    _ => error,
                });
            }
            let stream = response.bytes_stream().map_err(|error| {
                ProviderError::new(
                    ProviderErrorKind::Upstream,
                    format!("Antigravity upstream stream failed: {error}"),
                )
            });
            return Ok(translate_stream(
                Box::pin(stream),
                request.model.clone(),
                &request.payload,
            ));
        }
        Err(last_error.unwrap_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Upstream,
                "Antigravity has no upstream endpoint",
            )
        }))
    }

    pub(crate) async fn count_tokens(
        &self,
        credentials: &AntigravityCredentials,
        request: &ProviderRequest,
    ) -> Result<u64, ProviderError> {
        let payload = prepare_count_tokens_request(request)?;
        let user_agent = self.user_agent();
        let mut last_error = None;
        for (index, base_url) in self.base_urls.iter().enumerate() {
            let url = format!("{base_url}/{API_VERSION}:countTokens");
            let response = tokio::time::timeout(
                RESPONSE_HEADERS_TIMEOUT,
                self.http
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .header(reqwest::header::USER_AGENT, &user_agent)
                    .bearer_auth(credentials.access_token().expose_secret())
                    .body(payload.clone())
                    .send(),
            )
            .await;
            let response = match response {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    let provider_error = ProviderError::new(
                        ProviderErrorKind::Upstream,
                        format!("Antigravity countTokens request failed: {error}"),
                    );
                    if error.is_connect() && index + 1 < self.base_urls.len() {
                        last_error = Some(provider_error);
                        continue;
                    }
                    return Err(if error.is_connect() {
                        provider_error
                            .with_failover_reason(ProviderFailoverReason::PreconnectFailure)
                    } else {
                        provider_error
                    });
                }
                Err(_) => {
                    let error = ProviderError::new(
                        ProviderErrorKind::Upstream,
                        "Antigravity countTokens response headers timed out",
                    )
                    .with_failover_reason(ProviderFailoverReason::CapacityExhausted);
                    if index + 1 < self.base_urls.len() {
                        last_error = Some(error);
                        continue;
                    }
                    return Err(error);
                }
            };
            let status = response.status();
            if !status.is_success() {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_provider_retry_after);
                let error = status_error(response, status).await;
                if index + 1 < self.base_urls.len() && should_try_fallback(status, &error) {
                    last_error = Some(error);
                    continue;
                }
                return Err(match retry_after {
                    Some(value) if error.retry_after().is_none() => error.with_retry_after(value),
                    _ => error,
                });
            }
            let body = collect_bounded_body(response.bytes_stream(), MAX_ERROR_RESPONSE_SIZE)
                .await
                .map_err(|_| {
                    ProviderError::new(
                        ProviderErrorKind::Upstream,
                        "Antigravity countTokens response could not be read",
                    )
                })?;
            let value = serde_json::from_slice::<serde_json::Value>(&body).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Upstream,
                    "Antigravity countTokens response was not valid JSON",
                )
            })?;
            return value
                .get("totalTokens")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    ProviderError::new(
                        ProviderErrorKind::Upstream,
                        "Antigravity countTokens response is missing totalTokens",
                    )
                });
        }
        Err(last_error.unwrap_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Upstream,
                "Antigravity has no upstream endpoint",
            )
        }))
    }
}

async fn status_error(response: reqwest::Response, status: reqwest::StatusCode) -> ProviderError {
    let body = collect_bounded_body(response.bytes_stream(), MAX_ERROR_RESPONSE_SIZE)
        .await
        .unwrap_or_else(|_| bytes::Bytes::new());
    let parsed = serde_json::from_slice::<serde_json::Value>(&body).ok();
    let markers = parsed.as_ref().map(error_markers).unwrap_or_default();
    let message = parsed
        .as_ref()
        .and_then(|value| value.get("message").or_else(|| value.get("error")))
        .and_then(|value| value.as_str().or_else(|| value.get("message")?.as_str()))
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("Antigravity upstream request failed");
    let mut error = match status.as_u16() {
        401 => ProviderError::new(ProviderErrorKind::Authentication, message)
            .with_failover_reason(ProviderFailoverReason::AuthenticationExhausted),
        403 if markers.iter().any(|marker| {
            marker.contains("subscription")
                || marker.contains("unauthenticated")
                || marker.contains("permission_denied")
                || marker.contains("invalid_credentials")
        }) =>
        {
            ProviderError::new(ProviderErrorKind::Authentication, message)
                .with_failover_reason(ProviderFailoverReason::AuthenticationExhausted)
        }
        429 if markers.iter().any(|marker| {
            marker.contains("quota_exhausted")
                || marker.contains("resource_exhausted")
                || marker.contains("insufficient_g1_credits_balance")
                || marker.contains("quota exhausted")
        }) =>
        {
            ProviderError::new(ProviderErrorKind::Capacity, message)
                .with_failover_reason(ProviderFailoverReason::QuotaExhausted)
        }
        429 => ProviderError::new(ProviderErrorKind::RateLimited, message)
            .with_failover_reason(ProviderFailoverReason::RateLimited),
        500..=599 => ProviderError::new(ProviderErrorKind::Capacity, message)
            .with_failover_reason(ProviderFailoverReason::CapacityExhausted),
        400..=499 => ProviderError::new(ProviderErrorKind::InvalidRequest, message),
        _ => ProviderError::new(ProviderErrorKind::Upstream, message),
    };
    error = error.with_upstream_status(status.as_u16());
    error
}

fn should_try_fallback(status: reqwest::StatusCode, error: &ProviderError) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
        || error.failover_reason() == Some(ProviderFailoverReason::CapacityExhausted)
}

fn error_markers(value: &serde_json::Value) -> Vec<String> {
    let mut markers = Vec::new();
    collect_error_markers(value, &mut markers);
    markers
}

fn collect_error_markers(value: &serde_json::Value, markers: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                if matches!(key.as_str(), "code" | "status" | "reason" | "message") {
                    match value {
                        serde_json::Value::String(value) => {
                            markers.push(value.to_ascii_lowercase());
                        }
                        serde_json::Value::Number(value) => markers.push(value.to_string()),
                        _ => {}
                    }
                }
                collect_error_markers(value, markers);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_error_markers(value, markers);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Router, body::to_bytes, extract::Request, http::StatusCode, response::IntoResponse,
        routing::post,
    };
    use futures_util::StreamExt;
    use provider_core::{ProviderErrorKind, ProviderFailoverReason, RequestMetadata, WireFormat};
    use secrecy::SecretString;
    use serde_json::Value;
    use tokio::net::TcpListener;

    use super::*;

    #[derive(Clone, Default)]
    struct Calls {
        daily: Arc<Mutex<usize>>,
        production: Arc<Mutex<Vec<Value>>>,
    }

    async fn spawn(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(axum::serve(listener, router).into_future());
        format!("http://{address}")
    }

    #[tokio::test]
    async fn falls_back_from_daily_quota_to_production_and_preserves_cpa_body() {
        let calls = Calls::default();
        let daily_calls = calls.daily.clone();
        let production_calls = calls.production.clone();
        let app = Router::new().route(
            "/{*path}",
            post(move |request: Request| {
                let daily_calls = daily_calls.clone();
                let production_calls = production_calls.clone();
                async move {
                    let path = request.uri().path().to_owned();
                    let body = to_bytes(request.into_body(), 1 << 20)
                        .await
                        .expect("body");
                    if path.starts_with("/daily-") {
                        *daily_calls.lock().expect("daily lock") += 1;
                        (
                            StatusCode::TOO_MANY_REQUESTS,
                            [("content-type", "application/json")],
                            r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"quota"}}"#,
                        )
                            .into_response()
                    } else {
                        production_calls
                            .lock()
                            .expect("production lock")
                            .push(serde_json::from_slice(&body).expect("request JSON"));
                        (
                            StatusCode::OK,
                            [("content-type", "text/event-stream")],
                            "data: {\"response\":{\"responseId\":\"native-1\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}}\n\n",
                        )
                            .into_response()
                    }
                }
            }),
        );
        let base = spawn(app).await;
        let client = AntigravityClient::with_base_urls([
            format!("{base}/daily-"),
            format!("{base}/production-"),
        ]);
        let credentials = AntigravityCredentials::from_json(&SecretString::from(
            serde_json::json!({
                "type": "antigravity",
                "access_token": "access",
                "refresh_token": "refresh",
                "project_id": "project-1"
            })
            .to_string(),
        ))
        .expect("credentials");
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "gemini-3-flash".to_owned(),
            payload: bytes::Bytes::from_static(br#"{"input":"hello"}"#),
            metadata: RequestMetadata::default(),
        };
        let mut stream = client
            .execute_stream(&credentials, &request)
            .await
            .expect("fallback stream");
        while stream.next().await.is_some() {}
        assert_eq!(*calls.daily.lock().expect("daily lock"), 1);
        let production = calls.production.lock().expect("production lock");
        assert_eq!(production.len(), 1);
        assert_eq!(production[0]["project"], "project-1");
        assert_eq!(production[0]["requestType"], "agent");
        assert!(
            production[0]["requestId"]
                .as_str()
                .is_some_and(|value| value.starts_with("agent-"))
        );
    }

    #[tokio::test]
    async fn counts_tokens_through_cpa_endpoint_and_falls_back_to_production() {
        let calls = Calls::default();
        let daily_calls = calls.daily.clone();
        let production_calls = calls.production.clone();
        let app = Router::new().route(
            "/{*path}",
            post(move |request: Request| {
                let daily_calls = daily_calls.clone();
                let production_calls = production_calls.clone();
                async move {
                    let path = request.uri().path().to_owned();
                    let body = to_bytes(request.into_body(), 1 << 20)
                        .await
                        .expect("body");
                    if path.starts_with("/daily-") {
                        *daily_calls.lock().expect("daily lock") += 1;
                        (
                            StatusCode::TOO_MANY_REQUESTS,
                            [("content-type", "application/json")],
                            r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"quota"}}"#,
                        )
                            .into_response()
                    } else {
                        production_calls
                            .lock()
                            .expect("production lock")
                            .push(serde_json::from_slice(&body).expect("request JSON"));
                        (
                            StatusCode::OK,
                            [("content-type", "application/json")],
                            r#"{"totalTokens":42}"#,
                        )
                            .into_response()
                    }
                }
            }),
        );
        let base = spawn(app).await;
        let client = AntigravityClient::with_base_urls([
            format!("{base}/daily-"),
            format!("{base}/production-"),
        ]);
        let credentials = AntigravityCredentials::from_json(&SecretString::from(
            serde_json::json!({
                "type": "antigravity",
                "access_token": "access",
                "refresh_token": "refresh",
                "project_id": "project-1"
            })
            .to_string(),
        ))
        .expect("credentials");
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "gemini-3-flash".to_owned(),
            payload: bytes::Bytes::from_static(br#"{"input":"hello"}"#),
            metadata: RequestMetadata::default(),
        };
        let count = client
            .count_tokens(&credentials, &request)
            .await
            .expect("count tokens");
        assert_eq!(count, 42);
        assert_eq!(*calls.daily.lock().expect("daily lock"), 1);
        let production = calls.production.lock().expect("production lock");
        assert_eq!(production.len(), 1);
        assert!(production[0].get("project").is_none());
        assert!(production[0].get("model").is_none());
        assert!(production[0].get("requestType").is_none());
        assert!(production[0]["request"].get("sessionId").is_none());
        assert_eq!(production[0]["request"]["contents"][0]["role"], "user");
    }

    #[tokio::test]
    async fn classifies_cpa_http_errors_for_routing() {
        let app = Router::new()
            .route(
                "/quota",
                post(|| async {
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        r#"{"error":{"status":"RESOURCE_EXHAUSTED","message":"quota"}}"#,
                    )
                }),
            )
            .route(
                "/auth",
                post(|| async {
                    (
                        StatusCode::UNAUTHORIZED,
                        r#"{"error":{"status":"UNAUTHENTICATED","message":"expired"}}"#,
                    )
                }),
            );
        let base = spawn(app).await;
        let quota_response = reqwest::Client::new()
            .post(format!("{base}/quota"))
            .send()
            .await
            .expect("quota response");
        let quota = status_error(quota_response, StatusCode::TOO_MANY_REQUESTS).await;
        assert_eq!(quota.kind(), ProviderErrorKind::Capacity);
        assert_eq!(
            quota.failover_reason(),
            Some(ProviderFailoverReason::QuotaExhausted)
        );
        let auth_response = reqwest::Client::new()
            .post(format!("{base}/auth"))
            .send()
            .await
            .expect("auth response");
        let auth = status_error(auth_response, StatusCode::UNAUTHORIZED).await;
        assert_eq!(auth.kind(), ProviderErrorKind::Authentication);
        assert_eq!(
            auth.failover_reason(),
            Some(ProviderFailoverReason::AuthenticationExhausted)
        );
        assert!(should_try_fallback(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ProviderError::new(ProviderErrorKind::Capacity, "capacity")
        ));
    }
}
