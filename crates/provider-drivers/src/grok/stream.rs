use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use provider_core::{ProviderError, ProviderErrorKind, ProviderStream};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Duration;

use super::request::{GrokToolMappings, NamespaceToolRef};

const MAX_TOOL_SSE_FRAME_SIZE: usize = 8 * 1024 * 1024;
const GROK_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub(super) fn restore_tool_stream(
    inner: ProviderStream,
    tool_mappings: GrokToolMappings,
    model: &str,
) -> ProviderStream {
    let model = model.to_owned();
    struct State {
        inner: ProviderStream,
        pending: BytesMut,
        ready: VecDeque<Bytes>,
        restorer: GrokToolStreamRestorer,
        eof: bool,
        terminal_error: Option<ProviderError>,
    }

    Box::pin(stream::unfold(
        State {
            inner,
            pending: BytesMut::new(),
            ready: VecDeque::new(),
            restorer: GrokToolStreamRestorer::new(tool_mappings),
            eof: false,
            terminal_error: None,
        },
        move |mut state| {
            let model = model.clone();
            async move {
                loop {
                    if let Some(frame) = state.ready.pop_front() {
                        return Some((Ok(frame), state));
                    }
                    if let Some(frame_end) = find_sse_frame_end(&state.pending) {
                        if frame_end > MAX_TOOL_SSE_FRAME_SIZE {
                            let error = ProviderError::new(
                                ProviderErrorKind::Upstream,
                                "Grok upstream tool event exceeded the frame limit",
                            );
                            if !state.restorer.terminal_seen() {
                                state.ready.push_back(state.restorer.failure_frame(
                                    &model,
                                    "upstream_frame_too_large",
                                    "Grok upstream tool event exceeded the frame limit",
                                ));
                            }
                            state.terminal_error = Some(error);
                            state.pending.clear();
                            state.eof = true;
                            continue;
                        }
                        let frame = state.pending.split_to(frame_end).freeze();
                        state.ready.extend(state.restorer.restore_frame(&frame));
                        continue;
                    }
                    if state.eof {
                        if let Some(error) = state.terminal_error.take() {
                            return Some((Err(error), state));
                        }
                        if state.pending.is_empty() {
                            if !state.restorer.terminal_seen() {
                                state.ready.push_back(state.restorer.failure_frame(
                                    &model,
                                    "upstream_stream_ended",
                                    "Grok upstream stream ended before response completion",
                                ));
                                continue;
                            }
                            return None;
                        }
                        state.pending.clear();
                        if !state.restorer.terminal_seen() {
                            state.ready.push_back(state.restorer.failure_frame(
                                &model,
                                "upstream_incomplete_sse_frame",
                                "Grok upstream stream ended with an incomplete SSE frame",
                            ));
                        }
                        continue;
                    }
                    match tokio::time::timeout(GROK_STREAM_IDLE_TIMEOUT, state.inner.next()).await {
                        Err(_) => {
                            let error = ProviderError::new(
                                ProviderErrorKind::Upstream,
                                "Grok upstream stream idle timeout",
                            )
                            .with_failover_reason(
                                provider_core::ProviderFailoverReason::CapacityExhausted,
                            );
                            if !state.restorer.terminal_seen() {
                                state.ready.push_back(state.restorer.failure_frame(
                                    &model,
                                    "upstream_stream_timeout",
                                    "Grok upstream stream timed out before response completion",
                                ));
                            }
                            state.terminal_error = Some(error);
                            state.eof = true;
                        }
                        Ok(Some(Ok(chunk))) => {
                            state.pending.extend_from_slice(&chunk);
                            if state.pending.len() > MAX_TOOL_SSE_FRAME_SIZE
                                && find_sse_frame_end(&state.pending).is_none()
                            {
                                let error = ProviderError::new(
                                    ProviderErrorKind::Upstream,
                                    "Grok upstream tool event exceeded the frame limit",
                                );
                                if !state.restorer.terminal_seen() {
                                    state.ready.push_back(state.restorer.failure_frame(
                                        &model,
                                        "upstream_frame_too_large",
                                        "Grok upstream tool event exceeded the frame limit",
                                    ));
                                }
                                state.terminal_error = Some(error);
                                state.pending.clear();
                                state.eof = true;
                            }
                        }
                        Ok(Some(Err(error))) => {
                            if state.restorer.terminal_seen() {
                                state.eof = true;
                            } else {
                                state.ready.push_back(state.restorer.failure_frame(
                                    &model,
                                    "upstream_stream_error",
                                    "Grok upstream stream failed before response completion",
                                ));
                                state.terminal_error = Some(error);
                                state.eof = true;
                            }
                        }
                        Ok(None) => state.eof = true,
                    }
                }
            }
        },
    ))
}

#[derive(Clone, Debug)]
struct ClientToolCall {
    kind: ClientToolKind,
    upstream_name: String,
    name: String,
    namespace: Option<String>,
    call_id: String,
    item_id: String,
    output_index: i64,
    arguments: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClientToolKind {
    Custom,
    ToolSearch,
}

struct GrokToolStreamRestorer {
    mappings: GrokToolMappings,
    client_tool_calls: Vec<ClientToolCall>,
    completed_items: BTreeMap<i64, Value>,
    completed_items_fallback: Vec<Value>,
    next_sequence: Option<i64>,
    response_id: Option<String>,
    terminal_seen: bool,
}

impl GrokToolStreamRestorer {
    fn new(mappings: GrokToolMappings) -> Self {
        Self {
            mappings,
            client_tool_calls: Vec::new(),
            completed_items: BTreeMap::new(),
            completed_items_fallback: Vec::new(),
            next_sequence: None,
            response_id: None,
            terminal_seen: false,
        }
    }

