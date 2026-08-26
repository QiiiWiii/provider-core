use bytes::Bytes;
use futures_util::StreamExt;
use provider_core::{ProviderError, ProviderErrorKind};
use serde_json::{Value, json};

use super::super::request::ToolIdentity;
use super::format::{
    convert_usage, custom_tool_input, format_response_signature, normalize_response_id,
    stream_error,
};
use super::sse::frame_data;
use super::{ResponseStream, STREAM_IDLE_TIMEOUT};

impl ResponseStream {
    pub(super) async fn next_output(&mut self) -> Option<Result<Bytes, ProviderError>> {
        loop {
            if let Some(item) = self.ready.pop_front() {
                return Some(item);
            }
            if self.eof {
                if let Some(error) = self.terminal_error.take() {
                    return Some(Err(error));
                }
                return None;
            }
            match tokio::time::timeout(STREAM_IDLE_TIMEOUT, self.upstream.next()).await {
                Ok(Some(Ok(chunk))) => match self.decoder.push(&chunk) {
                    Ok(frames) => {
                        for frame in frames {
                            if let Err(error) = self.process_frame(&frame) {
                                self.fail(error);
                                break;
                            }
                        }
                    }
                    Err(_) => {
                        self.fail(ProviderError::new(
                            ProviderErrorKind::Upstream,
                            "Antigravity upstream sent an oversized event",
                        ));
                    }
                },
                Ok(Some(Err(error))) => self.fail(error),
                Ok(None) => {
                    if let Some(frame) = self.decoder.finish()
                        && let Err(error) = self.process_frame(&frame)
                    {
                        self.fail(error);
                    }
                    if !self.completed && self.terminal_error.is_none() {
                        self.fail(ProviderError::new(
                            ProviderErrorKind::Upstream,
                            "Antigravity upstream stream ended before a terminal finishReason",
                        ));
                    }
                    self.eof = true;
                }
                Err(_) => self.fail(
                    ProviderError::new(
                        ProviderErrorKind::Upstream,
                        "Antigravity upstream stream idle timeout",
                    )
                    .with_failover_reason(provider_core::ProviderFailoverReason::CapacityExhausted),
                ),
            }
        }
    }

