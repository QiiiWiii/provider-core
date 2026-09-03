use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD as BASE64, STANDARD_NO_PAD},
};
use bytes::Bytes;
use provider_core::{ProviderErrorKind, ProviderRequest, RequestMetadata, WireFormat};

use super::*;

#[test]
fn converts_responses_request_to_cpa_cloud_code_envelope() {
    let mut metadata = RequestMetadata::default();
    metadata.session_id = Some("session-1".to_owned());
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3-flash-agent".to_owned(),
        payload: Bytes::from(
            serde_json::json!({
                "model": "client-model",
                "instructions": "Be concise",
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hello"}]
                }],
                "tools": [{
                    "type": "function",
                    "name": "lookup",
                    "parameters": {"type": "object", "properties": {}}
                }],
                "tool_choice": {"type": "function", "name": "lookup"},
                "reasoning": {"effort": "high"},
                "max_output_tokens": 512
            })
            .to_string(),
        ),
        metadata,
    };
    let body = prepare_request(&request, "project-1").expect("request body");
    let value: Value = serde_json::from_slice(&body).expect("JSON body");
    assert_eq!(value["project"], "project-1");
    assert_eq!(value["model"], "gemini-3-flash-agent");
    assert_eq!(value["request"]["sessionId"], "session-1");
    assert_eq!(
        value["request"]["systemInstruction"]["parts"][0]["text"],
        "Be concise"
    );
    assert_eq!(value["request"]["contents"][0]["role"], "user");
    assert_eq!(
        value["request"]["tools"][0]["functionDeclarations"][0]["name"],
        "lookup"
    );
    assert_eq!(
        value["request"]["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"][0],
        "lookup"
    );
    assert_eq!(
        value["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "high"
    );
}

#[test]
fn maps_standalone_web_search_to_native_google_search() {
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3-flash-agent".to_owned(),
        payload: Bytes::from(
            serde_json::json!({
                "input": "search the web",
                "tools": [{"type": "web_search_preview"}]
            })
            .to_string(),
        ),
        metadata: RequestMetadata::default(),
    };

    let body = prepare_request(&request, "project-1").expect("request body");
    let value: Value = serde_json::from_slice(&body).expect("JSON body");
    assert_eq!(value["requestType"], "web_search");
    assert!(value.get("requestId").is_none());
    assert!(value["request"].get("sessionId").is_none());
    assert_eq!(value["request"]["tools"].as_array().map(Vec::len), Some(1));
    assert!(value["request"]["tools"][0].get("googleSearch").is_some());
    assert!(value["request"].get("toolConfig").is_none());
}

#[test]
fn drops_native_google_search_when_function_tools_are_also_present() {
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3-flash-agent".to_owned(),
        payload: Bytes::from(
            serde_json::json!({
                "input": "search and call a function",
                "tools": [
                    {
                        "type": "function",
                        "name": "lookup",
                        "parameters": {"type": "object", "properties": {}}
                    },
                    {"type": "web_search_preview"}
                ]
            })
            .to_string(),
        ),
        metadata: RequestMetadata::default(),
    };

    let body = prepare_request(&request, "project-1").expect("request body");
    let value: Value = serde_json::from_slice(&body).expect("JSON body");
    assert_eq!(value["requestType"], "agent");
    assert!(value.get("requestId").is_some());
    assert!(value["request"].get("sessionId").is_some());
    assert_eq!(value["request"]["tools"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        value["request"]["tools"][0]["functionDeclarations"][0]["name"],
        "lookup"
    );
    assert!(value["request"]["tools"][0].get("googleSearch").is_none());
    assert!(
        value["request"]
            .get("toolConfig")
            .and_then(|v| v.get("includeServerSideToolInvocations"))
            .is_none()
    );
}

#[test]
fn rejects_incremental_previous_response_without_complete_history() {
    let mut metadata = RequestMetadata::default();
    metadata.previous_response_id = Some("resp-1".to_owned());
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3-flash".to_owned(),
        payload: Bytes::from_static(br#"{"input":"continue"}"#),
        metadata,
    };
    let error = prepare_request(&request, "project-1").expect_err("history is required");
    assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
    assert!(error.message().contains("complete input history"));
}

