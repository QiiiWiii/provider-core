use bytes::Bytes;
use provider_core::{ProviderError, ProviderErrorKind, ProviderRequest, ProxyRequest, WireFormat};
use serde_json::{Map, Value};

use super::response::{ChatCompletionsResponseContext, ChatCompletionsResponseTranslator};

pub(crate) fn prepare_responses_request(
    request: ProxyRequest,
) -> Result<(ProviderRequest, ChatCompletionsResponseTranslator), ProviderError> {
    if request.format != WireFormat::OpenAiChatCompletions {
        return Err(invalid_request(
            "Chat Completions request adapter requires the Chat Completions protocol",
        ));
    }
    let model = request.model.trim().to_owned();
    if model.is_empty() {
        return Err(invalid_request("model must not be empty"));
    }
    let source: Value = serde_json::from_slice(&request.payload)
        .map_err(|_| invalid_request("Chat Completions request body must be valid JSON"))?;
    let source = source
        .as_object()
        .ok_or_else(|| invalid_request("Chat Completions request body must be a JSON object"))?;
    let messages = source
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_request("Chat Completions request requires a messages array"))?;

    reject_unsupported_fields(source)?;
    let mut input = Vec::new();
    append_messages(messages, &mut input)?;

    let mut body = Map::new();
    body.insert("model".to_owned(), Value::String(model.clone()));
    body.insert("input".to_owned(), Value::Array(input));
    body.insert("stream".to_owned(), Value::Bool(true));
    body.insert("store".to_owned(), Value::Bool(false));
    if let Some(max_tokens) = source.get("max_tokens").and_then(Value::as_u64) {
        body.insert(
            "max_output_tokens".to_owned(),
            Value::Number(max_tokens.into()),
        );
    }
    copy_number(source, &mut body, "temperature");
    copy_number(source, &mut body, "top_p");
    if let Some(reasoning) = convert_reasoning(source)? {
        body.insert("reasoning".to_owned(), reasoning);
        body.insert(
            "include".to_owned(),
            serde_json::json!(["reasoning.encrypted_content"]),
        );
    }
    if let Some(tools) = convert_tools(source.get("tools"))? {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    if let Some(tool_choice) = convert_tool_choice(source.get("tool_choice"))? {
        body.insert("tool_choice".to_owned(), tool_choice);
    }
    if let Some(parallel) = source.get("parallel_tool_calls").and_then(Value::as_bool) {
        body.insert("parallel_tool_calls".to_owned(), Value::Bool(parallel));
    }

    let payload = serde_json::to_vec(&Value::Object(body))
        .map(Bytes::from)
        .map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Internal,
                "failed to serialize converted Chat Completions request",
            )
        })?;
    let upstream = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: model.clone(),
        payload,
        metadata: request.metadata,
    };
    Ok((
        upstream,
        ChatCompletionsResponseTranslator::new(ChatCompletionsResponseContext::new(model)),
    ))
}

fn append_messages(messages: &[Value], input: &mut Vec<Value>) -> Result<(), ProviderError> {
    for message in messages {
        let message = message
            .as_object()
            .ok_or_else(|| invalid_request("Chat Completions messages must contain objects"))?;
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_request("Chat Completions message role is required"))?;
        match role {
            "system" | "developer" | "user" | "assistant" => {
                if role == "assistant" {
                    append_reasoning(message.get("reasoning_content"), input)?;
                }
                append_message(message, role, input)?;
                if role == "assistant" {
                    append_tool_calls(message.get("tool_calls"), input)?;
                }
            }
            "tool" => append_tool_output(message, input)?,
            _ => {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    format!("unsupported Chat Completions message role: {role}"),
                ));
            }
        }
    }
    Ok(())
}

fn append_reasoning(
    reasoning: Option<&Value>,
    input: &mut Vec<Value>,
) -> Result<(), ProviderError> {
    let Some(reasoning) = reasoning else {
        return Ok(());
    };
    let reasoning = reasoning
        .as_str()
        .ok_or_else(|| invalid_request("Chat Completions reasoning_content must be text"))?;
    if reasoning.is_empty() {
        return Ok(());
    }
    input.push(serde_json::json!({
        "type": "reasoning",
        "summary": [{ "type": "summary_text", "text": reasoning }]
    }));
    Ok(())
}

