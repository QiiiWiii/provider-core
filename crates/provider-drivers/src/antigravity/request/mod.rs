mod config;
mod content;
mod media;
mod schema;
mod tools;
mod validation;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

use std::collections::HashMap;

use bytes::Bytes;
use provider_core::{ProviderError, ProviderErrorKind, ProviderRequest};
use serde_json::{Map, Value, json};

use self::{
    config::convert_generation_config,
    content::{
        append_input_item, append_instruction_parts, content, stable_session_id,
        unix_timestamp_millis,
    },
    tools::{convert_tool_choice, convert_tools},
    validation::{invalid, validate_continuation_input},
};

#[derive(Clone)]
pub(crate) struct ToolIdentity {
    pub(crate) name: String,
    pub(crate) namespace: Option<String>,
    pub(crate) custom: bool,
}

pub(crate) struct ToolCatalog {
    pub(crate) declarations: Vec<Value>,
    pub(crate) web_search: bool,
    pub(crate) names: HashMap<String, String>,
    pub(crate) identities: HashMap<String, ToolIdentity>,
}

pub(crate) use tools::response_tool_identities;

pub(crate) fn prepare_request(
    request: &ProviderRequest,
    project_id: &str,
) -> Result<Bytes, ProviderError> {
    let project_id = project_id.trim();
    if project_id.is_empty() {
        return Err(invalid("Antigravity credential is missing project_id"));
    }
    let root: Value = serde_json::from_slice(&request.payload)
        .map_err(|_| invalid("OpenAI Responses request body must be valid JSON"))?;
    let root = root
        .as_object()
        .ok_or_else(|| invalid("OpenAI Responses request body must be a JSON object"))?;
    validate_continuation_input(request, root)?;
    let target_is_claude = request.model.to_ascii_lowercase().contains("claude");
    let tool_catalog = convert_tools(root, target_is_claude)?;
    let mut system_parts = Vec::new();
    if let Some(instructions) = root.get("instructions") {
        append_instruction_parts(instructions, &mut system_parts)?;
    }
    let mut contents = Vec::new();
    let mut function_names = HashMap::new();
    let mut pending_signature = None;
    match root.get("input") {
        Some(Value::Array(items)) => {
            for item in items {
                append_input_item(
                    item,
                    &mut contents,
                    &mut system_parts,
                    &mut function_names,
                    &tool_catalog.names,
                    &mut pending_signature,
                    target_is_claude,
                )?;
            }
        }
        Some(Value::String(text)) => {
            if !text.is_empty() {
                contents.push(content("user", vec![json!({"text": text})]));
            }
        }
        Some(Value::Null) | None => {}
        Some(_) => return Err(invalid("OpenAI Responses input must be a string or array")),
    }
    if !request.model.to_ascii_lowercase().contains("claude")
        && contents
            .first()
            .and_then(|content| content.get("role"))
            .and_then(Value::as_str)
            != Some("user")
    {
        contents.insert(0, content("user", vec![json!({"text": ""})]));
    }

    let stable_session = stable_session_id(&contents);
    let mut upstream_request = Map::new();
    upstream_request.insert("contents".to_owned(), Value::Array(contents));
    if !system_parts.is_empty() {
        upstream_request.insert(
            "systemInstruction".to_owned(),
            json!({"parts": system_parts}),
        );
    }
    let mut upstream_tools = Vec::new();
    if !tool_catalog.declarations.is_empty() {
        upstream_tools.push(json!({"functionDeclarations": tool_catalog.declarations}));
    }
    if tool_catalog.web_search && tool_catalog.declarations.is_empty() {
        upstream_tools.push(json!({"googleSearch": {}}));
    }
    if !upstream_tools.is_empty() {
        upstream_request.insert("tools".to_owned(), Value::Array(upstream_tools));
    }
    if let Some(tool_choice) = root.get("tool_choice") {
        upstream_request.insert(
            "toolConfig".to_owned(),
            json!({
                "functionCallingConfig": convert_tool_choice(tool_choice, &tool_catalog.names)?
            }),
        );
    } else if let Some(tool_config) = root.get("toolConfig").filter(|value| value.is_object()) {
        upstream_request.insert("toolConfig".to_owned(), tool_config.clone());
    }
    if let Some(generation_config) = convert_generation_config(root)? {
        upstream_request.insert("generationConfig".to_owned(), generation_config);
    }
    let is_claude_model = request.model.to_ascii_lowercase().contains("claude");
    if !is_claude_model
        && let Some(Value::Object(config)) = upstream_request.get_mut("generationConfig")
    {
        config.remove("maxOutputTokens");
    }
    if is_claude_model && !tool_catalog.declarations.is_empty() {
        upstream_request
            .entry("toolConfig".to_owned())
            .or_insert_with(|| json!({"functionCallingConfig": {}}));
        if let Some(mode) = upstream_request
            .get_mut("toolConfig")
            .and_then(Value::as_object_mut)
            .and_then(|tool_config| tool_config.get_mut("functionCallingConfig"))
            .and_then(Value::as_object_mut)
        {
            mode.insert("mode".to_owned(), Value::String("VALIDATED".to_owned()));
        }
    }
    let request_type = root
        .get("requestType")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if request.model.to_ascii_lowercase().contains("image") {
                "image_gen".to_owned()
            } else if tool_catalog.web_search && tool_catalog.declarations.is_empty() {
                "web_search".to_owned()
            } else {
                "agent".to_owned()
            }
        });

    if request_type != "web_search" {
        let session_id = root
            .get("sessionId")
            .or_else(|| root.get("session_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                request
                    .metadata
                    .session_id
                    .as_deref()
                    .or(request.metadata.routing_session_id.as_deref())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
            })
            .unwrap_or(stable_session);
        upstream_request.insert("sessionId".to_owned(), Value::String(session_id));
    }
    let request_id = if request.model.to_ascii_lowercase().contains("image") {
        Some(format!(
            "image_gen/{}/{}/12",
            unix_timestamp_millis(),
            uuid::Uuid::new_v4()
        ))
    } else if request_type == "web_search" {
        None
    } else {
        Some(format!("agent-{}", uuid::Uuid::new_v4()))
    };

    let mut body = Map::new();
    body.insert("project".to_owned(), Value::String(project_id.to_owned()));
    body.insert("request".to_owned(), Value::Object(upstream_request));
    body.insert("model".to_owned(), Value::String(request.model.clone()));
    body.insert(
        "userAgent".to_owned(),
        Value::String("antigravity".to_owned()),
    );
    body.insert("requestType".to_owned(), Value::String(request_type));
    if let Some(request_id) = request_id {
        body.insert("requestId".to_owned(), Value::String(request_id));
    }
    serde_json::to_vec(&Value::Object(body))
        .map(Bytes::from)
        .map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Internal,
                "failed to serialize Antigravity request",
            )
        })
}

pub(crate) fn prepare_count_tokens_request(
    request: &ProviderRequest,
) -> Result<Bytes, ProviderError> {
    let envelope = prepare_request(request, "count-tokens")?;
    let mut envelope: Value = serde_json::from_slice(&envelope).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Internal,
            "failed to decode prepared Antigravity count request",
        )
    })?;
    let mut upstream_request = envelope
        .as_object_mut()
        .and_then(|object| object.remove("request"))
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Internal,
                "prepared Antigravity count request is missing request",
            )
        })?;
    if let Some(object) = upstream_request.as_object_mut() {
        object.remove("sessionId");
    }
    serde_json::to_vec(&json!({"request": upstream_request}))
        .map(Bytes::from)
        .map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Internal,
                "failed to serialize Antigravity count request",
            )
        })
}
