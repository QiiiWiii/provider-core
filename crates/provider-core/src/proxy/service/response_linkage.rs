use std::sync::Arc;

use bytes::BytesMut;
use futures_util::{StreamExt, stream};

use crate::{AccountId, ProviderRouter, ProviderStream};

pub(super) fn observe_response_id(
    inner: ProviderStream,
    router: Arc<dyn ProviderRouter>,
    routing_scope: String,
    account_id: AccountId,
    bind_response_id_at_created: bool,
) -> ProviderStream {
    const MAX_RESPONSE_LINKAGE_BUFFER_SIZE: usize = 64 * 1024;

    struct State {
        inner: ProviderStream,
        router: Arc<dyn ProviderRouter>,
        routing_scope: String,
        account_id: AccountId,
        bind_response_id_at_created: bool,
        pending: BytesMut,
        response_id: Option<String>,
        bound: bool,
        completed: bool,
        linkage_disabled: bool,
    }

    Box::pin(stream::unfold(
        State {
            inner,
            router,
            routing_scope,
            account_id,
            bind_response_id_at_created,
            pending: BytesMut::new(),
            response_id: None,
            bound: false,
            completed: false,
            linkage_disabled: false,
        },
        |mut state| async move {
            let item = state.inner.next().await?;
            if let Ok(chunk) = &item
                && !state.completed
                && !state.linkage_disabled
            {
                state.pending.extend_from_slice(chunk);
                let events = take_response_linkage_events(&mut state.pending);
                for (event_type, response_id) in &events {
                    if event_type == "response.created" {
                        if let Some(response_id) = response_id {
                            state.response_id = Some(response_id.clone());
                            if state.bind_response_id_at_created && !state.bound {
                                state.router.bind_response_id(
                                    &state.routing_scope,
                                    response_id,
                                    &state.account_id,
                                );
                                state.bound = true;
                            }
                        }
                    } else if matches!(
                        event_type.as_str(),
                        "response.completed" | "response.incomplete"
                    ) {
                        if !state.bound {
                            let response_id = response_id.as_ref().or(state.response_id.as_ref());
                            if let Some(response_id) = response_id {
                                state.router.bind_response_id(
                                    &state.routing_scope,
                                    response_id,
                                    &state.account_id,
                                );
                                state.bound = true;
                            }
                        }
                        state.completed = true;
                    }
                }
                if state.pending.len() > MAX_RESPONSE_LINKAGE_BUFFER_SIZE {
                    state.pending.clear();
                    state.linkage_disabled = true;
                }
            }
            Some((item, state))
        },
    ))
}

pub(super) fn take_response_linkage_events(
    pending: &mut BytesMut,
) -> Vec<(String, Option<String>)> {
    let mut events = Vec::new();
    while let Some(frame_end) = find_sse_frame_end(pending) {
        let frame = pending.split_to(frame_end);
        for line in sse_lines(&frame) {
            let data = line
                .strip_prefix(b"data: ")
                .or_else(|| line.strip_prefix(b"data:"));
            let Some(data) = data else { continue };
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) else {
                continue;
            };
            let Some(event_type) = value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .filter(|event_type| {
                    matches!(
                        *event_type,
                        "response.created" | "response.completed" | "response.incomplete"
                    )
                })
            else {
                continue;
            };
            let response_id = value
                .get("response")
                .and_then(|response| response.get("id"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
            events.push((event_type.to_owned(), response_id));
        }
    }
    events
}

fn find_sse_frame_end(buffer: &[u8]) -> Option<usize> {
    let mut line_start = 0;
    let mut index = 0;
    while index < buffer.len() {
        if !matches!(buffer[index], b'\r' | b'\n') {
            index += 1;
            continue;
        }
        let crlf = buffer[index] == b'\r' && buffer.get(index + 1) == Some(&b'\n');
        let end = index + if crlf { 2 } else { 1 };
        if index == line_start {
            return Some(end);
        }
        line_start = end;
        index = end;
    }
    None
}

fn sse_lines(frame: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < frame.len() {
        if !matches!(frame[index], b'\r' | b'\n') {
            index += 1;
            continue;
        }
        lines.push(&frame[start..index]);
        let crlf = frame[index] == b'\r' && frame.get(index + 1) == Some(&b'\n');
        index += if crlf { 2 } else { 1 };
        start = index;
    }
    if start < frame.len() {
        lines.push(&frame[start..]);
    }
    lines
}
