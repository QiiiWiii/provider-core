use std::sync::Arc;

use axum::{
    body::Bytes,
    http::{HeaderMap, header},
};
use futures_util::{StreamExt, stream};
use provider_auth::AuthenticatedApiKey;
use provider_core::{ProviderError, ProviderErrorKind, ProviderStream, RequestClient, WireFormat};
use provider_usage::{
    DeliveryOutcome, EndpointProtocol, ExecutionOutcome, LogicalRequestStart, LogicalTracker,
};
use serde_json::Value;
use tracing::error;

use super::AppState;
use super::HttpError;
use super::claude_code;
use super::models::resolve_claude_model_id;
use super::request::parse_payload;

#[derive(Clone)]
struct RequestClientSnapshot {
    user_agent: Option<String>,
    client_type: RequestClient,
}

/// Parse the JSON envelope after authentication and create the logical request
/// regardless of whether parsing succeeded. Authentication is the tracking
/// boundary: a malformed request from a known key is still one user request.
pub(super) async fn parse_tracked_payload(
    state: &AppState,
    key: &AuthenticatedApiKey,
    protocol: WireFormat,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<(Value, Option<Arc<LogicalTracker>>), HttpError> {
    let request_client = RequestClientSnapshot {
        user_agent: request_user_agent(headers),
        client_type: request_client_type(protocol, headers, body),
    };
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
                request_client.clone(),
            )
            .await?;
            Ok((payload, logical))
        }
        Err(error) => {
            let logical =
                begin_tracking(state, key, protocol, None, None, None, request_client).await?;
            finish_before_bytes(logical.as_ref(), ExecutionOutcome::StableFailure).await;
            Err(error)
        }
    }
}

pub(super) async fn finish_before_bytes(
    logical: Option<&Arc<LogicalTracker>>,
    execution: ExecutionOutcome,
) {
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
    request_client: RequestClientSnapshot,
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
        user_agent: request_client.user_agent,
        client_type: request_client.client_type,
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

fn request_user_agent(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .map(ToOwned::to_owned)
}

fn request_client_type(protocol: WireFormat, headers: &HeaderMap, body: &[u8]) -> RequestClient {
    if protocol == WireFormat::ClaudeMessages {
        claude_code::request_metadata(headers, body, false).client
    } else {
        RequestClient::Unknown
    }
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
pub(super) fn observe_delivery(
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
