use provider_core::{ProviderError, ProviderErrorKind};
use serde_json::{Map, Value};
use std::collections::HashSet;

use crate::responses_history::item_call_id;

pub(super) fn reject_unknown_input_item_types(
    body: &Map<String, Value>,
) -> Result<(), ProviderError> {
    let Some(Value::Array(input)) = body.get("input") else {
        return Ok(());
    };
    for item in input {
        let Some(item) = item.as_object() else {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "Grok input items must be JSON objects",
            ));
        };
        match item.get("type").and_then(Value::as_str).map(str::trim) {
            None => {
                if item
                    .get("role")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .is_some_and(|role| !role.is_empty())
                {
                    continue;
                }
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "Grok input items require a supported type or role",
                ));
            }
            Some(
                "message"
                | "function_call"
                | "function_call_output"
                | "reasoning"
                | "compaction"
                | "compaction_summary",
            ) => {}
            Some(item_type) => {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    format!("Grok HTTP Responses does not support input item type `{item_type}`"),
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_tool_output_context(body: &Map<String, Value>) -> Result<(), ProviderError> {
    let Some(Value::Array(input)) = body.get("input") else {
        return Ok(());
    };
    let mut context_ids = HashSet::new();
    for item in input {
        let Some(item) = item.as_object() else {
            continue;
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        if item_type == "function_call"
            && let Some(call_id) = item_call_id(item)
        {
            context_ids.insert(call_id);
        }
    }
    for item in input {
        let Some(item) = item.as_object() else {
            continue;
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        if item_type != "function_call_output" {
            continue;
        }
        let Some(call_id) = item_call_id(item) else {
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
