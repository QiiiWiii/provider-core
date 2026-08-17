use futures_util::{StreamExt, stream};

use super::*;

#[tokio::test]
async fn converts_responses_text_reasoning_tools_and_usage_to_chat_chunks() {
    let upstream: ProviderStream = Box::pin(stream::iter([Ok(Bytes::from_static(
        br#"event: created
data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-5.6-sol","created_at":123}}

event: reasoning
data: {"type":"response.reasoning_text.delta","delta":"think"}

event: text
data: {"type":"response.output_text.delta","delta":"hello"}

event: tool
data: {"type":"response.output_item.added","item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"exec"}}

event: args
data: {"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"{\"cmd\":\"pwd\"}"}

event: completed
data: {"type":"response.completed","response":{"id":"resp_1","model":"gpt-5.6-sol","usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":4},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":17}}}

"#,
    ))]));
    let output = adapt_responses_stream_to_chat(
        upstream,
        ChatCompletionsResponseContext::new("fallback".to_owned(), true),
    )
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .expect("converted stream")
    .concat();
    let output = String::from_utf8(output).expect("UTF-8 SSE");
    assert!(output.contains(r#""role":"assistant""#));
    assert!(output.contains(r#""reasoning_content":"think""#));
    assert!(output.contains(r#""content":"hello""#));
    assert!(output.contains(r#""id":"call_1""#));
    assert!(output.contains(r#""name":"exec""#));
    assert!(output.contains(r#""arguments":"{\"cmd\":\"pwd\"}""#));
    assert!(output.contains(r#""finish_reason":"tool_calls""#));
    assert!(output.contains(r#""usage":null"#));
    assert!(output.contains(r#""prompt_tokens":12"#));
    assert!(output.contains(r#""cached_tokens":4"#));
    assert!(output.contains(r#""reasoning_tokens":2"#));
    assert!(output.ends_with("data: [DONE]\n\n"));
}

#[tokio::test]
async fn falls_back_to_completed_items_when_upstream_omits_deltas() {
    let upstream: ProviderStream = Box::pin(stream::iter([Ok(Bytes::from_static(
        br#"data: {"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"done"}]}}

data: {"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}

"#,
    ))]));
    let output = adapt_responses_stream_to_chat(
        upstream,
        ChatCompletionsResponseContext::new("model".to_owned(), false),
    )
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .expect("converted stream")
    .concat();
    let output = String::from_utf8(output).expect("UTF-8 SSE");
    assert!(output.contains(r#""content":"done""#));
    assert!(output.contains(r#""finish_reason":"stop""#));
    assert!(!output.contains(r#""usage":"#));
}
