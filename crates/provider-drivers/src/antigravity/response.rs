use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use provider_core::{ProviderError, ProviderErrorKind, ProviderStream};
use serde_json::{Map, Value, json};

use super::request::{ToolIdentity, response_tool_identities};

const MAX_PENDING_FRAME: usize = 1024 * 1024;
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub(crate) fn translate_stream(
    upstream: ProviderStream,
    model: String,
    request_payload: &[u8],
) -> ProviderStream {
    let target_claude = model.to_ascii_lowercase().contains("claude");
    Box::pin(stream::unfold(
        ResponseStream {
            upstream,
            model,
            target_claude,
            tool_identities: response_tool_identities(request_payload),
            decoder: SseDecoder::default(),
            ready: VecDeque::new(),
            response_id: format!("resp_{}", uuid::Uuid::new_v4().simple()),
            started: false,
            completed: false,
            eof: false,
            sequence: 0,
            next_output_index: 0,
            message: None,
            reasoning: None,
            usage: None,
            output: Vec::new(),
            seen_signatures: HashSet::new(),
            last_reasoning_signature: None,
            annotations: Vec::new(),
            terminal_error: None,
        },
        |mut state| async move {
            let item = state.next_output().await?;
            Some((item, state))
        },
    ))
}

struct ResponseStream {
    upstream: ProviderStream,
    model: String,
    target_claude: bool,
    tool_identities: std::collections::HashMap<String, ToolIdentity>,
    decoder: SseDecoder,
    ready: VecDeque<Result<Bytes, ProviderError>>,
    response_id: String,
    started: bool,
    completed: bool,
    eof: bool,
    sequence: u64,
    next_output_index: u64,
    message: Option<MessageState>,
    reasoning: Option<ReasoningState>,
    usage: Option<Value>,
    output: Vec<Value>,
    seen_signatures: HashSet<String>,
    last_reasoning_signature: Option<String>,
    annotations: Vec<Value>,
    terminal_error: Option<ProviderError>,
}

struct MessageState {
    id: String,
    output_index: u64,
    text: String,
    annotations: Vec<Value>,
}

struct ReasoningState {
    id: String,
    output_index: u64,
    text: String,
    signature: String,
}

impl ResponseStream {
    async fn next_output(&mut self) -> Option<Result<Bytes, ProviderError>> {
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

    fn start(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        self.emit(
            "response.created",
            json!({
            "response": {
                    "id": self.response_id.clone(),
                    "object": "response",
                    "status": "in_progress",
                    "model": self.model.clone(),
                    "output": [],
                    "created_at": 0
                }
            }),
        );
        self.emit(
            "response.in_progress",
            json!({
                "response": {
                    "id": self.response_id.clone(),
                    "object": "response",
                    "status": "in_progress",
                    "model": self.model.clone(),
                    "output": [],
                    "created_at": 0
                }
            }),
        );
    }

    fn open_message(&mut self) {
        if self.message.is_some() {
            return;
        }
        let output_index = self.next_output_index;
        self.next_output_index = self.next_output_index.saturating_add(1);
        let id = format!("msg_{}_{}", self.response_id, output_index);
        self.emit(
            "response.output_item.added",
            json!({
                "output_index": output_index,
                "item": {
                    "id": id.clone(),
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": []
                }
            }),
        );
        self.emit(
            "response.content_part.added",
            json!({
                "item_id": id.clone(),
                "output_index": output_index,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []}
            }),
        );
        self.message = Some(MessageState {
            id,
            output_index,
            text: String::new(),
            annotations: self.annotations.clone(),
        });
        self.annotations.clear();
    }

    fn close_message(&mut self) {
        let Some(message) = self.message.take() else {
            return;
        };
        self.emit(
            "response.output_text.done",
            json!({
                "item_id": message.id.clone(),
                "output_index": message.output_index,
                "content_index": 0,
                "text": message.text.clone()
            }),
        );
        self.emit(
            "response.content_part.done",
            json!({
                "item_id": message.id.clone(),
                "output_index": message.output_index,
                "content_index": 0,
                "part": {"type": "output_text", "text": message.text.clone(), "annotations": []}
            }),
        );
        let item = json!({
            "id": message.id.clone(),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": message.text, "annotations": message.annotations}]
        });
        self.emit(
            "response.output_item.done",
            json!({"output_index": message.output_index, "item": item.clone()}),
        );
        self.output.push(item);
    }

