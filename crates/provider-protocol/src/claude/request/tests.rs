use provider_core::{ProxyRequest, WireFormat};

use super::*;

#[test]
fn converts_claude_messages_to_responses() {
    let payload = Bytes::from_static(
        br#"{
            "model":"placeholder",
            "max_tokens":2048,
            "system":"Follow the repository instructions.",
            "thinking":{"type":"enabled","budget_tokens":10000},
            "tool_choice":{"type":"any"},
            "tools":[{
                "name":"shell",
                "description":"Run a command",
                "input_schema":{"type":"object","properties":{"cmd":{"type":"string"}}}
            }],
            "messages":[
                {"role":"user","content":"inspect the repository"},
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"check files","signature":"sig_1"},
                    {"type":"text","text":"I will inspect it."},
                    {"type":"tool_use","id":"call_1","name":"shell","input":{"cmd":"pwd"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_1","content":"/code/provider"}
                ]}
            ]
        }"#,
    );
    let request = ProxyRequest::new(WireFormat::ClaudeMessages, "grok-4.5", payload)
        .expect("request envelope");

    let (prepared, _) = prepare_responses_request(request).expect("converted request");
    let body: Value = serde_json::from_slice(&prepared.payload).expect("converted JSON");

    assert_eq!(body["model"], "grok-4.5");
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_output_tokens"], 2048);
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["tool_choice"], "required");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(
        body["tools"][0]["parameters"]["properties"]["cmd"]["type"],
        "string"
    );
    assert_eq!(body["input"][0]["role"], "developer");
    assert_eq!(body["input"][2]["type"], "reasoning");
    assert_eq!(body["input"][2]["summary"], serde_json::json!([]));
    assert_eq!(body["input"][2]["encrypted_content"], "sig_1");
    assert!(!body.to_string().contains("check files"));
    assert_eq!(body["input"][4]["type"], "function_call");
    assert_eq!(body["input"][4]["arguments"], r#"{"cmd":"pwd"}"#);
    assert_eq!(body["input"][5]["type"], "function_call_output");
    assert_eq!(body["input"][5]["output"], "/code/provider");
}

#[test]
fn accepts_claude_code_system_messages_and_drops_attribution() {
    let payload = Bytes::from_static(
        br#"{
            "system":[
                {"type":"text","text":"x-anthropic-billing-header: fingerprint"},
                {"type":"text","text":"Follow repository instructions."}
            ],
            "messages":[
                {"role":"system","content":"x-anthropic-billing-header: request metadata"},
                {"role":"system","content":[
                    {"type":"text","text":"Files changed on disk."}
                ]},
                {"role":"user","content":"continue"}
            ]
        }"#,
    );
    let request = ProxyRequest::new(WireFormat::ClaudeMessages, "grok-4.5", payload)
        .expect("request envelope");

    let (prepared, _) = prepare_responses_request(request).expect("converted request");
    let body: Value = serde_json::from_slice(&prepared.payload).expect("converted JSON");

    assert_eq!(body["input"].as_array().expect("input").len(), 3);
    assert_eq!(body["input"][0]["role"], "developer");
    assert_eq!(
        body["input"][0]["content"][0]["text"],
        "Follow repository instructions."
    );
    assert_eq!(body["input"][1]["role"], "user");
    assert_eq!(
        body["input"][1]["content"][0]["text"],
        "<system-reminder>\nFiles changed on disk.\n</system-reminder>"
    );
    assert_eq!(body["input"][2]["content"][0]["text"], "continue");
    assert!(!body.to_string().contains("x-anthropic-billing-header:"));
}

#[test]
fn converts_images_web_search_and_long_tool_ids() {
    let call_id = format!("toolu_{}", "a".repeat(100));
    let payload = serde_json::to_vec(&serde_json::json!({
        "temperature": 0.4,
        "top_p": 0.8,
        "tools": [{
            "type": "web_search_20250305",
            "name": "web_search",
            "allowed_domains": ["example.com"]
        }],
        "tool_choice": { "type": "tool", "name": "web_search" },
        "messages": [
            {"role":"user","content":[
                {"type":"text","text":"inspect"},
                {"type":"image","source":{
                    "type":"base64","media_type":"image/png","data":"aGVsbG8="
                }},
                {"type":"tool_reference","tool_name":"deferred"}
            ]},
            {"role":"assistant","content":[
                {"type":"redacted_thinking","data":"opaque"},
                {"type":"tool_use","id":call_id,"name":"lookup","input":{"q":"x"}}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":call_id,"content":[
                    {"type":"text","text":"result"},
                    {"type":"image","source":{
                        "type":"base64","mime_type":"image/jpeg","base64":"aW1hZ2U="
                    }}
                ]},
                {"type":"server_tool_use","id":"server_1","name":"web_search"},
                {"type":"web_search_tool_result","tool_use_id":"server_1","content":[]}
            ]}
        ]
    }))
    .expect("request JSON");
    let request = ProxyRequest::new(WireFormat::ClaudeMessages, "grok-4.5", payload.into())
        .expect("request envelope");

    let (prepared, _) = prepare_responses_request(request).expect("converted request");
    let body: Value = serde_json::from_slice(&prepared.payload).expect("converted JSON");

    assert_eq!(body["temperature"], 0.4);
    assert_eq!(body["top_p"], 0.8);
    assert_eq!(body["tools"][0]["type"], "web_search");
    assert_eq!(
        body["tools"][0]["filters"]["allowed_domains"][0],
        "example.com"
    );
    assert_eq!(body["tool_choice"]["type"], "web_search");
    assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
    assert_eq!(
        body["input"][0]["content"][1]["image_url"],
        "data:image/png;base64,aGVsbG8="
    );
    let mapped_call_id = body["input"][1]["call_id"]
        .as_str()
        .expect("mapped call ID");
    assert!(mapped_call_id.len() <= 64);
    assert_eq!(body["input"][2]["call_id"], mapped_call_id);
    assert_eq!(body["input"][2]["output"][0]["type"], "input_text");
    assert_eq!(body["input"][2]["output"][1]["type"], "input_image");
}

#[test]
fn drops_historical_thinking_without_signature() {
    let payload = Bytes::from_static(
        br#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"do not replay"},{"type":"text","text":"visible answer"}]}]}"#,
    );
    let request = ProxyRequest::new(WireFormat::ClaudeMessages, "grok-4.5", payload)
        .expect("request envelope");

    let (prepared, _) = prepare_responses_request(request).expect("converted request");
    let body: Value = serde_json::from_slice(&prepared.payload).expect("converted JSON");

    assert_eq!(body["input"].as_array().expect("input").len(), 1);
    assert_eq!(body["input"][0]["content"][0]["text"], "visible answer");
    assert!(!body.to_string().contains("do not replay"));
}

#[test]
fn rejects_unknown_claude_content_with_its_type() {
    let payload = Bytes::from_static(
        br#"{"messages":[{"role":"user","content":[{"type":"future_block"}]}]}"#,
    );
    let request = ProxyRequest::new(WireFormat::ClaudeMessages, "grok-4.5", payload)
        .expect("request envelope");

    let error = match prepare_responses_request(request) {
        Ok(_) => panic!("unsupported block should fail"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
    assert_eq!(
        error.message(),
        "unsupported Claude content block: future_block"
    );
}
