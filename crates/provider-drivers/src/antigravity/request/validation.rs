use provider_core::{ProviderError, ProviderErrorKind, ProviderRequest};
use serde_json::{Map, Value};

pub(super) fn validate_continuation_input(
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

pub(super) fn required_string<'a>(
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

pub(super) fn invalid(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}
