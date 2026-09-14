use futures_util::{StreamExt, stream};

use super::*;

async fn translated(chunks: Vec<&'static [u8]>, targets: ToolTargets) -> String {
    let upstream: ProviderStream = Box::pin(stream::iter(
        chunks
            .into_iter()
            .map(|chunk| Ok(Bytes::from_static(chunk))),
    ));
    let output = adapt_chat_stream(
        upstream,
        ResponsesResponseContext {
            model: "fallback".to_owned(),
            targets,
        },
    )
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()
    .expect("translated stream")
    .concat();
    String::from_utf8(output).expect("UTF-8 SSE")
}

async fn assert_failed_envelope(chunk: &'static [u8], message: &str) {
    let output = translated(vec![chunk], ToolTargets::new()).await;
    assert!(output.contains("event: response.failed"));
    assert!(output.contains(message));
    assert!(!output.contains("event: response.completed"));
}

#[tokio::test]
async fn converts_text_reasoning_function_tools_usage_and_terminal() {
    let output = translated(
        vec![
            br#"data: {"id":"chatcmpl_1","created":123,"model":"deepseek-v4.1-flash","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"think"},"finish_reason":null}]}

data: {"id":"chatcmpl_1","model":"deepseek-v4.1-flash","choices":[{"index":0,"delta":{"content":"hello","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":"}}]},"finish_reason":null}]}

"#,
            br#"data: {"id":"chatcmpl_1","model":"deepseek-v4.1-flash","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]},"finish_reason":"tool_calls"}],"usage":null}

data: {"id":"chatcmpl_1","model":"deepseek-v4.1-flash","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":5,"total_tokens":17,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":2}}}

data: [DONE]

"#,
        ],
        ToolTargets::from([("lookup".to_owned(), ToolTarget::Function)]),
    )
    .await;

    assert!(output.contains("event: response.created"));
    assert!(output.contains("response.reasoning_summary_text.delta"));
    assert!(output.contains(r#""delta":"think""#));
    assert!(output.contains("response.output_text.delta"));
    assert!(output.contains(r#""delta":"hello""#));
    assert!(output.contains("response.function_call_arguments.delta"));
    assert!(output.contains(r#""arguments":"{\"q\":\"x\"}""#));
    assert!(output.contains(r#""input_tokens":12"#));
    assert!(output.contains(r#""cached_tokens":4"#));
    assert!(output.contains(r#""reasoning_tokens":2"#));
    assert!(output.contains("event: response.completed"));
}

#[tokio::test]
async fn restores_custom_and_namespace_tool_calls() {
    let output = translated(
        vec![br#"data: {"id":"chatcmpl_2","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"exec","arguments":"{\"input\":\"pwd\"}"}},{"index":1,"id":"call_2","function":{"name":"terminal__run","arguments":"{\"input\":\"ls\"}"}}]},"finish_reason":"tool_calls"}]}

data: [DONE]

"#],
        ToolTargets::from([
            ("exec".to_owned(), ToolTarget::Custom),
            (
                "terminal__run".to_owned(),
                ToolTarget::Namespace {
                    namespace: "terminal".to_owned(),
                    name: "run".to_owned(),
                    custom: true,
                },
            ),
        ]),
    )
    .await;

    assert!(output.contains("response.custom_tool_call_input.done"));
    assert!(output.contains("response.custom_tool_call_input.delta"));
    assert!(output.contains(r#""type":"custom_tool_call""#));
    assert!(output.contains(r#""input":"pwd""#));
    assert!(output.contains(r#""namespace":"terminal""#));
    assert!(output.contains(r#""name":"run""#));
}

#[tokio::test]
async fn waits_for_split_tool_identity_before_emitting_the_item() {
    let output = translated(
        vec![br#"data: {"id":"chatcmpl_split","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"q\":\"x\"}"}}]},"finish_reason":null}]}

data: {"id":"chatcmpl_split","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_split","function":{"name":"lookup"}}]},"finish_reason":"tool_calls"}]}

data: [DONE]

"#],
        ToolTargets::from([("lookup".to_owned(), ToolTarget::Function)]),
    )
    .await;

    let added = output
        .find("response.output_item.added")
        .expect("item added");
    let arguments = output
        .find("response.function_call_arguments.delta")
        .expect("arguments delta");
    assert!(added < arguments);
    assert!(output.contains(r#""call_id":"call_split""#));
    assert!(output.contains(r#""delta":"{\"q\":\"x\"}""#));
}

#[tokio::test]
async fn fails_when_tool_identity_never_completes() {
    let output = translated(
        vec![br#"data: {"id":"chatcmpl_missing","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_missing","function":{"arguments":"{}"}}]},"finish_reason":"tool_calls"}]}

data: [DONE]

"#],
        ToolTargets::new(),
    )
    .await;

    assert!(output.contains("event: response.failed"));
    assert!(output.contains("without both id and name"));
    assert!(!output.contains("event: response.completed"));
}

#[tokio::test]
async fn fails_when_upstream_changes_tool_identity() {
    let output = translated(
        vec![br#"data: {"id":"chatcmpl_changed","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":null}]}

data: {"id":"chatcmpl_changed","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_2","function":{"name":"lookup"}}]},"finish_reason":"tool_calls"}]}

data: {"id":"chatcmpl_changed","choices":[{"index":0,"delta":{"content":"must be ignored"},"finish_reason":"stop"}]}

data: [DONE]

"#],
        ToolTargets::from([("lookup".to_owned(), ToolTarget::Function)]),
    )
    .await;

    assert!(output.contains("event: response.failed"));
    assert!(output.contains("changed a tool call id"));
    assert!(!output.contains("must be ignored"));
    assert!(!output.contains("event: response.completed"));
}

#[tokio::test]
async fn fails_when_upstream_changes_the_response_envelope() {
    assert_failed_envelope(
        br#"data: {"id":"chatcmpl_a","model":"model-a","created":1,"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}

data: {"id":"chatcmpl_b","model":"model-a","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: [DONE]

"#,
        "changed response id",
    )
    .await;
    assert_failed_envelope(
        br#"data: {"id":"chatcmpl_a","model":"model-a","created":1,"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}

data: {"id":"chatcmpl_a","model":"model-b","created":1,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: [DONE]

"#,
        "changed response model",
    )
    .await;
    assert_failed_envelope(
        br#"data: {"id":"chatcmpl_a","model":"model-a","created":1,"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}

data: {"id":"chatcmpl_a","model":"model-a","created":2,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: [DONE]

"#,
        "changed response creation time",
    )
    .await;
}

#[tokio::test]
async fn a_synthetic_response_id_cannot_be_replaced_after_created() {
    let output = translated(
        vec![
            br#"data: {"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}

data: {"id":"chatcmpl_late","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: [DONE]

"#,
        ],
        ToolTargets::new(),
    )
    .await;

    assert!(output.contains(r#""id":"resp_provider""#));
    assert!(output.contains("event: response.failed"));
    assert!(output.contains("changed response id"));
    assert!(!output.contains("event: response.completed"));
}

#[tokio::test]
async fn fails_when_upstream_calls_an_undeclared_tool() {
    let output = translated(
        vec![br#"data: {"id":"chatcmpl_undeclared","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"other","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}

data: [DONE]

"#],
        ToolTargets::from([("lookup".to_owned(), ToolTarget::Function)]),
    )
    .await;

    assert!(output.contains("event: response.failed"));
    assert!(output.contains("called undeclared tool other"));
    assert!(!output.contains("event: response.completed"));
}

#[tokio::test]
async fn ignores_frames_after_an_upstream_error_event() {
    let output = translated(
        vec![
            br#"data: {"error":{"code":"bad_gateway","message":"failed"}}

data: {"choices":[{"index":0,"delta":{"content":"must be ignored"},"finish_reason":"stop"}]}

data: [DONE]

"#,
        ],
        ToolTargets::new(),
    )
    .await;

    assert!(output.contains("event: error"));
    assert!(!output.contains("must be ignored"));
    assert!(!output.contains("event: response.completed"));
}

#[tokio::test]
async fn terminal_output_follows_first_seen_output_indexes() {
    let output = translated(
        vec![br#"data: {"id":"chatcmpl_order","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}]}

data: {"id":"chatcmpl_order","choices":[{"index":0,"delta":{"reasoning_content":"why","tool_calls":[{"index":0,"id":"call_1","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}

data: [DONE]

"#],
        ToolTargets::from([("lookup".to_owned(), ToolTarget::Function)]),
    )
    .await;

    let terminal = output
        .rsplit_once("event: response.completed")
        .expect("completed event")
        .1;
    let message = terminal.find(r#""type":"message""#).expect("message");
    let reasoning = terminal.find(r#""type":"reasoning""#).expect("reasoning");
    let tool = terminal
        .find(r#""type":"function_call""#)
        .expect("function call");
    assert!(message < reasoning && reasoning < tool);
}

#[tokio::test]
async fn maps_length_to_an_incomplete_response() {
    let output = translated(
        vec![br#"data: {"id":"chatcmpl_3","choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":"length"}]}

data: [DONE]

"#],
        ToolTargets::new(),
    )
    .await;
    assert!(output.contains("event: response.incomplete"));
    assert!(output.contains(r#""reason":"max_output_tokens""#));
}

#[tokio::test]
async fn rejects_streams_without_a_chat_terminal() {
    let upstream: ProviderStream = Box::pin(stream::iter([Ok(Bytes::from_static(
        br#"data: {"choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]}

"#,
    ))]));
    let results = adapt_chat_stream(
        upstream,
        ResponsesResponseContext {
            model: "model".to_owned(),
            targets: ToolTargets::new(),
        },
    )
    .collect::<Vec<_>>()
    .await;
    assert!(results.last().expect("terminal result").is_err());
}

#[tokio::test]
async fn done_without_finish_reason_is_a_failed_response() {
    let output = translated(
        vec![br#"data: {"id":"chatcmpl_missing_finish","choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]}

data: [DONE]

"#],
        ToolTargets::new(),
    )
    .await;

    assert!(output.contains("event: response.failed"));
    assert!(output.contains("ended without a finish_reason"));
    assert!(!output.contains("event: response.completed"));
}
