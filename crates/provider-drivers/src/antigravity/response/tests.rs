use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use provider_core::{ProviderErrorKind, ProviderStream};
use serde_json::json;

use super::format::stream_error;
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
    let mut translated = translate_stream(Box::pin(stream), "gemini-3-flash".to_owned(), br#"{}"#);
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