    fn open_reasoning(&mut self, signature: &str) {
        if self.reasoning.is_some() {
            return;
        }
        let signature = format_response_signature(signature, self.target_claude);
        let output_index = self.next_output_index;
        self.next_output_index = self.next_output_index.saturating_add(1);
        let id = format!("rs_{}_{}", self.response_id, output_index);
        self.emit(
            "response.output_item.added",
            json!({
                "output_index": output_index,
                "item": {
                    "id": id.clone(),
                    "type": "reasoning",
                    "status": "in_progress",
                    "encrypted_content": signature.clone(),
                    "summary": []
                }
            }),
        );
        self.emit(
            "response.reasoning_summary_part.added",
            json!({
                "item_id": id.clone(),
                "output_index": output_index,
                "summary_index": 0,
                "part": {"type": "summary_text", "text": ""}
            }),
        );
        self.reasoning = Some(ReasoningState {
            id,
            output_index,
            text: String::new(),
            signature,
        });
    }

    fn close_reasoning(&mut self) {
        let Some(reasoning) = self.reasoning.take() else {
            return;
        };
        self.emit(
            "response.reasoning_summary_text.done",
            json!({
                "item_id": reasoning.id.clone(),
                "output_index": reasoning.output_index,
                "summary_index": 0,
                "text": reasoning.text.clone()
            }),
        );
        self.emit(
            "response.reasoning_summary_part.done",
            json!({
                "item_id": reasoning.id.clone(),
                "output_index": reasoning.output_index,
                "summary_index": 0,
                "part": {"type": "summary_text", "text": reasoning.text.clone()}
            }),
        );
        let item = json!({
            "id": reasoning.id.clone(),
            "type": "reasoning",
            "status": "completed",
            "encrypted_content": reasoning.signature,
            "summary": [{"type": "summary_text", "text": reasoning.text}]
        });
        self.emit(
            "response.output_item.done",
            json!({"output_index": reasoning.output_index, "item": item.clone()}),
        );
        self.last_reasoning_signature =
            (!reasoning.signature.is_empty()).then_some(reasoning.signature.clone());
        self.output.push(item);
    }

    fn emit_reasoning_carrier(&mut self, signature: &str, direction: &str, target: &str) {
        let output_index = self.next_output_index;
        self.next_output_index = self.next_output_index.saturating_add(1);
        let id = format!("rs_{}_detached_{}", self.response_id, output_index);
        let carrier_signature = format_response_signature(signature, self.target_claude);
        let carrier = if self.target_claude {
            carrier_signature
        } else {
            encode_reasoning_carrier(&carrier_signature, direction, target)
        };
        let item = json!({
            "id": id,
            "type": "reasoning",
            "status": "completed",
            "encrypted_content": carrier,
            "summary": []
        });
        self.emit(
            "response.output_item.added",
            json!({
                "output_index": output_index,
                "item": {
                    "id": item["id"].clone(),
                    "type": "reasoning",
                    "status": "in_progress",
                    "encrypted_content": item["encrypted_content"].clone(),
                    "summary": []
                }
            }),
        );
        self.emit(
            "response.output_item.done",
            json!({"output_index": output_index, "item": item.clone()}),
        );
        self.output.push(item);
    }

