use bytes::Bytes;
use provider_core::{ProviderError, ProviderErrorKind, ProviderRequest, ProxyRequest, WireFormat};
use serde_json::{Map, Value};

use super::{
    ResponsesResponseTranslator, ToolTargets,
    tools::{convert_tool_choice, convert_tools, reject_required_tool_choice},
};
use validation::reject_unsupported_fields;

mod validation;

pub(crate) fn prepare_chat_request(
    request: ProxyRequest,
) -> Result<(ProviderRequest, ResponsesResponseTranslator), ProviderError> {
    if request.format != WireFormat::OpenAiResponses {
        return Err(invalid(
            "Responses request adapter requires the Responses protocol",
        ));
    }
    if request.metadata.previous_response_id.is_some() {
        return Err(invalid(
            "Chat Completions upstream cannot resolve previous_response_id; resend complete input history",
        ));
    }
    let source: Value = serde_json::from_slice(&request.payload)
        .map_err(|_| invalid("Responses request body must be valid JSON"))?;
    let source = source
        .as_object()
        .ok_or_else(|| invalid("Responses request body must be a JSON object"))?;
    reject_unsupported_fields(source)?;

    let mut tool_values = source
        .get("tools")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_array()
                .cloned()
                .ok_or_else(|| invalid("Responses tools must be an array"))
        })
        .transpose()?
        .unwrap_or_default();
    let input = source
        .get("input")
        .ok_or_else(|| invalid("Responses request requires input"))?;
    collect_additional_tools(input, &mut tool_values)?;
    let (tools, targets) = convert_tools(&tool_values)?;
    let messages = convert_input(source.get("instructions"), input, &targets)?;
    let parallel_tool_calls = optional_bool(source, "parallel_tool_calls")?;

    let mut body = Map::new();
    body.insert("model".to_owned(), Value::String(request.model.clone()));
    body.insert("messages".to_owned(), Value::Array(messages));
    body.insert("stream".to_owned(), Value::Bool(true));
    body.insert(
        "stream_options".to_owned(),
        serde_json::json!({"include_usage":true}),
    );
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
        if let Some(choice) = convert_tool_choice(source.get("tool_choice"), &targets)? {
            body.insert("tool_choice".to_owned(), choice);
        }
        if let Some(parallel) = parallel_tool_calls {
            body.insert("parallel_tool_calls".to_owned(), Value::Bool(parallel));
        }
    } else {
        reject_required_tool_choice(source.get("tool_choice"))?;
    }
    if let Some(limit) = optional_u64(source, "max_output_tokens")? {
        body.insert(
            "max_completion_tokens".to_owned(),
            Value::Number(limit.into()),
        );
    }
    copy_number(source, &mut body, "temperature")?;
    copy_number(source, &mut body, "top_p")?;
    copy_string(source, &mut body, "service_tier")?;
    if let Some(reasoning) = source.get("reasoning").filter(|value| !value.is_null()) {
        let reasoning = reasoning
            .as_object()
            .ok_or_else(|| invalid("Responses reasoning must be an object"))?;
        reject_unknown_non_null_fields(reasoning, &["effort", "summary"], "reasoning")?;
        if let Some(effort) = optional_string(reasoning, "effort")? {
            body.insert(
                "reasoning_effort".to_owned(),
                Value::String(effort.to_owned()),
            );
        }
        match reasoning.get("summary") {
            None | Some(Value::Null) => {}
            Some(Value::String(summary)) if summary == "auto" => {}
            Some(Value::String(_)) => {
                return Err(invalid(
                    "Responses reasoning summary mode cannot be preserved by Chat Completions",
                ));
            }
            Some(_) => return Err(invalid("Responses reasoning summary must be text or null")),
        }
    }
    if let Some(text) = source.get("text").filter(|value| !value.is_null()) {
        let text = text
            .as_object()
            .ok_or_else(|| invalid("Responses text must be an object"))?;
        reject_unknown_non_null_fields(text, &["format"], "text")?;
        if let Some(format) = text.get("format").filter(|value| !value.is_null()) {
            body.insert("response_format".to_owned(), convert_text_format(format)?);
        }
    }

    let payload = serde_json::to_vec(&Value::Object(body))
        .map(Bytes::from)
        .map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Internal,
                "failed to serialize converted Responses request",
            )
        })?;
    let upstream = ProviderRequest {
        format: WireFormat::OpenAiChatCompletions,
        model: request.model.clone(),
        payload,
        metadata: request.metadata,
    };
    Ok((
        upstream,
        ResponsesResponseTranslator::new(request.model, targets),
    ))
}