    fn terminal_seen(&self) -> bool {
        self.terminal_seen
    }

    fn failure_frame(&mut self, model: &str, code: &str, message: &str) -> Bytes {
        self.terminal_seen = true;
        let response_id = self.response_id.clone().unwrap_or_else(|| {
            let id = format!("resp_{}", uuid::Uuid::new_v4().simple());
            self.response_id = Some(id.clone());
            id
        });
        let payload = serde_json::json!({
            "type": "response.failed",
            "response": {
                "id": response_id,
                "object": "response",
                "model": model,
                "status": "failed",
                "output": [],
                "error": {"code": code, "message": message}
            }
        });
        let data = serde_json::to_vec(&payload).unwrap_or_else(|_| {
            br#"{"type":"response.failed","response":{"status":"failed","output":[]}}"#.to_vec()
        });
        let mut frame = Vec::with_capacity(data.len() + 32);
        frame.extend_from_slice(b"event: response.failed\ndata: ");
        frame.extend_from_slice(&data);
        frame.extend_from_slice(b"\n\n");
        Bytes::from(frame)
    }

    fn restore_frame(&mut self, frame: &[u8]) -> Vec<Bytes> {
        if sse_event_name(frame) == Some("ping") {
            return vec![ping_comment(frame)];
        }
        let Some(data) = sse_data_payload(frame) else {
            return vec![Bytes::copy_from_slice(frame)];
        };
        let Ok(payload) = serde_json::from_slice::<Value>(&data) else {
            return vec![Bytes::copy_from_slice(frame)];
        };
        self.restore_payload(payload)
            .into_iter()
            .map(|payload| rewrite_sse_frame(frame, &payload))
            .collect()
    }

    fn restore_payload(&mut self, mut payload: Value) -> Vec<Value> {
        let mut event_type = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if let Some(id) = payload
            .pointer("/response/id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            self.response_id = Some(id.to_owned());
        }
        if event_type == "response.completed"
            && let Some(status) = payload
                .pointer("/response/status")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|status| !status.is_empty() && *status != "completed")
        {
            event_type = match status {
                "incomplete" => "response.incomplete",
                "cancelled" => "response.cancelled",
                "canceled" => "response.canceled",
                _ => "response.failed",
            }
            .to_owned();
            payload["type"] = Value::String(event_type.clone());
            if let Some(response) = payload.get_mut("response").and_then(Value::as_object_mut)
                && !response.contains_key("error")
            {
                response.insert(
                    "error".to_owned(),
                    serde_json::json!({
                        "code": "upstream_non_success_terminal",
                        "message": "Grok upstream returned a non-success terminal status"
                    }),
                );
            }
        }
        let sequence = payload.get("sequence_number").and_then(Value::as_i64);
        self.begin_sequence(sequence);

        if event_type == "response.reasoning_text.done" {
            let mut text_done = payload.clone();
            normalize_reasoning_text_done(&mut text_done);
            let text_done = self.resequence(text_done, sequence, true);
            normalize_reasoning_part_done(&mut payload);
            return vec![text_done, self.generated_event(payload)];
        }
        let reasoning_changed = normalize_reasoning_payload(&mut payload);

        if is_terminal_response_event(&event_type) {
            self.terminal_seen = true;
            let mut changed = reasoning_changed;
            changed |= self.patch_terminal_output(&mut payload);
            changed |= restore_terminal_tool_payload(&mut payload, &self.mappings);
            return vec![self.resequence(payload, sequence, changed)];
        }