#[test]
fn accepts_previous_response_with_complete_history() {
    let mut metadata = RequestMetadata::default();
    metadata.previous_response_id = Some("resp-1".to_owned());
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3-flash".to_owned(),
        payload: Bytes::from(
            serde_json::json!({
                "previous_response_id": "resp-1",
                "input": [
                    {"type":"message","role":"user","content":"first"},
                    {"type":"message","role":"assistant","content":"answer"},
                    {"type":"message","role":"user","content":"continue"}
                ]
            })
            .to_string(),
        ),
        metadata,
    };
    prepare_request(&request, "project-1").expect("complete history");
}

#[test]
fn adds_cpa_validated_placeholders_only_to_claude_tool_schemas() {
    let request = |model: &str| {
        ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: model.to_owned(),
            payload: Bytes::from_static(
                br#"{"tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{"flag":{"type":"string"},"nested":{"type":"object"}}}}],"input":"hello"}"#,
            ),
            metadata: RequestMetadata::default(),
        }
    };

    let claude: Value = serde_json::from_slice(
        &prepare_request(&request("claude-sonnet-4-6"), "project-1").expect("Claude body"),
    )
    .expect("Claude JSON");
    let claude_schema = &claude["request"]["tools"][0]["functionDeclarations"][0]["parameters"];
    assert_eq!(
        claude_schema["required"],
        json!(["_"]),
        "schema={claude_schema}"
    );
    assert_eq!(claude_schema["properties"]["_"]["type"], "boolean");
    assert_eq!(claude_schema["properties"]["flag"]["type"], "string");
    assert_eq!(claude_schema["properties"]["nested"]["type"], "object");
    assert_eq!(
        claude_schema["properties"]["nested"]["properties"]["reason"]["type"],
        "string"
    );
    assert_eq!(
        claude_schema["properties"]["nested"]["required"],
        json!(["reason"])
    );

    let empty: Value = serde_json::from_slice(
            &prepare_request(
                &ProviderRequest {
                    format: WireFormat::OpenAiResponses,
                    model: "claude-sonnet-4-6".to_owned(),
                    payload: Bytes::from_static(
                        br#"{"tools":[{"type":"function","name":"empty","parameters":{"type":"object","properties":{}}}],"input":"hello"}"#,
                    ),
                    metadata: RequestMetadata::default(),
                },
                "project-1",
            )
            .expect("empty Claude body"),
        )
        .expect("empty Claude JSON");
    let empty_schema = &empty["request"]["tools"][0]["functionDeclarations"][0]["parameters"];
    assert_eq!(empty_schema["required"], json!(["reason"]));
    assert_eq!(empty_schema["properties"]["reason"]["type"], "string");

    let gemini: Value = serde_json::from_slice(
        &prepare_request(&request("gemini-3-flash"), "project-1").expect("Gemini body"),
    )
    .expect("Gemini JSON");
    let gemini_schema = &gemini["request"]["tools"][0]["functionDeclarations"][0]["parameters"];
    assert!(gemini_schema.get("required").is_none());
    assert!(gemini_schema["properties"].get("_").is_none());
    assert!(
        gemini_schema["properties"]["nested"]
            .get("required")
            .is_none()
    );
}

#[test]
fn prepares_count_tokens_body_without_stream_envelope_fields() {
    let mut metadata = RequestMetadata::default();
    metadata.session_id = Some("session-1".to_owned());
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3-flash".to_owned(),
        payload: Bytes::from_static(br#"{"input":"hello"}"#),
        metadata,
    };
    let body: Value =
        serde_json::from_slice(&prepare_count_tokens_request(&request).expect("count body"))
            .expect("count JSON");
    assert!(body.get("project").is_none());
    assert!(body.get("model").is_none());
    assert!(body.get("requestType").is_none());
    assert!(body.get("requestId").is_none());
    assert!(body["request"].get("sessionId").is_none());
    assert_eq!(body["request"]["contents"][0]["role"], "user");
}

