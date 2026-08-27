use axum::{
    Json,
    extract::State,
    http::{HeaderMap, header},
};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use provider_core::WireFormat;

use super::request::{authenticate_api_key, claude_code_models_request, load_key_account_filter};
use super::{AppState, HttpError, readiness::ensure_proxy_ready};

const CLAUDE_MODEL_PREFIX: &str = "claude-fable-5-dd-";

pub(super) async fn models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, HttpError> {
    let protocol = models_protocol(&headers);
    ensure_proxy_ready(&state, protocol)?;
    let key = authenticate_api_key(&state.api_keys, &headers, protocol)?;
    let account_ids = load_key_account_filter(&state.api_keys, &key, protocol).await?;
    let metadata = claude_code_models_request(&headers);
    let models = state.service.models_for_request(
        key.owner_user_id.as_str(),
        protocol,
        Some(&account_ids),
        &metadata,
    );
    Ok(Json(match protocol {
        WireFormat::ClaudeMessages => claude_models_response(models),
        WireFormat::OpenAiResponses | WireFormat::OpenAiChatCompletions => json!({
            "object": "list",
            "data": models
        }),
    }))
}

pub(super) fn models_protocol(headers: &HeaderMap) -> WireFormat {
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

pub(super) fn resolve_claude_model_id(id: &str) -> String {
    id.strip_prefix(CLAUDE_MODEL_PREFIX)
        .map_or_else(|| id.to_owned(), |encoded| encoded.chars().rev().collect())
}
