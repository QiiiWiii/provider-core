use futures_util::TryStreamExt;
use provider_core::{
    ProviderError, ProviderErrorKind, ProviderStream, RequestMetadata, parse_provider_retry_after,
};
use secrecy::ExposeSecret;
use std::time::Duration;

use super::{
    credentials::GrokCredentials,
    failure::status_error,
    identity::{DEFAULT_PROXY_BASE_URL, inference_headers},
    request::GrokToolMappings,
    stream::restore_tool_stream,
};

const CONVERSATION_ID_HEADER: &str = "x-grok-conv-id";
const GROK_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const GROK_RESPONSE_HEADERS_TIMEOUT: Duration = Duration::from_secs(30);

/// HTTP client for the Grok CLI Responses upstream.
#[derive(Clone)]
pub struct GrokClient {
    http: reqwest::Client,
    base_url: String,
}

impl Default for GrokClient {
    fn default() -> Self {
        Self::new()
    }
}

impl GrokClient {
    #[must_use]
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_PROXY_BASE_URL)
    }

    pub async fn execute_stream(
        &self,
        credentials: &GrokCredentials,
        payload: bytes::Bytes,
        model: &str,
        metadata: &RequestMetadata,
        agent_id: &str,
        tool_mappings: GrokToolMappings,
    ) -> Result<ProviderStream, ProviderError> {
        let user_id = credentials.upstream_user_id().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Internal,
                "Grok credential is missing upstream user ID",
            )
        })?;
        let request_id = uuid::Uuid::new_v4().to_string();
        let generated_session_id = uuid::Uuid::new_v4().to_string();
        let session_id = metadata
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(&generated_session_id);
        let request = inference_headers(
            self.http
                .post(format!("{}/responses", self.base_url))
                .bearer_auth(credentials.access_token().expose_secret())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::ACCEPT, "text/event-stream"),
        )
        .header("x-grok-user-id", user_id)
        .header(CONVERSATION_ID_HEADER, session_id)
        .header("x-grok-req-id", request_id)
        .header("x-grok-model-override", model)
        .header("x-grok-session-id", session_id)
        .header("x-grok-agent-id", agent_id)
        .body(payload);

        let response = match tokio::time::timeout(GROK_RESPONSE_HEADERS_TIMEOUT, request.send())
            .await
        {
            Ok(result) => result.map_err(|error| {
                let provider_error = ProviderError::new(
                    ProviderErrorKind::Upstream,
                    format!("Grok upstream request failed: {error}"),
                );
                if error.is_connect() {
                    provider_error.with_failover_reason(
                        provider_core::ProviderFailoverReason::PreconnectFailure,
                    )
                } else {
                    provider_error
                }
            })?,
            Err(_) => {
                return Err(ProviderError::new(
                    ProviderErrorKind::Upstream,
                    "Grok upstream response headers timed out",
                )
                .with_failover_reason(provider_core::ProviderFailoverReason::CapacityExhausted));
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
            let error = match retry_after {
                Some(value) if error.retry_after().is_none() => error.with_retry_after(value),
                None => error,
                Some(_) => error,
            };
            return Err(error);
        }

        let stream = response.bytes_stream().map_err(|error| {
            ProviderError::new(
                ProviderErrorKind::Upstream,
                format!("Grok upstream stream failed: {error}"),
            )
        });

        let stream: ProviderStream = Box::pin(stream);
        Ok(restore_tool_stream(stream, tool_mappings, model))
    }

    pub(crate) fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .connect_timeout(GROK_CONNECT_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }
}
#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use axum::{
        Json, Router,
        body::{Body, Bytes, to_bytes},
        extract::{Request, State},
        http::Response,
        routing::post,
    };
    use futures_util::{StreamExt, stream};
    use provider_core::{ProviderErrorKind, ProviderRetryHint};
    use reqwest::StatusCode;
    use serde_json::Value;
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::*;

    #[derive(Clone, Debug, Default)]
    struct CapturedRequest {
        authorization: String,
        token_auth: String,
        client_version: String,
        user_agent: String,
        conversation_id: String,
        authenticate_response: String,
        client_mode: String,
        client_identifier: String,
        user_id: String,
        request_id: String,
        model_override: String,
        session_id: String,
        agent_id: String,
        connection: String,
        body: Bytes,
    }

    type Capture = Arc<Mutex<Option<CapturedRequest>>>;

    async fn streaming_handler(State(capture): State<Capture>, request: Request) -> Response<Body> {
        let headers = request.headers().clone();
        let body = to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("request body");

        *capture.lock().expect("capture lock") = Some(CapturedRequest {
            authorization: header(&headers, reqwest::header::AUTHORIZATION.as_str()),
            token_auth: header(&headers, "x-xai-token-auth"),
            client_version: header(&headers, "x-grok-client-version"),
            user_agent: header(&headers, reqwest::header::USER_AGENT.as_str()),
            conversation_id: header(&headers, CONVERSATION_ID_HEADER),
            authenticate_response: header(&headers, "x-authenticateresponse"),
            client_mode: header(&headers, "x-grok-client-mode"),
            client_identifier: header(&headers, "x-grok-client-identifier"),
            user_id: header(&headers, "x-grok-user-id"),
            request_id: header(&headers, "x-grok-req-id"),
            model_override: header(&headers, "x-grok-model-override"),
            session_id: header(&headers, "x-grok-session-id"),
            agent_id: header(&headers, "x-grok-agent-id"),
            connection: header(&headers, reqwest::header::CONNECTION.as_str()),
            body,
        });

        let chunks = stream::iter([
            Ok::<_, std::convert::Infallible>(Bytes::from_static(
                b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_test\"}}\n\n",
            )),
            Ok(Bytes::from_static(
                b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_test\",\"output\":[]}}\n\n",
            )),
        ]);

        Response::builder()
            .header(reqwest::header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(chunks))
            .expect("streaming response")
    }

    async fn unauthorized_handler() -> StatusCode {
        StatusCode::UNAUTHORIZED
    }

    async fn invalid_encrypted_content_handler() -> (StatusCode, Json<Value>) {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "code": "invalid-argument",
                "error": "Could not decrypt the provided encrypted_content."
            })),
        )
    }

    async fn bad_credentials_handler() -> (StatusCode, Json<Value>) {
        (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "code": "unauthenticated:bad-credentials",
                "error": "The OAuth2 access token could not be validated."
            })),
        )
    }

    async fn unavailable_handler() -> (StatusCode, Json<Value>) {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "temporarily unavailable"})),
        )
    }

    async fn free_usage_exhausted_handler()
    -> (StatusCode, [(&'static str, &'static str); 1], Json<Value>) {
        (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "1")],
            Json(serde_json::json!({
                "code": "subscription:free-usage-exhausted",
                "error": "You've used all the included free usage for now."
            })),
        )
    }

    async fn entitlement_handler() -> (StatusCode, Json<Value>) {
        (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "code": "subscription_required",
                "error": "A subscription is required for this account."
            })),
        )
    }

    async fn content_policy_handler() -> (StatusCode, Json<Value>) {
        (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "code": "content_policy_violation",
                "error": "The request was rejected by the content policy."
            })),
        )
    }

    async fn proxied_free_usage_handler() -> (StatusCode, &'static str) {
        (
            StatusCode::BAD_GATEWAY,
            "subscription:free-usage-exhausted for model grok-4.5",
        )
    }

    fn header(headers: &reqwest::header::HeaderMap, name: &str) -> String {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    async fn spawn_server(router: Router) -> (String, JoinHandle<std::io::Result<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock upstream");
        let address = listener.local_addr().expect("mock upstream address");
        let handle = tokio::spawn(async move { axum::serve(listener, router).await });

        (format!("http://{address}/v1"), handle)
    }

    #[tokio::test]
    async fn sends_required_headers_and_streams_chunks() {
        let capture = Capture::default();
        let router = Router::new()
            .route("/v1/responses", post(streaming_handler))
            .with_state(capture.clone());
        let (base_url, server) = spawn_server(router).await;
        let client = GrokClient::with_base_url(base_url);
        let credentials = GrokCredentials::from_access_token("upstream-token");
        let payload = Bytes::from_static(br#"{"model":"grok-4.5","stream":true}"#);
        let mut metadata = RequestMetadata::default();
        metadata.session_id = Some("session-1".to_owned());

        let chunks = client
            .execute_stream(
                &credentials,
                payload.clone(),
                "grok-4.5",
                &metadata,
                "stable-account-agent-id",
                GrokToolMappings::default(),
            )
            .await
            .expect("stream response")
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("stream chunks");

        server.abort();

        let captured = capture
            .lock()
            .expect("capture lock")
            .clone()
            .expect("captured request");
        assert_eq!(captured.authorization, "Bearer upstream-token");
        assert_eq!(captured.token_auth, "xai-grok-cli");
        assert_eq!(captured.client_version, "0.2.105");
        assert!(captured.user_agent.starts_with("grok-shell/0.2.105 ("));
        assert_eq!(captured.conversation_id, "session-1");
        assert_eq!(captured.authenticate_response, "authenticate-response");
        assert_eq!(captured.client_mode, "headless");
        assert_eq!(captured.client_identifier, "grok-shell");
        assert_eq!(captured.user_id, "test-user");
        assert!(!captured.request_id.is_empty());
        assert_eq!(captured.model_override, "grok-4.5");
        assert_eq!(captured.session_id, "session-1");
        assert_eq!(captured.agent_id, "stable-account-agent-id");
        assert!(captured.connection.is_empty());
        assert_eq!(captured.body, payload);
        assert_eq!(chunks.len(), 2);
    }

    #[tokio::test]
    async fn maps_unauthorized_status_without_response_body() {
        let router = Router::new().route("/v1/responses", post(unauthorized_handler));
        let (base_url, server) = spawn_server(router).await;
        let client = GrokClient::with_base_url(base_url);
        let credentials = GrokCredentials::from_access_token("upstream-token");

        let error = match client
            .execute_stream(
                &credentials,
                Bytes::from_static(b"{}"),
                "grok-4.5",
                &RequestMetadata::default(),
                "stable-account-agent-id",
                GrokToolMappings::default(),
            )
            .await
        {
            Ok(_) => panic!("expected unauthorized response"),
            Err(error) => error,
        };

        server.abort();

        assert_eq!(error.kind(), ProviderErrorKind::Authentication);
        assert_eq!(
            error.message(),
            "Grok upstream returned HTTP 401 Unauthorized"
        );
        assert!(!error.message().contains("upstream-token"));
    }

    async fn execute_error(handler: axum::routing::MethodRouter) -> ProviderError {
        let router = Router::new().route("/v1/responses", handler);
        let (base_url, server) = spawn_server(router).await;
        let client = GrokClient::with_base_url(base_url);
        let credentials = GrokCredentials::from_access_token("upstream-token");
        let error = match client
            .execute_stream(
                &credentials,
                Bytes::from_static(b"{}"),
                "grok-4.5",
                &RequestMetadata::default(),
                "stable-account-agent-id",
                GrokToolMappings::default(),
            )
            .await
        {
            Ok(_) => panic!("expected upstream error"),
            Err(error) => error,
        };
        server.abort();
        error
    }

    #[tokio::test]
    async fn marks_only_decrypt_failures_for_reasoning_recovery() {
        let error = execute_error(post(invalid_encrypted_content_handler)).await;

        assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
        assert_eq!(error.upstream_status(), Some(400));
        assert_eq!(
            error.retry_hint(),
            Some(ProviderRetryHint::StripEncryptedReasoning)
        );
        assert!(error.message().contains("encrypted_content"));
    }

    #[tokio::test]
    async fn maps_grok_bad_credentials_for_refresh() {
        let error = execute_error(post(bad_credentials_handler)).await;

        assert_eq!(error.kind(), ProviderErrorKind::Authentication);
        assert_eq!(error.upstream_status(), Some(401));
        assert_eq!(error.retry_hint(), None);
        assert!(!error.message().contains("OAuth2 access token"));
    }

    #[tokio::test]
    async fn marks_server_errors_for_account_failover() {
        let error = execute_error(post(unavailable_handler)).await;

        assert_eq!(error.kind(), ProviderErrorKind::Upstream);
        assert_eq!(error.upstream_status(), Some(503));
        assert_eq!(
            error.failover_reason(),
            Some(provider_core::ProviderFailoverReason::CapacityExhausted)
        );
        assert!(error.message().contains("temporarily unavailable"));
    }

    #[tokio::test]
    async fn preserves_reviewed_free_usage_cooldown() {
        let error = execute_error(post(free_usage_exhausted_handler)).await;

        assert_eq!(error.kind(), ProviderErrorKind::RateLimited);
        assert_eq!(
            error.failover_reason(),
            Some(provider_core::ProviderFailoverReason::QuotaExhausted)
        );
        assert_eq!(error.retry_after(), Some(Duration::from_secs(24 * 60 * 60)));
    }

    #[tokio::test]
    async fn fails_over_entitlement_without_misclassifying_content_policy() {
        let entitlement = execute_error(post(entitlement_handler)).await;
        assert_eq!(entitlement.kind(), ProviderErrorKind::RateLimited);
        assert_eq!(entitlement.upstream_status(), Some(403));
        assert_eq!(
            entitlement.failover_reason(),
            Some(provider_core::ProviderFailoverReason::QuotaExhausted)
        );
        assert_eq!(
            entitlement.retry_after(),
            Some(Duration::from_secs(30 * 60))
        );

        let policy = execute_error(post(content_policy_handler)).await;
        assert_eq!(policy.kind(), ProviderErrorKind::Authentication);
        assert_eq!(policy.upstream_status(), Some(403));
        assert_eq!(policy.failover_reason(), None);
    }

    #[tokio::test]
    async fn classifies_free_usage_from_body_before_transport_status() {
        let error = execute_error(post(proxied_free_usage_handler)).await;
        assert_eq!(error.kind(), ProviderErrorKind::RateLimited);
        assert_eq!(
            error.failover_reason(),
            Some(provider_core::ProviderFailoverReason::QuotaExhausted)
        );
        assert_eq!(error.retry_after(), Some(Duration::from_secs(24 * 60 * 60)));
    }
}