fn collect_additional_tools(input: &Value, tools: &mut Vec<Value>) -> Result<(), ProviderError> {
    let Some(items) = input.as_array() else {
        return Ok(());
    };
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("additional_tools") {
            continue;
        }
        let additional = item
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("Responses additional_tools item requires a tools array"))?;
        tools.extend(additional.iter().cloned());
    }
    Ok(())
}

fn convert_input(
    instructions: Option<&Value>,
    input: &Value,
    targets: &ToolTargets,
) -> Result<Vec<Value>, ProviderError> {
    let mut messages = Vec::new();
    if let Some(instructions) = instructions.filter(|value| !value.is_null()) {
        let text = instructions
            .as_str()
            .ok_or_else(|| invalid("Responses instructions must be text"))?;
        if !text.is_empty() {
            messages.push(serde_json::json!({"role":"developer","content":text}));
        }
    }
    match input {
        Value::String(text) => messages.push(serde_json::json!({"role":"user","content":text})),
        Value::Array(items) => append_input_items(items, targets, &mut messages)?,
        _ => return Err(invalid("Responses input must be text or an array")),
    }
    if messages.is_empty() {
        return Err(invalid("Responses input cannot be empty after conversion"));
    }
    Ok(messages)
}

fn append_input_items(
    items: &[Value],
    targets: &ToolTargets,
    messages: &mut Vec<Value>,
) -> Result<(), ProviderError> {
    super::history::validate_tool_pairing(items)?;
    let mut assistant = AssistantMessage::default();
    for item in items {
        let item = item
            .as_object()
            .ok_or_else(|| invalid("Responses input items must be objects"))?;
        match item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message")
        {
            "message" => append_message_item(item, messages, &mut assistant)?,
            "agent_message" => append_agent_message(item, messages, &mut assistant)?,
            "reasoning" => append_reasoning_item(item, &mut assistant)?,
            "function_call" | "custom_tool_call" => {
                append_tool_call(item, targets, &mut assistant)?
            }
            "function_call_output" | "custom_tool_call_output" => {
                assistant.flush(messages);
                messages.push(tool_output_message(item)?);
            }
            "local_shell_call" | "shell_call" => {
                append_named_call(item, "local_shell", &mut assistant)?
            }
            "local_shell_call_output" | "shell_call_output" => {
                assistant.flush(messages);
                messages.push(tool_output_message(item)?);
            }
            "apply_patch_call" => append_named_call(item, "apply_patch", &mut assistant)?,
            "apply_patch_call_output" => {
                assistant.flush(messages);
                messages.push(tool_output_message(item)?);
            }
            "tool_search_call" => append_named_call(item, "tool_search", &mut assistant)?,
            "tool_search_output" => {
                assistant.flush(messages);
                messages.push(tool_search_output_message(item)?);
            }
            "mcp_tool_call" => append_mcp_call(item, &mut assistant)?,
            "mcp_tool_call_output" => {
                assistant.flush(messages);
                messages.push(tool_output_message(item)?);
            }
            "additional_tools" | "compaction_trigger" => {}
            "item_reference" => {
                return Err(invalid(
                    "Chat Completions upstream cannot resolve item_reference; resend complete input history",
                ));
            }
            "compaction" | "compaction_summary" | "context_compaction" => {
                return Err(invalid(
                    "Chat Completions upstream cannot decode Responses compaction state; resend complete input history",
                ));
            }
            item_type => {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    format!(
                        "Responses input item {item_type} cannot be converted to Chat Completions"
                    ),
                ));
            }
        }
    }
    assistant.flush(messages);
    Ok(())
}

#[derive(Default)]
struct AssistantMessage {
    content: Vec<Value>,
    reasoning: String,
    tool_calls: Vec<Value>,
}

