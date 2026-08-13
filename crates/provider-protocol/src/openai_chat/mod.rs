use bytes::Bytes;
use provider_core::{ProviderError, ProviderErrorKind, ProviderRequest};
use serde_json::Value;

pub(crate) fn omit_tool_images(request: &mut ProviderRequest) -> Result<(), ProviderError> {
    let mut body: Value = serde_json::from_slice(&request.payload)
        .map_err(|_| invalid_request("Chat Completions request body must be valid JSON"))?;
    let messages = body
        .as_object_mut()
        .and_then(|body| body.get_mut("messages"))
        .and_then(Value::as_array_mut)
        .ok_or_else(|| invalid_request("Chat Completions request requires messages"))?;
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("tool") {
            continue;
        }
        let Some(content) = message
            .as_object_mut()
            .and_then(|message| message.get_mut("content"))
        else {
            continue;
        };
        if content.is_array() {
            *content = Value::String(tool_output_without_images(content)?);
        }
    }
    request.payload = serde_json::to_vec(&body).map(Bytes::from).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Internal,
            "failed to serialize Chat Completions request",
        )
    })?;
    Ok(())
}

fn tool_output_without_images(output: &Value) -> Result<String, ProviderError> {
    let Value::Array(parts) = output else {
        return Ok(json_string(output));
    };
    let mut flattened = Vec::with_capacity(parts.len());
    for part in parts {
        let part = part
            .as_object()
            .ok_or_else(|| invalid_request("tool content parts must be objects"))?;
        match part.get("type").and_then(Value::as_str).unwrap_or_default() {
            "input_text" | "output_text" | "text" => flattened.push(
                part.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ),
            "input_image" | "image_url" | "image" => {
                flattened.push("[image omitted: unsupported by upstream]".to_owned())
            }
            _ => flattened.push(Value::Object(part.clone()).to_string()),
        }
    }
    Ok(flattened.join("\n\n"))
}

fn json_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn invalid_request(message: &'static str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

#[cfg(test)]
mod tests {
    use provider_core::{RequestMetadata, WireFormat};

    use super::*;

    #[test]
    fn native_chat_tool_arrays_are_unchanged_unless_omission_is_requested() {
        let request = || ProviderRequest {
            format: WireFormat::OpenAiChatCompletions,
            model: "upstream-model".to_owned(),
            payload: Bytes::from_static(
                br#"{"messages":[{"role":"tool","content":[{"type":"text","text":"ok"},{"type":"image_url","image_url":{"url":"data:image/png;base64,tool"}}]}]}"#,
            ),
            metadata: RequestMetadata::default(),
        };
        let original = request();
        let mut omitted = request();

        omit_tool_images(&mut omitted).expect("omit tool image");

        assert!(serde_json::from_slice::<Value>(&original.payload).expect("JSON")["messages"][0]
            ["content"]
            .is_array());
        assert_eq!(
            serde_json::from_slice::<Value>(&omitted.payload).expect("JSON")["messages"][0]
                ["content"],
            "ok\n\n[image omitted: unsupported by upstream]"
        );
    }
}
