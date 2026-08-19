use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use bytes::Bytes;
use provider_core::{
    ProviderError, ProviderErrorKind, ProviderRequest, RequestMetadata, WireFormat,
};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

const UNSUPPORTED_FIELDS: &[&str] = &[
    "previous_response_id",
    "prompt_cache_retention",
    "safety_identifier",
    "stream_options",
];

const MAX_ENCRYPTED_CONTENT_LEN: usize = 8 * 1024 * 1024;
const MIN_ENCRYPTED_CONTENT_DECODED_LEN: usize = 32;
const MIN_ENCRYPTED_CONTENT_ENTROPY_RATIO: f64 = 0.85;

#[derive(Debug)]
pub(crate) struct PreparedGrokRequest {
    pub(crate) payload: Bytes,
    pub(crate) model: String,
    pub(crate) metadata: RequestMetadata,
    pub(crate) tool_mappings: GrokToolMappings,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct GrokToolMappings {
    pub(crate) custom_tools: HashSet<String>,
    pub(crate) namespace_tools: HashMap<String, NamespaceToolRef>,
    pub(crate) tool_search: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NamespaceToolRef {
    pub(crate) namespace: String,
    pub(crate) name: String,
}

pub(crate) fn prepare_request(
    request: ProviderRequest,
) -> Result<PreparedGrokRequest, ProviderError> {
    if request.format != WireFormat::OpenAiResponses {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Grok driver requires the OpenAI Responses format",
        ));
    }

    let model = request.model.trim().to_owned();
    if model.is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "model must not be empty",
        ));
    }
    let mut payload: Value = serde_json::from_slice(&request.payload).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Grok Responses request body must be valid JSON",
        )
    })?;
    let body = payload.as_object_mut().ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Grok Responses request body must be a JSON object",
        )
    })?;
    if request.metadata.previous_response_id.is_some()
        || body
            .get("previous_response_id")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Grok HTTP Responses requires complete input history and does not support previous_response_id",
        ));
    }

    body.insert("model".to_owned(), Value::String(model.clone()));
    body.insert("stream".to_owned(), Value::Bool(true));
    for field in UNSUPPORTED_FIELDS {
        body.remove(*field);
    }
    normalize_model_fields(body, &model);

    promote_additional_tools(body);
    let tool_mappings = normalize_tools(body)?;
    normalize_input_namespace_calls(body);
    validate_tool_output_context(body)?;
    normalize_input(body, tool_mappings.tool_search);
    normalize_reasoning(body);

    let mut metadata = request.metadata;
    metadata.session_id = normalized_string(metadata.session_id.as_deref()).or_else(|| {
        body.get("prompt_cache_key")
            .and_then(Value::as_str)
            .and_then(|value| normalized_string(Some(value)))
    });
    if let Some(session_id) = metadata.session_id.as_ref() {
        body.insert(
            "prompt_cache_key".to_owned(),
            Value::String(session_id.clone()),
        );
    }

    let payload = serde_json::to_vec(&payload).map(Bytes::from).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Internal,
            "failed to serialize normalized Grok request",
        )
    })?;

    Ok(PreparedGrokRequest {
        payload,
        model,
        metadata,
        tool_mappings,
    })
}

pub(crate) fn strip_encrypted_reasoning_for_retry(
    request: &ProviderRequest,
) -> Result<Option<ProviderRequest>, ProviderError> {
    let mut payload: Value = serde_json::from_slice(&request.payload).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Grok Responses request body must be valid JSON",
        )
    })?;
    let Some(body) = payload.as_object_mut() else {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Grok Responses request body must be a JSON object",
        ));
    };
    let Some(Value::Array(input)) = body.get_mut("input") else {
        return Ok(None);
    };

    let mut changed = false;
    input.retain_mut(|item| {
        let Some(item) = item.as_object_mut() else {
            return true;
        };
        let is_reasoning = item.get("type").and_then(Value::as_str) == Some("reasoning");
        let is_compaction = matches!(
            item.get("type").and_then(Value::as_str),
            Some("compaction" | "compaction_summary")
        );
        if (!is_reasoning && !is_compaction) || !item.contains_key("encrypted_content") {
            return true;
        }
        changed = true;
        if !is_reasoning {
            return false;
        }
        item.remove("encrypted_content");
        if item.get("content").is_some_and(Value::is_null) {
            item.remove("content");
        }
        item.len() > 1
    });
    if !changed {
        return Ok(None);
    }

    let payload = serde_json::to_vec(&payload).map(Bytes::from).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Internal,
            "failed to serialize Grok encrypted reasoning retry",
        )
    })?;
    let mut retry = request.clone();
    retry.payload = payload;
    Ok(Some(retry))
}

fn normalized_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn normalize_tools(body: &mut Map<String, Value>) -> Result<GrokToolMappings, ProviderError> {
    let mut mappings = GrokToolMappings::default();
    let Some(tools) = body.remove("tools") else {
        normalize_tool_controls_without_tools(body)?;
        return Ok(mappings);
    };
    if tools.is_null() {
        normalize_tool_controls_without_tools(body)?;
        return Ok(mappings);
    }
    let Value::Array(tools) = tools else {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Grok tools must be an array",
        ));
    };

    let tools = flatten_namespace_tools(tools);
    let mut normalized = Vec::new();
    for (tool, namespace_reference) in tools {
        let was_custom = tool.get("type").and_then(Value::as_str) == Some("custom");
        let was_tool_search = tool.get("type").and_then(Value::as_str) == Some("tool_search");
        let tool = if was_tool_search {
            if mappings.tool_search {
                continue;
            }
            mappings.tool_search = true;
            tool_search_proxy_tool()
        } else if let Some(tool) = normalize_tool(tool) {
            tool
        } else {
            continue;
        };
        if let Some(name) = tool.get("name").and_then(Value::as_str).map(str::to_owned) {
            if was_custom {
                mappings.custom_tools.insert(name.clone());
            }
            if let Some(namespace_reference) = namespace_reference {
                mappings.namespace_tools.insert(name, namespace_reference);
            }
        }
        normalized.push(tool);
    }
    let mut unique_tools = HashSet::new();
    for key in normalized.iter().filter_map(tool_key) {
        if !unique_tools.insert(key) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "Grok tool names must remain unique after namespace normalization",
            ));
        }
    }
    if normalized.is_empty() {
        normalize_tool_controls_without_tools(body)?;
    } else {
        body.insert("tools".to_owned(), Value::Array(normalized));
        normalize_tool_choice(body)?;
    }
    Ok(mappings)
}

