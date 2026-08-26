use std::sync::Arc;

use provider_core::{ProviderStream, usage::RawUsageFields};
use serde_json::Value;

use super::usage_observer::{ObservedFrame, observe_usage};

#[must_use]
pub fn observe_claude_messages_usage(
    upstream: ProviderStream,
    attempt: Arc<dyn provider_core::usage::AttemptTracking>,
) -> ProviderStream {
    observe_usage(upstream, attempt, extract_claude_messages_facts)
}

fn extract_claude_messages_facts(frame: &[u8]) -> Option<ObservedFrame> {
    if !contains_subslice(frame, b"usage")
        && !contains_subslice(frame, b"message")
        && !contains_subslice(frame, b"content_block")
        && !contains_subslice(frame, b"error")
    {
        return None;
    }

    let event: Value = serde_json::from_slice(frame).ok()?;
    let event_type = event.get("type").and_then(Value::as_str);
    let message = event.get("message");
    let usage = message
        .and_then(|message| message.get("usage"))
        .or_else(|| event.get("usage"))
        .filter(|usage| usage.is_object());
    let model = message
        .and_then(|message| message.get("model"))
        .or_else(|| event.get("model"))
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty() && model.len() <= super::usage_observer::MAX_MODEL_LEN)
        .map(ToOwned::to_owned);
    let successful_terminal = claude_terminal(event_type, &event);
    let first_token = matches!(
        event_type,
        Some("content_block_start" | "content_block_delta")
    ) || (event_type == Some("message")
        && event
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|content| !content.is_empty()));

    if usage.is_none() && model.is_none() && successful_terminal.is_none() && !first_token {
        return None;
    }
    let fields = usage
        .map(RawUsageFields::from_claude_usage)
        .map(|mut fields| {
            if event_type == Some("message_start") {
                fields.output = None;
                fields.reasoning = None;
                fields.total = None;
            }
            fields
        });
    Some(ObservedFrame {
        fields,
        model,
        first_token,
        successful_terminal,
    })
}

fn claude_terminal(event_type: Option<&str>, event: &Value) -> Option<bool> {
    match event_type {
        Some("message_stop") => Some(true),
        Some("error") => Some(false),
        Some("message_delta") => event
            .get("delta")
            .and_then(|delta| delta.get("stop_reason"))
            .and_then(Value::as_str)
            .and_then(claude_incomplete_stop_reason),
        Some("message") => match event.get("stop_reason").and_then(Value::as_str) {
            Some(reason) => claude_stop_reason(reason),
            None => Some(true),
        },
        _ => None,
    }
}

fn claude_stop_reason(reason: &str) -> Option<bool> {
    if reason.is_empty() {
        return None;
    }
    Some(!matches!(
        reason,
        "max_tokens" | "model_context_window_exceeded"
    ))
}