    fn process_inline_data(&mut self, data: &Value) {
        self.close_reasoning();
        self.close_message();
        let output_index = self.next_output_index;
        self.next_output_index = self.next_output_index.saturating_add(1);
        let item_id = format!("ig_{}_{}", self.response_id, output_index);
        let mime_type = data
            .get("mimeType")
            .or_else(|| data.get("mime_type"))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("image/png");
        let output_format = mime_type
            .split_once('/')
            .map(|(_, format)| format)
            .unwrap_or("png");
        let encoded = data.get("data").and_then(Value::as_str).unwrap_or_default();
        self.emit(
            "response.output_item.added",
            json!({
                "output_index": output_index,
                "item": {
                    "id": item_id.clone(),
                    "type": "image_generation_call",
                    "status": "in_progress",
                    "output_format": output_format
                }
            }),
        );
        if !encoded.is_empty() {
            self.emit(
                "response.image_generation_call.partial_image",
                json!({
                    "output_index": output_index,
                    "item_id": item_id.clone(),
                    "output_format": output_format,
                    "partial_image_b64": encoded,
                    "partial_image_index": 0
                }),
            );
        }
        let item = json!({
            "id": item_id,
            "type": "image_generation_call",
            "status": "completed",
            "output_format": output_format,
            "result": encoded
        });
        self.emit(
            "response.output_item.done",
            json!({"output_index": output_index, "item": item.clone()}),
        );
        self.output.push(item);
    }

    fn capture_grounding(&mut self, metadata: Option<&Value>) {
        let Some(chunks) = metadata
            .and_then(|metadata| metadata.get("groundingChunks"))
            .and_then(Value::as_array)
        else {
            return;
        };
        for chunk in chunks {
            let Some(web) = chunk.get("web").and_then(Value::as_object) else {
                continue;
            };
            let Some(url) = web.get("uri").and_then(Value::as_str) else {
                continue;
            };
            if url.trim().is_empty() {
                continue;
            }
            self.annotations.push(json!({
                "type": "url_citation",
                "url": url,
                "title": web.get("title").and_then(Value::as_str).unwrap_or(url),
                "start_index": 0,
                "end_index": 0
            }));
        }
    }

    fn complete(&mut self, finish_reason: Option<&str>) {
        if self.completed {
            return;
        }
        self.start();
        self.close_reasoning();
        self.close_message();
        let mut response = Map::new();
        response.insert("id".to_owned(), Value::String(self.response_id.clone()));
        response.insert("object".to_owned(), Value::String("response".to_owned()));
        let incomplete_reason = incomplete_reason(finish_reason);
        response.insert(
            "status".to_owned(),
            Value::String(
                if incomplete_reason.is_some() {
                    "incomplete"
                } else {
                    "completed"
                }
                .to_owned(),
            ),
        );
        response.insert("model".to_owned(), Value::String(self.model.clone()));
        response.insert("output".to_owned(), Value::Array(self.output.clone()));
        if let Some(reason) = incomplete_reason {
            response.insert("incomplete_details".to_owned(), json!({"reason": reason}));
        }
        if let Some(usage) = self.usage.clone() {
            response.insert("usage".to_owned(), usage);
        }
        self.emit(
            if incomplete_reason.is_some() {
                "response.incomplete"
            } else {
                "response.completed"
            },
            json!({"response": response}),
        );
        self.completed = true;
    }

    fn fail(&mut self, error: ProviderError) {
        if self.completed {
            self.terminal_error = Some(error);
            self.eof = true;
            return;
        }
        self.start();
        self.close_reasoning();
        self.close_message();
        self.emit(
            "response.failed",
            json!({
                "response": {
                    "id": self.response_id.clone(),
                    "object": "response",
                    "status": "failed",
                    "model": self.model.clone(),
                    "output": [],
                    "error": {"code": "upstream_error", "message": error.message()}
                }
            }),
        );
        self.completed = true;
        self.terminal_error = Some(error);
        self.eof = true;
    }

    fn emit(&mut self, event: &str, mut payload: Value) {
        let Some(object) = payload.as_object_mut() else {
            return;
        };
        object.insert("type".to_owned(), Value::String(event.to_owned()));
        object.insert("sequence_number".to_owned(), Value::from(self.sequence));
        self.sequence = self.sequence.saturating_add(1);
        let data = serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec());
        let mut frame = Vec::with_capacity(data.len() + event.len() + 32);
        frame.extend_from_slice(b"event: ");
        frame.extend_from_slice(event.as_bytes());
        frame.extend_from_slice(b"\ndata: ");
        frame.extend_from_slice(&data);
        frame.extend_from_slice(b"\n\n");
        self.ready.push_back(Ok(Bytes::from(frame)));
    }
}

