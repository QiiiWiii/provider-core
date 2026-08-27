mod events;
mod format;
mod items;
mod sse;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};

use bytes::Bytes;
use futures_util::stream;
use provider_core::{ProviderError, ProviderStream};
use serde_json::Value;

use self::sse::SseDecoder;
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
