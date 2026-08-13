use futures_util::{StreamExt, stream};

use super::*;

#[tokio::test]
async fn converts_split_responses_events_to_claude_blocks() {
    let upstream: ProviderStream = Box::pin(stream::iter([
        Ok(Bytes::from_static(
            b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"grok-4.5\"}}\n\nevent: reasoning\ndata: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"think\"}\n\nevent: done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"sig_1\"}}\n\nevent: text\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hel",
        )),
        Ok(Bytes::from_static(
            b"lo\"}\n\nevent: text_done\ndata: {\"type\":\"response.content_part.done\",\"part\":{\"type\":\"output_text\"}}\n\nevent: tool\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"shell\"}}\n\nevent: args\ndata: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}\n\nevent: tool_done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"shell\",\"arguments\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}}\n\nevent: completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":4}}}\n\n",
        )),
    ]));
    let converted = adapt_responses_stream_to_claude(
        upstream,
        ClaudeResponseContext::new("grok-4.5".to_owned(), HashMap::new()),
    )
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .expect("converted stream")
    .concat();
    let output = String::from_utf8(converted).expect("UTF-8 SSE");

    assert!(output.contains("event: message_start"));
    assert!(output.contains(r#""type":"thinking_delta""#));
    assert!(output.contains(r#""thinking":"think""#));
    assert!(output.contains(r#""type":"signature_delta""#));
    assert!(output.contains(r#""signature":"sig_1""#));
    assert!(output.contains(r#""type":"text_delta""#));
    assert!(output.contains(r#""text":"hello""#));
    assert!(output.contains(r#""type":"tool_use""#));
    assert!(output.contains(r#""id":"call_1""#));
    assert!(output.contains(r#""name":"shell""#));
    assert!(output.contains(r#""type":"input_json_delta""#));
    assert!(output.contains(r#""partial_json":"{\"cmd\":\"pwd\"}""#));
    assert!(output.contains(r#""stop_reason":"tool_use""#));
    assert!(output.contains("event: message_stop"));
}

#[tokio::test]
async fn converts_web_search_and_incomplete_reason() {
    let upstream: ProviderStream = Box::pin(stream::iter([Ok(Bytes::from_static(
        br#"event: created
data: {"type":"response.created","response":{"id":"resp_1","model":"grok-4.5"}}

event: search
data: {"type":"response.output_item.done","item":{"type":"web_search_call","id":"ws_1","action":{"type":"search","query":"weather","sources":[{"url":"https://example.com","title":"Weather"}]}}}

event: incomplete
data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"},"usage":{"input_tokens":3,"output_tokens":1,"input_tokens_details":{"cached_tokens":1}}}}

"#,
    ))]));
    let converted = adapt_responses_stream_to_claude(
        upstream,
        ClaudeResponseContext::new("grok-4.5".to_owned(), HashMap::new()),
    )
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .expect("converted stream")
    .concat();
    let output = String::from_utf8(converted).expect("UTF-8 SSE");

    assert!(output.contains(r#""type":"server_tool_use""#));
    assert!(output.contains(r#""id":"ws_1""#));
    assert!(output.contains(r#""partial_json":"{\"query\":\"weather\"}""#));
    assert!(output.contains(r#""type":"web_search_tool_result""#));
    assert!(output.contains(r#""url":"https://example.com""#));
    assert!(output.contains(r#""web_search_requests":1"#));
    assert!(output.contains(r#""cache_read_input_tokens":1"#));
    assert!(output.contains(r#""input_tokens":2"#));
    assert!(output.contains(r#""stop_reason":"refusal""#));
    assert!(output.contains("event: message_stop"));
}

#[tokio::test]
async fn converts_failed_and_canceled_responses_to_claude_errors() {
    for (event_type, expected_message) in [
        ("response.failed", "boom"),
        ("response.canceled", "Upstream response was canceled"),
        ("response.cancelled", "Upstream response was canceled"),
    ] {
        let event = if event_type == "response.failed" {
            format!(
                "data: {{\"type\":\"{event_type}\",\"response\":{{\"status\":\"failed\",\"error\":{{\"code\":\"server_error\",\"message\":\"boom\"}}}}}}\n\n"
            )
        } else {
            format!(
                "data: {{\"type\":\"{event_type}\",\"response\":{{\"status\":\"canceled\"}}}}\n\n"
            )
        };
        let upstream: ProviderStream = Box::pin(stream::iter([Ok(Bytes::from(event))]));
        let converted = adapt_responses_stream_to_claude(
            upstream,
            ClaudeResponseContext::new("grok-4.5".to_owned(), HashMap::new()),
        )
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("converted stream")
        .concat();
        let output = String::from_utf8(converted).expect("UTF-8 SSE");

        assert!(output.contains("event: error"), "{event_type}: {output}");
        assert!(output.contains(expected_message), "{event_type}: {output}");
        assert!(
            !output.contains("event: message_stop"),
            "{event_type}: {output}"
        );
    }
}