#[test]
fn maps_function_output_to_cpa_result_field() {
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3-flash".to_owned(),
        payload: Bytes::from(
            serde_json::json!({
                "input": [
                    {
                        "type": "reasoning",
                        "encrypted_content": format!(
                            "cpa-gemini-responses-carrier-v1:next:function:{}",
                            STANDARD_NO_PAD.encode(b"native-signature")
                        ),
                        "summary": []
                    },
                    {"type":"function_call","call_id":"call-1","name":"lookup","arguments":"{}"},
                    {"type":"function_call_output","call_id":"call-1","output":"done"}
                ]
            })
            .to_string(),
        ),
        metadata: RequestMetadata::default(),
    };
    let body = prepare_request(&request, "project-1").expect("request body");
    let value: Value = serde_json::from_slice(&body).expect("JSON body");
    assert_eq!(
        value["request"]["contents"][2]["parts"][0]["functionResponse"]["response"]["result"],
        "done"
    );
}

#[test]
fn uses_cpa_image_envelope_and_stable_session_shape() {
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3.1-flash-image".to_owned(),
        payload: Bytes::from_static(br#"{"input":"draw a cat"}"#),
        metadata: RequestMetadata::default(),
    };
    let body = prepare_request(&request, "project-1").expect("request body");
    let value: Value = serde_json::from_slice(&body).expect("JSON body");
    assert_eq!(value["requestType"], "image_gen");
    assert!(
        value["requestId"]
            .as_str()
            .is_some_and(|value| value.starts_with("image_gen/") && value.ends_with("/12"))
    );
    assert!(
        value["request"]["sessionId"]
            .as_str()
            .is_some_and(|value| value.starts_with('-'))
    );
}

#[test]
fn converts_media_structured_output_and_additional_namespace_tools() {
    let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "gemini-3-flash".to_owned(),
            payload: Bytes::from(
                serde_json::json!({
                    "text": {"format": {"type": "json_schema", "schema": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"answer": {"type": "string", "format": "uri"}},
                        "required": ["answer"]
                    }}},
                    "input": [
                        {"type": "message", "role": "user", "content": [
                            {"type": "input_audio", "data": "UklGRg==", "format": "wav"},
                            {"type": "input_image", "image_url": "data:image/png;base64,AA=="}
                        ]},
                        {"type": "additional_tools", "tools": [{
                            "type": "namespace", "name": "functions",
                            "tools": [{"type": "custom", "name": "exec"}]
                        }]},
                        {
                            "type": "reasoning",
                            "encrypted_content": format!(
                                "cpa-gemini-responses-carrier-v1:next:function:{}",
                                STANDARD_NO_PAD.encode(b"native-signature")
                            ),
                            "summary": []
                        },
                        {"type": "custom_tool_call", "call_id": "call-1", "name": "exec", "namespace": "functions", "input": "pwd"}
                    ]
                })
                .to_string(),
            ),
            metadata: RequestMetadata::default(),
        };
    let body = prepare_request(&request, "project-1").expect("request body");
    let value: Value = serde_json::from_slice(&body).expect("JSON body");
    let parts = value["request"]["contents"][0]["parts"]
        .as_array()
        .expect("content parts");
    assert_eq!(parts[0]["inlineData"]["mimeType"], "audio/wav");
    assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
    assert_eq!(
        value["request"]["contents"][1]["parts"][0]["functionCall"]["name"],
        "functions__exec"
    );
    assert_eq!(
        value["request"]["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert_eq!(
        value["request"]["generationConfig"]["responseJsonSchema"]["additionalProperties"],
        false
    );
    assert!(
        value["request"]["generationConfig"]["responseJsonSchema"]["properties"]["answer"]
            .get("format")
            .is_none()
    );
}

