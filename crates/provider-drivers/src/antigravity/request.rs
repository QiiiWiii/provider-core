use std::{
    collections::{BTreeSet, HashMap, HashSet},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD as BASE64, STANDARD_NO_PAD},
};
use bytes::Bytes;
use provider_core::{ProviderError, ProviderErrorKind, ProviderRequest};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

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

struct ToolDescriptor {
    name: String,
    local_name: Option<String>,
    namespace: Option<String>,
    description: Option<Value>,
    parameters: Option<Value>,
    custom: bool,
}

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

fn stable_session_id(contents: &[Value]) -> String {
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

fn unix_timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

fn append_input_item(
    item: &Value,
    contents: &mut Vec<Value>,
    system_parts: &mut Vec<Value>,
    function_names: &mut HashMap<String, String>,
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
        "additional_tools" => return Ok(()),
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
            let signature = object
                .get("_cpa_reasoning_signature")
                .or_else(|| object.get("thought_signature"))
                .or_else(|| object.get("thoughtSignature"))
                .or_else(|| object.get("signature"))
                .and_then(Value::as_str)
                .and_then(|value| normalize_reasoning_signature(value, target_is_claude))
                .or_else(|| pending_signature.take());
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
            let mut part = json!({
                "functionCall": {
                    "id": call_id,
                    "name": name,
                    "args": arguments
                }
            });
            if let Some(signature) = signature.filter(|value| !value.trim().is_empty()) {
                part["thoughtSignature"] = Value::String(signature);
            }
            contents.push(content("model", vec![part]));
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
        "computer_call" | "computer_call_output" => {
            return Err(invalid(
                "Antigravity does not support OpenAI computer call items",
            ));
        }
        _ => return Err(invalid("unsupported OpenAI Responses input item type")),
    }
    Ok(())
}

fn append_instruction_parts(value: &Value, output: &mut Vec<Value>) -> Result<(), ProviderError> {
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

fn convert_content(value: Option<&Value>, role: &str) -> Result<Vec<Value>, ProviderError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    match value {
        Value::String(text) => Ok((!text.is_empty())
            .then(|| json!({"text": text}))
            .into_iter()
            .collect()),
        Value::Array(values) => values
            .iter()
            .map(|value| convert_content_part(value, role))
            .collect(),
        Value::Null => Ok(Vec::new()),
        _ => Err(invalid("OpenAI Responses message content is invalid")),
    }
}

fn convert_content_part(value: &Value, role: &str) -> Result<Value, ProviderError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("OpenAI Responses content parts must be objects"))?;
    match object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "text" | "input_text" | "output_text" | "refusal" => Ok(json!({
            "text": object.get("text").and_then(Value::as_str).unwrap_or_default()
        })),
        "input_image" | "image_url" => convert_image_part(object),
        "input_audio" | "audio" => convert_audio_part(object),
        "input_file" | "file" | "input_video" | "video" => convert_file_part(object),
        _ if role == "system" => Err(invalid("unsupported OpenAI Responses instruction part")),
        _ => Err(invalid("unsupported OpenAI Responses content part")),
    }
}

fn convert_image_part(object: &Map<String, Value>) -> Result<Value, ProviderError> {
    let image = object
        .get("image_url")
        .or_else(|| object.get("url"))
        .and_then(|value| value.as_str().or_else(|| value.get("url")?.as_str()))
        .or_else(|| object.get("file_id").and_then(Value::as_str))
        .ok_or_else(|| invalid("OpenAI Responses image part requires an image URL"))?;
    convert_media_value(image, "image/png", "image")
}

fn convert_audio_part(object: &Map<String, Value>) -> Result<Value, ProviderError> {
    let value = object
        .get("data")
        .or_else(|| object.get("audio_url"))
        .or_else(|| object.get("url"))
        .and_then(|value| value.as_str().or_else(|| value.get("url")?.as_str()))
        .ok_or_else(|| invalid("OpenAI Responses audio part requires data or a URL"))?;
    let format = object
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let default_mime = audio_mime_type(format);
    if value.starts_with("data:") {
        return convert_media_value(value, default_mime, "audio");
    }
    if object.get("data").is_some() {
        return Ok(json!({
            "inlineData": {"mimeType": default_mime, "data": value}
        }));
    }
    Ok(json!({"fileData": {"fileUri": value}}))
}