fn append_message(
    message: &Map<String, Value>,
    role: &str,
    input: &mut Vec<Value>,
) -> Result<(), ProviderError> {
    let Some(content) = message.get("content") else {
        return Ok(());
    };
    if content.is_null() {
        return Ok(());
    }
    let response_role = if matches!(role, "system" | "developer") {
        "developer"
    } else {
        role
    };
    let parts = match content {
        Value::String(text) if text.is_empty() => Vec::new(),
        Value::String(text) => vec![text_part(response_role, text)],
        Value::Array(parts) => parts
            .iter()
            .map(|part| convert_content_part(part, response_role))
            .collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err(invalid_request(
                "Chat Completions message content must be text, null, or an array",
            ));
        }
    };
    if !parts.is_empty() {
        input.push(serde_json::json!({
            "type": "message",
            "role": response_role,
            "content": parts
        }));
    }
    Ok(())
}

fn convert_content_part(part: &Value, role: &str) -> Result<Value, ProviderError> {
    let part = part
        .as_object()
        .ok_or_else(|| invalid_request("Chat Completions content parts must be objects"))?;
    match part.get("type").and_then(Value::as_str).unwrap_or_default() {
        "text" | "input_text" | "output_text" => Ok(text_part(
            role,
            part.get("text").and_then(Value::as_str).unwrap_or_default(),
        )),
        "image_url" | "input_image" if role == "user" => {
            let image_url = part
                .get("image_url")
                .and_then(|value| value.as_str().or_else(|| value.get("url")?.as_str()))
                .ok_or_else(|| invalid_request("Chat Completions image part requires a URL"))?;
            let detail = part
                .get("detail")
                .or_else(|| part.get("image_url").and_then(|value| value.get("detail")))
                .and_then(Value::as_str)
                .unwrap_or("auto");
            Ok(serde_json::json!({
                "type": "input_image",
                "image_url": image_url,
                "detail": detail
            }))
        }
        part_type => Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            format!("unsupported Chat Completions content part: {part_type}"),
        )),
    }
}

fn text_part(role: &str, text: &str) -> Value {
    serde_json::json!({
        "type": if role == "assistant" { "output_text" } else { "input_text" },
        "text": text
    })
}

fn append_tool_calls(tools: Option<&Value>, input: &mut Vec<Value>) -> Result<(), ProviderError> {
    let Some(tools) = tools else {
        return Ok(());
    };
    let tools = tools
        .as_array()
        .ok_or_else(|| invalid_request("Chat Completions tool_calls must be an array"))?;
    for tool in tools {
        let tool = tool
            .as_object()
            .ok_or_else(|| invalid_request("Chat Completions tool calls must be objects"))?;
        if tool.get("type").and_then(Value::as_str) != Some("function") {
            return Err(invalid_request(
                "only function Chat Completions tool calls are supported",
            ));
        }
        let call_id = required_string(tool, "id", "Chat Completions tool call requires an id")?;
        let function = tool
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid_request("Chat Completions tool call requires function"))?;
        let name = required_string(
            function,
            "name",
            "Chat Completions tool call requires a function name",
        )?;
        let arguments = function
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}");
        input.push(serde_json::json!({
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": arguments
        }));
    }
    Ok(())
}

fn append_tool_output(
    message: &Map<String, Value>,
    input: &mut Vec<Value>,
) -> Result<(), ProviderError> {
    let call_id = required_string(
        message,
        "tool_call_id",
        "Chat Completions tool message requires tool_call_id",
    )?;
    let output = match message.get("content") {
        Some(Value::String(text)) => Value::String(text.clone()),
        Some(Value::Array(parts)) => Value::String(
            parts
                .iter()
                .map(tool_output_part)
                .collect::<Result<Vec<_>, _>>()?
                .join("\n"),
        ),
        Some(Value::Null) | None => Value::String(String::new()),
        Some(value) => Value::String(value.to_string()),
    };
    input.push(serde_json::json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": output
    }));
    Ok(())
}