fn normalize_tool_controls_without_tools(
    body: &mut Map<String, Value>,
) -> Result<(), ProviderError> {
    body.remove("parallel_tool_calls");
    let Some(choice) = body.remove("tool_choice") else {
        return Ok(());
    };
    let optional = match &choice {
        Value::String(mode) => matches!(mode.as_str(), "auto" | "none"),
        Value::Object(choice) => choice
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|mode| matches!(mode, "auto" | "none")),
        Value::Null => true,
        _ => false,
    };
    if optional {
        Ok(())
    } else {
        Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Grok tool_choice requires at least one supported tool",
        ))
    }
}

fn flatten_namespace_tools(tools: Vec<Value>) -> Vec<(Value, Option<NamespaceToolRef>)> {
    let mut flattened = Vec::new();
    for mut tool in tools {
        let Some(tool_object) = tool.as_object_mut() else {
            flattened.push((tool, None));
            continue;
        };
        if tool_object.get("type").and_then(Value::as_str) != Some("namespace") {
            flattened.push((tool, None));
            continue;
        }
        let namespace = tool_object
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let nested = tool_object.remove("tools");
        let (Some(namespace), Some(Value::Array(nested))) = (namespace, nested) else {
            continue;
        };
        for mut nested_tool in nested {
            let Some(nested_object) = nested_tool.as_object_mut() else {
                continue;
            };
            let Some(name) = nested_object
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
            else {
                continue;
            };
            let qualified = qualify_namespace_tool_name(&namespace, &name);
            nested_object.insert("name".to_owned(), Value::String(qualified.clone()));
            flattened.push((
                nested_tool,
                Some(NamespaceToolRef {
                    namespace: namespace.clone(),
                    name,
                }),
            ));
        }
    }
    flattened
}

fn qualify_namespace_tool_name(namespace: &str, name: &str) -> String {
    if name.starts_with("mcp__") {
        name.to_owned()
    } else {
        format!("{}__{}", namespace.trim_end_matches("__"), name)
    }
}

fn promote_additional_tools(body: &mut Map<String, Value>) {
    let Some(Value::Array(input)) = body.get_mut("input") else {
        return;
    };
    let mut promoted = Vec::new();
    input.retain_mut(|item| {
        let Some(item) = item.as_object_mut() else {
            return true;
        };
        if item.get("type").and_then(Value::as_str) != Some("additional_tools") {
            return true;
        }
        if let Some(Value::Array(tools)) = item.remove("tools") {
            promoted.extend(tools);
        }
        false
    });
    if promoted.is_empty() {
        return;
    }
    match body.get_mut("tools") {
        Some(Value::Array(tools)) => tools.extend(promoted),
        None | Some(Value::Null) => {
            body.insert("tools".to_owned(), Value::Array(promoted));
        }
        Some(_) => {}
    }
}

fn normalize_tool(mut tool: Value) -> Option<Value> {
    let Some(tool_object) = tool.as_object_mut() else {
        return Some(tool);
    };
    let Some(tool_type) = tool_object
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Some(tool);
    };

    match tool_type.as_str() {
        "image_generation" | "namespace" => return None,
        "custom" => {
            tool_object.insert("type".to_owned(), Value::String("function".to_owned()));
            tool_object.remove("format");
            tool_object.insert("parameters".to_owned(), custom_tool_schema());
        }
        "function" => {
            normalize_function_parameters(tool_object);
        }
        "web_search" => {
            tool_object.remove("external_web_access");
        }
        _ => {}
    }

    Some(tool)
}

fn tool_search_proxy_tool() -> Value {
    serde_json::json!({
        "type": "function",
        "name": "tool_search",
        "description": "Search and load Codex tools, plugins, connectors, and MCP namespaces for the current task.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query for tools or connectors to load."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of tool groups to return."
                }
            },
            "required": ["query"]
        }
    })
}

fn normalize_tool_choice(body: &mut Map<String, Value>) -> Result<(), ProviderError> {
    let available = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(tool_key)
        .collect::<HashSet<_>>();
    let Some(mut value) = body.remove("tool_choice") else {
        return Ok(());
    };
    if value.get("type").and_then(Value::as_str) == Some("web_search") {
        value = serde_json::json!({
            "type": "allowed_tools",
            "mode": "required",
            "tools": [value]
        });
    }
    let Some(choice) = value.as_object_mut() else {
        body.insert("tool_choice".to_owned(), value);
        return Ok(());
    };
    if choice.get("type").and_then(Value::as_str) == Some("allowed_tools") {
        let Some(Value::Array(tools)) = choice.get_mut("tools") else {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "Grok allowed_tools choice requires a tools array",
            ));
        };
        tools.retain_mut(|tool| normalize_tool_choice_ref(tool, &available));
        if !tools.is_empty() {
            body.insert("tool_choice".to_owned(), value);
            return Ok(());
        }
        let required = choice.get("mode").and_then(Value::as_str) == Some("required");
        return if required {
            Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "Grok required allowed_tools choice contains no supported tools",
            ))
        } else {
            Ok(())
        };
    }
    if normalize_tool_choice_ref(&mut value, &available) {
        body.insert("tool_choice".to_owned(), value);
        Ok(())
    } else {
        Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "Grok forced tool_choice does not reference a supported tool",
        ))
    }
}