fn convert_file_part(object: &Map<String, Value>) -> Result<Value, ProviderError> {
    let value = object
        .get("file_data")
        .or_else(|| object.get("file_url"))
        .or_else(|| object.get("video_url"))
        .or_else(|| object.get("url"))
        .or_else(|| object.get("file_id"))
        .and_then(|value| value.as_str().or_else(|| value.get("url")?.as_str()))
        .ok_or_else(|| invalid("OpenAI Responses file part requires a URL or file data"))?;
    let mime_type = object
        .get("mime_type")
        .or_else(|| object.get("media_type"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("application/octet-stream");
    convert_media_value(value, mime_type, "file")
}

fn convert_media_value(
    value: &str,
    default_mime: &str,
    kind: &str,
) -> Result<Value, ProviderError> {
    if let Some((mime_type, data)) = value.strip_prefix("data:").and_then(decode_data_url) {
        return Ok(json!({"inlineData": {"mimeType": mime_type, "data": data}}));
    }
    if value.trim().is_empty() {
        return Err(invalid(match kind {
            "audio" => "OpenAI Responses audio part is empty",
            "file" => "OpenAI Responses file part is empty",
            _ => "OpenAI Responses image part is empty",
        }));
    }
    if kind == "audio" && value.starts_with("base64,") {
        return Ok(json!({
            "inlineData": {"mimeType": default_mime, "data": value.trim_start_matches("base64,")}
        }));
    }
    Ok(json!({"fileData": {"fileUri": value}}))
}

fn audio_mime_type(format: &str) -> &str {
    match format.trim().to_ascii_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "webm" => "audio/webm",
        "pcm16" => "audio/pcm",
        "g711_ulaw" | "g711_alaw" => "audio/basic",
        _ => "audio/wav",
    }
}

fn decode_data_url(value: &str) -> Option<(&str, String)> {
    let (metadata, encoded) = value.split_once(',')?;
    let mime_type = metadata
        .strip_prefix("data:")
        .unwrap_or(metadata)
        .split(';')
        .next()?
        .trim();
    let encoded = encoded.trim();
    let data = if metadata.contains(";base64") {
        BASE64.decode(encoded).ok()?
    } else {
        encoded.as_bytes().to_vec()
    };
    Some((mime_type, BASE64.encode(data)))
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
    Some(value.to_owned())
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

fn convert_tools(
    root: &Map<String, Value>,
    require_validated_placeholder: bool,
) -> Result<ToolCatalog, ProviderError> {
    let mut descriptors = Vec::new();
    let mut web_search = false;
    append_tool_source(root.get("tools"), &mut descriptors, &mut web_search)?;
    if let Some(Value::Array(items)) = root.get("input") {
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
                append_tool_source(item.get("tools"), &mut descriptors, &mut web_search)?;
            }
        }
    }

    let names = build_tool_name_map(&descriptors);
    let identities = descriptors
        .iter()
        .filter_map(|descriptor| {
            let upstream_name = names.get(&descriptor.name)?.clone();
            Some((
                upstream_name,
                ToolIdentity {
                    name: descriptor
                        .local_name
                        .clone()
                        .unwrap_or_else(|| descriptor.name.clone()),
                    namespace: descriptor.namespace.clone(),
                    custom: descriptor.custom,
                },
            ))
        })
        .collect();
    let mut declarations = Vec::new();
    let mut seen = BTreeSet::new();
    for descriptor in descriptors {
        if !seen.insert(descriptor.name.clone()) {
            continue;
        }
        let name = names
            .get(&descriptor.name)
            .cloned()
            .unwrap_or_else(|| sanitize_function_name(&descriptor.name));
        let parameters = if descriptor.custom {
            json!({
                "type": "object",
                "properties": {"input": {"type": "string"}},
                "required": ["input"]
            })
        } else {
            descriptor
                .parameters
                .unwrap_or_else(|| json!({"type":"object","properties":{}}))
        };
        let mut declaration = Map::new();
        declaration.insert("name".to_owned(), Value::String(name));
        if let Some(description) = descriptor.description {
            declaration.insert("description".to_owned(), description);
        }
        declaration.insert(
            "parameters".to_owned(),
            sanitize_schema(parameters, require_validated_placeholder),
        );
        declarations.push(Value::Object(declaration));
    }
    Ok(ToolCatalog {
        declarations,
        web_search,
        names,
        identities,
    })
}

pub(crate) fn response_tool_identities(payload: &[u8]) -> HashMap<String, ToolIdentity> {
    let Ok(Value::Object(root)) = serde_json::from_slice(payload) else {
        return HashMap::new();
    };
    convert_tools(&root, false)
        .map(|catalog| catalog.identities)
        .unwrap_or_default()
}

fn append_tool_source(
    value: Option<&Value>,
    descriptors: &mut Vec<ToolDescriptor>,
    web_search: &mut bool,
) -> Result<(), ProviderError> {
    let Some(value) = value else {
        return Ok(());
    };
    let tools = value
        .as_array()
        .ok_or_else(|| invalid("OpenAI Responses tools must be an array"))?;
    for tool in tools {
        let object = tool
            .as_object()
            .ok_or_else(|| invalid("OpenAI Responses tools must contain objects"))?;
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "function" | "custom" | "" => {
                descriptors.push(tool_descriptor(tool, None, kind == "custom")?);
            }
            "namespace" => {
                let namespace =
                    required_string(object, "name", "OpenAI namespace tool requires a name")?;
                let children = object
                    .get("tools")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid("OpenAI namespace tool requires a tools array"))?;
                for child in children {
                    let child_object = child
                        .as_object()
                        .ok_or_else(|| invalid("OpenAI namespace tools must contain objects"))?;
                    let child_kind = child_object
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("function");
                    if !matches!(child_kind, "function" | "custom" | "") {
                        continue;
                    }
                    let child_name = tool_name(child_object)
                        .ok_or_else(|| invalid("OpenAI namespace tool requires child names"))?;
                    let qualified = qualify_tool_name(namespace, child_name);
                    descriptors.push(tool_descriptor(
                        child,
                        Some((qualified, child_name.to_owned(), namespace.to_owned())),
                        child_kind == "custom",
                    )?);
                }
            }
            "web_search_preview" | "web_search_preview_2025_03_11" | "web_search" => {
                *web_search = true;
            }
            _ => return Err(invalid("unsupported OpenAI Responses tool type")),
        }
    }
    Ok(())
}