    fn process_frame(&mut self, frame: &[u8]) -> Result<(), ProviderError> {
        let Some(payload) = frame_data(frame) else {
            return Ok(());
        };
        let value: Value = serde_json::from_slice(&payload).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Upstream,
                "Antigravity upstream returned invalid JSON",
            )
        })?;
        self.process_payload(&value)
    }

    fn process_payload(&mut self, payload: &Value) -> Result<(), ProviderError> {
        if self.completed {
            return Ok(());
        }
        if payload.get("error").is_some() {
            return Err(stream_error(payload.get("error").unwrap_or(payload)));
        }
        let response = payload.get("response").unwrap_or(payload);
        if response.get("error").is_some() {
            return Err(stream_error(response.get("error").unwrap_or(response)));
        }
        if let Some(response_id) = response
            .get("responseId")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            && !self.started
        {
            self.response_id = normalize_response_id(response_id);
        }
        if let Some(usage) = response.get("usageMetadata") {
            self.usage = Some(convert_usage(usage));
        }
        if let Some(usage) = payload.get("usageMetadata") {
            self.usage = Some(convert_usage(usage));
        }
        self.start();
        if let Some(candidate) = response
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
        {
            self.capture_grounding(candidate.get("groundingMetadata"));
            if let Some(parts) = candidate
                .get("content")
                .and_then(|content| content.get("parts"))
                .and_then(Value::as_array)
            {
                for part in parts {
                    self.process_part(part)?;
                }
            }
            if let Some(reason) = candidate
                .get("finishReason")
                .and_then(Value::as_str)
                .filter(|reason| {
                    !reason.trim().is_empty()
                        && !matches!(*reason, "UNSPECIFIED" | "FINISH_REASON_UNSPECIFIED")
                })
            {
                self.complete(Some(reason));
            }
        }
        Ok(())
    }

    fn process_part(&mut self, part: &Value) -> Result<(), ProviderError> {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if part
                .get("thought")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                self.close_message();
                let signature = part
                    .get("thoughtSignature")
                    .or_else(|| part.get("thought_signature"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !signature.is_empty() {
                    self.seen_signatures.insert(signature.to_owned());
                }
                if self
                    .reasoning
                    .as_ref()
                    .is_some_and(|reasoning| reasoning.signature != signature)
                {
                    self.close_reasoning();
                }
                self.open_reasoning(signature);
                if !text.is_empty() {
                    if let Some(reasoning) = self.reasoning.as_mut() {
                        reasoning.text.push_str(text);
                    }
                    self.emit(
                        "response.reasoning_summary_text.delta",
                        json!({
                            "item_id": self.reasoning.as_ref().map(|value| value.id.clone()).unwrap_or_default(),
                            "output_index": self.reasoning.as_ref().map_or(0, |value| value.output_index),
                            "summary_index": 0,
                            "delta": text
                        }),
                    );
                }
            } else {
                if let Some(signature) = part
                    .get("thoughtSignature")
                    .or_else(|| part.get("thought_signature"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    && self.seen_signatures.insert(signature.to_owned())
                {
                    self.close_message();
                    self.emit_reasoning_carrier(signature, "next", "text");
                }
                self.close_reasoning();
                self.last_reasoning_signature = None;
                self.open_message();
                if !text.is_empty() {
                    if let Some(message) = self.message.as_mut() {
                        message.text.push_str(text);
                    }
                    self.emit(
                        "response.output_text.delta",
                        json!({
                            "item_id": self.message.as_ref().map(|value| value.id.clone()).unwrap_or_default(),
                            "output_index": self.message.as_ref().map_or(0, |value| value.output_index),
                            "content_index": 0,
                            "delta": text
                        }),
                    );
                }
            }
        }
        if let Some(function_call) = part.get("functionCall").and_then(Value::as_object) {
            self.close_reasoning();
            self.close_message();
            let from_previous_reasoning = part
                .get("thoughtSignature")
                .or_else(|| part.get("thought_signature"))
                .and_then(Value::as_str)
                .is_none()
                && self.last_reasoning_signature.is_some();
            let signature = part
                .get("thoughtSignature")
                .or_else(|| part.get("thought_signature"))
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .or_else(|| self.last_reasoning_signature.take());
            if let Some(signature) = signature.as_deref()
                && (from_previous_reasoning || self.seen_signatures.insert(signature.to_owned()))
                && !self.target_claude
            {
                self.emit_reasoning_carrier(signature, "next", "function");
            }
            let tool_signature = signature
                .as_deref()
                .map(|signature| format_response_signature(signature, self.target_claude));
            let output_index = self.next_output_index;
            let upstream_name = function_call
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("function");
            let identity = self
                .tool_identities
                .get(upstream_name)
                .cloned()
                .unwrap_or_else(|| ToolIdentity {
                    name: upstream_name.to_owned(),
                    namespace: None,
                    custom: false,
                });
            let call_id = function_call
                .get("id")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("call_{output_index}"));
            let arguments = function_call
                .get("args")
                .map(Value::to_string)
                .unwrap_or_else(|| "{}".to_owned());
            self.next_output_index = self.next_output_index.saturating_add(1);
            let item_prefix = if identity.custom { "ctc" } else { "fc" };
            let item_id = format!("{item_prefix}_{}_{}", self.response_id, output_index);
            let item_type = if identity.custom {
                "custom_tool_call"
            } else {
                "function_call"
            };
            let input = if identity.custom {
                custom_tool_input(function_call.get("args").unwrap_or(&Value::Null))
            } else {
                String::new()
            };
            let mut added_item = if identity.custom {
                json!({
                    "id": item_id.clone(),
                    "type": item_type,
                    "status": "in_progress",
                    "call_id": call_id.clone(),
                    "name": identity.name.clone(),
                    "input": input.clone()
                })
            } else {
                json!({
                    "id": item_id.clone(),
                    "type": item_type,
                    "status": "in_progress",
                    "call_id": call_id.clone(),
                    "name": identity.name.clone(),
                    "arguments": ""
                })
            };
            if let Some(namespace) = identity.namespace.clone() {
                added_item["namespace"] = Value::String(namespace);
            }
            if let Some(signature) = tool_signature.as_deref() {
                added_item["signature"] = Value::String(signature.to_owned());
            }
            self.emit(
                "response.output_item.added",
                json!({
                    "output_index": output_index,
                    "item": added_item
                }),
            );
            if identity.custom {
                self.emit(
                    "response.custom_tool_call_input.done",
                    json!({
                        "output_index": output_index,
                        "item_id": item_id.clone(),
                        "input": input.clone()
                    }),
                );
            } else {
                self.emit(
                    "response.function_call_arguments.delta",
                    json!({
                        "output_index": output_index,
                        "item_id": item_id.clone(),
                        "delta": arguments.clone()
                    }),
                );
                self.emit(
                    "response.function_call_arguments.done",
                    json!({
                        "output_index": output_index,
                        "item_id": item_id.clone(),
                        "arguments": arguments.clone()
                    }),
                );
            }
            let mut item = if identity.custom {
                json!({
                    "id": item_id.clone(),
                    "type": item_type,
                    "status": "completed",
                    "name": identity.name,
                    "call_id": call_id.clone(),
                    "input": input
                })
            } else {
                json!({
                    "id": item_id.clone(),
                    "type": item_type,
                    "status": "completed",
                    "name": identity.name,
                    "call_id": call_id.clone(),
                    "arguments": arguments.clone()
                })
            };
            if let Some(namespace) = self
                .tool_identities
                .get(upstream_name)
                .and_then(|identity| identity.namespace.clone())
            {
                item["namespace"] = Value::String(namespace);
            }
            if let Some(signature) = tool_signature {
                item["signature"] = Value::String(signature);
            }
            self.emit(
                "response.output_item.done",
                json!({"output_index": output_index, "item": item.clone()}),
            );
            self.output.push(item);
        }
        if let Some(data) = part.get("inlineData").or_else(|| part.get("inline_data")) {
            self.process_inline_data(data);
        }
        Ok(())
    }
}