impl AssistantMessage {
    fn flush(&mut self, messages: &mut Vec<Value>) {
        if self.content.is_empty() && self.reasoning.is_empty() && self.tool_calls.is_empty() {
            return;
        }
        let mut message = Map::new();
        message.insert("role".to_owned(), Value::String("assistant".to_owned()));
        message.insert(
            "content".to_owned(),
            if self.content.is_empty() {
                Value::Null
            } else if self.content.len() == 1
                && self.content[0].get("type").and_then(Value::as_str) == Some("text")
            {
                self.content[0].get("text").cloned().unwrap_or(Value::Null)
            } else {
                Value::Array(std::mem::take(&mut self.content))
            },
        );
        if !self.reasoning.is_empty() {
            message.insert(
                "reasoning_content".to_owned(),
                Value::String(std::mem::take(&mut self.reasoning)),
            );
        }
        if !self.tool_calls.is_empty() {
            message.insert(
                "tool_calls".to_owned(),
                Value::Array(std::mem::take(&mut self.tool_calls)),
            );
        }
        self.content.clear();
        messages.push(Value::Object(message));
    }
}

fn append_message_item(
    item: &Map<String, Value>,
    messages: &mut Vec<Value>,
    assistant: &mut AssistantMessage,
) -> Result<(), ProviderError> {
    let role = required_string(item, "role", "Responses message requires a role")?;
    let content = convert_message_content(item.get("content"), role)?;
    if role == "assistant" {
        assistant.content.extend(content);
        return Ok(());
    }
    if !matches!(role, "system" | "developer" | "user") {
        return Err(invalid(
            "Responses message role cannot be converted to Chat Completions",
        ));
    }
    assistant.flush(messages);
    messages.push(serde_json::json!({
        "role":role,
        "content":if content.len() == 1 && content[0].get("type").and_then(Value::as_str) == Some("text") {
            content[0].get("text").cloned().unwrap_or(Value::String(String::new()))
        } else {
            Value::Array(content)
        }
    }));
    Ok(())
}

fn append_agent_message(
    item: &Map<String, Value>,
    messages: &mut Vec<Value>,
    assistant: &mut AssistantMessage,
) -> Result<(), ProviderError> {
    let content = item
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("Responses agent_message requires content items"))?;
    let mut parts = Vec::new();
    for part in content {
        let part = part
            .as_object()
            .ok_or_else(|| invalid("Responses agent_message content items must be objects"))?;
        let text = match part.get("type").and_then(Value::as_str) {
            Some("input_text") => part.get("text").and_then(Value::as_str),
            Some("encrypted_content") => {
                let Some(text) = part.get("encrypted_content").and_then(Value::as_str) else {
                    if part.get("text").is_some_and(Value::is_null) {
                        continue;
                    }
                    return Err(invalid(
                        "Responses agent_message encrypted_content must be text",
                    ));
                };
                Some(text)
            }
            _ => {
                return Err(invalid(
                    "Responses agent_message contains an unsupported content item",
                ));
            }
        }
        .ok_or_else(|| invalid("Responses agent_message content must be text"))?;
        parts.push(serde_json::json!({"type":"text","text":text}));
    }
    if parts.is_empty() {
        return Ok(());
    }
    assistant.flush(messages);
    messages.push(serde_json::json!({"role":"user","content":parts}));
    Ok(())
}

fn convert_message_content(
    content: Option<&Value>,
    role: &str,
) -> Result<Vec<Value>, ProviderError> {
    let Some(content) = content else {
        return Ok(Vec::new());
    };
    match content {
        Value::String(text) => Ok(vec![serde_json::json!({"type":"text","text":text})]),
        Value::Array(parts) => parts
            .iter()
            .map(|part| convert_content_part(part, role))
            .collect(),
        Value::Null => Ok(Vec::new()),
        _ => Err(invalid(
            "Responses message content must be text, null, or an array",
        )),
    }
}

fn convert_content_part(part: &Value, role: &str) -> Result<Value, ProviderError> {
    let part = part
        .as_object()
        .ok_or_else(|| invalid("Responses content parts must be objects"))?;
    match part.get("type").and_then(Value::as_str).unwrap_or_default() {
        "input_text" | "output_text" | "text" => {
            let text = required_text(part, "text", "Responses text content requires text")?;
            Ok(serde_json::json!({"type":"text","text":text}))
        }
        "input_image" if role == "user" => {
            let url = part
                .get("image_url")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("Responses input_image requires image_url"))?;
            let detail = optional_string(part, "detail")?.unwrap_or("auto");
            Ok(serde_json::json!({"type":"image_url","image_url":{"url":url,"detail":detail}}))
        }
        part_type => Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            format!("Responses content part {part_type} cannot be converted to Chat Completions"),
        )),
    }
}