        match event_type.as_str() {
            "response.output_item.added" => {
                let client_tool = self.record_client_tool_item(&payload);
                let changed = reasoning_changed
                    | if let Some(index) = client_tool {
                        restore_client_tool_item(
                            &mut payload,
                            "item",
                            &self.client_tool_calls[index],
                            "",
                        )
                    } else {
                        restore_namespace_event(&mut payload, &self.mappings)
                    };
                vec![self.resequence(payload, sequence, changed)]
            }
            "response.function_call_arguments.delta" => {
                if let Some(index) = self.client_tool_call_for(&payload) {
                    if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                        self.client_tool_calls[index].arguments.push_str(delta);
                    }
                    return Vec::new();
                }
                let changed =
                    reasoning_changed | restore_namespace_event(&mut payload, &self.mappings);
                vec![self.resequence(payload, sequence, changed)]
            }
            "response.function_call_arguments.done" => {
                let Some(index) = self.client_tool_call_for(&payload) else {
                    let changed =
                        reasoning_changed | restore_namespace_event(&mut payload, &self.mappings);
                    return vec![self.resequence(payload, sequence, changed)];
                };
                if let Some(arguments) = payload.get("arguments").and_then(Value::as_str)
                    && !arguments.is_empty()
                {
                    self.client_tool_calls[index].arguments = arguments.to_owned();
                }
                let call = self.client_tool_calls[index].clone();
                if call.kind == ClientToolKind::ToolSearch {
                    return Vec::new();
                }
                let input = custom_tool_input(&call.arguments);
                let mut events = Vec::with_capacity(2);
                if !input.is_empty() {
                    events.push(self.generated_event(serde_json::json!({
                        "type": "response.custom_tool_call_input.delta",
                        "output_index": call.output_index,
                        "item_id": call.item_id,
                        "delta": input,
                    })));
                }
                events.push(self.generated_event(serde_json::json!({
                    "type": "response.custom_tool_call_input.done",
                    "output_index": call.output_index,
                    "item_id": call.item_id,
                    "call_id": call.call_id,
                    "name": call.name,
                    "input": input,
                })));
                if let Some(namespace) = call.namespace {
                    events.last_mut().expect("generated input done event")["namespace"] =
                        Value::String(namespace);
                }
                events
            }
            "response.output_item.done" => {
                let client_tool = self.record_client_tool_item(&payload);
                let changed = reasoning_changed
                    | if let Some(index) = client_tool {
                        let call = self.client_tool_calls[index].clone();
                        let input = custom_tool_input(&call.arguments);
                        let changed = restore_client_tool_item(&mut payload, "item", &call, &input);
                        self.client_tool_calls.remove(index);
                        changed
                    } else {
                        restore_namespace_event(&mut payload, &self.mappings)
                    };
                self.record_completed_item(&payload);
                vec![self.resequence(payload, sequence, changed)]
            }
            _ => {
                let changed =
                    reasoning_changed | restore_namespace_event(&mut payload, &self.mappings);
                vec![self.resequence(payload, sequence, changed)]
            }
        }
    }

    fn begin_sequence(&mut self, sequence: Option<i64>) {
        if self.next_sequence.is_none() {
            self.next_sequence = sequence;
        }
    }

    fn resequence(&mut self, mut payload: Value, sequence: Option<i64>, changed: bool) -> Value {
        let Some(next) = self.next_sequence else {
            return payload;
        };
        if changed || sequence != Some(next) {
            payload["sequence_number"] = Value::from(next);
        }
        self.next_sequence = Some(next.saturating_add(1));
        payload
    }

    fn generated_event(&mut self, mut payload: Value) -> Value {
        if let Some(next) = self.next_sequence {
            payload["sequence_number"] = Value::from(next);
            self.next_sequence = Some(next.saturating_add(1));
        }
        payload
    }

    fn record_client_tool_item(&mut self, payload: &Value) -> Option<usize> {
        let item = payload.get("item")?.as_object()?;
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return None;
        }
        let name = item.get("name")?.as_str()?;
        let kind = if self.mappings.custom_tools.contains(name) {
            ClientToolKind::Custom
        } else if self.mappings.tool_search && name == "tool_search" {
            ClientToolKind::ToolSearch
        } else {
            return None;
        };
        let item_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let output_index = payload
            .get("output_index")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let existing = self.client_tool_calls.iter().position(|call| {
            (!item_id.is_empty() && call.item_id == item_id)
                || (!call_id.is_empty() && call.call_id == call_id)
                || (item_id.is_empty()
                    && call_id.is_empty()
                    && call.output_index == output_index
                    && call.upstream_name == name)
        });
        let index = existing.unwrap_or_else(|| {
            let reference = self.mappings.namespace_tools.get(name);
            self.client_tool_calls.push(ClientToolCall {
                kind,
                upstream_name: name.to_owned(),
                name: reference.map_or_else(|| name.to_owned(), |item| item.name.clone()),
                namespace: reference.map(|item| item.namespace.clone()),
                call_id: call_id.to_owned(),
                item_id: item_id.to_owned(),
                output_index,
                arguments: String::new(),
            });
            self.client_tool_calls.len() - 1
        });
        if let Some(arguments) = item.get("arguments").and_then(Value::as_str)
            && !arguments.is_empty()
        {
            self.client_tool_calls[index].arguments = arguments.to_owned();
        }
        Some(index)
    }

    fn client_tool_call_for(&self, payload: &Value) -> Option<usize> {
        let item_id = payload
            .get("item_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let call_id = payload
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let output_index = payload.get("output_index").and_then(Value::as_i64);
        let name = payload
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        self.client_tool_calls.iter().position(|call| {
            (!item_id.is_empty() && call.item_id == item_id)
                || (!call_id.is_empty() && call.call_id == call_id)
                || output_index.is_some_and(|value| call.output_index == value)
                || (item_id.is_empty()
                    && call_id.is_empty()
                    && output_index.is_none()
                    && !name.is_empty()
                    && call.upstream_name == name)
        })
    }

    fn record_completed_item(&mut self, payload: &Value) {
        let Some(item) = payload.get("item").cloned() else {
            return;
        };
        if let Some(index) = payload.get("output_index").and_then(Value::as_i64) {
            self.completed_items.insert(index, item);
        } else {
            self.completed_items_fallback.push(item);
        }
    }

    fn patch_terminal_output(&self, payload: &mut Value) -> bool {
        let Some(response) = payload.get_mut("response").and_then(Value::as_object_mut) else {
            return false;
        };
        if response
            .get("output")
            .and_then(Value::as_array)
            .is_some_and(|output| !output.is_empty())
        {
            return false;
        }
        if self.completed_items.is_empty() && self.completed_items_fallback.is_empty() {
            return false;
        }
        let mut output = self.completed_items.values().cloned().collect::<Vec<_>>();
        output.extend(self.completed_items_fallback.iter().cloned());
        response.insert("output".to_owned(), Value::Array(output));
        true
    }
}

fn find_sse_frame_end(buffer: &[u8]) -> Option<usize> {
    let mut line_start = 0;
    let mut index = 0;
    while index < buffer.len() {
        if !matches!(buffer[index], b'\r' | b'\n') {
            index += 1;
            continue;
        }
        let ending_len =
            usize::from(buffer[index] == b'\r' && buffer.get(index + 1) == Some(&b'\n')) + 1;
        let end = index + ending_len;
        if index == line_start {
            return Some(end);
        }
        line_start = end;
        index = end;
    }
    None
}

