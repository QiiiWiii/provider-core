use provider_core::{RequestMetadata, WireFormat};

use super::*;

fn convert(body: Value) -> (Value, ResponsesResponseTranslator) {
    let model = body["model"].as_str().unwrap_or("test-model").to_owned();
    let request = ProxyRequest::new(
        WireFormat::OpenAiResponses,
        model,
        Bytes::from(serde_json::to_vec(&body).expect("request JSON")),
    )
    .expect("proxy request");
    let (request, response) = prepare_chat_request(request).expect("converted request");
    assert_eq!(request.format, WireFormat::OpenAiChatCompletions);
    (
        serde_json::from_slice(&request.payload).expect("Chat Completions JSON"),
        response,
    )
}

#[test]
fn converts_codex_text_reasoning_images_tools_and_controls() {
    let (body, _) = convert(serde_json::json!({
        "model":"deepseek-v4.1-flash",
        "stream":true,
        "store":false,
        "prompt_cache_key":"codex-session",
        "client_metadata":{"turn_id":42},
        "stream_options":{"include_usage":true,"include_obfuscation":false},
        "include":["reasoning.encrypted_content"],
        "instructions":"be useful",
        "input":[
            {"type":"message","role":"user","content":[
                {"type":"input_text","text":"inspect"},
                {"type":"input_image","image_url":"data:image/png;base64,a","detail":"high"}
            ]},
            {"type":"reasoning","summary":[{"type":"summary_text","text":"prior thought"}],"encrypted_content":"opaque"},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"checking"}]},
            {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"query\":\"x\"}"},
            {"type":"function_call_output","call_id":"call_1","output":{"ok":true}},
            {"type":"custom_tool_call","call_id":"call_2","name":"exec","input":"pwd"},
            {"type":"custom_tool_call_output","call_id":"call_2","output":"/tmp"},
            {"type":"agent_message","content":[
                {"type":"input_text","text":"Payload:"},
                {"type":"encrypted_content","encrypted_content":"delegated task"}
            ]}
        ],
        "tools":[
            {"type":"function","name":"lookup","description":"look up","parameters":{"type":"object"},"strict":true},
            {"type":"custom","name":"exec","description":"run text","format":{"type":"text"}}
        ],
        "tool_choice":"auto",
        "parallel_tool_calls":true,
        "max_output_tokens":2048,
        "temperature":0.2,
        "reasoning":{"effort":"high","summary":"auto"},
        "text":{"format":{"type":"json_schema","name":"result","schema":{"type":"object"},"strict":true}}
    }));

    assert_eq!(body["model"], "deepseek-v4.1-flash");
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["messages"][0]["role"], "developer");
    assert_eq!(body["messages"][1]["content"][1]["type"], "image_url");
    assert_eq!(body["messages"][2]["reasoning_content"], "prior thought");
    assert_eq!(
        body["messages"][2]["tool_calls"][0]["function"]["name"],
        "lookup"
    );
    assert_eq!(body["messages"][3]["role"], "tool");
    assert_eq!(
        body["messages"][4]["tool_calls"][0]["function"]["name"],
        "exec"
    );
    assert_eq!(
        body["messages"][4]["tool_calls"][0]["function"]["arguments"],
        "{\"input\":\"pwd\"}"
    );
    assert_eq!(body["messages"][6]["content"][1]["text"], "delegated task");
    assert_eq!(body["tools"][0]["function"]["strict"], true);
    assert_eq!(
        body["tools"][1]["function"]["parameters"]["required"][0],
        "input"
    );
    assert_eq!(body["max_completion_tokens"], 2048);
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert!(body.get("prompt_cache_key").is_none());
    assert!(body.get("client_metadata").is_none());
}

#[test]
fn converts_custom_grammar_to_a_string_function() {
    let (body, _) = convert(serde_json::json!({
        "model":"deepseek-v4.1-flash",
        "input":"produce a command",
        "tools":[{
            "type":"custom",
            "name":"exec",
            "format":{
                "type":"grammar",
                "syntax":"lark",
                "definition":"start: /.+/"
            }
        }]
    }));

    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "exec");
    assert_eq!(
        body["tools"][0]["function"]["parameters"],
        serde_json::json!({
            "type":"object",
            "properties":{"input":{"type":"string"}},
            "required":["input"],
            "additionalProperties":false
        })
    );
    assert!(body["tools"][0]["function"].get("format").is_none());
}

#[test]
fn converts_recorded_codex_agent_message_with_unreadable_placeholder() {
    let source: Value = serde_json::from_slice(include_bytes!(
        "../../../../provider-drivers/src/codex/fixtures/agent_message_session_01a01e85.json"
    ))
    .expect("recorded Codex request");
    let (body, _) = convert(source);

    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(
        body["messages"][0]["content"][0]["text"],
        "Message Type: NEW_TASK\nTask name: /root/backend_analysis\nSender: /root\nPayload:\n"
    );
    assert_eq!(
        body["messages"][0]["content"][1]["text"],
        "对仓库做后端只读分析……"
    );
    assert_eq!(
        body["messages"][0]["content"]
            .as_array()
            .expect("converted agent message")
            .len(),
        2
    );
}

