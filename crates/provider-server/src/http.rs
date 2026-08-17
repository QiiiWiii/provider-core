use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{StreamExt, stream};
use provider_auth::{ApiKeyAuthenticator, AuthError, AuthService, AuthenticatedApiKey};
#[cfg(test)]
use provider_auth::{ApiKeyPatch, CreateApiKeyInput};
use provider_core::{
    ProviderError, ProviderErrorKind, ProviderStream, ProxyRequest, ProxyRequestError,
    ProxyService, WireFormat,
};
use provider_management::ProviderManager;
use provider_usage::{
    DeliveryOutcome, EndpointProtocol, ExecutionOutcome, LogicalRequestStart, LogicalTracker,
    UsageTracking,
};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tracing::error;

mod request;
mod static_ui;

#[cfg(test)]
use request::{
    CLAUDE_CODE_SESSION_HEADER, claude_code_cache_key, claude_code_session_id, proxy_request,
    unix_timestamp,
};
use request::{
    authenticate_api_key, load_key_account_filter, parse_payload, proxy_request_for_key,
    proxy_request_for_key_from_payload,
};
use static_ui::ui_service;

const CLAUDE_MODEL_PREFIX: &str = "claude-fable-5-dd-";
const PUBLIC_DIR: &str = "/app/public";
pub(crate) const MAX_PROXY_BODY_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_MANAGEMENT_BODY_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
struct AppState {
    service: ProxyService,
    api_keys: ApiKeyAuthenticator,
    /// `None` disables usage tracking entirely, which is how every path stays
    /// working when there is no database to record into.
    usage: Option<Arc<UsageTracking>>,
    proxy_readiness: ProxyReadiness,
}

#[derive(Clone)]
pub struct ProxyReadiness(Arc<AtomicBool>);

pub(crate) struct ManagementRouterConfig {
    pub(crate) usage: Option<crate::usage_http::UsageServices>,
    pub(crate) trusted_proxy_ip: Option<std::net::IpAddr>,
    pub(crate) proxy_readiness: ProxyReadiness,
}

impl ProxyReadiness {
    pub fn new(ready: bool) -> Self {
        Self(Arc::new(AtomicBool::new(ready)))
    }

    pub(crate) fn signal(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }

    fn get(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub fn router(service: ProxyService, api_keys: ApiKeyAuthenticator) -> Router {
    router_with_usage(service, api_keys, None)
}

pub fn router_with_usage(
    service: ProxyService,
    api_keys: ApiKeyAuthenticator,
    usage: Option<Arc<UsageTracking>>,
) -> Router {
    router_with_usage_and_readiness(service, api_keys, usage, ProxyReadiness::new(true))
}

fn router_with_usage_and_readiness(
    service: ProxyService,
    api_keys: ApiKeyAuthenticator,
    usage: Option<Arc<UsageTracking>>,
    proxy_readiness: ProxyReadiness,
) -> Router {
    Router::new()
        .route("/healthz", get(liveness))
        .route("/livez", get(liveness))
        .route("/readyz", get(readiness))
        .route("/v1/models", get(models))
        .route("/v1/responses", post(responses))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .layer(DefaultBodyLimit::max(MAX_PROXY_BODY_BYTES))
        .layer(middleware::from_fn(reject_compressed_request))
        .with_state(AppState {
            service,
            api_keys,
            usage,
            proxy_readiness,
        })
        .fallback_service(ui_service(PUBLIC_DIR))
}

pub fn router_with_management(
    service: ProxyService,
    manager: ProviderManager,
    auth: AuthService,
    api_keys: ApiKeyAuthenticator,
) -> Router {
    router_with_management_and_usage(service, manager, auth, api_keys, None, None)
}

pub fn router_with_management_and_usage(
    service: ProxyService,
    manager: ProviderManager,
    auth: AuthService,
    api_keys: ApiKeyAuthenticator,
    usage: Option<crate::usage_http::UsageServices>,
    trusted_proxy_ip: Option<std::net::IpAddr>,
) -> Router {
    router_with_management_usage_and_readiness(
        service,
        manager,
        auth,
        api_keys,
        ManagementRouterConfig {
            usage,
            trusted_proxy_ip,
            proxy_readiness: ProxyReadiness::new(true),
        },
    )
}

pub(crate) fn router_with_management_usage_and_readiness(
    service: ProxyService,
    manager: ProviderManager,
    auth: AuthService,
    api_keys: ApiKeyAuthenticator,
    config: ManagementRouterConfig,
) -> Router {
    let ManagementRouterConfig {
        usage,
        trusted_proxy_ip,
        proxy_readiness,
    } = config;
    let auth_state = crate::auth_http::AuthHttpState::new(
        auth.clone(),
        api_keys.clone(),
        manager.clone(),
        trusted_proxy_ip,
    );
    let mut management = crate::management_http::router(manager, usage.clone());
    if let Some(usage) = &usage {
        // Behind the same session guard as the rest of management: usage is read
        // by a logged-in person, never with a proxy API key.
        management = management.merge(crate::usage_http::router(usage.clone()));
    }
    let management = crate::auth_http::protect(management, auth)
        .layer(DefaultBodyLimit::max(MAX_MANAGEMENT_BODY_BYTES))
        .layer(middleware::from_fn(reject_compressed_request));
    router_with_usage_and_readiness(
        service,
        api_keys,
        usage.map(|usage| usage.tracking),
        proxy_readiness,
    )
    .merge(crate::auth_http::router(auth_state))
    .merge(management)
}

pub(crate) async fn reject_compressed_request(request: Request, next: Next) -> Response {
    let compressed = request
        .headers()
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|value| {
            value.to_str().map_or(true, |value| {
                value
                    .split(',')
                    .map(str::trim)
                    .any(|encoding| !encoding.eq_ignore_ascii_case("identity"))
            })
        });
    if compressed {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(json!({
                "error": {
                    "type": "invalid_request_error",
                    "message": "compressed request bodies are not supported"
                }
            })),
        )
            .into_response();
    }
    next.run(request).await
}

async fn liveness() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn readiness(State(state): State<AppState>) -> Response {
    let database_ready = state.api_keys.quota_ledger_ready().await.is_ok();
    let writer_ready = state
        .usage
        .as_ref()
        .is_none_or(|usage| usage.quota_ledger_ready());
    let providers_ready = state.proxy_readiness.get();
    let ready = database_ready && writer_ready && providers_ready;
    (
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(json!({
            "status": if ready { "ready" } else { "not_ready" },
            "database": database_ready,
            "quota_ledger": writer_ready,
            "providers": providers_ready
        })),
    )
        .into_response()
}

async fn models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    let protocol = models_protocol(&headers);
    ensure_proxy_ready(&state, protocol)?;
    let key = authenticate_api_key(&state.api_keys, &headers, protocol)?;
    let account_ids = load_key_account_filter(&state.api_keys, &key, protocol).await?;
    let models = state
        .service
        .models(key.owner_user_id.as_str(), protocol, Some(&account_ids));
    Ok(Json(match protocol {
        WireFormat::ClaudeMessages => claude_models_response(models),
        WireFormat::OpenAiResponses | WireFormat::OpenAiChatCompletions => json!({
            "object": "list",
            "data": models
        }),
    }))
}

fn models_protocol(headers: &HeaderMap) -> WireFormat {
    if headers
        .get("anthropic-version")
        .is_some_and(|value| !value.is_empty())
        || headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("claude-cli"))
    {
        WireFormat::ClaudeMessages
    } else {
        WireFormat::OpenAiResponses
    }
}