fn tool_descriptor(
    tool: &Value,
    qualified_name: Option<(String, String, String)>,
    custom: bool,
) -> Result<ToolDescriptor, ProviderError> {
    let object = tool
        .as_object()
        .ok_or_else(|| invalid("OpenAI Responses tools must contain objects"))?;
    let (name, local_name, namespace) = if let Some((name, local_name, namespace)) = qualified_name
    {
        (name, Some(local_name), Some(namespace))
    } else {
        (
            tool_name(object)
                .ok_or_else(|| invalid("OpenAI function tool requires a name"))?
                .to_owned(),
            None,
            None,
        )
    };
    let function = object.get("function").and_then(Value::as_object);
    let description = object
        .get("description")
        .or_else(|| function.and_then(|value| value.get("description")))
        .cloned();
    let parameters = [
        object.get("parameters"),
        object.get("parametersJsonSchema"),
        object.get("input_schema"),
        function.and_then(|value| value.get("parameters")),
        function.and_then(|value| value.get("parametersJsonSchema")),
    ]
    .into_iter()
    .flatten()
    .next()
    .cloned();
    Ok(ToolDescriptor {
        name,
        local_name,
        namespace,
        description,
        parameters,
        custom,
    })
}

fn tool_name(object: &Map<String, Value>) -> Option<&str> {
    object
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| {
            object
                .get("function")
                .and_then(|value| value.get("name"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn qualify_tool_name(namespace: &str, child: &str) -> String {
    let namespace = namespace.trim();
    let child = child.trim();
    if namespace.is_empty()
        || child.is_empty()
        || child.starts_with("mcp__")
        || child == namespace
        || child.starts_with(&format!("{namespace}__"))
    {
        return child.to_owned();
    }
    if namespace.ends_with("__") {
        format!("{namespace}{child}")
    } else {
        format!("{namespace}__{child}")
    }
}

fn build_tool_name_map(descriptors: &[ToolDescriptor]) -> HashMap<String, String> {
    let unique_names = descriptors
        .iter()
        .map(|descriptor| descriptor.name.clone())
        .collect::<BTreeSet<_>>();
    let mut base_counts = HashMap::new();
    for name in &unique_names {
        *base_counts
            .entry(sanitize_function_name(name))
            .or_insert(0_usize) += 1;
    }
    let mut used = HashMap::new();
    let mut result = HashMap::new();
    for name in unique_names {
        let base = sanitize_function_name(&name);
        let mapped = if base_counts.get(&base) == Some(&1) && !used.contains_key(&base) {
            base
        } else {
            disambiguate_tool_name(&base, &name, &used)
        };
        used.insert(mapped.clone(), name.clone());
        result.insert(name, mapped);
    }
    for descriptor in descriptors {
        if let Some(local_name) = descriptor.local_name.as_ref()
            && !result.contains_key(local_name)
            && let Some(mapped) = result.get(&descriptor.name).cloned()
        {
            result.insert(local_name.clone(), mapped);
        }
    }
    result
}

fn disambiguate_tool_name(base: &str, original: &str, used: &HashMap<String, String>) -> String {
    for attempt in 0_u32.. {
        let digest = Sha256::digest(format!("{original}\0{attempt}"));
        let suffix = digest[..6]
            .iter()
            .map(|value| format!("{value:02x}"))
            .collect::<String>();
        let suffix = format!("_{suffix}");
        let prefix = &base[..base.len().min(64 - suffix.len())];
        let candidate = format!("{prefix}{suffix}");
        if !used.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!("tool name disambiguation must find a candidate")
}

fn sanitize_function_name(name: &str) -> String {
    let mut sanitized = name
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() || matches!(value, '_' | '.' | ':' | '-') {
                value
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        sanitized.push('_');
    }
    if !sanitized
        .chars()
        .next()
        .is_some_and(|value| value.is_ascii_alphabetic() || value == '_')
    {
        sanitized.insert(0, '_');
    }
    sanitized.truncate(64);
    sanitized
}

fn mapped_tool_name(
    object: &Map<String, Value>,
    name: &str,
    tool_names: &HashMap<String, String>,
) -> String {
    let qualified = object
        .get("namespace")
        .and_then(Value::as_str)
        .map(|namespace| qualify_tool_name(namespace, name));
    qualified
        .as_ref()
        .and_then(|value| tool_names.get(value))
        .or_else(|| tool_names.get(name))
        .cloned()
        .unwrap_or_else(|| sanitize_function_name(name))
}

fn convert_tool_choice(
    value: &Value,
    tool_names: &HashMap<String, String>,
) -> Result<Value, ProviderError> {
    let (mode, allowed_name) = match value {
        Value::String(value) => match value.as_str() {
            "auto" => ("AUTO", None),
            "required" | "any" => ("ANY", None),
            "none" => ("NONE", None),
            _ => return Err(invalid("unsupported OpenAI tool_choice value")),
        },
        Value::Object(object) => {
            let kind = object
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match kind {
                "none" => ("NONE", None),
                "auto" => ("AUTO", None),
                "required" | "any" => ("ANY", None),
                "function" | "custom" | "tool" | "" => {
                    let name = object
                        .get("name")
                        .or_else(|| object.get("function").and_then(|value| value.get("name")))
                        .or_else(|| object.get("custom").and_then(|value| value.get("name")))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| invalid("OpenAI function tool_choice requires a name"))?;
                    let namespace = object
                        .get("namespace")
                        .or_else(|| {
                            object
                                .get("function")
                                .and_then(|value| value.get("namespace"))
                        })
                        .or_else(|| {
                            object
                                .get("custom")
                                .and_then(|value| value.get("namespace"))
                        })
                        .and_then(Value::as_str);
                    let name = namespace
                        .map(|namespace| qualify_tool_name(namespace, name))
                        .unwrap_or_else(|| name.to_owned());
                    let name = tool_names
                        .get(&name)
                        .cloned()
                        .unwrap_or_else(|| sanitize_function_name(&name));
                    ("ANY", Some(name))
                }
                _ => return Err(invalid("unsupported OpenAI tool_choice value")),
            }
        }
        _ => return Err(invalid("unsupported OpenAI tool_choice value")),
    };
    let mut config = Map::new();
    config.insert("mode".to_owned(), Value::String(mode.to_owned()));
    if let Some(allowed_name) = allowed_name {
        config.insert(
            "allowedFunctionNames".to_owned(),
            Value::Array(vec![Value::String(allowed_name)]),
        );
    }
    Ok(Value::Object(config))
}

fn sanitize_schema(value: Value, require_validated_placeholder: bool) -> Value {
    let value = sanitize_schema_inner(inline_local_refs(value), false);
    if require_validated_placeholder {
        add_validated_placeholders(value)
    } else {
        value
    }
}

fn sanitize_response_schema(value: Value) -> Value {
    sanitize_schema_inner(inline_local_refs(value), true)
}

fn inline_local_refs(value: Value) -> Value {
    let definitions = value
        .get("$defs")
        .or_else(|| value.get("definitions"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    resolve_local_refs(value, &definitions, &mut HashSet::new())
}

fn resolve_local_refs(
    value: Value,
    definitions: &Map<String, Value>,
    resolving: &mut HashSet<String>,
) -> Value {
    match value {
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| resolve_local_refs(value, definitions, resolving))
                .collect(),
        ),
        Value::Object(mut object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str)
                && let Some(name) = reference
                    .strip_prefix("#/$defs/")
                    .or_else(|| reference.strip_prefix("#/definitions/"))
                && let Some(target) = definitions.get(name)
                && resolving.insert(name.to_owned())
            {
                let mut resolved = resolve_local_refs(target.clone(), definitions, resolving);
                resolving.remove(name);
                if let Value::Object(resolved_object) = &mut resolved {
                    object.remove("$ref");
                    for (key, value) in object {
                        resolved_object
                            .insert(key, resolve_local_refs(value, definitions, resolving));
                    }
                    return resolved;
                }
                return resolved;
            }
            Value::Object(
                object
                    .into_iter()
                    .map(|(key, value)| (key, resolve_local_refs(value, definitions, resolving)))
                    .collect(),
            )
        }
        value => value,
    }
}

fn sanitize_schema_inner(value: Value, response: bool) -> Value {
    let Value::Object(mut object) = value else {
        return value;
    };

    if let Some(constant) = object.remove("const")
        && !object.contains_key("enum")
    {
        object.insert("enum".to_owned(), Value::Array(vec![constant]));
    }

    if let Some(schema_type) = object.remove("type") {
        match schema_type {
            Value::Array(types) => {
                let nullable = types.iter().any(|value| value.as_str() == Some("null"));
                if let Some(value) = types
                    .iter()
                    .find(|value| value.as_str().is_some_and(|value| value != "null"))
                    .cloned()
                {
                    object.insert("type".to_owned(), value);
                }
                if nullable {
                    object.insert("nullable".to_owned(), Value::Bool(true));
                }
            }
            schema_type => {
                object.insert("type".to_owned(), schema_type);
            }
        }
    }

    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(branches)) = object.remove(key) {
            let nullable = branches.iter().any(is_null_schema);
            let selected = branches
                .into_iter()
                .find(|branch| !is_null_schema(branch))
                .unwrap_or_else(|| json!({"type": "string"}));
            let mut selected = match sanitize_schema_inner(selected, response) {
                Value::Object(selected) => selected,
                value => Map::from_iter([(String::from("type"), value)]),
            };
            if nullable {
                selected.insert("nullable".to_owned(), Value::Bool(true));
            }
            merge_schema_fields(&mut selected, &mut object);
            object = selected;
            break;
        }
    }

    if let Some(Value::Array(branches)) = object.remove("allOf") {
        let mut merged = Map::new();
        for branch in branches {
            if let Value::Object(branch) = sanitize_schema_inner(branch, response) {
                merge_schema_fields(&mut merged, &mut branch.clone());
            }
        }
        merge_schema_fields(&mut merged, &mut object);
        object = merged;
    }

    if !object.contains_key("type")
        && !object.contains_key("properties")
        && object
            .values()
            .all(|value| value.is_object() || value.is_null())
        && !object.is_empty()
    {
        let properties = std::mem::take(&mut object);
        object.insert("type".to_owned(), Value::String("object".to_owned()));
        object.insert("properties".to_owned(), Value::Object(properties));
    }

    if let Some(Value::Object(properties)) = object.remove("properties") {
        object.insert(
            "properties".to_owned(),
            Value::Object(
                properties
                    .into_iter()
                    .map(|(name, schema)| (name, sanitize_schema_inner(schema, response)))
                    .collect(),
            ),
        );
    }
    if let Some(items) = object.remove("items") {
        object.insert("items".to_owned(), sanitize_schema_inner(items, response));
    }

    if !response {
        if let Some(enum_values) = object.remove("enum") {
            append_description(&mut object, format!("Allowed values: {}", enum_values));
        }
        if let Some(additional) = object.remove("additionalProperties")
            && additional == Value::Bool(false)
        {
            append_description(&mut object, "No extra properties allowed".to_owned());
        }
    } else {
        let remove_enum = object
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|values| values.iter().all(Value::is_boolean));
        if remove_enum {
            object.remove("enum");
        }
        if object.get("additionalProperties") != Some(&Value::Bool(false)) {
            object.remove("additionalProperties");
        }
    }

    for key in [
        "$schema",
        "$id",
        "$comment",
        "default",
        "examples",
        "format",
        "patternProperties",
        "unevaluatedProperties",
        "additionalItems",
        "propertyNames",
        "dependentRequired",
        "dependentSchemas",
        "uniqueItems",
        "not",
        "if",
        "then",
        "else",
        "$defs",
        "definitions",
        "$ref",
        "discriminator",
        "xml",
        "externalDocs",
    ] {
        object.remove(key);
    }
    if !response {
        object.remove("title");
    }

    let properties = object
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(Value::Array(required)) = object.get_mut("required") {
        required.retain(|value| {
            value
                .as_str()
                .is_some_and(|name| properties.contains_key(name))
        });
        required.dedup();
        if required.is_empty() {
            object.remove("required");
        }
    }

    Value::Object(object)
}