fn append_reasoning_item(
    item: &Map<String, Value>,
    assistant: &mut AssistantMessage,
) -> Result<(), ProviderError> {
    if let Some(summary) = item.get("summary").filter(|value| !value.is_null()) {
        let summary = summary
            .as_array()
            .ok_or_else(|| invalid("Responses reasoning summary must be an array"))?;
        for part in summary {
            let part = part
                .as_object()
                .ok_or_else(|| invalid("Responses reasoning summary items must be objects"))?;
            if part.get("type").and_then(Value::as_str) != Some("summary_text") {
                return Err(invalid(
                    "Responses reasoning summary items must have type summary_text",
                ));
            }
            let text = required_text(
                part,
                "text",
                "Responses reasoning summary item requires text",
            )?;
            assistant.reasoning.push_str(text);
        }
    }
    Ok(())
}

fn append_tool_call(
    item: &Map<String, Value>,
    targets: &ToolTargets,
    assistant: &mut AssistantMessage,
) -> Result<(), ProviderError> {
    let call_id = required_string(item, "call_id", "Responses tool call requires call_id")?;
    let name = qualified_name(item)?;
    let arguments = if item.get("type").and_then(Value::as_str) == Some("custom_tool_call") {
        let input = required_text(item, "input", "Responses custom tool call requires input")?;
        serde_json::json!({"input":input}).to_string()
    } else {
        json_arguments(
            item.get("arguments"),
            "Responses function call requires arguments",
        )?
    };
    if !targets.contains_key(&name) {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            format!("Responses history references undeclared tool {name}"),
        ));
    }
    assistant
        .tool_calls
        .push(chat_tool_call(call_id, &name, arguments));
    Ok(())
}

fn append_named_call(
    item: &Map<String, Value>,
    name: &str,
    assistant: &mut AssistantMessage,
) -> Result<(), ProviderError> {
    let call_id = required_string(
        item,
        "call_id",
        "Responses hosted tool call requires call_id",
    )?;
    let arguments = item
        .get("arguments")
        .or_else(|| item.get("action"))
        .or_else(|| item.get("operation"));
    assistant.tool_calls.push(chat_tool_call(
        call_id,
        name,
        json_arguments(arguments, "Responses hosted tool call requires arguments")?,
    ));
    Ok(())
}

fn append_mcp_call(
    item: &Map<String, Value>,
    assistant: &mut AssistantMessage,
) -> Result<(), ProviderError> {
    let call_id = required_string(item, "call_id", "Responses MCP tool call requires call_id")?;
    let name = qualified_name(item)?;
    assistant.tool_calls.push(chat_tool_call(
        call_id,
        &name,
        json_arguments(
            item.get("arguments"),
            "Responses MCP tool call requires arguments",
        )?,
    ));
    Ok(())
}

fn chat_tool_call(call_id: &str, name: &str, arguments: String) -> Value {
    serde_json::json!({
        "id":call_id,
        "type":"function",
        "function":{"name":name,"arguments":arguments}
    })
}

fn tool_output_message(item: &Map<String, Value>) -> Result<Value, ProviderError> {
    let call_id = required_string(item, "call_id", "Responses tool output requires call_id")?;
    let output = item
        .get("output")
        .cloned()
        .ok_or_else(|| invalid("Responses tool output requires output"))?;
    Ok(
        serde_json::json!({"role":"tool","tool_call_id":call_id,"content":convert_tool_output(output)?}),
    )
}

fn tool_search_output_message(item: &Map<String, Value>) -> Result<Value, ProviderError> {
    let call_id = required_string(
        item,
        "call_id",
        "Responses tool search output requires call_id",
    )?;
    let output = item
        .get("tools")
        .cloned()
        .ok_or_else(|| invalid("Responses tool search output requires tools"))?;
    Ok(serde_json::json!({"role":"tool","tool_call_id":call_id,"content":output.to_string()}))
}

fn output_text(output: Value) -> String {
    output
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| output.to_string())
}

fn convert_tool_output(output: Value) -> Result<Value, ProviderError> {
    let Value::Array(parts) = output else {
        return Ok(Value::String(output_text(output)));
    };
    let parts = parts
        .into_iter()
        .map(|part| convert_content_part(&part, "user"))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Value::Array(parts))
}