fn normalize_tool_choice_ref(value: &mut Value, available: &HashSet<(String, String)>) -> bool {
    let Some(choice) = value.as_object_mut() else {
        return false;
    };
    let Some(tool_type) = choice
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return false;
    };
    if matches!(tool_type.as_str(), "image_generation" | "namespace") {
        return false;
    }
    if tool_type == "tool_search" {
        choice.insert("type".to_owned(), Value::String("function".to_owned()));
        choice.insert("name".to_owned(), Value::String("tool_search".to_owned()));
    }
    if tool_type == "custom" {
        choice.insert("type".to_owned(), Value::String("function".to_owned()));
    }
    if let Some(namespace) = choice
        .remove("namespace")
        .and_then(|value| value.as_str().map(str::to_owned))
        && let Some(name) = choice
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned)
    {
        choice.insert(
            "name".to_owned(),
            Value::String(qualify_namespace_tool_name(&namespace, &name)),
        );
    }
    let normalized_type = if matches!(tool_type.as_str(), "custom" | "tool_search") {
        "function"
    } else {
        tool_type.as_str()
    };
    let name = choice
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    available.contains(&(normalized_type.to_owned(), name.to_owned()))
}

fn normalize_input_namespace_calls(body: &mut Map<String, Value>) {
    let Some(Value::Array(input)) = body.get_mut("input") else {
        return;
    };
    for item in input {
        let Some(item) = item.as_object_mut() else {
            continue;
        };
        if !matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call" | "custom_tool_call")
        ) {
            continue;
        }
        let Some(namespace) = item
            .remove("namespace")
            .and_then(|value| value.as_str().map(str::to_owned))
        else {
            continue;
        };
        let Some(name) = item.get("name").and_then(Value::as_str).map(str::to_owned) else {
            continue;
        };
        item.insert(
            "name".to_owned(),
            Value::String(qualify_namespace_tool_name(&namespace, &name)),
        );
    }
}

fn tool_key(tool: &Value) -> Option<(String, String)> {
    let tool = tool.as_object()?;
    let tool_type = tool.get("type")?.as_str()?.to_owned();
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Some((tool_type, name))
}

fn validate_tool_output_context(body: &Map<String, Value>) -> Result<(), ProviderError> {
    let Some(Value::Array(input)) = body.get("input") else {
        return Ok(());
    };
    let mut context_ids = HashSet::new();
    for item in input {
        let Some(item) = item.as_object() else {
            continue;
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        if matches!(
            item_type,
            "function_call"
                | "custom_tool_call"
                | "local_shell_call"
                | "tool_search_call"
                | "mcp_tool_call"
        ) && let Some(call_id) = item
            .get("call_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            context_ids.insert(call_id);
        }
        if item_type == "item_reference"
            && let Some(id) = item
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        {
            context_ids.insert(id);
        }
    }
    for item in input {
        let Some(item) = item.as_object() else {
            continue;
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        if !matches!(
            item_type,
            "function_call_output"
                | "custom_tool_call_output"
                | "tool_search_output"
                | "mcp_tool_call_output"
        ) {
            continue;
        }
        let Some(call_id) = item
            .get("call_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "Grok tool output requires a non-empty call_id",
            ));
        };
        if !context_ids.contains(call_id) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "Grok tool output requires matching tool call context in input; previous_response_id-only continuation is unsupported",
            ));
        }
    }
    Ok(())
}

fn empty_object_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {}
    })
}

fn safe_function_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": true
    })
}

fn normalize_function_parameters(tool: &mut Map<String, Value>) {
    let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
    let is_automation_update = name.eq_ignore_ascii_case("codex_app__automation_update");
    let mut parameters = tool
        .remove("parameters")
        .filter(|value| !value.is_null())
        .unwrap_or_else(empty_object_schema);

    normalize_object_root_union_branches(&mut parameters);
    let needs_safe_schema = is_automation_update || !root_unions_are_object_only(&parameters);
    tool.insert(
        "parameters".to_owned(),
        if needs_safe_schema {
            safe_function_schema()
        } else {
            parameters
        },
    );
    if needs_safe_schema && tool.get("strict").and_then(Value::as_bool) == Some(true) {
        tool.insert("strict".to_owned(), Value::Bool(false));
    }
}

fn normalize_object_root_union_branches(parameters: &mut Value) {
    let Some(parameters) = parameters.as_object_mut() else {
        return;
    };
    if parameters.get("type").and_then(Value::as_str) != Some("object") {
        return;
    }
    for union_name in ["anyOf", "oneOf"] {
        let Some(Value::Array(branches)) = parameters.get_mut(union_name) else {
            continue;
        };
        for branch in branches {
            let Some(branch) = branch.as_object_mut() else {
                continue;
            };
            if !branch.contains_key("type") {
                branch.insert("type".to_owned(), Value::String("object".to_owned()));
            }
        }
    }
}

fn root_unions_are_object_only(parameters: &Value) -> bool {
    let Some(parameters) = parameters.as_object() else {
        return true;
    };
    for union_name in ["anyOf", "oneOf"] {
        let Some(Value::Array(branches)) = parameters.get(union_name) else {
            continue;
        };
        if branches.iter().any(|branch| {
            branch
                .get("type")
                .is_none_or(|schema_type| !schema_type_is_object_only(schema_type))
        }) {
            return false;
        }
    }
    true
}

fn schema_type_is_object_only(schema_type: &Value) -> bool {
    match schema_type {
        Value::String(value) => value.trim().eq_ignore_ascii_case("object"),
        Value::Array(values) if !values.is_empty() => values.iter().all(|value| {
            value
                .as_str()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("object"))
        }),
        _ => false,
    }
}

fn custom_tool_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "input": { "type": "string" }
        },
        "required": ["input"],
        "additionalProperties": false
    })
}

fn normalize_input(body: &mut Map<String, Value>, tool_search: bool) {
    let Some(Value::Array(input)) = body.get_mut("input") else {
        return;
    };

    input.retain_mut(|item| normalize_input_item(item, tool_search));
}