#[test]
fn decodes_cpa_reasoning_carrier_for_following_tool_call() {
    let signature = "native-signature";
    let carrier = format!(
        "cpa-gemini-responses-carrier-v1:next:function:{}",
        STANDARD_NO_PAD.encode(signature.as_bytes())
    );
    let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "gemini-3-flash".to_owned(),
            payload: Bytes::from(
                serde_json::json!({
                    "input": [
                        {"type": "reasoning", "encrypted_content": carrier, "summary": []},
                        {"type": "function_call", "call_id": "call-1", "name": "lookup", "arguments": "{}"}
                    ]
                })
                .to_string(),
            ),
            metadata: RequestMetadata::default(),
        };
    let body = prepare_request(&request, "project-1").expect("request body");
    let value: Value = serde_json::from_slice(&body).expect("JSON body");
    assert_eq!(
        value["request"]["contents"][1]["parts"][0]["thoughtSignature"],
        signature
    );
}

#[test]
fn normalizes_claude_thinking_signature_to_antigravity_r_form() {
    let single_layer = BASE64.encode([0x12_u8, 0x01, 0x02]);
    let expected = BASE64.encode(single_layer.as_bytes());
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "claude-sonnet-4-6".to_owned(),
        payload: Bytes::from(
            serde_json::json!({
                "input": [{
                    "type": "reasoning",
                    "encrypted_content": single_layer,
                    "summary": [{"type": "summary_text", "text": "think"}]
                }]
            })
            .to_string(),
        ),
        metadata: RequestMetadata::default(),
    };
    let body = prepare_request(&request, "project-1").expect("request body");
    let value: Value = serde_json::from_slice(&body).expect("JSON body");
    assert_eq!(
        value["request"]["contents"][0]["parts"][0]["thoughtSignature"],
        expected
    );
}

#[test]
fn drops_cross_provider_claude_thinking_signature() {
    let request = ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "claude-sonnet-4-6".to_owned(),
            payload: Bytes::from_static(
                br#"{"input":[{"type":"reasoning","encrypted_content":"gpt#not-claude","summary":[{"type":"summary_text","text":"drop"}]}]}"#,
            ),
            metadata: RequestMetadata::default(),
        };
    let body = prepare_request(&request, "project-1").expect("request body");
    let value: Value = serde_json::from_slice(&body).expect("JSON body");
    assert!(
        value["request"]["contents"]
            .as_array()
            .is_none_or(|contents| contents.iter().all(|content| {
                content["parts"].as_array().is_none_or(|parts| {
                    parts
                        .iter()
                        .all(|part| !part["thought"].as_bool().unwrap_or(false))
                })
            }))
    );
}

#[test]
fn converts_web_search_call_history_into_function_call() {
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3.7-flash".to_owned(),
        payload: Bytes::from_static(
            br#"{
                "input":[
                    {
                        "type":"web_search_call",
                        "id":"ws_1",
                        "status":"completed",
                        "action":{"type":"search","query":"weather tokyo"}
                    },
                    {"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}
                ]
            }"#,
        ),
        metadata: RequestMetadata::default(),
    };
    let body = prepare_request(&request, "project-1").expect("web_search_call history");
    let value: Value = serde_json::from_slice(&body).expect("JSON body");
    let texts: Vec<String> = value["request"]["contents"]
        .as_array()
        .expect("contents")
        .iter()
        .flat_map(|content| content["parts"].as_array().cloned().unwrap_or_default())
        .filter_map(|part| part.get("text").and_then(Value::as_str).map(str::to_owned))
        .collect();
    assert!(
        texts.iter().any(|text| text.contains("`web_search`")),
        "{texts:?}"
    );
    assert!(
        value["request"]["contents"]
            .as_array()
            .expect("contents")
            .iter()
            .flat_map(|content| content["parts"].as_array().cloned().unwrap_or_default())
            .all(|part| part.get("functionCall").is_none())
    );
}

