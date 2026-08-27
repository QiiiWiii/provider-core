use std::sync::Arc;

use axum::{
    Json,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use provider_auth::{AuthError, AuthenticatedApiKey};
use provider_core::{ProxyRequest, WireFormat};
use provider_usage::{ExecutionOutcome, LogicalTracker};
use serde_json::{Value, json};
use tracing::error;

use super::claude_code;
use super::readiness::ensure_proxy_ready;
use super::request::{
    authenticate_api_key, load_key_account_filter, parse_payload,
    proxy_request_for_key_from_payload, proxy_request_for_key_from_payload_with_count_tokens,
};
use super::tracking::{finish_before_bytes, observe_delivery, parse_tracked_payload};
use super::{AppState, HttpError};

#[derive(Clone, Copy, Eq, PartialEq)]
enum ProxyResponseMode {
    EventStream,
    Json,
}

pub(super) async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HttpError> {
    proxy_stream(state, headers, body, WireFormat::OpenAiResponses).await
}

pub(super) async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HttpError> {
    proxy_stream(state, headers, body, WireFormat::OpenAiChatCompletions).await
}

pub(super) async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HttpError> {
    proxy_stream(state, headers, body, WireFormat::ClaudeMessages).await
}

pub(super) async fn proxy_stream(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
    protocol: WireFormat,
) -> Result<Response, HttpError> {
    ensure_proxy_ready(&state, protocol)?;
    let key = authenticate_api_key(&state.api_keys, &headers, protocol)?;
    let (payload, logical) = parse_tracked_payload(&state, &key, protocol, &headers, &body).await?;
    let response_mode = match proxy_response_mode(protocol, &headers, &body, &payload) {
        Ok(mode) => mode,
        Err(error) => {
            finish_before_bytes(logical.as_ref(), ExecutionOutcome::StableFailure).await;
            return Err(error);
        }
    };
    let request = match proxy_request_for_key_from_payload(protocol, &headers, body, payload, &key)
    {
        Ok(request) => request,
        Err(error) => {
            finish_before_bytes(logical.as_ref(), ExecutionOutcome::StableFailure).await;
            return Err(error);
        }
    };
    proxy_prepared_stream(&state, &key, request, logical, response_mode).await
}

fn proxy_response_mode(
    protocol: WireFormat,
    headers: &HeaderMap,
    body: &[u8],
    payload: &Value,
) -> Result<ProxyResponseMode, HttpError> {
    if payload.as_object().and_then(|root| root.get("stream")) == Some(&Value::Bool(true)) {
        return Ok(ProxyResponseMode::EventStream);
    }
    if protocol == WireFormat::ClaudeMessages && claude_code::non_streaming_helper(headers, body) {
        return Ok(ProxyResponseMode::Json);
    }
    require_stream_true(protocol, payload)?;
    Ok(ProxyResponseMode::EventStream)
}

pub(super) fn require_stream_true(protocol: WireFormat, payload: &Value) -> Result<(), HttpError> {
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

pub(super) async fn count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, HttpError> {
    ensure_proxy_ready(&state, WireFormat::ClaudeMessages)?;
    let key = authenticate_api_key(&state.api_keys, &headers, WireFormat::ClaudeMessages)?;
    let payload = parse_payload(WireFormat::ClaudeMessages, &body)?;
    let request = proxy_request_for_key_from_payload_with_count_tokens(
        WireFormat::ClaudeMessages,
        &headers,
        body,
        payload,
        &key,
    )?;
    let account_ids =
        load_key_account_filter(&state.api_keys, &key, WireFormat::ClaudeMessages).await?;
    let count = state
        .service
        .count_tokens(key.owner_user_id.as_str(), request, Some(&account_ids))
        .await
        .map_err(|error| HttpError::from_provider(WireFormat::ClaudeMessages, error))?;

    Ok(Json(json!({ "input_tokens": count })))
}

async fn proxy_prepared_stream(
    state: &AppState,
    key: &AuthenticatedApiKey,
    request: ProxyRequest,
    logical: Option<Arc<LogicalTracker>>,
    response_mode: ProxyResponseMode,
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
    let mut response = Response::builder().status(StatusCode::OK).header(
        header::CONTENT_TYPE,
        match response_mode {
            ProxyResponseMode::EventStream => "text/event-stream",
            ProxyResponseMode::Json => "application/json",
        },
    );
    if response_mode == ProxyResponseMode::EventStream {
        response = response.header(header::CACHE_CONTROL, "no-cache");
    }
    response
        .body(body)
        .map_err(|_| HttpError::internal(protocol))
}