fn sse_event_name(frame: &[u8]) -> Option<&str> {
    for (line, _) in sse_lines(frame) {
        if let Some(event) = line
            .strip_prefix(b"event: ")
            .or_else(|| line.strip_prefix(b"event:"))
            && let Ok(event) = std::str::from_utf8(event)
        {
            return Some(event.trim());
        }
    }
    None
}

fn sse_data_payload(frame: &[u8]) -> Option<Vec<u8>> {
    let mut payload = Vec::new();
    for (line, _) in sse_lines(frame) {
        let Some(data) = line
            .strip_prefix(b"data: ")
            .or_else(|| line.strip_prefix(b"data:"))
        else {
            continue;
        };
        if !payload.is_empty() {
            payload.push(b'\n');
        }
        payload.extend_from_slice(data);
    }
    (!payload.is_empty()).then_some(payload)
}

fn ping_comment(frame: &[u8]) -> Bytes {
    if frame.windows(2).any(|window| window == b"\r\n") {
        Bytes::from_static(b": ping\r\n\r\n")
    } else if frame.contains(&b'\r') {
        Bytes::from_static(b": ping\r\r")
    } else {
        Bytes::from_static(b": ping\n\n")
    }
}

fn rewrite_sse_frame(frame: &[u8], payload: &Value) -> Bytes {
    let mut output = Vec::with_capacity(frame.len());
    let event_type = payload.get("type").and_then(Value::as_str);
    let mut wrote_data = false;
    for (content, ending) in sse_lines(frame) {
        if content.starts_with(b"data:") {
            if !wrote_data {
                output.extend_from_slice(b"data: ");
                if serde_json::to_writer(&mut output, payload).is_err() {
                    return Bytes::copy_from_slice(frame);
                }
                output.extend_from_slice(ending);
                wrote_data = true;
            }
        } else if content.starts_with(b"event:") {
            output.extend_from_slice(b"event: ");
            output.extend_from_slice(event_type.unwrap_or_default().as_bytes());
            output.extend_from_slice(ending);
        } else {
            output.extend_from_slice(content);
            output.extend_from_slice(ending);
        }
    }
    Bytes::from(output)
}

fn sse_lines(frame: &[u8]) -> Vec<(&[u8], &[u8])> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < frame.len() {
        if !matches!(frame[index], b'\r' | b'\n') {
            index += 1;
            continue;
        }
        let end = if frame[index] == b'\r' && frame.get(index + 1) == Some(&b'\n') {
            index + 2
        } else {
            index + 1
        };
        lines.push((&frame[start..index], &frame[index..end]));
        start = end;
        index = end;
    }
    if start < frame.len() {
        lines.push((&frame[start..], &[]));
    }
    lines
}

fn normalize_reasoning_payload(payload: &mut Value) -> bool {
    let mut changed = match payload.get("type").and_then(Value::as_str) {
        Some("response.reasoning_text.delta") => {
            payload["type"] = Value::String("response.reasoning_summary_text.delta".to_owned());
            normalize_summary_index(payload);
            true
        }
        Some("response.content_part.added")
            if payload.pointer("/part/type").and_then(Value::as_str) == Some("reasoning_text") =>
        {
            payload["type"] = Value::String("response.reasoning_summary_part.added".to_owned());
            payload["part"]["type"] = Value::String("summary_text".to_owned());
            normalize_summary_index(payload);
            true
        }
        Some("response.content_part.done")
            if payload.pointer("/part/type").and_then(Value::as_str) == Some("reasoning_text") =>
        {
            payload["type"] = Value::String("response.reasoning_summary_part.done".to_owned());
            payload["part"]["type"] = Value::String("summary_text".to_owned());
            normalize_summary_index(payload);
            true
        }
        _ => false,
    };
    if let Some(item) = payload.get_mut("item") {
        changed |= normalize_reasoning_item(item);
    }
    if let Some(output) = payload
        .get_mut("response")
        .and_then(|response| response.get_mut("output"))
        .and_then(Value::as_array_mut)
    {
        for item in output {
            changed |= normalize_reasoning_item(item);
        }
    }
    changed
}

fn normalize_reasoning_text_done(payload: &mut Value) {
    payload["type"] = Value::String("response.reasoning_summary_text.done".to_owned());
    normalize_summary_index(payload);
}

fn normalize_reasoning_part_done(payload: &mut Value) {
    payload["type"] = Value::String("response.reasoning_summary_part.done".to_owned());
    let text = payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    payload["part"] = serde_json::json!({ "type": "summary_text", "text": text });
    if let Some(object) = payload.as_object_mut() {
        object.remove("text");
    }
    normalize_summary_index(payload);
}

fn normalize_summary_index(payload: &mut Value) {
    if payload.get("summary_index").is_none()
        && let Some(content_index) = payload.get("content_index").cloned()
    {
        payload["summary_index"] = content_index;
    }
    if let Some(object) = payload.as_object_mut() {
        object.remove("content_index");
    }
}