fn add_validated_placeholders(value: Value) -> Value {
    let Value::Object(mut object) = value else {
        return value;
    };

    if let Some(Value::Object(properties)) = object.remove("properties") {
        object.insert(
            "properties".to_owned(),
            Value::Object(
                properties
                    .into_iter()
                    .map(|(name, schema)| (name, add_validated_placeholders(schema)))
                    .collect(),
            ),
        );
    }
    if let Some(items) = object.remove("items") {
        object.insert("items".to_owned(), add_validated_placeholders(items));
    }

    if object.get("type").and_then(Value::as_str) == Some("object") {
        let properties = object
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let has_required = object
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|required| !required.is_empty());
        if properties.is_empty() {
            let mut properties = properties;
            properties.insert(
                "reason".to_owned(),
                json!({
                    "type": "string",
                    "description": "Brief explanation of why you are calling this tool"
                }),
            );
            object.insert("properties".to_owned(), Value::Object(properties));
            object.insert("required".to_owned(), json!(["reason"]));
        } else if !has_required {
            let mut properties = properties;
            properties
                .entry("_".to_owned())
                .or_insert_with(|| json!({"type": "boolean"}));
            object.insert("properties".to_owned(), Value::Object(properties));
            object.insert("required".to_owned(), json!(["_"]));
        }
    }

    Value::Object(object)
}