fn normalize_reasoning(body: &mut Map<String, Value>) {
    let Some(Value::Array(input)) = body.get_mut("input") else {
        return;
    };
    input.retain_mut(|item| {
        let Some(item) = item.as_object_mut() else {
            return true;
        };
        let is_reasoning = item.get("type").and_then(Value::as_str) == Some("reasoning");
        let is_compaction = matches!(
            item.get("type").and_then(Value::as_str),
            Some("compaction" | "compaction_summary")
        );
        if !is_reasoning && !is_compaction {
            return true;
        }
        if is_reasoning {
            item.remove("status");
            if item.get("content").is_some_and(Value::is_null) {
                item.remove("content");
            }
        }
        let encrypted_content_is_valid = match item.get("encrypted_content") {
            None => return true,
            Some(Value::String(value)) => is_grok_encrypted_content(value),
            Some(_) => false,
        };
        if encrypted_content_is_valid {
            return true;
        }
        if !is_reasoning {
            return false;
        }
        item.remove("encrypted_content");
        item.get("summary")
            .and_then(Value::as_array)
            .is_some_and(|summary| !summary.is_empty())
            || item
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|content| !content.is_empty())
    });
}

fn is_grok_encrypted_content(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed != value {
        return false;
    }
    let value = trimmed;
    if value.is_empty()
        || value.len() > MAX_ENCRYPTED_CONTENT_LEN
        || value.starts_with("gAAAA")
        || matches!(value.len(), 4_340 | 12_946)
        || value.contains('=')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
    {
        return false;
    }
    STANDARD_NO_PAD.decode(value).is_ok_and(|decoded| {
        decoded.len() >= MIN_ENCRYPTED_CONTENT_DECODED_LEN
            && !is_foreign_signature_envelope(value, &decoded)
            && byte_entropy_ratio(&decoded) >= MIN_ENCRYPTED_CONTENT_ENTROPY_RATIO
    })
}

fn byte_entropy_ratio(bytes: &[u8]) -> f64 {
    if bytes.len() <= 1 {
        return 0.0;
    }
    let mut counts = [0_usize; 256];
    for byte in bytes {
        counts[usize::from(*byte)] += 1;
    }
    let length = bytes.len() as f64;
    let entropy = counts
        .into_iter()
        .filter(|count| *count > 0)
        .map(|count| {
            let probability = count as f64 / length;
            -probability * probability.log2()
        })
        .sum::<f64>();
    entropy / (bytes.len().min(256) as f64).log2()
}

fn is_foreign_signature_envelope(encoded: &str, decoded: &[u8]) -> bool {
    match encoded.as_bytes().first() {
        Some(b'C') => is_claude_cais_envelope(decoded),
        Some(b'E') => is_claude_classic_envelope(decoded) || is_gemini_envelope(decoded),
        Some(b'R') => std::str::from_utf8(decoded).is_ok_and(|inner| {
            inner.starts_with('E')
                && STANDARD_NO_PAD
                    .decode(inner)
                    .or_else(|_| STANDARD.decode(inner))
                    .is_ok_and(|inner| is_claude_classic_envelope(&inner))
        }),
        _ => false,
    }
}

fn is_claude_classic_envelope(decoded: &[u8]) -> bool {
    protobuf_fields(decoded)
        .and_then(|fields| protobuf_bytes_field(&fields, 2))
        .and_then(protobuf_fields)
        .and_then(|fields| protobuf_bytes_field(&fields, 1))
        .and_then(protobuf_fields)
        .is_some_and(|fields| protobuf_varint_field(&fields, 1).is_some())
}

fn is_claude_cais_envelope(decoded: &[u8]) -> bool {
    let Some(fields) = protobuf_fields(decoded) else {
        return false;
    };
    if protobuf_varint_field(&fields, 1).is_none() {
        return false;
    }
    let Some(channel) = protobuf_bytes_field(&fields, 2)
        .and_then(protobuf_fields)
        .and_then(|fields| protobuf_bytes_field(&fields, 1))
        .and_then(protobuf_fields)
    else {
        return false;
    };
    protobuf_varint_field(&channel, 1).is_some()
        && protobuf_bytes_field(&channel, 5).is_some_and(|value| !value.is_empty())
        && protobuf_bytes_field(&channel, 6).is_some_and(|value| value.starts_with(b"claude-"))
}

fn is_gemini_envelope(decoded: &[u8]) -> bool {
    let Some(fields) = protobuf_fields(decoded) else {
        return false;
    };
    if fields.len() != 1 || fields[0].0 != 2 || fields[0].1 != 2 {
        return false;
    }
    let Some(container) = protobuf_fields(fields[0].2) else {
        return false;
    };
    container.len() == 1
        && container[0].0 == 1
        && container[0].1 == 2
        && container[0].2.first() == Some(&0x01)
}

fn protobuf_fields(input: &[u8]) -> Option<Vec<(u64, u8, &[u8])>> {
    let mut fields = Vec::new();
    let mut offset = 0;
    while offset < input.len() {
        let (tag, tag_len) = protobuf_varint(&input[offset..])?;
        offset += tag_len;
        let field_number = tag >> 3;
        let wire_type = (tag & 0x07) as u8;
        if field_number == 0 {
            return None;
        }
        let start = offset;
        match wire_type {
            0 => {
                let (_, length) = protobuf_varint(&input[offset..])?;
                offset += length;
            }
            1 => offset = offset.checked_add(8)?,
            2 => {
                let (length, length_len) = protobuf_varint(&input[offset..])?;
                offset += length_len;
                let value_start = offset;
                offset = offset.checked_add(usize::try_from(length).ok()?)?;
                if offset > input.len() {
                    return None;
                }
                fields.push((field_number, wire_type, &input[value_start..offset]));
                continue;
            }
            5 => offset = offset.checked_add(4)?,
            _ => return None,
        }
        if offset > input.len() {
            return None;
        }
        fields.push((field_number, wire_type, &input[start..offset]));
    }
    Some(fields)
}

