use std::collections::VecDeque;

use bytes::Bytes;
use futures_util::{StreamExt, stream};
use provider_core::{ProviderError, ProviderErrorKind, ProviderStream};
use serde_json::Value;

use crate::sse::SseDecoder;

use super::{ChatEventConverter, ResponsesResponseContext};

pub(super) fn adapt_chat_stream(
    upstream: ProviderStream,
    context: ResponsesResponseContext,
) -> ProviderStream {
    let state = ChatStreamAdapter {
        upstream,
        decoder: SseDecoder::default(),
        converter: ChatEventConverter::new(context),
        output: VecDeque::new(),
        upstream_done: false,
    };
    Box::pin(stream::unfold(state, |mut state| async move {
        let item = state.next_output().await?;
        Some((item, state))
    }))
}

struct ChatStreamAdapter {
    upstream: ProviderStream,
    decoder: SseDecoder,
    converter: ChatEventConverter,
    output: VecDeque<Result<Bytes, ProviderError>>,
    upstream_done: bool,
}

impl ChatStreamAdapter {
    async fn next_output(&mut self) -> Option<Result<Bytes, ProviderError>> {
        loop {
            if let Some(output) = self.output.pop_front() {
                return Some(output);
            }
            if self.upstream_done {
                return None;
            }
            match self.upstream.next().await {
                Some(Ok(chunk)) => match self.decoder.push(&chunk) {
                    Ok(frames) => {
                        for frame in frames {
                            self.convert_frame(frame);
                        }
                    }
                    Err(_) => {
                        self.upstream_done = true;
                        return Some(Err(crate::sse::frame_too_large_error()));
                    }
                },
                Some(Err(error)) => {
                    self.upstream_done = true;
                    return Some(Err(error));
                }
                None => {
                    if let Some(frame) = self.decoder.finish() {
                        self.convert_frame(frame);
                    }
                    if !self.converter.terminal {
                        if self.converter.finish_reason.is_some() {
                            self.output
                                .extend(self.converter.complete().into_iter().map(Ok));
                        } else {
                            self.output.push_back(Err(ProviderError::new(
                                ProviderErrorKind::Upstream,
                                "Chat Completions upstream ended without a terminal event",
                            )));
                        }
                    }
                    self.upstream_done = true;
                }
            }
        }
    }

    fn convert_frame(&mut self, frame: Bytes) {
        if self.converter.terminal {
            return;
        }
        if frame == "[DONE]" {
            if self.converter.finish_reason.is_some() {
                self.output
                    .extend(self.converter.complete().into_iter().map(Ok));
            } else {
                self.output.extend(
                    self.converter
                        .fail("Chat Completions upstream ended without a finish_reason")
                        .into_iter()
                        .map(Ok),
                );
            }
            return;
        }
        match serde_json::from_slice::<Value>(&frame) {
            Ok(event) => self
                .output
                .extend(self.converter.convert(&event).into_iter().map(Ok)),
            Err(_) => {
                self.converter.terminal = true;
                self.upstream_done = true;
                self.output.push_back(Err(ProviderError::new(
                    ProviderErrorKind::Upstream,
                    "Chat Completions upstream returned an invalid SSE JSON event",
                )));
            }
        }
    }
}
