use std::{
    collections::{HashMap, HashSet},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD as BASE64, STANDARD_NO_PAD},
};
use provider_core::ProviderError;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::media::{convert_content, convert_image_part};
use super::tools::mapped_tool_name;
use super::validation::{invalid, required_string};

pub(super) fn stable_session_id(contents: &[Value]) -> String {
    let text = contents
        .iter()
        .find(|content| content.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        });
    if let Some(text) = text {
        let digest = Sha256::digest(text.as_bytes());
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        return format!("-{}", u64::from_be_bytes(bytes) & i64::MAX as u64);
    }
    let value = uuid::Uuid::new_v4().as_u128() as u64 & i64::MAX as u64;
    format!("-{value}")
}

pub(super) fn unix_timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

pub(super) fn append_input_item(
    item: &Value,
    contents: &mut Vec<Value>,
    system_parts: &mut Vec<Value>,
    function_names: &mut HashMap<String, String>,
    flattened_calls: &mut HashSet<String>,
    tool_names: &HashMap<String, String>,
    pending_signature: &mut Option<String>,
    target_is_claude: bool,
) -> Result<(), ProviderError> {
    let object = item
        .as_object()
        .ok_or_else(|| invalid("OpenAI Responses input items must be objects"))?;
    let item_type = object
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| object.get("role").map(|_| "message"))
        .unwrap_or_default();
    match item_type {
        "additional_tools" | "compaction" | "compaction_summary" => return Ok(()),
        "message" => {
            let role = object
                .get("role")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("OpenAI Responses message role is required"))?;
            let role = match role {
                "assistant" | "model" => "model",
                "user" => "user",
                "developer" | "system" => "system",
                _ => return Err(invalid("unsupported OpenAI Responses message role")),
            };
            let mut parts = convert_content(object.get("content"), role)?;
            if role == "model"
                && let Some(signature) = pending_signature.take()
            {
                attach_signature(&mut parts, signature);
            }
            if role == "system" {
                system_parts.extend(parts);
            } else if !parts.is_empty() {
                contents.push(content(role, parts));
            }
        }
        "reasoning" => {
            let mut parts = Vec::new();
            let signature = object
                .get("encrypted_content")
                .and_then(Value::as_str)
                .and_then(|value| normalize_reasoning_signature(value, target_is_claude));
            if let Some(summary) = object.get("summary").and_then(Value::as_array) {
                for value in summary {
                    let text = value
                        .as_str()
                        .or_else(|| value.get("text").and_then(Value::as_str));
                    if let Some(text) = text
                        && !text.is_empty()
                    {
                        if target_is_claude && signature.is_none() {
                            continue;
                        }
                        let mut part = json!({"text": text, "thought": true});
                        if let Some(signature) = signature
                            .as_deref()
                            .filter(|value| !value.trim().is_empty())
                        {
                            part["thoughtSignature"] = Value::String(signature.to_owned());
                        }
                        parts.push(part);
                    }
                }
            }
            if !parts.is_empty() {
                contents.push(content("model", parts));
            }
            if let Some(signature) = signature.filter(|value| !value.trim().is_empty()) {
                *pending_signature = Some(signature);
            }
        }
        "function_call" | "custom_tool_call" => {
            let call_id = required_string(object, "call_id", "function_call requires call_id")?;
            let raw_name = required_string(object, "name", "function_call requires name")?;
            let name = mapped_tool_name(object, raw_name, tool_names);
            let hosted_history = is_hosted_history_function(&name);
            let signature = if hosted_history {
                None
            } else {
                object
                    .get("_cpa_reasoning_signature")
                    .or_else(|| object.get("thought_signature"))
                    .or_else(|| object.get("thoughtSignature"))
                    .or_else(|| object.get("signature"))
                    .and_then(Value::as_str)
                    .and_then(|value| normalize_reasoning_signature(value, target_is_claude))
                    .or_else(|| pending_signature.take())
            };
            let arguments = if item_type == "custom_tool_call" {
                let input = object
                    .get("input")
                    .cloned()
                    .unwrap_or(Value::String(String::new()));
                if input.is_string() {
                    json!({"input": input})
                } else {
                    input
                }
                .to_string()
            } else {
                object
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}")
                    .to_owned()
            };
            let arguments: Value = serde_json::from_str(&arguments)
                .map_err(|_| invalid("function_call arguments must be valid JSON"))?;
            function_names.insert(call_id.to_owned(), name.clone());
            let signature = signature.filter(|value| !value.trim().is_empty());
            if !target_is_claude && signature.is_none() {
                flattened_calls.insert(call_id.to_owned());
                contents.push(content(
                    "model",
                    vec![json!({
                        "text": format!("Called `{name}` with {arguments}")
                    })],
                ));
            } else {
                let mut part = json!({
                    "functionCall": {
                        "id": call_id,
                        "name": name,
                        "args": arguments
                    }
                });
                if let Some(signature) = signature {
                    part["thoughtSignature"] = Value::String(signature);
                }
                contents.push(content("model", vec![part]));
            }
        }
        "function_call_output" | "custom_tool_call_output" => {
            let call_id =
                required_string(object, "call_id", "function_call_output requires call_id")?;
            let raw_name = object
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .or_else(|| function_names.get(call_id).cloned())
                .unwrap_or_else(|| "unknown".to_owned());
            let name = mapped_tool_name(object, &raw_name, tool_names);
            let output = object
                .get("output")
                .or_else(|| object.get("input"))
                .cloned()
                .unwrap_or(Value::Null);
            let (result, media_parts) = convert_tool_output(output);
            if flattened_calls.contains(call_id) {
                let mut parts = vec![json!({
                    "text": format!("Result from `{name}`: {result}")
                })];
                parts.extend(media_parts);
                contents.push(content("user", parts));
            } else {
                let mut function_response = json!({
                    "functionResponse": {
                        "id": call_id,
                        "name": name,
                        "response": {"result": result}
                    }
                });
                if !media_parts.is_empty() {
                    function_response["functionResponse"]["parts"] = Value::Array(media_parts);
                }
                contents.push(content("user", vec![function_response]));
            }
        }
        "computer_call" | "computer_call_output" => {
            return Err(invalid(
                "Antigravity does not support OpenAI computer call items",
            ));
        }
        _ => return Err(invalid("unsupported OpenAI Responses input item type")),
    }
    Ok(())
}

