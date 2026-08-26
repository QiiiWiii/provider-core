use provider_core::ProviderError;
use serde_json::{Map, Value, json};

use super::schema::sanitize_response_schema;
use super::validation::invalid;

pub(super) fn convert_generation_config(
    root: &Map<String, Value>,
) -> Result<Option<Value>, ProviderError> {
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