fn is_null_schema(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("null")
}

fn merge_schema_fields(target: &mut Map<String, Value>, source: &mut Map<String, Value>) {
    if let (Some(Value::Object(target_props)), Some(Value::Object(source_props))) =
        (target.get_mut("properties"), source.remove("properties"))
    {
        target_props.extend(source_props);
    }
    if let (Some(Value::Array(target_required)), Some(Value::Array(source_required))) =
        (target.get_mut("required"), source.remove("required"))
    {
        target_required.extend(source_required);
    }
    let source = std::mem::take(source);
    for (key, value) in source {
        target.entry(key).or_insert(value);
    }
}

fn append_description(object: &mut Map<String, Value>, hint: String) {
    let hint = hint.trim().to_owned();
    if hint.is_empty() {
        return;
    }
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let description = match description {
        Some(description) if !description.is_empty() => format!("{description}. {hint}"),
        _ => hint,
    };
    object.insert("description".to_owned(), Value::String(description));
}

fn convert_generation_config(root: &Map<String, Value>) -> Result<Option<Value>, ProviderError> {
    let mut config = Map::new();
    if let Some(value) = root.get("max_output_tokens") {
        let value = value
            .as_u64()
            .ok_or_else(|| invalid("max_output_tokens must be an integer"))?;
        config.insert("maxOutputTokens".to_owned(), Value::from(value));
    }
    if let Some(value) = root.get("temperature") {
        let value = value
            .as_f64()
            .ok_or_else(|| invalid("temperature must be a number"))?;
        config.insert("temperature".to_owned(), json!(value));
    }
    if let Some(value) = root.get("top_p") {
        let value = value
            .as_f64()
            .ok_or_else(|| invalid("top_p must be a number"))?;
        config.insert("topP".to_owned(), json!(value));
    }
    if let Some(value) = root.get("stop_sequences") {
        let value = value
            .as_array()
            .ok_or_else(|| invalid("stop_sequences must be an array"))?;
        if value.iter().any(|value| !value.is_string()) {
            return Err(invalid("stop_sequences must contain strings"));
        }
        config.insert("stopSequences".to_owned(), Value::Array(value.clone()));
    }
    if let Some(effort) = root
        .get("reasoning")
        .and_then(|value| value.get("effort"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let effort = effort.to_ascii_lowercase();
        let thinking = match effort.as_str() {
            "auto" => json!({"thinkingBudget": -1}),
            "none" | "disabled" => json!({"thinkingBudget": 0}),
            _ => json!({"thinkingLevel": effort}),
        };
        config.insert("thinkingConfig".to_owned(), thinking);
    }
    if let Some(format) = structured_output_format(root) {
        match format.get("type").and_then(Value::as_str) {
            Some("json_object") => {
                config.insert(
                    "responseMimeType".to_owned(),
                    Value::String("application/json".to_owned()),
                );
            }
            Some("json_schema") => {
                config.insert(
                    "responseMimeType".to_owned(),
                    Value::String("application/json".to_owned()),
                );
                if let Some(schema) = format.get("schema").or_else(|| {
                    format
                        .get("json_schema")
                        .and_then(|value| value.get("schema"))
                }) {
                    config.insert(
                        "responseJsonSchema".to_owned(),
                        sanitize_response_schema(schema.clone()),
                    );
                }
            }
            Some(other) => return Err(invalid(other)),
            None => return Err(invalid("structured output format requires a type")),
        }
    }
    Ok((!config.is_empty()).then_some(Value::Object(config)))
}

fn structured_output_format(root: &Map<String, Value>) -> Option<Value> {
    root.get("text")
        .and_then(Value::as_object)
        .and_then(|text| text.get("format"))
        .cloned()
        .or_else(|| root.get("response_format").cloned())
}

fn content(role: &str, parts: Vec<Value>) -> Value {
    json!({"role": role, "parts": parts})
}

fn validate_continuation_input(
    request: &ProviderRequest,
    root: &Map<String, Value>,
) -> Result<(), ProviderError> {
    let previous_response_id = request
        .metadata
        .previous_response_id
        .as_deref()
        .or_else(|| root.get("previous_response_id").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let complete_history = root
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("role").and_then(Value::as_str) == Some("assistant")
                    || matches!(
                        item.get("type").and_then(Value::as_str),
                        Some("reasoning" | "function_call" | "custom_tool_call")
                    )
            })
        });
    if previous_response_id.is_some() && !complete_history {
        return Err(invalid(
            "Antigravity Responses continuation requires complete input history",
        ));
    }
    let Some(items) = root.get("input").and_then(Value::as_array) else {
        return Ok(());
    };
    let call_ids = items
        .iter()
        .filter(|item| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("function_call" | "custom_tool_call")
            )
        })
        .filter_map(|item| item.get("call_id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    let has_orphan_output = items.iter().filter_map(|item| {
        let item_type = item.get("type").and_then(Value::as_str)?;
        if !matches!(
            item_type,
            "function_call_output" | "custom_tool_call_output"
        ) {
            return None;
        }
        let call_id = item.get("call_id").and_then(Value::as_str)?;
        Some(!call_ids.contains(&call_id))
    });
    if has_orphan_output.into_iter().any(|orphan| orphan) && !complete_history {
        return Err(invalid(
            "Antigravity tool output requires its complete function call history",
        ));
    }
    Ok(())
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    message: &str,
) -> Result<&'a str, ProviderError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid(message))
}