fn protobuf_varint(input: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0_u64;
    for (index, byte) in input.iter().copied().take(10).enumerate() {
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

fn protobuf_bytes_field<'a>(fields: &[(u64, u8, &'a [u8])], number: u64) -> Option<&'a [u8]> {
    fields
        .iter()
        .find(|(field, wire, _)| *field == number && *wire == 2)
        .map(|(_, _, value)| *value)
}

fn protobuf_varint_field(fields: &[(u64, u8, &[u8])], number: u64) -> Option<u64> {
    fields
        .iter()
        .find(|(field, wire, _)| *field == number && *wire == 0)
        .and_then(|(_, _, value)| protobuf_varint(value).map(|(value, _)| value))
}

fn normalize_model_fields(body: &mut Map<String, Value>, model: &str) {
    let model = normalized_model_name(model).to_ascii_lowercase();
    if model == "grok-4.5" {
        for field in [
            "stop",
            "presence_penalty",
            "presencePenalty",
            "frequency_penalty",
            "frequencyPenalty",
        ] {
            body.remove(field);
        }
    }
    if model.starts_with("grok-4.20") {
        body.remove("logprobs");
        body.remove("top_logprobs");
    }
    normalize_reasoning_effort(body, &model);
}

fn normalized_model_name(model: &str) -> &str {
    model
        .trim()
        .rsplit_once('/')
        .map_or(model.trim(), |(_, model)| model.trim())
}

fn normalize_reasoning_effort(body: &mut Map<String, Value>, model: &str) {
    let supports_effort = matches!(
        model.to_ascii_lowercase().as_str(),
        "grok-4.5"
            | "grok-4.5-latest"
            | "grok-4.6"
            | "grok-4.6-latest"
            | "grok-4.3"
            | "grok-4.3-latest"
            | "grok-3-mini"
            | "grok-3-mini-fast"
            | "grok-4.20-0309-reasoning"
            | "grok-4.20-reasoning"
            | "grok-4.20-multi-agent-0309"
    );

    if let Some(Value::Object(reasoning)) = body.get_mut("reasoning")
        && let Some(effort) = reasoning.remove("effort")
        && supports_effort
        && let Some(effort) = normalized_reasoning_effort_value(&effort)
    {
        reasoning.insert("effort".to_owned(), Value::String(effort));
    }
    if body
        .get("reasoning")
        .is_some_and(|value| value.as_object().is_some_and(Map::is_empty))
    {
        body.remove("reasoning");
    }

    let snake = body.remove("reasoning_effort");
    let camel = body.remove("reasoningEffort");
    if supports_effort
        && let Some(effort) = snake
            .as_ref()
            .and_then(normalized_reasoning_effort_value)
            .or_else(|| camel.as_ref().and_then(normalized_reasoning_effort_value))
    {
        body.insert("reasoning_effort".to_owned(), Value::String(effort));
    }
}

fn normalized_reasoning_effort_value(value: &Value) -> Option<String> {
    let value = value.as_str()?.trim().to_ascii_lowercase();
    let compact = value
        .chars()
        .filter(|character| !matches!(character, '-' | '_' | ' '))
        .collect::<String>();
    match compact.as_str() {
        "none" | "low" | "medium" | "high" => Some(compact),
        "minimal" => Some("low".to_owned()),
        "xhigh" | "extrahigh" | "max" | "ultra" => Some("high".to_owned()),
        _ => None,
    }
}

fn normalize_input_item(item: &mut Value, tool_search: bool) -> bool {
    let Some(item_object) = item.as_object_mut() else {
        return true;
    };
    let Some(item_type) = item_object.get("type").and_then(Value::as_str) else {
        return true;
    };

    match item_type {
        "custom_tool_call" => {
            let has_call_id = non_empty_field(item_object, "call_id");
            let has_name = non_empty_field(item_object, "name");
            if !has_call_id || !has_name {
                return false;
            }

            let input = item_object.remove("input").unwrap_or(Value::Null);
            item_object.insert("type".to_owned(), Value::String("function_call".to_owned()));
            item_object.insert(
                "arguments".to_owned(),
                Value::String(custom_tool_arguments(input)),
            );
        }
        "custom_tool_call_output" => {
            if !non_empty_field(item_object, "call_id") {
                return false;
            }

            let output = item_object.remove("output").unwrap_or(Value::Null);
            item_object.insert(
                "type".to_owned(),
                Value::String("function_call_output".to_owned()),
            );
            item_object.insert(
                "output".to_owned(),
                Value::String(custom_tool_output(output)),
            );
        }
        "tool_search_call" if tool_search => {
            if !non_empty_field(item_object, "call_id") {
                return false;
            }
            let arguments = item_object.remove("arguments").unwrap_or(Value::Null);
            item_object.insert("type".to_owned(), Value::String("function_call".to_owned()));
            item_object.insert("name".to_owned(), Value::String("tool_search".to_owned()));
            item_object.insert(
                "arguments".to_owned(),
                Value::String(json_object_string(arguments)),
            );
            item_object.remove("execution");
        }
        "tool_search_output" if tool_search => {
            if !non_empty_field(item_object, "call_id") {
                return false;
            }
            let output = item_object.remove("output").unwrap_or(Value::Null);
            item_object.insert(
                "type".to_owned(),
                Value::String("function_call_output".to_owned()),
            );
            item_object.insert(
                "output".to_owned(),
                Value::String(custom_tool_output(output)),
            );
        }
        _ => {}
    }

    true
}

fn json_object_string(value: Value) -> String {
    match value {
        Value::String(value) => value,
        Value::Null => "{}".to_owned(),
        value => value.to_string(),
    }
}

fn non_empty_field(object: &Map<String, Value>, field: &str) -> bool {
    object
        .get(field)
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
}

fn custom_tool_arguments(input: Value) -> String {
    let input = match input {
        Value::String(text) => text,
        Value::Null => String::new(),
        value => value.to_string(),
    };
    serde_json::json!({ "input": input }).to_string()
}