pub(super) fn append_instruction_parts(
    value: &Value,
    output: &mut Vec<Value>,
) -> Result<(), ProviderError> {
    match value {
        Value::String(text) => {
            if !text.is_empty() {
                output.push(json!({"text": text}));
            }
        }
        Value::Array(values) => {
            for value in values {
                if let Some(text) = value.as_str() {
                    if !text.is_empty() {
                        output.push(json!({"text": text}));
                    }
                } else if let Some(text) = value.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        output.push(json!({"text": text}));
                    }
                } else {
                    return Err(invalid("OpenAI Responses instructions must contain text"));
                }
            }
        }
        _ => return Err(invalid("OpenAI Responses instructions must be text")),
    }
    Ok(())
}

fn attach_signature(parts: &mut [Value], signature: String) {
    if let Some(part) = parts.iter_mut().find(|part| {
        part.get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty())
    }) {
        part["thoughtSignature"] = Value::String(signature);
    }
}

fn convert_tool_output(output: Value) -> (Value, Vec<Value>) {
    let parsed = match output {
        Value::String(text) => serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)),
        value => value,
    };
    match parsed {
        Value::Array(values) => {
            let mut text = Vec::new();
            let mut raw = Vec::new();
            let mut media = Vec::new();
            for value in values {
                if let Some(object) = value.as_object() {
                    let kind = object
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if matches!(kind, "input_image" | "image_url" | "image")
                        && let Ok(part) = convert_image_part(object)
                    {
                        media.push(part);
                        continue;
                    }
                    if matches!(kind, "text" | "input_text" | "output_text")
                        && let Some(value) = object.get("text").and_then(Value::as_str)
                    {
                        text.push(value.to_owned());
                        continue;
                    }
                }
                raw.push(value);
            }
            let result = if !raw.is_empty() {
                Value::Array(raw)
            } else {
                Value::String(text.join("\n"))
            };
            (result, media)
        }
        Value::Object(object)
            if matches!(
                object.get("type").and_then(Value::as_str),
                Some("input_image" | "image_url" | "image")
            ) =>
        {
            match convert_image_part(&object) {
                Ok(part) => (Value::String(String::new()), vec![part]),
                Err(_) => (Value::Object(object), Vec::new()),
            }
        }
        value => (value, Vec::new()),
    }
}

