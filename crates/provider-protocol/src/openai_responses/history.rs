use std::collections::{HashMap, HashSet};

use provider_core::{ProviderError, ProviderErrorKind};
use serde_json::{Map, Value};

#[derive(Clone, Copy, Eq, PartialEq)]
enum ToolCallKind {
    Function,
    Custom,
    Shell,
    ApplyPatch,
    ToolSearch,
    Mcp,
}

pub(super) fn validate_tool_pairing(items: &[Value]) -> Result<(), ProviderError> {
    let mut pending = HashMap::new();
    let mut seen = HashSet::new();
    for item in items {
        let Some(item) = item.as_object() else {
            continue;
        };
        let Some(item_type) = item.get("type").and_then(Value::as_str) else {
            continue;
        };
        if let Some(kind) = call_kind(item_type) {
            let call_id = required_call_id(item)?;
            if !seen.insert(call_id.to_owned()) {
                return Err(invalid("Responses tool call_id values must be unique"));
            }
            pending.insert(call_id.to_owned(), kind);
            continue;
        }
        if let Some(kind) = output_kind(item_type) {
            let call_id = required_call_id(item)?;
            match pending.remove(call_id) {
                Some(pending_kind) if pending_kind == kind => {}
                Some(_) => {
                    return Err(invalid(
                        "Responses tool output type must match its preceding tool call",
                    ));
                }
                None => {
                    return Err(invalid(
                        "Responses tool output requires a preceding tool call with the same call_id",
                    ));
                }
            }
            continue;
        }
        if !pending.is_empty() && !matches!(item_type, "additional_tools" | "compaction_trigger") {
            return Err(invalid(
                "Responses tool outputs must immediately follow their tool calls",
            ));
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(invalid(
            "Responses tool calls in complete history require matching outputs",
        ))
    }
}

fn call_kind(item_type: &str) -> Option<ToolCallKind> {
    match item_type {
        "function_call" => Some(ToolCallKind::Function),
        "custom_tool_call" => Some(ToolCallKind::Custom),
        "local_shell_call" | "shell_call" => Some(ToolCallKind::Shell),
        "apply_patch_call" => Some(ToolCallKind::ApplyPatch),
        "tool_search_call" => Some(ToolCallKind::ToolSearch),
        "mcp_tool_call" => Some(ToolCallKind::Mcp),
        _ => None,
    }
}

fn output_kind(item_type: &str) -> Option<ToolCallKind> {
    match item_type {
        "function_call_output" => Some(ToolCallKind::Function),
        "custom_tool_call_output" => Some(ToolCallKind::Custom),
        "local_shell_call_output" | "shell_call_output" => Some(ToolCallKind::Shell),
        "apply_patch_call_output" => Some(ToolCallKind::ApplyPatch),
        "tool_search_output" => Some(ToolCallKind::ToolSearch),
        "mcp_tool_call_output" => Some(ToolCallKind::Mcp),
        _ => None,
    }
}

fn required_call_id(item: &Map<String, Value>) -> Result<&str, ProviderError> {
    item.get("call_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid("Responses tool history requires a call_id"))
}

fn invalid(message: &'static str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}