fn claude_incomplete_stop_reason(reason: &str) -> Option<bool> {
    matches!(reason, "max_tokens" | "model_context_window_exceeded").then_some(false)
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use futures_util::{StreamExt, stream};
    use provider_core::{
        ProviderStream,
        usage::{AttemptTracking, RawUsageFields},
    };

    use super::*;

    #[derive(Default)]
    struct RecordingAttempt {
        finished: Mutex<Option<Option<RawUsageFields>>>,
        cancelled: Mutex<bool>,
        observation_lost: Mutex<bool>,
        first_token: Mutex<bool>,
        success_terminal: Mutex<bool>,
        model: Mutex<Option<String>>,
    }

    impl RecordingAttempt {
        fn reported(&self) -> Option<Option<RawUsageFields>> {
            *self.finished.lock().expect("finished lock")
        }

        fn saw_success_terminal(&self) -> bool {
            *self.success_terminal.lock().expect("terminal lock")
        }

        fn saw_first_token(&self) -> bool {
            *self.first_token.lock().expect("first token lock")
        }
    }

    impl AttemptTracking for RecordingAttempt {
        fn stream_opened(&self) {}

        fn first_token_observed(&self) {
            *self.first_token.lock().expect("first token lock") = true;
        }

        fn success_terminal_observed(&self) {
            *self.success_terminal.lock().expect("terminal lock") = true;
        }

        fn provider_model_observed(&self, model: &str) {
            *self.model.lock().expect("model lock") = Some(model.to_owned());
        }

        fn observation_lost(&self) {
            *self.observation_lost.lock().expect("lost lock") = true;
        }

        fn finished(&self, fields: Option<RawUsageFields>) {
            let mut slot = self.finished.lock().expect("finished lock");
            assert!(slot.is_none(), "an attempt must be told exactly once");
            *slot = Some(fields);
        }

        fn cancelled(&self, fields: Option<RawUsageFields>) {
            *self.cancelled.lock().expect("cancelled lock") = true;
            self.finished(fields);
        }

        fn failed(&self, _answered: bool) {}
    }

    fn recording_attempt() -> (Arc<RecordingAttempt>, Arc<dyn AttemptTracking>) {
        let attempt = Arc::new(RecordingAttempt::default());
        (Arc::clone(&attempt), attempt)
    }

    fn byte_stream(chunks: Vec<&'static str>) -> ProviderStream {
        Box::pin(stream::iter(
            chunks
                .into_iter()
                .map(|chunk| Ok(Bytes::from_static(chunk.as_bytes()))),
        ))
    }

    const CLAUDE_START: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{",
        "\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",",
        "\"model\":\"claude-opus-5\",\"content\":[],\"stop_reason\":null,",
        "\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":50,",
        "\"cache_creation_input_tokens\":20,\"output_tokens\":1}}}\n\n"
    );

    const CLAUDE_OUTPUT: &str = concat!(
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n"
    );

    const CLAUDE_DELTA: &str = concat!(
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{",
        "\"stop_reason\":\"end_turn\",\"stop_sequence\":null},",
        "\"usage\":{\"output_tokens\":25,",
        "\"output_tokens_details\":{\"thinking_tokens\":4}}}\n\n"
    );

    const CLAUDE_DELTA_FINAL: &str = concat!(
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{",
        "\"stop_reason\":\"end_turn\",\"stop_sequence\":null},",
        "\"usage\":{\"output_tokens\":26}}\n\n"
    );

    const CLAUDE_STOP: &str = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    #[tokio::test]
    async fn usage_merges_start_and_delta_without_adding_output() {
        let (observed, attempt) = recording_attempt();
        let chunks = vec![
            CLAUDE_START,
            CLAUDE_OUTPUT,
            CLAUDE_DELTA,
            CLAUDE_DELTA_FINAL,
            CLAUDE_STOP,
        ];
        let stream = observe_claude_messages_usage(byte_stream(chunks), attempt);
        let _: Vec<Bytes> = stream.map(|item| item.expect("no error")).collect().await;

        let fields = observed
            .reported()
            .expect("attempt told")
            .expect("usage observed");
        assert_eq!(fields.input, Some(100));
        assert_eq!(fields.cache_read, Some(50));
        assert_eq!(fields.cache_write, Some(20));
        assert_eq!(fields.output, Some(26));
        assert_eq!(fields.reasoning, Some(4));
        assert_eq!(fields.total, None);
        assert_eq!(
            observed.model.lock().expect("model lock").as_deref(),
            Some("claude-opus-5")
        );
        assert!(observed.saw_first_token());
        assert!(observed.saw_success_terminal());
    }

    #[tokio::test]
    async fn error_terminal_keeps_usage_without_success() {
        let (observed, attempt) = recording_attempt();
        let error = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\"}}\n\n";
        let stream = observe_claude_messages_usage(
            byte_stream(vec![CLAUDE_START, error, CLAUDE_STOP]),
            attempt,
        );
        let _: Vec<_> = stream.collect().await;

        assert!(!observed.saw_success_terminal());
        assert_eq!(
            observed
                .reported()
                .expect("attempt told")
                .expect("start usage kept")
                .input,
            Some(100)
        );
    }

    #[tokio::test]
    async fn missing_stop_does_not_prove_success() {
        let (observed, attempt) = recording_attempt();
        let stream = observe_claude_messages_usage(
            byte_stream(vec![CLAUDE_START, CLAUDE_OUTPUT, CLAUDE_DELTA]),
            attempt,
        );
        let _: Vec<_> = stream.collect().await;

        assert!(!observed.saw_success_terminal());
        assert!(observed.reported().expect("attempt told").is_some());
    }

    #[tokio::test]
    async fn message_start_placeholder_output_is_not_final_usage() {
        let (observed, attempt) = recording_attempt();
        let stream =
            observe_claude_messages_usage(byte_stream(vec![CLAUDE_START, CLAUDE_STOP]), attempt);
        let _: Vec<_> = stream.collect().await;

        let fields = observed
            .reported()
            .expect("attempt told")
            .expect("input usage observed");
        assert_eq!(fields.input, Some(100));
        assert_eq!(fields.output, None);
        assert!(observed.saw_success_terminal());
    }

    #[tokio::test]
    async fn usage_accepts_sse_frames_split_across_chunks() {
        let (observed, attempt) = recording_attempt();
        let split = CLAUDE_START.len() / 2;
        let upstream: ProviderStream = Box::pin(stream::iter([
            Ok(Bytes::copy_from_slice(&CLAUDE_START.as_bytes()[..split])),
            Ok(Bytes::copy_from_slice(&CLAUDE_START.as_bytes()[split..])),
            Ok(Bytes::from_static(CLAUDE_STOP.as_bytes())),
        ]));
        let stream = observe_claude_messages_usage(upstream, attempt);
        let _: Vec<_> = stream.collect().await;

        assert_eq!(
            observed
                .reported()
                .expect("attempt told")
                .expect("usage observed")
                .input,
            Some(100)
        );
        assert!(observed.saw_success_terminal());
    }

    #[tokio::test]
    async fn max_tokens_terminal_is_incomplete() {
        let (observed, attempt) = recording_attempt();
        let delta = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":25}}\n\n";
        let stream = observe_claude_messages_usage(
            byte_stream(vec![CLAUDE_START, delta, CLAUDE_STOP]),
            attempt,
        );
        let _: Vec<_> = stream.collect().await;

        assert!(!observed.saw_success_terminal());
        assert_eq!(
            observed
                .reported()
                .expect("attempt told")
                .expect("usage observed")
                .output,
            Some(25)
        );
    }

    #[tokio::test]
    async fn non_streaming_message_usage_is_observed() {
        let message = r#"{"type":"message","id":"msg_2","model":"claude-haiku-4-5-20251001","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":3}}"#;
        let (observed, attempt) = recording_attempt();
        let stream = observe_claude_messages_usage(byte_stream(vec![message]), attempt);
        let _: Vec<_> = stream.collect().await;

        let fields = observed
            .reported()
            .expect("attempt told")
            .expect("usage observed");
        assert_eq!(fields.input, Some(10));
        assert_eq!(fields.output, Some(3));
        assert!(observed.saw_first_token());
        assert!(observed.saw_success_terminal());
    }
}