fn claude_models_response(models: Vec<provider_core::ProviderModel>) -> Value {
    let data = models
        .into_iter()
        .map(|model| {
            let id = ensure_claude_model_id(&model.id);
            let mut value = json!({
                "id": id,
                "type": "model",
                "display_name": model.id,
            });
            if let Some(created_at) = model.created.and_then(format_timestamp) {
                value["created_at"] = Value::String(created_at);
            }
            value
        })
        .collect::<Vec<_>>();
    let first_id = data
        .first()
        .and_then(|model| model.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let last_id = data
        .last()
        .and_then(|model| model.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    json!({
        "data": data,
        "has_more": false,
        "first_id": first_id,
        "last_id": last_id,
    })
}

fn format_timestamp(timestamp: u64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp(i64::try_from(timestamp).ok()?)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

fn ensure_claude_model_id(id: &str) -> String {
    if id.starts_with("claude-") {
        id.to_owned()
    } else {
        format!(
            "{CLAUDE_MODEL_PREFIX}{}",
            id.chars().rev().collect::<String>()
        )
    }
}

fn resolve_claude_model_id(id: &str) -> String {
    id.strip_prefix(CLAUDE_MODEL_PREFIX)
        .map_or_else(|| id.to_owned(), |encoded| encoded.chars().rev().collect())
}

async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HttpError> {
    proxy_stream(state, headers, body, WireFormat::OpenAiResponses).await
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HttpError> {
    proxy_stream(state, headers, body, WireFormat::OpenAiChatCompletions).await
}

async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HttpError> {
    proxy_stream(state, headers, body, WireFormat::ClaudeMessages).await
}

async fn proxy_stream(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
    protocol: WireFormat,
) -> Result<Response, HttpError> {
    ensure_proxy_ready(&state, protocol)?;
    let key = authenticate_api_key(&state.api_keys, &headers, protocol)?;
    let (payload, logical) = parse_tracked_payload(&state, &key, protocol, &body).await?;
    if let Err(error) = require_stream_true(protocol, &payload) {
        finish_before_bytes(logical.as_ref(), ExecutionOutcome::StableFailure).await;
        return Err(error);
    }
    let request = match proxy_request_for_key_from_payload(protocol, &headers, body, payload, &key)
    {
        Ok(request) => request,
        Err(error) => {
            finish_before_bytes(logical.as_ref(), ExecutionOutcome::StableFailure).await;
            return Err(error);
        }
    };
    proxy_prepared_stream(&state, &key, request, logical).await
}

fn require_stream_true(protocol: WireFormat, payload: &Value) -> Result<(), HttpError> {
    let Some(root) = payload.as_object() else {
        return Err(HttpError::invalid_request(
            protocol,
            "request body must be a JSON object",
        ));
    };
    if root.get("stream") != Some(&Value::Bool(true)) {
        return Err(HttpError::invalid_request(protocol, "stream must be true"));
    }
    Ok(())
}

async fn count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    ensure_proxy_ready(&state, WireFormat::ClaudeMessages)?;
    let key = authenticate_api_key(&state.api_keys, &headers, WireFormat::ClaudeMessages)?;
    let request = proxy_request_for_key(WireFormat::ClaudeMessages, &headers, body, &key)?;
    let account_ids =
        load_key_account_filter(&state.api_keys, &key, WireFormat::ClaudeMessages).await?;
    let count = state
        .service
        .count_tokens(key.owner_user_id.as_str(), request, Some(&account_ids))
        .await
        .map_err(|error| HttpError::from_provider(WireFormat::ClaudeMessages, error))?;

    Ok(Json(json!({ "input_tokens": count })))
}

fn ensure_proxy_ready(state: &AppState, protocol: WireFormat) -> Result<(), HttpError> {
    if state.proxy_readiness.get() {
        Ok(())
    } else {
        Err(HttpError::new(
            protocol,
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "provider runtime recovery is incomplete",
        ))
    }
}

async fn proxy_prepared_stream(
    state: &AppState,
    key: &AuthenticatedApiKey,
    request: ProxyRequest,
    logical: Option<Arc<LogicalTracker>>,
) -> Result<Response, HttpError> {
    let protocol = request.format;
    if key.quota_limit_atoms.is_some() {
        // Finite keys charge from observed usage. Without tracking there is no
        // durable spend path, so admission must fail closed.
        if logical.is_none() || state.usage.is_none() {
            let request_id = logical
                .as_ref()
                .map_or("untracked", |tracker| tracker.request_id());
            error!(
                request_id,
                api_key_id = %key.key_id,
                error = "usage tracking is unavailable",
                "quota accounting admission failed"
            );
            return Err(HttpError::service_unavailable(
                protocol,
                "quota accounting is unavailable",
            ));
        }
        match state.api_keys.admit_quota(key).await {
            Ok(()) => {}
            Err(AuthError::QuotaExceeded) => {
                finish_before_bytes(logical.as_ref(), ExecutionOutcome::StableFailure).await;
                return Err(HttpError::rate_limited(
                    protocol,
                    "API key USD quota has been exhausted",
                ));
            }
            Err(error) => {
                let request_id = logical
                    .as_ref()
                    .map_or("untracked", |tracker| tracker.request_id());
                error!(
                    request_id,
                    api_key_id = %key.key_id,
                    error = %error,
                    "quota accounting admission failed"
                );
                finish_before_bytes(logical.as_ref(), ExecutionOutcome::StableFailure).await;
                return Err(HttpError::service_unavailable(
                    protocol,
                    "quota accounting is unavailable",
                ));
            }
        }
    }
    let account_ids = match load_key_account_filter(&state.api_keys, key, protocol).await {
        Ok(account_ids) => account_ids,
        Err(error) => {
            finish_before_bytes(logical.as_ref(), ExecutionOutcome::StableFailure).await;
            return Err(error);
        }
    };
    let prepared =
        match state
            .service
            .prepare_stream(key.owner_user_id.as_str(), request, Some(&account_ids))
        {
            Ok(prepared) => prepared,
            Err(error) => {
                finish_before_bytes(logical.as_ref(), ExecutionOutcome::StableFailure).await;
                return Err(HttpError::from_provider(protocol, error));
            }
        };

    let tracking = logical.as_ref().map(LogicalTracker::request_tracking);

    if key.quota_limit_atoms.is_some() {
        let tracker = logical
            .as_ref()
            .expect("finite quota requests require a logical tracker");
        if let Err(error) = tracker.mark_quota_dispatched().await {
            error!(
                request_id = tracker.request_id(),
                api_key_id = %key.key_id,
                error = %error,
                "quota accounting dispatch marker failed"
            );
            finish_before_bytes(logical.as_ref(), ExecutionOutcome::StableFailure).await;
            return Err(HttpError::service_unavailable(
                protocol,
                "quota accounting is unavailable",
            ));
        }
    }

    let stream = match prepared.execute_stream(tracking.as_ref()).await {
        Ok(stream) => stream,
        Err(error) => {
            finish_before_bytes(logical.as_ref(), ExecutionOutcome::StableFailure).await;
            return Err(HttpError::from_provider(protocol, error));
        }
    };

    let body = Body::from_stream(observe_delivery(stream, logical));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .map_err(|_| HttpError::internal(protocol))
}

/// Parse the JSON envelope after authentication and create the logical request
/// regardless of whether parsing succeeded. Authentication is the tracking
/// boundary: a malformed request from a known key is still one user request.
async fn parse_tracked_payload(
    state: &AppState,
    key: &AuthenticatedApiKey,
    protocol: WireFormat,
    body: &Bytes,
) -> Result<(Value, Option<Arc<LogicalTracker>>), HttpError> {
    match parse_payload(protocol, body) {
        Ok(payload) => {
            let client_model_raw = payload
                .as_object()
                .and_then(|payload| payload.get("model"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let routing_model = client_model_raw.as_deref().map(|model| {
                if protocol == WireFormat::ClaudeMessages {
                    resolve_claude_model_id(model)
                } else {
                    model.to_owned()
                }
            });
            let reasoning_effort = request_reasoning_effort(&payload);
            let logical = begin_tracking(
                state,
                key,
                protocol,
                client_model_raw,
                routing_model,
                reasoning_effort,
            )
            .await?;
            Ok((payload, logical))
        }
        Err(error) => {
            let logical = begin_tracking(state, key, protocol, None, None, None).await?;
            finish_before_bytes(logical.as_ref(), ExecutionOutcome::StableFailure).await;
            Err(error)
        }
    }
}

async fn finish_before_bytes(logical: Option<&Arc<LogicalTracker>>, execution: ExecutionOutcome) {
    if let Some(logical) = logical {
        logical.record_execution(execution);
        logical.record_delivery(DeliveryOutcome::ErrorBeforeBytes);
        if let Some(receipt) = logical.finish() {
            let _ = receipt.persisted().await;
        }
    }
}

/// Record the start of a logical request, if usage is being tracked.
///
/// Ordinary usage statistics remain fail-open. Finite-quota requests instead
/// create their durable accounting claim here and fail closed before dispatch
/// when accounting is unavailable.
async fn begin_tracking(
    state: &AppState,
    key: &AuthenticatedApiKey,
    protocol: WireFormat,
    client_model_raw: Option<String>,
    routing_model: Option<String>,
    reasoning_effort: Option<String>,
) -> Result<Option<Arc<LogicalTracker>>, HttpError> {
    let Some(usage) = state.usage.as_ref() else {
        if key.quota_limit_atoms.is_some() {
            error!(
                api_key_id = %key.key_id,
                error = "usage tracking is not configured",
                "quota accounting request start failed"
            );
            return Err(HttpError::service_unavailable(
                protocol,
                "quota accounting is unavailable",
            ));
        }
        return Ok(None);
    };
    let start = LogicalRequestStart {
        request_id: uuid::Uuid::new_v4().to_string(),
        owner_user_id: key.owner_user_id.to_string(),
        api_key_id: Some(key.key_id.to_string()),
        api_key_label: Some(key.label.clone()),
        api_key_group_label: Some(key.group_label.clone()),
        endpoint: Some(match protocol {
            WireFormat::OpenAiResponses => EndpointProtocol::Responses,
            WireFormat::OpenAiChatCompletions => EndpointProtocol::ChatCompletions,
            WireFormat::ClaudeMessages => EndpointProtocol::Messages,
        }),
        client_model_raw,
        routing_model,
        reasoning_effort,
        started_at_ms: provider_usage::system_clock_ms(),
    };
    if key.quota_limit_atoms.is_some() {
        let request_id = start.request_id.clone();
        return match usage.begin_quota_request(start).await {
            Ok(logical) => Ok(Some(logical)),
            Err(error) => {
                error!(
                    request_id,
                    api_key_id = %key.key_id,
                    error = %error,
                    "quota accounting request start failed"
                );
                Err(HttpError::service_unavailable(
                    protocol,
                    "quota accounting is unavailable",
                ))
            }
        };
    }
    Ok(Some(usage.begin_request(start).await))
}

/// Capture the client-declared reasoning level without interpreting provider
/// output tokens as a request setting. Responses uses `reasoning.effort`, while
/// Chat Completions clients commonly use the flat `reasoning_effort` spelling.
fn request_reasoning_effort(payload: &Value) -> Option<String> {
    let value = payload
        .get("reasoning")
        .and_then(Value::as_object)
        .and_then(|reasoning| reasoning.get("effort"))
        .or_else(|| payload.get("reasoning_effort"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 32)?;
    Some(value.to_owned())
}

/// Wrap the response body so the logical request learns how delivery ended.
///
/// This is the only place that can tell a clean end from a client that hung up,
/// because both happen after the handler has already returned the response.
fn observe_delivery(
    stream: ProviderStream,
    logical: Option<Arc<LogicalTracker>>,
) -> ProviderStream {
    struct Delivery {
        inner: Option<ProviderStream>,
        logical: Option<Arc<LogicalTracker>>,
        sent_bytes: bool,
    }

    impl Drop for Delivery {
        fn drop(&mut self) {
            // A downstream disconnect is a cancellation boundary. Close the
            // upstream observer first so the attempt and final_attempt_id are
            // committed, then finish the logical request as client-dropped.
            drop(self.inner.take());
            if let Some(logical) = self.logical.as_ref() {
                logical.record_delivery(DeliveryOutcome::ClientDrop);
                logical.finish();
            }
        }
    }

    Box::pin(stream::unfold(
        Delivery {
            inner: Some(stream),
            logical,
            sent_bytes: false,
        },
        |mut state| async move {
            let item = match state.inner.as_mut() {
                Some(inner) => inner.next().await,
                None => return None,
            };
            match item {
                Some(Ok(chunk)) => {
                    state.sent_bytes = true;
                    Some((Ok(chunk), state))
                }
                Some(Err(error)) => {
                    // The body error is terminal to the downstream. Drop the
                    // usage observer now so the attempt closes before logical.
                    drop(state.inner.take());
                    let receipt = if let Some(logical) = state.logical.as_ref() {
                        logical.record_execution(ExecutionOutcome::TranslatorOrStreamError);
                        logical.record_delivery(if state.sent_bytes {
                            DeliveryOutcome::ErrorAfterBytes
                        } else {
                            DeliveryOutcome::ErrorBeforeBytes
                        });
                        logical.finish()
                    } else {
                        None
                    };
                    if let Some(receipt) = receipt {
                        let _ = receipt.persisted().await;
                    }
                    Some((Err(error), state))
                }
                None => {
                    drop(state.inner.take());
                    let receipt = if let Some(logical) = state.logical.as_ref() {
                        logical.record_delivery(DeliveryOutcome::CleanEof);
                        logical.finish()
                    } else {
                        None
                    };
                    if let Some(receipt) = receipt
                        && !receipt.persisted().await
                    {
                        return Some((
                            Err(ProviderError::new(
                                ProviderErrorKind::Internal,
                                "quota ledger stopped before persisting request",
                            )),
                            state,
                        ));
                    }
                    None
                }
            }
        },
    ))
}

struct HttpError {
    status: StatusCode,
    body: Value,
}

impl HttpError {
    fn authentication(protocol: WireFormat) -> Self {
        Self::new(
            protocol,
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid API key",
        )
    }

    fn invalid_request(protocol: WireFormat, message: &'static str) -> Self {
        Self::new(
            protocol,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            message,
        )
    }

    fn service_unavailable(protocol: WireFormat, message: &'static str) -> Self {
        Self::new(
            protocol,
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            message,
        )
    }

    fn internal(protocol: WireFormat) -> Self {
        Self::new(
            protocol,
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "internal server error",
        )
    }

    fn rate_limited(protocol: WireFormat, message: &str) -> Self {
        Self::new(
            protocol,
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            message,
        )
    }

    fn from_proxy_request(protocol: WireFormat, error: ProxyRequestError) -> Self {
        Self::invalid_request(
            protocol,
            match error {
                ProxyRequestError::EmptyModel => "model must be a non-empty string",
            },
        )
    }

    fn from_provider(protocol: WireFormat, error: ProviderError) -> Self {
        let (status, error_type) = match error.kind() {
            ProviderErrorKind::InvalidRequest => (StatusCode::BAD_REQUEST, "invalid_request_error"),
            ProviderErrorKind::Authentication => (StatusCode::UNAUTHORIZED, "authentication_error"),
            ProviderErrorKind::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error"),
            ProviderErrorKind::Capacity => (StatusCode::SERVICE_UNAVAILABLE, "api_error"),
            ProviderErrorKind::Upstream => (StatusCode::BAD_GATEWAY, "api_error"),
            ProviderErrorKind::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "api_error"),
        };
        Self::new(protocol, status, error_type, error.message())
    }

    fn new(protocol: WireFormat, status: StatusCode, error_type: &str, message: &str) -> Self {
        let body = match protocol {
            WireFormat::OpenAiResponses | WireFormat::OpenAiChatCompletions => json!({
                "error": { "type": error_type, "message": message }
            }),
            WireFormat::ClaudeMessages => json!({
                "type": "error",
                "error": { "type": error_type, "message": message }
            }),
        };
        Self { status, body }
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

#[cfg(test)]
#[path = "http/tests.rs"]
mod tests;