fn custom_tool_output(output: Value) -> String {
    match output {
        Value::String(text) => text,
        Value::Null => String::new(),
        value => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_codex_request_for_grok() {
        let payload = Bytes::from_static(
            br#"{
                "model":"client-model",
                "stream":false,
                "stream_options":{"include_usage":true},
                "prompt_cache_key":" session-from-body ",
                "tools":[
                    {"type":"custom","name":"shell"},
                    {"type":"custom","name":"apply_patch"},
                    {"type":"function","name":"lookup"},
                    {"type":"tool_search"},
                    {"type":"web_search","external_web_access":true}
                ],
                "input":[
                    {"type":"custom_tool_call","call_id":"call_1","name":"shell","input":"pwd"},
                    {"type":"custom_tool_call_output","call_id":"call_1","output":{"ok":true}}
                ]
            }"#,
        );
        let mut request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "grok-4.5".to_owned(),
            payload,
            metadata: RequestMetadata::default(),
        };
        request.metadata.session_id = Some(" metadata-session ".to_owned());

        let prepared = prepare_request(request).expect("prepared request");
        let body: Value = serde_json::from_slice(&prepared.payload).expect("normalized JSON");

        assert_eq!(body["model"], "grok-4.5");
        assert_eq!(body["stream"], true);
        assert!(body.get("stream_options").is_none());
        assert_eq!(body["prompt_cache_key"], "metadata-session");
        assert_eq!(
            prepared.metadata.session_id.as_deref(),
            Some("metadata-session")
        );
        assert_eq!(body["tools"].as_array().expect("tools").len(), 5);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");
        assert_eq!(body["tools"][0]["parameters"]["required"][0], "input");
        assert_eq!(
            body["tools"][0]["parameters"]["additionalProperties"],
            false
        );
        assert_eq!(body["tools"][1]["name"], "apply_patch");
        assert_eq!(body["tools"][1]["parameters"]["required"][0], "input");
        assert_eq!(body["tools"][2]["parameters"]["type"], "object");
        assert_eq!(body["tools"][3]["name"], "tool_search");
        assert!(body["tools"][4].get("external_web_access").is_none());
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][0]["arguments"], r#"{"input":"pwd"}"#);
        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["input"][1]["output"], r#"{"ok":true}"#);
        assert!(prepared.tool_mappings.custom_tools.contains("shell"));
        assert!(prepared.tool_mappings.custom_tools.contains("apply_patch"));
        assert!(prepared.tool_mappings.tool_search);
    }

    #[test]
    fn rejects_invalid_json_without_echoing_request() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "grok-4.5".to_owned(),
            payload: Bytes::from_static(br#"{"secret":"do-not-echo""#),
            metadata: RequestMetadata::default(),
        };

        let error = prepare_request(request).expect_err("invalid JSON");

        assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
        assert!(!error.message().contains("do-not-echo"));
    }

    #[test]
    fn rejects_previous_response_id_instead_of_silently_dropping_context() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "grok-4.5".to_owned(),
            payload: Bytes::from_static(
                br#"{"previous_response_id":"resp_previous","input":"continue"}"#,
            ),
            metadata: RequestMetadata::default(),
        };

        let error = prepare_request(request).expect_err("unsupported continuation");

        assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
        assert!(error.message().contains("complete input history"));
    }

    #[test]
    fn drops_cross_provider_reasoning_signatures_and_keeps_grok_replay() {
        let grok_signature = STANDARD_NO_PAD.encode((0_u8..64).collect::<Vec<_>>());
        let payload = serde_json::to_vec(&serde_json::json!({
            "input": [
                {
                    "type": "reasoning",
                    "summary": [{"type":"summary_text","text":"Claude summary"}],
                    "encrypted_content": "Eclaude-signature"
                },
                {
                    "type": "reasoning",
                    "status": "completed",
                    "content": null,
                    "summary": [{"type":"summary_text","text":"Grok summary"}],
                    "encrypted_content": grok_signature
                }
            ]
        }))
        .expect("request JSON");
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "grok-4.5".to_owned(),
            payload: payload.into(),
            metadata: RequestMetadata::default(),
        };

        let prepared = prepare_request(request).expect("prepared request");
        let body: Value = serde_json::from_slice(&prepared.payload).expect("normalized JSON");

        assert_eq!(body["input"].as_array().expect("input").len(), 2);
        assert_eq!(body["input"][0]["summary"][0]["text"], "Claude summary");
        assert!(body["input"][0].get("encrypted_content").is_none());
        assert_eq!(body["input"][1]["encrypted_content"], grok_signature);
        assert_eq!(body["input"][1]["summary"][0]["text"], "Grok summary");
        assert!(body["input"][1].get("status").is_none());
        assert!(body["input"][1].get("content").is_none());
    }

    #[test]
    fn strips_only_encrypted_reasoning_for_recovery_retry() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "grok-4.5".to_owned(),
            payload: Bytes::from_static(br#"{
                "input":[
                    {"type":"reasoning","summary":[{"type":"summary_text","text":"keep"}],"content":null,"encrypted_content":"opaque"},
                    {"type":"compaction","encrypted_content":"opaque"},
                    {"type":"message","role":"user","content":"continue"}
                ]
            }"#),
            metadata: RequestMetadata::default(),
        };

        let retry = strip_encrypted_reasoning_for_retry(&request)
            .expect("retry request")
            .expect("changed request");
        let body: Value = serde_json::from_slice(&retry.payload).expect("retry JSON");

        assert_eq!(body["input"].as_array().expect("input").len(), 2);
        assert_eq!(body["input"][0]["summary"][0]["text"], "keep");
        assert!(body["input"][0].get("encrypted_content").is_none());
        assert!(body["input"][0].get("content").is_none());
        assert_eq!(body["input"][1]["type"], "message");
    }

    #[test]
    fn promotes_additional_tools_and_prunes_orphaned_choices() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "grok-4.5".to_owned(),
            payload: Bytes::from_static(
                br#"{
                "tools":[
                    {"type":"function","name":"lookup","parameters":null},
                    {"type":"tool_search"},
                    {"type":"namespace","name":"codex_app","tools":[
                        {"type":"function","name":"inner"}
                    ]}
                ],
                "tool_choice":{"type":"allowed_tools","mode":"required","tools":[
                    {"type":"function","name":"lookup"},
                    {"type":"tool_search"},
                    {"type":"function","namespace":"codex_app","name":"inner"}
                ]},
                "input":[
                    {"type":"additional_tools","role":"developer","tools":[
                        {"type":"function","name":"extra"}
                    ]},
                    {"type":"message","role":"user","content":"hello"}
                ]
            }"#,
            ),
            metadata: RequestMetadata::default(),
        };

        let prepared = prepare_request(request).expect("prepared request");
        let body: Value = serde_json::from_slice(&prepared.payload).expect("normalized JSON");

        assert_eq!(body["tools"].as_array().expect("tools").len(), 4);
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");
        assert_eq!(body["tools"][1]["name"], "tool_search");
        assert_eq!(body["tools"][2]["name"], "codex_app__inner");
        assert_eq!(body["tools"][3]["name"], "extra");
        assert_eq!(body["tools"][3]["parameters"]["type"], "object");
        assert_eq!(body["input"].as_array().expect("input").len(), 1);
        assert_eq!(
            body["tool_choice"]["tools"]
                .as_array()
                .expect("choices")
                .len(),
            3
        );
        assert_eq!(body["tool_choice"]["tools"][0]["name"], "lookup");
        assert_eq!(body["tool_choice"]["tools"][1]["name"], "tool_search");
        assert_eq!(body["tool_choice"]["tools"][2]["name"], "codex_app__inner");
        assert_eq!(
            prepared
                .tool_mappings
                .namespace_tools
                .get("codex_app__inner"),
            Some(&NamespaceToolRef {
                namespace: "codex_app".to_owned(),
                name: "inner".to_owned(),
            })
        );
    }

    #[test]
    fn rejects_unpaired_tool_outputs_before_upstream() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "grok-4.5".to_owned(),
            payload: Bytes::from_static(
                br#"{"input":[{"type":"function_call_output","call_id":"call_missing","output":"done"}]}"#,
            ),
            metadata: RequestMetadata::default(),
        };

        let error = prepare_request(request).expect_err("unpaired output");

        assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
        assert!(error.message().contains("matching tool call context"));
    }

    #[test]
    fn rejects_namespace_tool_name_collisions() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "grok-4.5".to_owned(),
            payload: Bytes::from_static(
                br#"{
                "tools":[
                    {"type":"function","name":"codex_app__inner"},
                    {"type":"namespace","name":"codex_app","tools":[
                        {"type":"function","name":"inner"}
                    ]}
                ],
                "input":"hello"
            }"#,
            ),
            metadata: RequestMetadata::default(),
        };

        let error = prepare_request(request).expect_err("namespace collision");

        assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
        assert!(error.message().contains("unique"));
    }

    #[test]
    fn normalizes_namespaced_custom_tool_history_with_reversible_mappings() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "grok-4.5".to_owned(),
            payload: Bytes::from_static(
                br#"{
                    "tools":[{"type":"namespace","name":"terminal","tools":[
                        {"type":"custom","name":"exec","format":{"type":"text"}}
                    ]}],
                    "input":[
                        {"type":"custom_tool_call","namespace":"terminal","name":"exec","call_id":"call_1","input":"pwd"},
                        {"type":"custom_tool_call_output","call_id":"call_1","output":"done"}
                    ]
                }"#,
            ),
            metadata: RequestMetadata::default(),
        };

        let prepared = prepare_request(request).expect("prepared request");
        let body: Value = serde_json::from_slice(&prepared.payload).expect("normalized JSON");

        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "terminal__exec");
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][0]["name"], "terminal__exec");
        assert!(body["input"][0].get("namespace").is_none());
        assert_eq!(body["input"][0]["arguments"], r#"{"input":"pwd"}"#);
        assert!(
            prepared
                .tool_mappings
                .custom_tools
                .contains("terminal__exec")
        );
        assert_eq!(
            prepared.tool_mappings.namespace_tools.get("terminal__exec"),
            Some(&NamespaceToolRef {
                namespace: "terminal".to_owned(),
                name: "exec".to_owned(),
            })
        );
    }

    #[test]
    fn normalizes_tool_search_declaration_choice_and_history() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "grok-4.5".to_owned(),
            payload: Bytes::from_static(
                br#"{
                    "tools":[{"type":"tool_search"}],
                    "tool_choice":{"type":"tool_search"},
                    "input":[
                        {"type":"tool_search_call","call_id":"search_1","arguments":{"query":"git"},"execution":"client"},
                        {"type":"tool_search_output","call_id":"search_1","output":{"groups":["git"]}}
                    ]
                }"#,
            ),
            metadata: RequestMetadata::default(),
        };

        let prepared = prepare_request(request).expect("prepared request");
        let body: Value = serde_json::from_slice(&prepared.payload).expect("normalized JSON");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "tool_search");
        assert_eq!(body["tool_choice"]["type"], "function");
        assert_eq!(body["tool_choice"]["name"], "tool_search");
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][0]["name"], "tool_search");
        assert_eq!(body["input"][0]["arguments"], r#"{"query":"git"}"#);
        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["input"][1]["output"], r#"{"groups":["git"]}"#);
        assert!(prepared.tool_mappings.tool_search);
    }

    #[test]
    fn preserves_namespaced_apply_patch_with_response_mappings() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "grok-4.5".to_owned(),
            payload: Bytes::from_static(
                br#"{
                "tools":[
                    {"type":"namespace","name":"codex_app","tools":[
                        {"type":"custom","name":"apply_patch"}
                    ]}
                ],
                "input":"hello"
            }"#,
            ),
            metadata: RequestMetadata::default(),
        };

        let prepared = prepare_request(request).expect("prepared request");

        let body: Value = serde_json::from_slice(&prepared.payload).expect("normalized JSON");
        assert_eq!(body["tools"][0]["name"], "codex_app__apply_patch");
        assert!(
            prepared
                .tool_mappings
                .custom_tools
                .contains("codex_app__apply_patch")
        );
        assert_eq!(
            prepared
                .tool_mappings
                .namespace_tools
                .get("codex_app__apply_patch"),
            Some(&NamespaceToolRef {
                namespace: "codex_app".to_owned(),
                name: "apply_patch".to_owned(),
            })
        );
    }

    #[test]
    fn rewrites_forced_tool_search_to_its_function_proxy() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "grok-4.5".to_owned(),
            payload: Bytes::from_static(
                br#"{
                "tools":[
                    {"type":"function","name":"lookup"},
                    {"type":"tool_search"}
                ],
                "tool_choice":{"type":"tool_search"},
                "input":"hello"
            }"#,
            ),
            metadata: RequestMetadata::default(),
        };

        let prepared = prepare_request(request).expect("prepared request");
        let body: Value = serde_json::from_slice(&prepared.payload).expect("normalized JSON");
        assert_eq!(body["tool_choice"]["type"], "function");
        assert_eq!(body["tool_choice"]["name"], "tool_search");
        assert!(prepared.tool_mappings.tool_search);
    }

    #[test]
    fn accepts_high_entropy_32_byte_grok_content_without_broad_prefix_rejection() {
        let mut decoded = (0_u8..32).collect::<Vec<_>>();
        decoded[0] = 0x12;
        let encrypted_content = STANDARD_NO_PAD.encode(decoded);
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "grok-4.5".to_owned(),
            payload: serde_json::to_vec(&serde_json::json!({
                "input": [{
                    "type": "reasoning",
                    "summary": [{"type":"summary_text","text":"keep"}],
                    "encrypted_content": encrypted_content
                }]
            }))
            .expect("request JSON")
            .into(),
            metadata: RequestMetadata::default(),
        };

        let prepared = prepare_request(request).expect("prepared request");
        let body: Value = serde_json::from_slice(&prepared.payload).expect("normalized JSON");

        assert_eq!(body["input"][0]["encrypted_content"], encrypted_content);
    }

    #[test]
    fn removes_low_entropy_content_but_preserves_reasoning_summary() {
        let encrypted_content = STANDARD_NO_PAD.encode([0xa5; 64]);
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "grok-4.5".to_owned(),
            payload: serde_json::to_vec(&serde_json::json!({
                "input": [{
                    "type": "reasoning",
                    "summary": [{"type":"summary_text","text":"keep"}],
                    "encrypted_content": encrypted_content
                }]
            }))
            .expect("request JSON")
            .into(),
            metadata: RequestMetadata::default(),
        };

        let prepared = prepare_request(request).expect("prepared request");
        let body: Value = serde_json::from_slice(&prepared.payload).expect("normalized JSON");

        assert_eq!(body["input"][0]["summary"][0]["text"], "keep");
        assert!(body["input"][0].get("encrypted_content").is_none());
    }

    #[test]
    fn normalizes_root_union_schemas_and_automation_update_safely() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "grok-4.5".to_owned(),
            payload: Bytes::from_static(br#"{
                "tools":[
                    {"type":"function","name":"crop","strict":true,"parameters":{
                        "type":"object","oneOf":[{"required":["radius"]},{"required":["size"]}]
                    }},
                    {"type":"function","name":"nullable","strict":true,"parameters":{
                        "anyOf":[{"type":"object"},{"type":"null"}]
                    }},
                    {"type":"function","name":"codex_app__automation_update","strict":true,"parameters":{
                        "type":"object","oneOf":[{"type":"object"}],"$defs":{"large":{}}
                    }}
                ],
                "input":"hello"
            }"#),
            metadata: RequestMetadata::default(),
        };

        let prepared = prepare_request(request).expect("prepared request");
        let body: Value = serde_json::from_slice(&prepared.payload).expect("normalized JSON");

        assert_eq!(body["tools"][0]["parameters"]["oneOf"][0]["type"], "object");
        assert_eq!(body["tools"][0]["strict"], true);
        for index in [1, 2] {
            assert_eq!(body["tools"][index]["parameters"]["type"], "object");
            assert_eq!(
                body["tools"][index]["parameters"]["additionalProperties"],
                true
            );
            assert_eq!(body["tools"][index]["strict"], false);
            assert!(body["tools"][index]["parameters"].get("anyOf").is_none());
            assert!(body["tools"][index]["parameters"].get("oneOf").is_none());
        }
    }

    #[test]
    fn rewrites_forced_web_search_as_required_allowed_tools() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "grok-4.5".to_owned(),
            payload: Bytes::from_static(
                br#"{"tools":[{"type":"web_search"}],"tool_choice":{"type":"web_search"},"input":"hello"}"#,
            ),
            metadata: RequestMetadata::default(),
        };

        let prepared = prepare_request(request).expect("prepared request");
        let body: Value = serde_json::from_slice(&prepared.payload).expect("normalized JSON");

        assert_eq!(body["tool_choice"]["type"], "allowed_tools");
        assert_eq!(body["tool_choice"]["mode"], "required");
        assert_eq!(body["tool_choice"]["tools"][0]["type"], "web_search");
    }

    #[test]
    fn cleans_model_specific_sampling_and_reasoning_fields() {
        let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "xai/grok-4.20-0309-non-reasoning".to_owned(),
            payload: Bytes::from_static(
                br#"{
                "input":"hello",
                "stop":["done"],
                "presence_penalty":0.1,
                "logprobs":true,
                "top_logprobs":5,
                "reasoning":{"effort":"high","summary":"auto"},
                "reasoningEffort":"max"
            }"#,
            ),
            metadata: RequestMetadata::default(),
        };

        let prepared = prepare_request(request).expect("prepared request");
        let body: Value = serde_json::from_slice(&prepared.payload).expect("normalized JSON");

        assert_eq!(body["stop"][0], "done");
        assert_eq!(body["presence_penalty"], 0.1);
        assert!(body.get("logprobs").is_none());
        assert!(body.get("top_logprobs").is_none());
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert!(body["reasoning"].get("effort").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("reasoningEffort").is_none());
    }
}