fn normalize_reasoning_item(item: &mut Value) -> bool {
    let Some(object) = item.as_object_mut() else {
        return false;
    };
    if object.get("type").and_then(Value::as_str) != Some("reasoning") {
        return false;
    }
    let mut changed = false;
    if let Some(summary) = object.get_mut("summary").and_then(Value::as_array_mut) {
        for part in summary {
            if part.get("type").and_then(Value::as_str) == Some("reasoning_text") {
                part["type"] = Value::String("summary_text".to_owned());
                changed = true;
            }
        }
    }
    let reasoning_parts = object
        .get("content")
        .and_then(Value::as_array)
        .map(|content| {
            content
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("reasoning_text"))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !reasoning_parts.is_empty() {
        let mut summary = Vec::with_capacity(reasoning_parts.len());
        for mut part in reasoning_parts {
            part["type"] = Value::String("summary_text".to_owned());
            summary.push(part);
        }
        object.insert("summary".to_owned(), Value::Array(summary));
        object.remove("content");
        changed = true;
    }
    changed
}

fn is_terminal_response_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "response.completed"
            | "response.done"
            | "response.incomplete"
            | "response.failed"
            | "response.cancelled"
            | "response.canceled"
    )
}

fn restore_terminal_tool_payload(payload: &mut Value, mappings: &GrokToolMappings) -> bool {
    let Some(output) = payload
        .get_mut("response")
        .and_then(|response| response.get_mut("output"))
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let mut changed = false;
    for item in output {
        changed |= restore_tool_item(item, mappings);
    }
    changed
}

fn restore_namespace_event(payload: &mut Value, mappings: &GrokToolMappings) -> bool {
    let mut changed = payload
        .get_mut("item")
        .is_some_and(|item| restore_namespace_tool_item(item, &mappings.namespace_tools));
    if matches!(
        payload.get("type").and_then(Value::as_str),
        Some("response.function_call_arguments.delta" | "response.function_call_arguments.done")
    ) && let Some(reference) = payload
        .get("name")
        .and_then(Value::as_str)
        .and_then(|name| mappings.namespace_tools.get(name))
        .cloned()
    {
        payload["name"] = Value::String(reference.name);
        changed = true;
    }
    changed
}

fn restore_tool_item(item: &mut Value, mappings: &GrokToolMappings) -> bool {
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if mappings.custom_tools.contains(&name) {
        let arguments = item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let input = custom_tool_input(arguments);
        let mut changed = restore_custom_item_value(item, &input);
        changed |= restore_custom_namespace(item, &name, &mappings.namespace_tools);
        return changed;
    }
    if mappings.tool_search && name == "tool_search" {
        return restore_tool_search_item_value(item, true);
    }
    restore_namespace_tool_item(item, &mappings.namespace_tools)
}

fn restore_client_tool_item(
    payload: &mut Value,
    field: &str,
    call: &ClientToolCall,
    input: &str,
) -> bool {
    payload.get_mut(field).is_some_and(|item| match call.kind {
        ClientToolKind::Custom => {
            let changed = restore_custom_item_value(item, input);
            if let Some(object) = item.as_object_mut() {
                object.insert("name".to_owned(), Value::String(call.name.clone()));
                if let Some(namespace) = &call.namespace {
                    object.insert("namespace".to_owned(), Value::String(namespace.clone()));
                }
            }
            changed
        }
        ClientToolKind::ToolSearch => restore_tool_search_item_value(item, false),
    })
}

fn restore_custom_namespace(
    item: &mut Value,
    upstream_name: &str,
    namespace_tools: &HashMap<String, NamespaceToolRef>,
) -> bool {
    let Some(reference) = namespace_tools.get(upstream_name) else {
        return false;
    };
    let Some(item) = item.as_object_mut() else {
        return false;
    };
    item.insert("name".to_owned(), Value::String(reference.name.clone()));
    item.insert(
        "namespace".to_owned(),
        Value::String(reference.namespace.clone()),
    );
    true
}

fn restore_custom_item_value(item: &mut Value, input: &str) -> bool {
    let Some(item) = item.as_object_mut() else {
        return false;
    };
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return false;
    }
    item.insert(
        "type".to_owned(),
        Value::String("custom_tool_call".to_owned()),
    );
    item.insert("input".to_owned(), Value::String(input.to_owned()));
    item.remove("arguments");
    item.remove("namespace");
    true
}

fn restore_tool_search_item_value(item: &mut Value, terminal: bool) -> bool {
    let Some(item) = item.as_object_mut() else {
        return false;
    };
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return false;
    }
    item.insert(
        "type".to_owned(),
        Value::String("tool_search_call".to_owned()),
    );
    item.remove("name");
    item.remove("namespace");
    if terminal {
        item.insert("execution".to_owned(), Value::String("client".to_owned()));
        if let Some(arguments) = item.get("arguments").and_then(Value::as_str)
            && let Ok(arguments) = serde_json::from_str::<Value>(arguments)
        {
            item.insert("arguments".to_owned(), arguments);
        }
    } else if item
        .get("arguments")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        item.insert("arguments".to_owned(), Value::String("{}".to_owned()));
    }
    true
}

fn custom_tool_input(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| value.get("input").cloned())
        .map(|input| match input {
            Value::String(value) => value,
            Value::Null => String::new(),
            value => value.to_string(),
        })
        .unwrap_or_default()
}