#[test]
fn converts_local_shell_history_into_function_items() {
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3.7-flash".to_owned(),
        payload: Bytes::from_static(
            br#"{
                "input":[
                    {"type":"local_shell_call","call_id":"shell_1","status":"completed","action":{"type":"exec","command":["ls"]}},
                    {"type":"local_shell_call_output","call_id":"shell_1","output":"ok"},
                    {"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}
                ]
            }"#,
        ),
        metadata: RequestMetadata::default(),
    };
    let body = prepare_request(&request, "project-1").expect("local_shell history");
    let value: Value = serde_json::from_slice(&body).expect("JSON body");
    let texts: Vec<String> = value["request"]["contents"]
        .as_array()
        .expect("contents")
        .iter()
        .flat_map(|content| content["parts"].as_array().cloned().unwrap_or_default())
        .filter_map(|part| part.get("text").and_then(Value::as_str).map(str::to_owned))
        .collect();
    assert!(
        texts.iter().any(|text| text.contains("`local_shell`")),
        "{texts:?}"
    );
    assert!(texts.iter().any(|text| text.contains("ok")), "{texts:?}");
    assert!(
        value["request"]["contents"]
            .as_array()
            .expect("contents")
            .iter()
            .flat_map(|content| content["parts"].as_array().cloned().unwrap_or_default())
            .all(
                |part| part.get("functionCall").is_none() && part.get("functionResponse").is_none()
            )
    );
}

#[test]
fn rejects_item_reference_instead_of_silently_dropping_context() {
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3.7-flash".to_owned(),
        payload: Bytes::from_static(
            br#"{"input":[{"type":"item_reference","id":"msg_1"},{"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}]}"#,
        ),
        metadata: RequestMetadata::default(),
    };
    let error =
        prepare_request(&request, "project-1").expect_err("item_reference must be rejected");
    assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
    assert!(error.message().contains("item_reference"));
    assert!(error.message().contains("complete input history"));
}

#[test]
fn strips_encrypted_from_function_declaration_schemas() {
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3.7-flash".to_owned(),
        payload: Bytes::from(
            serde_json::json!({
                "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
                "tools": [{
                    "type": "namespace",
                    "name": "collaboration",
                    "tools": [
                        {"type":"function","name":"spawn_agent","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
                        {"type":"function","name":"followup_task","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},
                        {"type":"function","name":"send_input","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true,"contentEncoding":"base64","example":"x","deprecated":true,"x-codex":true},"data":{"type":"string"}}}}
                    ]
                }]
            })
            .to_string(),
        ),
        metadata: RequestMetadata::default(),
    };
    let body = prepare_request(&request, "project-1").expect("request body");
    let value: Value = serde_json::from_slice(&body).expect("JSON body");
    let declarations = value["request"]["tools"][0]["functionDeclarations"]
        .as_array()
        .expect("functionDeclarations");
    assert!(declarations.len() >= 3);
    for declaration in declarations {
        let properties = declaration["parameters"]["properties"]
            .as_object()
            .expect("properties");
        for schema in properties.values() {
            if let Some(object) = schema.as_object() {
                for key in object.keys() {
                    assert!(
                        matches!(
                            key.as_str(),
                            "type"
                                | "description"
                                | "nullable"
                                | "properties"
                                | "items"
                                | "required"
                        ),
                        "Cloud Code Schema leftover field `{key}` in {}",
                        declaration["name"]
                    );
                }
            }
        }
    }
}

#[test]
fn drops_grok_encrypted_content_thought_signature_for_gemini() {
    let grok_like = STANDARD_NO_PAD.encode([0x3a_u8; 40]);
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3.7-flash".to_owned(),
        payload: Bytes::from(
            serde_json::json!({
                "input": [
                    {
                        "type": "reasoning",
                        "encrypted_content": grok_like,
                        "summary": [{"type": "summary_text", "text": "think"}]
                    },
                    {"type": "function_call", "call_id": "call-1", "name": "lookup", "arguments": "{}"}
                ]
            })
            .to_string(),
        ),
        metadata: RequestMetadata::default(),
    };
    let body = prepare_request(&request, "project-1").expect("request body");
    let value: Value = serde_json::from_slice(&body).expect("JSON body");
    let parts: Vec<Value> = value["request"]["contents"]
        .as_array()
        .expect("contents")
        .iter()
        .flat_map(|content| content["parts"].as_array().cloned().unwrap_or_default())
        .collect();
    assert!(parts.iter().all(|part| part.get("thoughtSignature").is_none()));
    assert!(parts.iter().all(|part| part.get("functionCall").is_none()));
    assert!(parts.iter().any(|part| {
        part["text"]
            .as_str()
            .is_some_and(|text| text.contains("`lookup`"))
    }));
}

#[test]
fn does_not_attach_thought_signature_to_hosted_history_function_calls() {
    let signature = "native-signature";
    let carrier = format!(
        "cpa-gemini-responses-carrier-v1:next:function:{}",
        STANDARD_NO_PAD.encode(signature.as_bytes())
    );
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3.7-flash".to_owned(),
        payload: Bytes::from(
            serde_json::json!({
                "input": [
                    {"type": "reasoning", "encrypted_content": carrier, "summary": []},
                    {
                        "type": "web_search_call",
                        "id": "ws_1",
                        "status": "completed",
                        "action": {"type": "search", "query": "tokyo"}
                    },
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}]}
                ]
            })
            .to_string(),
        ),
        metadata: RequestMetadata::default(),
    };
    let body = prepare_request(&request, "project-1").expect("request body");
    let value: Value = serde_json::from_slice(&body).expect("JSON body");
    assert!(
        value["request"]["contents"]
            .as_array()
            .expect("contents")
            .iter()
            .flat_map(|content| content["parts"].as_array().cloned().unwrap_or_default())
            .all(|part| part.get("functionCall").is_none())
    );
}