fn convert_usage(value: &Value) -> Value {
    let input = value
        .get("promptTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let output = value
        .get("candidatesTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let reasoning = value
        .get("thoughtsTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    json!({
        "input_tokens": input,
        "output_tokens": output.saturating_add(reasoning),
        "total_tokens": value
            .get("totalTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| input.saturating_add(output).saturating_add(reasoning)),
        "input_tokens_details": {"cached_tokens": value.get("cachedContentTokenCount").and_then(Value::as_u64).unwrap_or_default()},
        "output_tokens_details": {"reasoning_tokens": reasoning}
    })
}

fn normalize_response_id(value: &str) -> String {
    if value.starts_with("resp_") {
        value.to_owned()
    } else {
        format!("resp_{value}")
    }
}

fn format_response_signature(signature: &str, target_claude: bool) -> String {
    if !target_claude || !signature.starts_with('R') {
        return signature.to_owned();
    }
    STANDARD
        .decode(signature)
        .ok()
        .and_then(|decoded| String::from_utf8(decoded).ok())
        .filter(|decoded| decoded.starts_with('E'))
        .unwrap_or_else(|| signature.to_owned())
}

fn encode_reasoning_carrier(signature: &str, direction: &str, target: &str) -> String {
    format!(
        "cpa-gemini-responses-carrier-v1:{direction}:{target}:{}",
        STANDARD_NO_PAD.encode(signature.as_bytes())
    )
}

fn custom_tool_input(value: &Value) -> String {
    let Some(input) = value.get("input") else {
        return value.to_string();
    };
    input
        .as_str()
        .map_or_else(|| input.to_string(), ToOwned::to_owned)
}

fn incomplete_reason(finish_reason: Option<&str>) -> Option<&'static str> {
    let finish_reason = finish_reason
        .unwrap_or_default()
        .trim()
        .to_ascii_uppercase();
    match finish_reason.as_str() {
        "STOP" | "TOOL_CALLS" => None,
        "MAX_TOKENS" | "MAX_OUTPUT_TOKENS" | "LENGTH" => Some("max_output_tokens"),
        "SAFETY"
        | "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "RECITATION"
        | "SPII"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION"
        | "MODEL_ARMOR" => Some("content_filter"),
        "MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL" => Some("tool_error"),
        _ => Some("other"),
    }
}

fn stream_error(error: &Value) -> ProviderError {
    let mut markers = Vec::new();
    collect_error_markers(error, &mut markers);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("Antigravity upstream returned an error");
    let status = markers.iter().find_map(|marker| marker.parse::<u16>().ok());
    let quota_exhausted = markers
        .iter()
        .any(|marker| marker.contains("resource_exhausted") || marker.contains("quota_exhausted"));
    let authentication_error = markers.iter().any(|marker| {
        marker.contains("unauthenticated")
            || marker.contains("permission_denied")
            || marker.contains("invalid_credentials")
    });
    let mut provider_error = match status {
        Some(401 | 403) => ProviderError::new(ProviderErrorKind::Authentication, message)
            .with_failover_reason(provider_core::ProviderFailoverReason::AuthenticationExhausted),
        Some(429) if quota_exhausted => ProviderError::new(ProviderErrorKind::Capacity, message)
            .with_failover_reason(provider_core::ProviderFailoverReason::QuotaExhausted),
        Some(429) => ProviderError::new(ProviderErrorKind::RateLimited, message)
            .with_failover_reason(provider_core::ProviderFailoverReason::RateLimited),
        _ if quota_exhausted => ProviderError::new(ProviderErrorKind::Capacity, message)
            .with_failover_reason(provider_core::ProviderFailoverReason::QuotaExhausted),
        _ if authentication_error => ProviderError::new(ProviderErrorKind::Authentication, message)
            .with_failover_reason(provider_core::ProviderFailoverReason::AuthenticationExhausted),
        _ => ProviderError::new(ProviderErrorKind::Upstream, message),
    };
    if let Some(status) = status {
        provider_error = provider_error.with_upstream_status(status);
    }
    provider_error
}

fn collect_error_markers(value: &Value, markers: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if matches!(key.as_str(), "code" | "status" | "reason" | "message") {
                    match value {
                        Value::String(value) => markers.push(value.to_ascii_lowercase()),
                        Value::Number(value) => markers.push(value.to_string()),
                        _ => {}
                    }
                }
                collect_error_markers(value, markers);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_error_markers(value, markers);
            }
        }
        _ => {}
    }
}