fn restore_namespace_tool_item(
    item: &mut Value,
    namespace_tools: &HashMap<String, NamespaceToolRef>,
) -> bool {
    let Some(item) = item.as_object_mut() else {
        return false;
    };
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return false;
    }
    let Some(reference) = item
        .get("name")
        .and_then(Value::as_str)
        .and_then(|name| namespace_tools.get(name))
    else {
        return false;
    };
    item.insert("name".to_owned(), Value::String(reference.name.clone()));
    item.insert(
        "namespace".to_owned(),
        Value::String(reference.namespace.clone()),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_namespace_tool_calls_in_stream_events() {
        let mappings = GrokToolMappings {
            namespace_tools: HashMap::from([(
                "codex_app__inner".to_owned(),
                NamespaceToolRef {
                    namespace: "codex_app".to_owned(),
                    name: "inner".to_owned(),
                },
            )]),
            tool_search: false,
            ..GrokToolMappings::default()
        };
        let mut restorer = GrokToolStreamRestorer::new(mappings);
        let frame = br#"event: response.output_item.done
data: {"type":"response.output_item.done","item":{"type":"function_call","name":"codex_app__inner","call_id":"call_1","arguments":"{}"}}

"#;

        let restored = restorer.restore_frame(frame).remove(0);
        let data = restored
            .split(|byte| *byte == b'\n')
            .find_map(|line| line.strip_prefix(b"data: "))
            .expect("data line");
        let payload: Value = serde_json::from_slice(data).expect("restored event JSON");

        assert_eq!(payload["item"]["name"], "inner");
        assert_eq!(payload["item"]["namespace"], "codex_app");
        assert_eq!(payload["item"]["call_id"], "call_1");

        let crlf_frame = b"event: response.output_item.done\r\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"name\":\"codex_app__inner\"}}\r\n\r\n";
        let restored = restorer.restore_frame(crlf_frame).remove(0);
        assert!(restored.ends_with(b"\r\n\r\n"));
        assert_eq!(find_sse_frame_end(&restored), Some(restored.len()));
    }

    #[test]
    fn restores_custom_tool_stream_lifecycle_and_sequences() {
        let mut restorer = GrokToolStreamRestorer::new(GrokToolMappings {
            custom_tools: std::collections::HashSet::from(["shell".to_owned()]),
            ..GrokToolMappings::default()
        });
        let added = restorer.restore_frame(
            b"data: {\"type\":\"response.output_item.added\",\"sequence_number\":7,\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"shell\",\"arguments\":\"\",\"status\":\"in_progress\"}}\n\n",
        );
        assert_eq!(added.len(), 1);
        let added = frame_payload(&added[0]);
        assert_eq!(added["sequence_number"], 7);
        assert_eq!(added["item"]["type"], "custom_tool_call");
        assert_eq!(added["item"]["input"], "");
        assert!(added["item"].get("arguments").is_none());

        let delta = restorer.restore_frame(
            b"data: {\"type\":\"response.function_call_arguments.delta\",\"sequence_number\":8,\"output_index\":0,\"item_id\":\"item_1\",\"delta\":\"{\\\"input\\\":\\\"pw\"}\n\n",
        );
        assert!(delta.is_empty());
        let done = restorer.restore_frame(
            b"data: {\"type\":\"response.function_call_arguments.done\",\"sequence_number\":9,\"output_index\":0,\"item_id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"shell\",\"arguments\":\"{\\\"input\\\":\\\"pwd\\\"}\"}\n\n",
        );
        assert_eq!(done.len(), 2);
        let input_delta = frame_payload(&done[0]);
        assert_eq!(input_delta["type"], "response.custom_tool_call_input.delta");
        assert_eq!(input_delta["sequence_number"], 8);
        assert_eq!(input_delta["delta"], "pwd");
        let input_done = frame_payload(&done[1]);
        assert_eq!(input_done["type"], "response.custom_tool_call_input.done");
        assert_eq!(input_done["sequence_number"], 9);
        assert_eq!(input_done["input"], "pwd");

        let closed = restorer.restore_frame(
            b"data: {\"type\":\"response.output_item.done\",\"sequence_number\":10,\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"shell\",\"arguments\":\"{\\\"input\\\":\\\"pwd\\\"}\",\"status\":\"completed\"}}\n\n",
        );
        let closed = frame_payload(&closed[0]);
        assert_eq!(closed["sequence_number"], 10);
        assert_eq!(closed["item"]["type"], "custom_tool_call");
        assert_eq!(closed["item"]["input"], "pwd");
    }

    #[test]
    fn restores_custom_tools_in_terminal_response_events() {
        let mut restorer = GrokToolStreamRestorer::new(GrokToolMappings {
            custom_tools: std::collections::HashSet::from(["terminal__exec".to_owned()]),
            namespace_tools: HashMap::from([(
                "terminal__exec".to_owned(),
                NamespaceToolRef {
                    namespace: "terminal".to_owned(),
                    name: "exec".to_owned(),
                },
            )]),
            tool_search: false,
        });
        let frames = restorer.restore_frame(
            b"data: {\"type\":\"response.completed\",\"sequence_number\":4,\"response\":{\"output\":[{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"terminal__exec\",\"arguments\":\"{\\\"input\\\":\\\"pwd\\\"}\"}]}}\n\n",
        );
        let payload = frame_payload(&frames[0]);
        assert_eq!(payload["response"]["output"][0]["type"], "custom_tool_call");
        assert_eq!(payload["response"]["output"][0]["name"], "exec");
        assert_eq!(payload["response"]["output"][0]["namespace"], "terminal");
        assert_eq!(payload["response"]["output"][0]["input"], "pwd");
        assert!(payload["response"]["output"][0].get("arguments").is_none());
    }

    #[test]
    fn restores_interleaved_namespaced_custom_calls_consistently() {
        let mut restorer = GrokToolStreamRestorer::new(GrokToolMappings {
            custom_tools: std::collections::HashSet::from([
                "terminal__exec".to_owned(),
                "shell".to_owned(),
            ]),
            namespace_tools: HashMap::from([(
                "terminal__exec".to_owned(),
                NamespaceToolRef {
                    namespace: "terminal".to_owned(),
                    name: "exec".to_owned(),
                },
            )]),
            tool_search: false,
        });
        let first_added = restorer.restore_frame(
            b"event: response.output_item.added\r\ndata: {\"type\":\"response.output_item.added\",\"sequence_number\":0,\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"terminal__exec\",\"arguments\":\"\"}}\r\n\r\n",
        );
        assert!(first_added[0].ends_with(b"\r\n\r\n"));
        let first_added = frame_payload(&first_added[0]);
        assert_eq!(first_added["item"]["type"], "custom_tool_call");
        assert_eq!(first_added["item"]["name"], "exec");
        assert_eq!(first_added["item"]["namespace"], "terminal");

        restorer.restore_frame(
            b"data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"item_2\",\"call_id\":\"call_2\",\"name\":\"shell\",\"arguments\":\"\"}}\n\n",
        );
        assert!(
            restorer
                .restore_frame(
                    b"data: {\"type\":\"response.function_call_arguments.delta\",\"sequence_number\":2,\"output_index\":0,\"item_id\":\"item_1\",\"delta\":\"{\\\"input\\\":\\\"first\\\"}\"}\n\n",
                )
                .is_empty()
        );
        assert!(
            restorer
                .restore_frame(
                    b"data: {\"type\":\"response.function_call_arguments.delta\",\"sequence_number\":3,\"output_index\":1,\"item_id\":\"item_2\",\"delta\":\"{\\\"input\\\":\\\"second\\\"}\"}\n\n",
                )
                .is_empty()
        );
        let second_done = restorer.restore_frame(
            b"data: {\"type\":\"response.function_call_arguments.done\",\"sequence_number\":4,\"output_index\":1,\"item_id\":\"item_2\",\"call_id\":\"call_2\",\"name\":\"shell\"}\n\n",
        );
        let second_done = frame_payload(second_done.last().expect("second done event"));
        assert_eq!(second_done["sequence_number"], 3);
        assert_eq!(second_done["name"], "shell");
        assert_eq!(second_done["input"], "second");

        let first_done = restorer.restore_frame(
            b"data: {\"type\":\"response.function_call_arguments.done\",\"sequence_number\":5,\"output_index\":0,\"item_id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"terminal__exec\"}\n\n",
        );
        let first_done = frame_payload(first_done.last().expect("first done event"));
        assert_eq!(first_done["sequence_number"], 5);
        assert_eq!(first_done["name"], "exec");
        assert_eq!(first_done["namespace"], "terminal");
        assert_eq!(first_done["input"], "first");
    }

    #[test]
    fn restores_tool_search_stream_and_terminal_lifecycle() {
        let mut restorer = GrokToolStreamRestorer::new(GrokToolMappings {
            tool_search: true,
            ..GrokToolMappings::default()
        });
        let added = restorer.restore_frame(
            b"data: {\"type\":\"response.output_item.added\",\"sequence_number\":0,\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"search_item\",\"call_id\":\"search_1\",\"name\":\"tool_search\",\"arguments\":\"\"}}\n\n",
        );
        let added = frame_payload(&added[0]);
        assert_eq!(added["item"]["type"], "tool_search_call");
        assert!(added["item"].get("name").is_none());
        assert_eq!(added["item"]["arguments"], "{}");

        assert!(
            restorer
                .restore_frame(
                    b"data: {\"type\":\"response.function_call_arguments.delta\",\"sequence_number\":1,\"output_index\":0,\"item_id\":\"search_item\",\"delta\":\"{\\\"query\\\":\\\"git\\\"}\"}\n\n",
                )
                .is_empty()
        );
        assert!(
            restorer
                .restore_frame(
                    b"data: {\"type\":\"response.function_call_arguments.done\",\"sequence_number\":2,\"output_index\":0,\"item_id\":\"search_item\",\"arguments\":\"{\\\"query\\\":\\\"git\\\"}\"}\n\n",
                )
                .is_empty()
        );
        let done = restorer.restore_frame(
            b"data: {\"type\":\"response.output_item.done\",\"sequence_number\":3,\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"search_item\",\"call_id\":\"search_1\",\"name\":\"tool_search\",\"arguments\":\"{\\\"query\\\":\\\"git\\\"}\"}}\n\n",
        );
        let done = frame_payload(&done[0]);
        assert_eq!(done["sequence_number"], 1);
        assert_eq!(done["item"]["type"], "tool_search_call");
        assert!(done["item"].get("name").is_none());

        let completed = restorer.restore_frame(
            b"data: {\"type\":\"response.completed\",\"sequence_number\":4,\"response\":{\"output\":[{\"type\":\"function_call\",\"id\":\"search_item\",\"call_id\":\"search_1\",\"name\":\"tool_search\",\"arguments\":\"{\\\"query\\\":\\\"git\\\"}\"}]}}\n\n",
        );
        let completed = frame_payload(&completed[0]);
        assert_eq!(completed["sequence_number"], 2);
        assert_eq!(
            completed["response"]["output"][0]["type"],
            "tool_search_call"
        );
        assert_eq!(completed["response"]["output"][0]["execution"], "client");
        assert_eq!(
            completed["response"]["output"][0]["arguments"]["query"],
            "git"
        );
    }

    #[test]
    fn filters_billing_ping_frames_for_strict_responses_clients() {
        let mut restorer = GrokToolStreamRestorer::new(GrokToolMappings::default());

        let lf = restorer.restore_frame(
            b"event: ping\ndata: {\"type\":\"ping\",\"x-opencode-type\":\"inference-cost\"}\n\n",
        );
        assert_eq!(lf, vec![Bytes::from_static(b": ping\n\n")]);

        let crlf = restorer.restore_frame(b"event: ping\r\ndata: {\"type\":\"ping\"}\r\n\r\n");
        assert_eq!(crlf, vec![Bytes::from_static(b": ping\r\n\r\n")]);

        let cr = restorer.restore_frame(b"event: ping\rdata: {\"type\":\"ping\"}\r\r");
        assert_eq!(cr, vec![Bytes::from_static(b": ping\r\r")]);
        assert_eq!(find_sse_frame_end(b"event: ping\r\rnext"), Some(13));
    }

    #[test]
    fn normalizes_reasoning_events_and_resequences_expanded_done() {
        let mut restorer = GrokToolStreamRestorer::new(GrokToolMappings::default());
        let delta = restorer.restore_frame(
            b"event: response.reasoning_text.delta\ndata: {\"type\":\"response.reasoning_text.delta\",\"sequence_number\":3,\"item_id\":\"rs_1\",\"content_index\":0,\"delta\":\"think\"}\n\n",
        );
        let delta = frame_payload(&delta[0]);
        assert_eq!(delta["type"], "response.reasoning_summary_text.delta");
        assert_eq!(delta["summary_index"], 0);
        assert!(delta.get("content_index").is_none());

        let done = restorer.restore_frame(
            b"event: response.reasoning_text.done\ndata: {\"type\":\"response.reasoning_text.done\",\"sequence_number\":4,\"item_id\":\"rs_1\",\"content_index\":0,\"text\":\"think\"}\n\n",
        );
        assert_eq!(done.len(), 2);
        let text_done = frame_payload(&done[0]);
        let part_done = frame_payload(&done[1]);
        assert_eq!(text_done["type"], "response.reasoning_summary_text.done");
        assert_eq!(text_done["sequence_number"], 4);
        assert_eq!(part_done["type"], "response.reasoning_summary_part.done");
        assert_eq!(part_done["sequence_number"], 5);
        assert_eq!(part_done["part"]["type"], "summary_text");
        assert_eq!(part_done["part"]["text"], "think");
    }

    #[test]
    fn rebuilds_missing_terminal_output_from_completed_items() {
        let mut restorer = GrokToolStreamRestorer::new(GrokToolMappings::default());
        let item = restorer.restore_frame(
            b"data: {\"type\":\"response.output_item.done\",\"sequence_number\":0,\"output_index\":2,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello\"}]}}\n\n",
        );
        assert_eq!(item.len(), 1);

        let completed = restorer.restore_frame(
            b"data: {\"type\":\"response.completed\",\"sequence_number\":1,\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        );
        let completed = frame_payload(&completed[0]);
        assert_eq!(completed["response"]["output"][0]["id"], "msg_1");
        assert_eq!(
            completed["response"]["output"][0]["content"][0]["text"],
            "hello"
        );
    }

    #[test]
    fn accepts_multiline_sse_json_and_emits_one_data_line() {
        let mut restorer = GrokToolStreamRestorer::new(GrokToolMappings::default());
        let frames = restorer.restore_frame(
            b"event: response.created\ndata: {\"type\":\ndata: \"response.created\",\"sequence_number\":0}\n\n",
        );
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0]
                .split(|byte| *byte == b'\n')
                .filter(|line| line.starts_with(b"data:"))
                .count(),
            1
        );
        assert_eq!(frame_payload(&frames[0])["type"], "response.created");
    }

    #[tokio::test]
    async fn emits_failed_terminal_when_upstream_ends_without_completion() {
        let upstream: ProviderStream = Box::pin(stream::iter([
            Ok::<Bytes, ProviderError>(Bytes::from_static(
                b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            )),
            Ok(Bytes::from_static(
                b"data: {\"type\":\"response.output_text.delta\"}",
            )),
        ]));
        let mut restored = restore_tool_stream(upstream, GrokToolMappings::default(), "grok-4.5");
        let mut failure = None;
        let mut partial_forwarded = false;
        while let Some(chunk) = restored.next().await {
            let chunk = chunk.expect("terminal failure is sent as SSE");
            let Some(data) = sse_data_payload(&chunk) else {
                continue;
            };
            let payload: Value = serde_json::from_slice(&data).expect("event JSON");
            if payload["type"] == "response.failed" {
                failure = Some(payload);
            } else if payload["type"] == "response.output_text.delta" {
                partial_forwarded = true;
            }
        }
        let failure = failure.expect("missing response.failed terminal");
        assert!(
            !partial_forwarded,
            "incomplete SSE data must not be forwarded"
        );
        assert_eq!(failure["response"]["id"], "resp_1");
        assert_eq!(failure["response"]["model"], "grok-4.5");
        assert_eq!(failure["response"]["status"], "failed");
    }

    #[test]
    fn normalizes_non_success_completed_status_to_failure_terminal() {
        let mut restorer = GrokToolStreamRestorer::new(GrokToolMappings::default());
        let frames = restorer.restore_frame(
            b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_failed\",\"status\":\"failed\",\"output\":[]}}\n\n",
        );
        let payload = frame_payload(&frames[0]);
        assert_eq!(payload["type"], "response.failed");
        assert_eq!(payload["response"]["status"], "failed");
        assert_eq!(
            payload["response"]["error"]["code"],
            "upstream_non_success_terminal"
        );
        assert!(restorer.terminal_seen());
    }

    fn frame_payload(frame: &[u8]) -> Value {
        let data = frame
            .split(|byte| *byte == b'\n')
            .find_map(|line| line.strip_prefix(b"data: "))
            .expect("data line");
        serde_json::from_slice(data).expect("event JSON")
    }
}
