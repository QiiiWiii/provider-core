use bytes::Bytes;
use provider_core::{ProviderError, ProviderErrorKind};
use serde_json::{Map, Value, json};

use super::format::{encode_reasoning_carrier, format_response_signature, incomplete_reason};
use super::{MessageState, ReasoningState, ResponseStream};

impl ResponseStream {
    pub(super) fn start(&mut self) {
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

    pub(super) fn open_message(&mut self) {
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

    pub(super) fn close_message(&mut self) {
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

    pub(super) fn open_reasoning(&mut self, signature: &str) {
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

    pub(super) fn close_reasoning(&mut self) {
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

    pub(super) fn emit_reasoning_carrier(
        &mut self,
        signature: &str,
        direction: &str,
        target: &str,
    ) {
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

    pub(super) fn process_inline_data(&mut self, data: &Value) {
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

    pub(super) fn capture_grounding(&mut self, metadata: Option<&Value>) {
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

    pub(super) fn complete(&mut self, finish_reason: Option<&str>) {
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

    pub(super) fn fail(&mut self, error: ProviderError) {
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
        if self.terminal_error.is_none() {
            self.terminal_error = Some(error);
        }
        self.eof = true;
    }

    pub(super) fn emit(&mut self, event: &str, mut payload: Value) {
        let Some(object) = payload.as_object_mut() else {
            return;
        };
        object.insert("type".to_owned(), Value::String(event.to_owned()));
        object.insert("sequence_number".to_owned(), Value::from(self.sequence));
        self.sequence = self.sequence.saturating_add(1);
        let data = match serde_json::to_vec(&payload) {
            Ok(data) => data,
            Err(error) => {
                self.terminal_error = Some(ProviderError::new(
                    ProviderErrorKind::Internal,
                    format!("failed to serialize Antigravity response event: {error}"),
                ));
                self.completed = true;
                self.eof = true;
                return;
            }
        };
        let mut frame = Vec::with_capacity(data.len() + event.len() + 32);
        frame.extend_from_slice(b"event: ");
        frame.extend_from_slice(event.as_bytes());
        frame.extend_from_slice(b"\ndata: ");
        frame.extend_from_slice(&data);
        frame.extend_from_slice(b"\n\n");
        self.ready.push_back(Ok(Bytes::from(frame)));
    }
}