fn frame_data(frame: &[u8]) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    for line in frame.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(value) = line
            .strip_prefix(b"data: ")
            .or_else(|| line.strip_prefix(b"data:"))
        else {
            continue;
        };
        if !data.is_empty() {
            data.push(b'\n');
        }
        data.extend_from_slice(value);
    }
    (!data.is_empty()).then_some(data)
}

#[derive(Default)]
struct SseDecoder {
    buffer: BytesMut,
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<Bytes>, ()> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(end) = frame_end(&self.buffer) {
            let frame = self.buffer.split_to(end);
            frames.push(frame.freeze());
        }
        if self.buffer.len() > MAX_PENDING_FRAME {
            self.buffer.clear();
            return Err(());
        }
        Ok(frames)
    }

    fn finish(&mut self) -> Option<Bytes> {
        (!self.buffer.is_empty()).then(|| self.buffer.split().freeze())
    }
}

fn frame_end(buffer: &[u8]) -> Option<usize> {
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(lf), Some(crlf)) => Some(lf.min(crlf) + 2),
        (Some(end), None) => Some(end + 2),
        (None, Some(end)) => Some(end + 4),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures_util::{StreamExt, stream};
    use provider_core::ProviderStream;
    use serde_json::json;

    use super::*;

    async fn collect(upstream: ProviderStream, request: &[u8]) -> String {
        collect_model(upstream, "gemini-3-flash", request).await
    }

    async fn collect_model(upstream: ProviderStream, model: &str, request: &[u8]) -> String {
        let mut translated = translate_stream(upstream, model.to_owned(), request);
        let mut output = Vec::new();
        while let Some(chunk) = translated.next().await {
            output.extend_from_slice(&chunk.expect("translated response"));
        }
        String::from_utf8(output).expect("SSE is UTF-8")
    }

    #[tokio::test]
    async fn translates_reasoning_tools_and_custom_namespace_identity() {
        let request = json!({
            "model": "gemini-3-flash",
            "input": [{
                "type": "additional_tools",
                "tools": [{"type": "namespace", "name": "functions", "tools": [
                    {"type": "custom", "name": "exec"}
                ]}]
            }]
        })
        .to_string();
        let frame = format!(
            "data: {}\n\n",
            json!({
                "response": {
                    "responseId": "native-1",
                    "candidates": [{
                        "content": {"parts": [
                            {"thought": true, "text": "thinking", "thoughtSignature": "sig-1"},
                            {"functionCall": {"id": "native-call", "name": "functions__exec", "args": {"input": "pwd"}}},
                            {"text": "done"}
                        ]},
                        "finishReason": "STOP",
                        "groundingMetadata": {"groundingChunks": [{"web": {"uri": "https://example.test", "title": "Example"}}]}
                    }],
                    "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 4, "thoughtsTokenCount": 2, "totalTokenCount": 9}
                }
            })
        );
        let split = frame.as_bytes();
        let midpoint = split.len() / 2;
        let stream = stream::iter([
            Ok::<Bytes, ProviderError>(Bytes::copy_from_slice(&split[..midpoint])),
            Ok(Bytes::copy_from_slice(&split[midpoint..])),
        ]);
        let output = collect(Box::pin(stream), request.as_bytes()).await;
        assert!(output.contains("response.in_progress"));
        assert!(output.contains("cpa-gemini-responses-carrier-v1:"));
        assert!(output.contains("\"type\":\"custom_tool_call\""));
        assert!(output.contains("\"name\":\"exec\""));
        assert!(output.contains("\"namespace\":\"functions\""));
        assert!(output.contains("response.custom_tool_call_input.done"));
        assert!(output.contains("https://example.test"));
        assert!(output.contains("\"input_tokens\":3"));
        assert!(output.contains("response.completed"));
    }

    #[tokio::test]
    async fn translates_inline_image_and_incomplete_finish_reason() {
        let request = br#"{"model":"gemini-3.1-flash-image","input":"draw"}"#;
        let frame = Bytes::from_static(
            br#"data: {"response":{"responseId":"image-1","candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"AA=="}}]},"finishReason":"MAX_TOKENS"}]}}

"#,
        );
        let stream = stream::iter([Ok::<Bytes, ProviderError>(frame)]);
        let output = collect(Box::pin(stream), request).await;
        assert!(output.contains("response.image_generation_call.partial_image"));
        assert!(output.contains("\"type\":\"image_generation_call\""));
        assert!(output.contains("\"result\":\"AA==\""));
        assert!(output.contains("response.incomplete"));
        assert!(output.contains("max_output_tokens"));
    }

    #[tokio::test]
    async fn fails_stream_that_ends_without_a_terminal_finish_reason() {
        let frame = Bytes::from_static(
            br#"data: {"response":{"responseId":"truncated-1","candidates":[{"content":{"parts":[{"text":"partial"}]}}]}}

"#,
        );
        let stream = stream::iter([Ok::<Bytes, ProviderError>(frame)]);
        let mut translated =
            translate_stream(Box::pin(stream), "gemini-3-flash".to_owned(), br#"{}"#);
        let mut output = Vec::new();
        let mut saw_error = false;
        while let Some(chunk) = translated.next().await {
            match chunk {
                Ok(chunk) => output.extend_from_slice(&chunk),
                Err(error) => {
                    assert!(error.message().contains("before a terminal finishReason"));
                    saw_error = true;
                }
            }
        }
        let output = String::from_utf8(output).expect("SSE is UTF-8");
        assert!(saw_error);
        assert!(output.contains("response.failed"));
        assert!(!output.contains("response.completed"));
    }

    #[tokio::test]
    async fn classifies_tool_finish_reasons_as_incomplete() {
        let frame = Bytes::from_static(
            br#"data: {"response":{"responseId":"tool-error-1","candidates":[{"content":{"parts":[{"text":"partial"}]},"finishReason":"MALFORMED_FUNCTION_CALL"}]}}

"#,
        );
        let stream = stream::iter([Ok::<Bytes, ProviderError>(frame)]);
        let output = collect(
            Box::pin(stream),
            br#"{"model":"gemini-3-flash","input":"hello"}"#,
        )
        .await;
        assert!(output.contains("response.incomplete"));
        assert!(output.contains("tool_error"));
        assert!(!output.contains("response.completed"));
    }

    #[tokio::test]
    async fn carries_claude_tool_signature_in_client_tool_item() {
        let request = br#"{"model":"claude-sonnet-4-6","tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}],"input":"hello"}"#;
        let single_layer = STANDARD.encode([0x12_u8, 0x01, 0x02]);
        let double_layer = STANDARD.encode(single_layer.as_bytes());
        let frame = format!(
            "data: {}\n\n",
            json!({
                "response": {
                    "responseId": "claude-1",
                    "candidates": [{
                        "content": {"parts": [
                            {"thought": true, "text": "think", "thoughtSignature": double_layer},
                            {"functionCall": {"id": "call-1", "name": "lookup", "args": {"q": "x"}}}
                        ]},
                        "finishReason": "STOP"
                    }]
                }
            })
        );
        let stream = stream::iter([Ok::<Bytes, ProviderError>(Bytes::from(frame))]);
        let output = collect_model(Box::pin(stream), "claude-sonnet-4-6", request).await;
        assert!(output.contains(r#""signature":"EgEC""#));
    }

    #[test]
    fn classifies_structured_stream_error() {
        let error = stream_error(&json!({
            "code": 429,
            "status": "RESOURCE_EXHAUSTED",
            "message": "quota"
        }));
        assert_eq!(error.kind(), ProviderErrorKind::Capacity);
        assert_eq!(
            error.failover_reason(),
            Some(provider_core::ProviderFailoverReason::QuotaExhausted)
        );
        assert_eq!(error.upstream_status(), Some(429));
    }
}