fn convert_text_format(format: &Value) -> Result<Value, ProviderError> {
    let format = format
        .as_object()
        .ok_or_else(|| invalid("Responses text format must be an object"))?;
    let format_type = required_string(format, "type", "Responses text format requires a type")?;
    match format_type {
        "text" => {
            reject_unknown_non_null_fields(format, &["type"], "text.format")?;
            Ok(serde_json::json!({"type":"text"}))
        }
        "json_object" => {
            reject_unknown_non_null_fields(format, &["type"], "text.format")?;
            Ok(serde_json::json!({"type":"json_object"}))
        }
        "json_schema" => {
            reject_unknown_non_null_fields(
                format,
                &["type", "name", "description", "schema", "strict"],
                "text.format",
            )?;
            let name = required_string(
                format,
                "name",
                "Responses json_schema format requires a name",
            )?;
            let schema = format
                .get("schema")
                .filter(|value| value.is_object())
                .ok_or_else(|| invalid("Responses json_schema format requires an object schema"))?;
            let description = optional_string(format, "description")?;
            let strict = optional_bool(format, "strict")?.unwrap_or(false);
            Ok(serde_json::json!({
                "type":"json_schema",
                "json_schema":{
                    "name":name,
                    "description":description,
                    "schema":schema,
                    "strict":strict
                }
            }))
        }
        _ => Err(invalid("unsupported Responses text format")),
    }
}

fn qualified_name(object: &Map<String, Value>) -> Result<String, ProviderError> {
    let name = required_string(object, "name", "Responses tool reference requires a name")?;
    Ok(object.get("namespace").and_then(Value::as_str).map_or_else(
        || name.to_owned(),
        |namespace| format!("{}__{name}", namespace.trim_end_matches("__")),
    ))
}

fn json_arguments(
    value: Option<&Value>,
    missing_message: &'static str,
) -> Result<String, ProviderError> {
    match value {
        None | Some(Value::Null) => Err(invalid(missing_message)),
        Some(Value::String(value)) => {
            serde_json::from_str::<Value>(value)
                .map_err(|_| invalid("Responses tool arguments must contain valid JSON"))?;
            Ok(value.clone())
        }
        Some(value) => Ok(value.to_string()),
    }
}

fn reject_unknown_non_null_fields(
    object: &Map<String, Value>,
    allowed: &[&str],
    parent: &str,
) -> Result<(), ProviderError> {
    if let Some((field, _)) = object
        .iter()
        .find(|(field, value)| !allowed.contains(&field.as_str()) && !value.is_null())
    {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            format!("Responses {parent}.{field} cannot be converted to Chat Completions"),
        ));
    }
    Ok(())
}

fn required_text<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    message: &'static str,
) -> Result<&'a str, ProviderError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(message))
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
        .ok_or_else(|| invalid(message))
}

fn copy_number(
    source: &Map<String, Value>,
    target: &mut Map<String, Value>,
    field: &str,
) -> Result<(), ProviderError> {
    if let Some(value) = source.get(field).filter(|value| !value.is_null()) {
        if !value.is_number() {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!("Responses {field} must be a number or null"),
            ));
        }
        target.insert(field.to_owned(), value.clone());
    }
    Ok(())
}

fn copy_string(
    source: &Map<String, Value>,
    target: &mut Map<String, Value>,
    field: &str,
) -> Result<(), ProviderError> {
    if let Some(value) = optional_string(source, field)? {
        target.insert(field.to_owned(), Value::String(value.to_owned()));
    }
    Ok(())
}

fn optional_bool(source: &Map<String, Value>, field: &str) -> Result<Option<bool>, ProviderError> {
    match source.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            format!("Responses {field} must be a boolean or null"),
        )),
    }
}

fn optional_u64(source: &Map<String, Value>, field: &str) -> Result<Option<u64>, ProviderError> {
    match source.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!("Responses {field} must be a non-negative integer or null"),
            )
        }),
    }
}

fn optional_string<'a>(
    source: &'a Map<String, Value>,
    field: &str,
) -> Result<Option<&'a str>, ProviderError> {
    match source.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            format!("Responses {field} must be text or null"),
        )),
    }
}

fn invalid(message: &'static str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

#[cfg(test)]
mod tests;
