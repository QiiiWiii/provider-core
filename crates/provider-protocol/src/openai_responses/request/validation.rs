use provider_core::{ProviderError, ProviderErrorKind};
use serde_json::{Map, Value};

use super::{invalid, optional_bool, optional_string, reject_unknown_non_null_fields};

pub(super) fn reject_unsupported_fields(source: &Map<String, Value>) -> Result<(), ProviderError> {
    reject_unknown_non_null_fields(
        source,
        &[
            "background",
            "include",
            "input",
            "instructions",
            "max_output_tokens",
            "model",
            "parallel_tool_calls",
            "prompt_cache_key",
            "reasoning",
            "service_tier",
            "store",
            "stream",
            "stream_options",
            "temperature",
            "text",
            "tool_choice",
            "tools",
            "top_p",
            "client_metadata",
        ],
        "request",
    )?;
    optional_string(source, "prompt_cache_key")?;
    match source.get("client_metadata") {
        None | Some(Value::Null | Value::Object(_)) => {}
        Some(_) => {
            return Err(invalid(
                "Responses client_metadata must be an object or null",
            ));
        }
    }
    if let Some(stream_options) = source
        .get("stream_options")
        .filter(|value| !value.is_null())
    {
        let stream_options = stream_options
            .as_object()
            .ok_or_else(|| invalid("Responses stream_options must be an object"))?;
        reject_unknown_non_null_fields(
            stream_options,
            &["include_obfuscation", "include_usage"],
            "stream_options",
        )?;
        optional_bool(stream_options, "include_obfuscation")?;
        optional_bool(stream_options, "include_usage")?;
    }
    if source
        .get("background")
        .is_some_and(|value| !value.is_null() && value != &Value::Bool(false))
    {
        return Err(invalid(
            "Responses field background cannot be converted to Chat Completions",
        ));
    }
    for field in [
        "conversation",
        "max_tool_calls",
        "modalities",
        "previous_response_id",
        "prompt",
    ] {
        if source.get(field).is_some_and(|value| !value.is_null()) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!("Responses field {field} cannot be converted to Chat Completions"),
            ));
        }
    }
    match source.get("store") {
        None | Some(Value::Null | Value::Bool(false)) => {}
        Some(Value::Bool(true)) => {
            return Err(invalid(
                "Responses field store=true cannot be converted to stateless Chat Completions",
            ));
        }
        Some(_) => return Err(invalid("Responses store must be a boolean or null")),
    }
    if let Some(include) = source.get("include").filter(|value| !value.is_null()) {
        let include = include
            .as_array()
            .ok_or_else(|| invalid("Responses include must be an array"))?;
        if include
            .iter()
            .any(|value| value.as_str() != Some("reasoning.encrypted_content"))
        {
            return Err(invalid(
                "Responses include requests data unavailable from Chat Completions",
            ));
        }
    }
    Ok(())
}