fn invalid(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use provider_core::{ProviderRequest, RequestMetadata, WireFormat};

    use super::*;

    #[test]
    fn converts_responses_request_to_cpa_cloud_code_envelope() {
        let mut metadata = RequestMetadata::default();
        metadata.session_id = Some("session-1".to_owned());
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "gemini-3-flash-agent".to_owned(),
            payload: Bytes::from(
                serde_json::json!({
                    "model": "client-model",
                    "instructions": "Be concise",
                    "input": [{
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "hello"}]
                    }],
                    "tools": [{
                        "type": "function",
                        "name": "lookup",
                        "parameters": {"type": "object", "properties": {}}
                    }],
                    "tool_choice": {"type": "function", "name": "lookup"},
                    "reasoning": {"effort": "high"},
                    "max_output_tokens": 512
                })
                .to_string(),
            ),
            metadata,
        };
        let body = prepare_request(&request, "project-1").expect("request body");
        let value: Value = serde_json::from_slice(&body).expect("JSON body");
        assert_eq!(value["project"], "project-1");
        assert_eq!(value["model"], "gemini-3-flash-agent");
        assert_eq!(value["request"]["sessionId"], "session-1");
        assert_eq!(
            value["request"]["systemInstruction"]["parts"][0]["text"],
            "Be concise"
        );
        assert_eq!(value["request"]["contents"][0]["role"], "user");
        assert_eq!(
            value["request"]["tools"][0]["functionDeclarations"][0]["name"],
            "lookup"
        );
        assert_eq!(
            value["request"]["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"][0],
            "lookup"
        );
        assert_eq!(
            value["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
    }

    #[test]
    fn maps_standalone_web_search_to_native_google_search() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "gemini-3-flash-agent".to_owned(),
            payload: Bytes::from(
                serde_json::json!({
                    "input": "search the web",
                    "tools": [{"type": "web_search_preview"}]
                })
                .to_string(),
            ),
            metadata: RequestMetadata::default(),
        };

        let body = prepare_request(&request, "project-1").expect("request body");
        let value: Value = serde_json::from_slice(&body).expect("JSON body");
        assert_eq!(value["requestType"], "web_search");
        assert!(value.get("requestId").is_none());
        assert!(value["request"].get("sessionId").is_none());
        assert_eq!(value["request"]["tools"].as_array().map(Vec::len), Some(1));
        assert!(value["request"]["tools"][0].get("googleSearch").is_some());
        assert!(value["request"].get("toolConfig").is_none());
    }

    #[test]
    fn drops_native_google_search_when_function_tools_are_also_present() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "gemini-3-flash-agent".to_owned(),
            payload: Bytes::from(
                serde_json::json!({
                    "input": "search and call a function",
                    "tools": [
                        {
                            "type": "function",
                            "name": "lookup",
                            "parameters": {"type": "object", "properties": {}}
                        },
                        {"type": "web_search_preview"}
                    ]
                })
                .to_string(),
            ),
            metadata: RequestMetadata::default(),
        };

        let body = prepare_request(&request, "project-1").expect("request body");
        let value: Value = serde_json::from_slice(&body).expect("JSON body");
        assert_eq!(value["requestType"], "agent");
        assert!(value.get("requestId").is_some());
        assert!(value["request"].get("sessionId").is_some());
        assert_eq!(value["request"]["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            value["request"]["tools"][0]["functionDeclarations"][0]["name"],
            "lookup"
        );
        assert!(value["request"]["tools"][0].get("googleSearch").is_none());
        assert!(
            value["request"]
                .get("toolConfig")
                .and_then(|v| v.get("includeServerSideToolInvocations"))
                .is_none()
        );
    }

    #[test]
    fn rejects_incremental_previous_response_without_complete_history() {
        let mut metadata = RequestMetadata::default();
        metadata.previous_response_id = Some("resp-1".to_owned());
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "gemini-3-flash".to_owned(),
            payload: Bytes::from_static(br#"{"input":"continue"}"#),
            metadata,
        };
        let error = prepare_request(&request, "project-1").expect_err("history is required");
        assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
        assert!(error.message().contains("complete input history"));
    }

    #[test]
    fn accepts_previous_response_with_complete_history() {
        let mut metadata = RequestMetadata::default();
        metadata.previous_response_id = Some("resp-1".to_owned());
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "gemini-3-flash".to_owned(),
            payload: Bytes::from(
                serde_json::json!({
                    "previous_response_id": "resp-1",
                    "input": [
                        {"type":"message","role":"user","content":"first"},
                        {"type":"message","role":"assistant","content":"answer"},
                        {"type":"message","role":"user","content":"continue"}
                    ]
                })
                .to_string(),
            ),
            metadata,
        };
        prepare_request(&request, "project-1").expect("complete history");
    }

    #[test]
    fn adds_cpa_validated_placeholders_only_to_claude_tool_schemas() {
        let request = |model: &str| {
            ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: model.to_owned(),
            payload: Bytes::from_static(
                br#"{"tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{"flag":{"type":"string"},"nested":{"type":"object"}}}}],"input":"hello"}"#,
            ),
            metadata: RequestMetadata::default(),
        }
        };

        let claude: Value = serde_json::from_slice(
            &prepare_request(&request("claude-sonnet-4-6"), "project-1").expect("Claude body"),
        )
        .expect("Claude JSON");
        let claude_schema = &claude["request"]["tools"][0]["functionDeclarations"][0]["parameters"];
        assert_eq!(
            claude_schema["required"],
            json!(["_"]),
            "schema={claude_schema}"
        );
        assert_eq!(claude_schema["properties"]["_"]["type"], "boolean");
        assert_eq!(claude_schema["properties"]["flag"]["type"], "string");
        assert_eq!(claude_schema["properties"]["nested"]["type"], "object");
        assert_eq!(
            claude_schema["properties"]["nested"]["properties"]["reason"]["type"],
            "string"
        );
        assert_eq!(
            claude_schema["properties"]["nested"]["required"],
            json!(["reason"])
        );

        let empty: Value = serde_json::from_slice(
            &prepare_request(
                &ProviderRequest {
                    format: WireFormat::OpenAiResponses,
                    model: "claude-sonnet-4-6".to_owned(),
                    payload: Bytes::from_static(
                        br#"{"tools":[{"type":"function","name":"empty","parameters":{"type":"object","properties":{}}}],"input":"hello"}"#,
                    ),
                    metadata: RequestMetadata::default(),
                },
                "project-1",
            )
            .expect("empty Claude body"),
        )
        .expect("empty Claude JSON");
        let empty_schema = &empty["request"]["tools"][0]["functionDeclarations"][0]["parameters"];
        assert_eq!(empty_schema["required"], json!(["reason"]));
        assert_eq!(empty_schema["properties"]["reason"]["type"], "string");

        let gemini: Value = serde_json::from_slice(
            &prepare_request(&request("gemini-3-flash"), "project-1").expect("Gemini body"),
        )
        .expect("Gemini JSON");
        let gemini_schema = &gemini["request"]["tools"][0]["functionDeclarations"][0]["parameters"];
        assert!(gemini_schema.get("required").is_none());
        assert!(gemini_schema["properties"].get("_").is_none());
        assert!(
            gemini_schema["properties"]["nested"]
                .get("required")
                .is_none()
        );
    }

    #[test]
    fn prepares_count_tokens_body_without_stream_envelope_fields() {
        let mut metadata = RequestMetadata::default();
        metadata.session_id = Some("session-1".to_owned());
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "gemini-3-flash".to_owned(),
            payload: Bytes::from_static(br#"{"input":"hello"}"#),
            metadata,
        };
        let body: Value =
            serde_json::from_slice(&prepare_count_tokens_request(&request).expect("count body"))
                .expect("count JSON");
        assert!(body.get("project").is_none());
        assert!(body.get("model").is_none());
        assert!(body.get("requestType").is_none());
        assert!(body.get("requestId").is_none());
        assert!(body["request"].get("sessionId").is_none());
        assert_eq!(body["request"]["contents"][0]["role"], "user");
    }

    #[test]
    fn maps_function_output_to_cpa_result_field() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "gemini-3-flash".to_owned(),
            payload: Bytes::from_static(
                br#"{"input":[{"type":"function_call","call_id":"call-1","name":"lookup","arguments":"{}"},{"type":"function_call_output","call_id":"call-1","output":"done"}]}"#,
            ),
            metadata: RequestMetadata::default(),
        };
        let body = prepare_request(&request, "project-1").expect("request body");
        let value: Value = serde_json::from_slice(&body).expect("JSON body");
        assert_eq!(
            value["request"]["contents"][2]["parts"][0]["functionResponse"]["response"]["result"],
            "done"
        );
    }

    #[test]
    fn uses_cpa_image_envelope_and_stable_session_shape() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "gemini-3.1-flash-image".to_owned(),
            payload: Bytes::from_static(br#"{"input":"draw a cat"}"#),
            metadata: RequestMetadata::default(),
        };
        let body = prepare_request(&request, "project-1").expect("request body");
        let value: Value = serde_json::from_slice(&body).expect("JSON body");
        assert_eq!(value["requestType"], "image_gen");
        assert!(
            value["requestId"]
                .as_str()
                .is_some_and(|value| value.starts_with("image_gen/") && value.ends_with("/12"))
        );
        assert!(
            value["request"]["sessionId"]
                .as_str()
                .is_some_and(|value| value.starts_with('-'))
        );
    }

    #[test]
    fn converts_media_structured_output_and_additional_namespace_tools() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "gemini-3-flash".to_owned(),
            payload: Bytes::from(
                serde_json::json!({
                    "text": {"format": {"type": "json_schema", "schema": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"answer": {"type": "string", "format": "uri"}},
                        "required": ["answer"]
                    }}},
                    "input": [
                        {"type": "message", "role": "user", "content": [
                            {"type": "input_audio", "data": "UklGRg==", "format": "wav"},
                            {"type": "input_image", "image_url": "data:image/png;base64,AA=="}
                        ]},
                        {"type": "additional_tools", "tools": [{
                            "type": "namespace", "name": "functions",
                            "tools": [{"type": "custom", "name": "exec"}]
                        }]},
                        {"type": "custom_tool_call", "call_id": "call-1", "name": "exec", "namespace": "functions", "input": "pwd"}
                    ]
                })
                .to_string(),
            ),
            metadata: RequestMetadata::default(),
        };
        let body = prepare_request(&request, "project-1").expect("request body");
        let value: Value = serde_json::from_slice(&body).expect("JSON body");
        let parts = value["request"]["contents"][0]["parts"]
            .as_array()
            .expect("content parts");
        assert_eq!(parts[0]["inlineData"]["mimeType"], "audio/wav");
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(
            value["request"]["contents"][1]["parts"][0]["functionCall"]["name"],
            "functions__exec"
        );
        assert_eq!(
            value["request"]["generationConfig"]["responseMimeType"],
            "application/json"
        );
        assert_eq!(
            value["request"]["generationConfig"]["responseJsonSchema"]["additionalProperties"],
            false
        );
        assert!(
            value["request"]["generationConfig"]["responseJsonSchema"]["properties"]["answer"]
                .get("format")
                .is_none()
        );
    }

    #[test]
    fn decodes_cpa_reasoning_carrier_for_following_tool_call() {
        let signature = "native-signature";
        let carrier = format!(
            "cpa-gemini-responses-carrier-v1:next:function:{}",
            STANDARD_NO_PAD.encode(signature.as_bytes())
        );
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "gemini-3-flash".to_owned(),
            payload: Bytes::from(
                serde_json::json!({
                    "input": [
                        {"type": "reasoning", "encrypted_content": carrier, "summary": []},
                        {"type": "function_call", "call_id": "call-1", "name": "lookup", "arguments": "{}"}
                    ]
                })
                .to_string(),
            ),
            metadata: RequestMetadata::default(),
        };
        let body = prepare_request(&request, "project-1").expect("request body");
        let value: Value = serde_json::from_slice(&body).expect("JSON body");
        assert_eq!(
            value["request"]["contents"][1]["parts"][0]["thoughtSignature"],
            signature
        );
    }

    #[test]
    fn normalizes_claude_thinking_signature_to_antigravity_r_form() {
        let single_layer = BASE64.encode([0x12_u8, 0x01, 0x02]);
        let expected = BASE64.encode(single_layer.as_bytes());
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "claude-sonnet-4-6".to_owned(),
            payload: Bytes::from(
                serde_json::json!({
                    "input": [{
                        "type": "reasoning",
                        "encrypted_content": single_layer,
                        "summary": [{"type": "summary_text", "text": "think"}]
                    }]
                })
                .to_string(),
            ),
            metadata: RequestMetadata::default(),
        };
        let body = prepare_request(&request, "project-1").expect("request body");
        let value: Value = serde_json::from_slice(&body).expect("JSON body");
        assert_eq!(
            value["request"]["contents"][0]["parts"][0]["thoughtSignature"],
            expected
        );
    }

    #[test]
    fn drops_cross_provider_claude_thinking_signature() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "claude-sonnet-4-6".to_owned(),
            payload: Bytes::from_static(
                br#"{"input":[{"type":"reasoning","encrypted_content":"gpt#not-claude","summary":[{"type":"summary_text","text":"drop"}]}]}"#,
            ),
            metadata: RequestMetadata::default(),
        };
        let body = prepare_request(&request, "project-1").expect("request body");
        let value: Value = serde_json::from_slice(&body).expect("JSON body");
        assert!(
            value["request"]["contents"]
                .as_array()
                .is_none_or(|contents| contents.iter().all(|content| {
                    content["parts"].as_array().is_none_or(|parts| {
                        parts
                            .iter()
                            .all(|part| !part["thought"].as_bool().unwrap_or(false))
                    })
                }))
        );
    }
}