#[test]
fn flattens_unsigned_exec_history_instead_of_function_call() {
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3.7-flash".to_owned(),
        payload: Bytes::from(
            serde_json::json!({
                "input": [
                    {"type":"function_call","call_id":"exec-1","name":"exec","arguments":"{\"input\":\"pwd\"}"},
                    {"type":"function_call_output","call_id":"exec-1","output":"/tmp"},
                    {"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}
                ]
            })
            .to_string(),
        ),
        metadata: RequestMetadata::default(),
    };
    let body = prepare_request(&request, "project-1").expect("request body");
    let value: Value = serde_json::from_slice(&body).expect("JSON body");
    let parts: Vec<Value> = value["request"]["contents"]
        .as_array()
        .expect("contents")
        .iter()
        .flat_map(|content| content["parts"].as_array().cloned().unwrap_or_default())
        .collect();
    assert!(parts.iter().all(|part| part.get("functionCall").is_none()));
    assert!(
        parts
            .iter()
            .all(|part| part.get("functionResponse").is_none())
    );
    assert!(parts.iter().any(|part| {
        part["text"]
            .as_str()
            .is_some_and(|text| text.contains("`exec`"))
    }));
}

#[test]
fn keeps_signed_exec_function_call() {
    let signature = "native-signature";
    let carrier = format!(
        "cpa-gemini-responses-carrier-v1:next:function:{}",
        STANDARD_NO_PAD.encode(signature.as_bytes())
    );
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3.7-flash".to_owned(),
        payload: Bytes::from(
            serde_json::json!({
                "input": [
                    {"type":"reasoning","encrypted_content": carrier, "summary": []},
                    {"type":"function_call","call_id":"exec-1","name":"exec","arguments":"{\"input\":\"pwd\"}"},
                    {"type":"function_call_output","call_id":"exec-1","output":"/tmp"}
                ]
            })
            .to_string(),
        ),
        metadata: RequestMetadata::default(),
    };
    let body = prepare_request(&request, "project-1").expect("request body");
    let value: Value = serde_json::from_slice(&body).expect("JSON body");
    let call_part = value["request"]["contents"]
        .as_array()
        .expect("contents")
        .iter()
        .find_map(|content| {
            content["parts"]
                .as_array()?
                .iter()
                .find(|part| part.get("functionCall").is_some())
                .cloned()
        })
        .expect("signed functionCall");
    assert_eq!(call_part["functionCall"]["name"], "exec");
    assert_eq!(call_part["thoughtSignature"], signature);
}