fn decode_reasoning_carrier(value: &str) -> Option<String> {
    let value = value.trim();
    let payload = value.strip_prefix("cpa-gemini-responses-carrier-v1:")?;
    let (direction, rest) = payload.split_once(':')?;
    if !matches!(direction, "next" | "previous" | "standalone") {
        return None;
    }
    let (target, encoded) = rest.split_once(':')?;
    if !matches!(target, "text" | "function" | "any") {
        return None;
    }
    let decoded = STANDARD_NO_PAD.decode(encoded).ok()?;
    if decoded.is_empty() {
        return None;
    }
    let signature = String::from_utf8(decoded).ok()?;
    (!signature.starts_with("cpa-gemini-responses-carrier-v1:")).then_some(signature)
}

fn normalize_reasoning_signature(value: &str, target_is_claude: bool) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.starts_with("cpa-gemini-responses-carrier-v1:") {
        return decode_reasoning_carrier(value)
            .and_then(|signature| normalize_reasoning_signature(&signature, target_is_claude));
    }
    let value = strip_signature_provider_prefix(value);
    if target_is_claude {
        return normalize_antigravity_claude_signature(value);
    }
    if value.starts_with("gpt#") || value.starts_with("claude#") {
        return None;
    }
    if is_foreign_gemini_thought_signature(value) {
        return None;
    }
    Some(value.to_owned())
}

fn is_hosted_history_function(name: &str) -> bool {
    matches!(
        name,
        "web_search"
            | "file_search"
            | "computer"
            | "code_interpreter"
            | "image_generation"
            | "local_shell"
            | "apply_patch"
            | "program"
            | "tool_search"
    )
}

fn is_foreign_gemini_thought_signature(value: &str) -> bool {
    if value.starts_with("gAAAA") || matches!(value.len(), 4_340 | 12_946) {
        return true;
    }
    if value.contains('=') || value.contains('-') || value.contains('_') {
        return false;
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
    {
        return false;
    }
    STANDARD_NO_PAD
        .decode(value)
        .is_ok_and(|decoded| decoded.len() >= 32)
}

fn strip_signature_provider_prefix(value: &str) -> &str {
    for prefix in ["claude-cais#", "ccmax#", "cais#", "claude#", "gemini#"] {
        if let Some(value) = value.strip_prefix(prefix) {
            return value;
        }
    }
    value
}

fn normalize_antigravity_claude_signature(value: &str) -> Option<String> {
    if value.starts_with('R') {
        let decoded = BASE64.decode(value).ok()?;
        if decoded.first().copied() != Some(b'E') {
            return None;
        }
        let inner = String::from_utf8(decoded).ok()?;
        let payload = BASE64.decode(&inner).ok()?;
        return (payload.first().copied() == Some(0x12)).then(|| value.to_owned());
    }
    if !value.starts_with('E') {
        return None;
    }
    let payload = BASE64.decode(value).ok()?;
    (payload.first().copied() == Some(0x12)).then(|| BASE64.encode(value.as_bytes()))
}

pub(super) fn content(role: &str, parts: Vec<Value>) -> Value {
    json!({"role": role, "parts": parts})
}