fn tool_output_part(part: &Value) -> Result<String, ProviderError> {
    let part = part
        .as_object()
        .ok_or_else(|| invalid_request("Chat Completions tool content parts must be objects"))?;
    match part.get("type").and_then(Value::as_str).unwrap_or_default() {
        "text" | "input_text" | "output_text" => Ok(part
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()),
        part_type => Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            format!("unsupported Chat Completions tool content part: {part_type}"),
        )),
    }
}

fn convert_tools(tools: Option<&Value>) -> Result<Option<Vec<Value>>, ProviderError> {
    let Some(tools) = tools else {
        return Ok(None);
    };
    let tools = tools
        .as_array()
        .ok_or_else(|| invalid_request("Chat Completions tools must be an array"))?;
    let mut converted = Vec::with_capacity(tools.len());
    for tool in tools {
        let tool = tool
            .as_object()
            .ok_or_else(|| invalid_request("Chat Completions tools must contain objects"))?;
        if tool.get("type").and_then(Value::as_str) != Some("function") {
            return Err(invalid_request(
                "only function Chat Completions tools are supported",
            ));
        }
        let function = tool
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid_request("Chat Completions tool requires function"))?;
        let name = required_string(
            function,
            "name",
            "Chat Completions tool requires a function name",
        )?;
        let mut converted_tool = Map::new();
        converted_tool.insert("type".to_owned(), Value::String("function".to_owned()));
        converted_tool.insert("name".to_owned(), Value::String(name.to_owned()));
        if let Some(description) = function.get("description") {
            converted_tool.insert("description".to_owned(), description.clone());
        }
        converted_tool.insert(
            "parameters".to_owned(),
            function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"type":"object","properties":{}})),
        );
        if let Some(strict) = function.get("strict").and_then(Value::as_bool) {
            converted_tool.insert("strict".to_owned(), Value::Bool(strict));
        }
        converted.push(Value::Object(converted_tool));
    }
    Ok(Some(converted))
}

fn convert_tool_choice(choice: Option<&Value>) -> Result<Option<Value>, ProviderError> {
    let Some(choice) = choice else {
        return Ok(None);
    };
    if let Some(choice) = choice.as_str() {
        return match choice {
            "none" | "auto" | "required" => Ok(Some(Value::String(choice.to_owned()))),
            _ => Err(invalid_request("unsupported Chat Completions tool_choice")),
        };
    }
    let choice = choice
        .as_object()
        .ok_or_else(|| invalid_request("Chat Completions tool_choice must be text or an object"))?;
    if choice.get("type").and_then(Value::as_str) != Some("function") {
        return Err(invalid_request("unsupported Chat Completions tool_choice"));
    }
    let function = choice
        .get("function")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_request("function tool_choice requires function"))?;
    let name = required_string(
        function,
        "name",
        "function tool_choice requires a function name",
    )?;
    Ok(Some(serde_json::json!({"type":"function","name":name})))
}

fn convert_reasoning(source: &Map<String, Value>) -> Result<Option<Value>, ProviderError> {
    let thinking = source
        .get("thinking")
        .and_then(Value::as_object)
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str);
    if thinking == Some("disabled") {
        return Ok(None);
    }
    if thinking.is_some_and(|value| value != "enabled") {
        return Err(invalid_request(
            "unsupported Chat Completions thinking mode",
        ));
    }
    Ok(source
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(|effort| serde_json::json!({"effort":effort,"summary":"auto"})))
}

fn reject_unsupported_fields(source: &Map<String, Value>) -> Result<(), ProviderError> {
    for field in [
        "audio",
        "frequency_penalty",
        "logit_bias",
        "logprobs",
        "modalities",
        "n",
        "prediction",
        "presence_penalty",
        "response_format",
        "seed",
        "service_tier",
        "stop",
        "top_logprobs",
        "web_search_options",
    ] {
        if source.get(field).is_some_and(|value| !value.is_null()) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!("Chat Completions field {field} cannot be converted to Responses"),
            ));
        }
    }
    Ok(())
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    message: &'static str,
) -> Result<&'a str, ProviderError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid_request(message))
}

fn copy_number(source: &Map<String, Value>, target: &mut Map<String, Value>, field: &str) {
    if let Some(value) = source.get(field).filter(|value| value.is_number()) {
        target.insert(field.to_owned(), value.clone());
    }
}

fn invalid_request(message: &'static str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

#[cfg(test)]
mod tests;