#[test]
fn converts_namespaces_additional_tools_and_hosted_client_tools() {
    let (body, _) = convert(serde_json::json!({
        "model":"deepseek-v4.1-flash",
        "input":[
            {"type":"additional_tools","tools":[
                {"type":"namespace","name":"terminal","tools":[{"type":"custom","name":"exec"}]}
            ]},
            {"type":"custom_tool_call","namespace":"terminal","name":"exec","call_id":"c1","input":"pwd"},
            {"type":"custom_tool_call_output","call_id":"c1","output":"ok"},
            {"type":"local_shell_call","call_id":"c2","action":{"type":"exec","command":["ls"]}},
            {"type":"local_shell_call_output","call_id":"c2","output":"file"},
            {"type":"apply_patch_call","call_id":"c3","operation":{"type":"update_file","path":"a"}},
            {"type":"apply_patch_call_output","call_id":"c3","output":"done"}
        ],
        "tools":[{"type":"local_shell"},{"type":"apply_patch"}]
    }));

    assert!(
        body["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .any(|tool| tool["function"]["name"] == "terminal__exec")
    );
    assert!(
        body["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .any(|tool| tool["function"]["name"] == "local_shell")
    );
    assert!(
        body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .any(|message| message["tool_calls"][0]["function"]["name"] == "apply_patch")
    );
}

#[test]
fn rejects_state_and_hosted_features_that_cannot_be_preserved() {
    let mut metadata = RequestMetadata::default();
    metadata.previous_response_id = Some("resp_previous".to_owned());
    let request = ProxyRequest::new(
        WireFormat::OpenAiResponses,
        "deepseek-v4.1-flash",
        Bytes::from_static(br#"{"model":"deepseek-v4.1-flash","input":"hello"}"#),
    )
    .expect("proxy request")
    .with_metadata(metadata);
    let error = prepare_chat_request(request).expect_err("previous response is unsupported");
    assert!(error.message().contains("complete input history"));

    for body in [
        serde_json::json!({"model":"m","input":[{"type":"item_reference","id":"item_1"}]}),
        serde_json::json!({"model":"m","input":[{"type":"compaction","encrypted_content":"opaque"}]}),
        serde_json::json!({"model":"m","input":"hello","tools":[{"type":"web_search"}]}),
        serde_json::json!({"model":"m","input":"hello","store":true}),
        serde_json::json!({"model":"m","input":"hello","include":["message.output_text.logprobs"]}),
        serde_json::json!({"model":"m","input":"hello","max_tool_calls":2}),
        serde_json::json!({"model":"m","input":"hello","max_output_tokens":"10"}),
        serde_json::json!({"model":"m","input":"hello","temperature":"low"}),
        serde_json::json!({"model":"m","input":"hello","parallel_tool_calls":"yes"}),
        serde_json::json!({"model":"m","input":"hello","store":"false"}),
        serde_json::json!({"model":"m","input":"hello","truncation":"auto"}),
        serde_json::json!({"model":"m","input":"hello","tools":[{"type":"custom","name":"shell","format":{"type":"grammar","syntax":"ebnf","definition":"start: /.+/"}}]}),
        serde_json::json!({"model":"m","input":"hello","tools":[{"type":"custom","name":"shell","format":{"type":"grammar","syntax":"lark"}}]}),
    ] {
        let request = ProxyRequest::new(
            WireFormat::OpenAiResponses,
            "m",
            Bytes::from(serde_json::to_vec(&body).expect("request JSON")),
        )
        .expect("proxy request");
        assert!(prepare_chat_request(request).is_err());
    }
}

#[test]
fn rejects_allowed_tool_subset_that_chat_cannot_express() {
    let body = serde_json::json!({
        "model":"m",
        "input":"hello",
        "tools":[{"type":"function","name":"a"},{"type":"function","name":"b"}],
        "tool_choice":{"type":"allowed_tools","mode":"required","tools":[
            {"type":"function","name":"a"},{"type":"function","name":"b"}
        ]}
    });
    let request = ProxyRequest::new(
        WireFormat::OpenAiResponses,
        "m",
        Bytes::from(serde_json::to_vec(&body).expect("request JSON")),
    )
    .expect("proxy request");
    let error = prepare_chat_request(request).expect_err("subset is unsupported");
    assert!(error.message().contains("allowed_tools"));
}

#[test]
fn validates_auto_allowed_tools_by_identity_not_count() {
    let (body, _) = convert(serde_json::json!({
        "model":"m",
        "input":"hello",
        "tools":[{"type":"function","name":"a"},{"type":"function","name":"b"}],
        "tool_choice":{"type":"allowed_tools","mode":"auto","tools":[
            {"type":"function","name":"b"},{"type":"function","name":"a"}
        ]}
    }));
    assert_eq!(body["tool_choice"], "auto");

    for allowed in [
        serde_json::json!([
            {"type":"function","name":"a"},{"type":"function","name":"c"}
        ]),
        serde_json::json!([
            {"type":"function","name":"a"},{"type":"function","name":"a"}
        ]),
    ] {
        let body = serde_json::json!({
            "model":"m",
            "input":"hello",
            "tools":[{"type":"function","name":"a"},{"type":"function","name":"b"}],
            "tool_choice":{"type":"allowed_tools","mode":"auto","tools":allowed}
        });
        let request = ProxyRequest::new(
            WireFormat::OpenAiResponses,
            "m",
            Bytes::from(serde_json::to_vec(&body).expect("request JSON")),
        )
        .expect("proxy request");
        let error = prepare_chat_request(request).expect_err("invalid allowed tool set");
        assert!(error.message().contains("allowed_tools"));
    }

    let body = serde_json::json!({
        "model":"m",
        "input":"hello",
        "tools":[{"type":"custom","name":"a"}],
        "tool_choice":{"type":"allowed_tools","mode":"auto","tools":[
            {"type":"function","name":"a"}
        ]}
    });
    let request = ProxyRequest::new(
        WireFormat::OpenAiResponses,
        "m",
        Bytes::from(serde_json::to_vec(&body).expect("request JSON")),
    )
    .expect("proxy request");
    let error = prepare_chat_request(request).expect_err("tool type mismatch");
    assert!(error.message().contains("allowed_tools"));
}

#[test]
fn rejects_lossy_nested_controls_and_history_values() {
    for body in [
        serde_json::json!({"model":"m","input":"hello","reasoning":{"summary":"detailed"}}),
        serde_json::json!({"model":"m","input":"hello","reasoning":{"effort":"high","future":true}}),
        serde_json::json!({"model":"m","input":"hello","text":{"verbosity":"high"}}),
        serde_json::json!({"model":"m","input":"hello","text":{"format":{"type":"json_schema","schema":{"type":"object"}}}}),
        serde_json::json!({"model":"m","input":"hello","text":{"format":{"type":"json_schema","name":"result"}}}),
        serde_json::json!({"model":"m","input":"hello","text":{"format":{"type":"json_schema","name":"result","schema":{"type":"object"},"strict":"true"}}}),
        serde_json::json!({"model":"m","input":[{"type":"message","role":"user","content":[{"type":"input_text"}]}]}),
        serde_json::json!({"model":"m","input":[{"type":"reasoning","summary":[{"type":"summary_text"}]}]}),
        serde_json::json!({
            "model":"m",
            "input":[{"type":"custom_tool_call","call_id":"call_1","name":"shell"}],
            "tools":[{"type":"custom","name":"shell","format":{"type":"text"}}]
        }),
        serde_json::json!({"model":"m","input":[{"type":"function_call_output","call_id":"call_1"}]}),
        serde_json::json!({"model":"m","input":[{"type":"tool_search_output","call_id":"call_1"}]}),
        serde_json::json!({
            "model":"m",
            "input":[
                {"type":"function_call","call_id":"call_1","name":"lookup"},
                {"type":"function_call_output","call_id":"call_1","output":"done"}
            ],
            "tools":[{"type":"function","name":"lookup"}]
        }),
        serde_json::json!({
            "model":"m",
            "input":[{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"}],
            "tools":[{"type":"function","name":"lookup"}]
        }),
        serde_json::json!({
            "model":"m",
            "input":[
                {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"done"},
                {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"done"}
            ],
            "tools":[{"type":"function","name":"lookup"}]
        }),
        serde_json::json!({
            "model":"m",
            "input":[
                {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"},
                {"type":"message","role":"user","content":"continue"},
                {"type":"function_call_output","call_id":"call_1","output":"done"}
            ],
            "tools":[{"type":"function","name":"lookup"}]
        }),
        serde_json::json!({
            "model":"m",
            "input":[
                {"type":"custom_tool_call","call_id":"call_1","name":"shell","input":"pwd"},
                {"type":"function_call_output","call_id":"call_1","output":"done"}
            ],
            "tools":[{"type":"custom","name":"shell"}]
        }),
        serde_json::json!({"model":"m","input":"hello","tools":[{"type":"function","name":"lookup","strict":"true"}]}),
        serde_json::json!({"model":"m","input":"hello","tools":[{"type":"function","name":"lookup","parameters":"object"}]}),
        serde_json::json!({"model":"m","input":"hello","tools":[{"type":"function","name":"lookup","description":42}]}),
        serde_json::json!({"model":"m","input":"hello","prompt_cache_key":42}),
        serde_json::json!({"model":"m","input":"hello","client_metadata":"opaque"}),
        serde_json::json!({"model":"m","input":"hello","stream_options":"usage"}),
        serde_json::json!({"model":"m","input":"hello","stream_options":{"include_usage":"yes"}}),
        serde_json::json!({"model":"m","input":"hello","stream_options":{"future":true}}),
    ] {
        let request = ProxyRequest::new(
            WireFormat::OpenAiResponses,
            "m",
            Bytes::from(serde_json::to_vec(&body).expect("request JSON")),
        )
        .expect("proxy request");
        assert!(prepare_chat_request(request).is_err());
    }
}
