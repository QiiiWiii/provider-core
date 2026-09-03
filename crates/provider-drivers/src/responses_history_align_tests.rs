use bytes::Bytes;
use provider_core::{ProviderRequest, RequestMetadata, WireFormat};
use serde_json::{Value, json};

use crate::antigravity::prepare_antigravity_request as prepare_antigravity;
use crate::grok::prepare_grok_request as prepare_grok;

fn request(model: &str, input: Value) -> ProviderRequest {
    ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: model.to_owned(),
        payload: Bytes::from(json!({ "input": input }).to_string()),
        metadata: RequestMetadata::default(),
    }
}

fn grok_result(input: Value) -> Result<(), String> {
    prepare_grok(request("grok-4.6", input))
        .map(|_| ())
        .map_err(|error| error.message().to_owned())
}

fn anti_result(input: Value) -> Result<(), String> {
    prepare_antigravity(&request("gemini-3.7-flash", input), "project-1")
        .map(|_| ())
        .map_err(|error| error.message().to_owned())
}

fn accept_pair(label: &str, input: Value) {
    let grok = grok_result(input.clone());
    let anti = anti_result(input);
    assert!(
        grok.is_ok() && anti.is_ok(),
        "{label}: grok={grok:?} antigravity={anti:?}"
    );
}

fn reject_pair(label: &str, input: Value, needle: &str) {
    let grok = grok_result(input.clone());
    let anti = anti_result(input);
    assert!(
        grok.as_ref().is_err_and(|message| message.contains(needle))
            && anti.as_ref().is_err_and(|message| message.contains(needle)),
        "{label}: grok={grok:?} antigravity={anti:?}"
    );
}

#[test]
fn grok_and_antigravity_accept_the_same_codex_history_items() {
    accept_pair(
        "message",
        json!([{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]),
    );
    accept_pair(
        "reasoning",
        json!([
            {"type":"reasoning","summary":[{"type":"summary_text","text":"think"}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
    accept_pair(
        "function_call",
        json!([
            {"type":"function_call","call_id":"c1","name":"lookup","arguments":"{}"},
            {"type":"function_call_output","call_id":"c1","output":"ok"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
    accept_pair(
        "custom_tool_call",
        json!([
            {"type":"custom_tool_call","call_id":"c1","name":"exec","input":"pwd"},
            {"type":"custom_tool_call_output","call_id":"c1","output":"/tmp"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
    accept_pair(
        "web_search_call",
        json!([
            {"type":"web_search_call","id":"ws_1","status":"completed","action":{"type":"search","query":"tokyo"}},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
    accept_pair(
        "local_shell_call",
        json!([
            {"type":"local_shell_call","call_id":"shell_1","action":{"type":"exec","command":["ls"]}},
            {"type":"local_shell_call_output","call_id":"shell_1","output":"ok"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
    accept_pair(
        "shell_call",
        json!([
            {"type":"shell_call","call_id":"shell_2","action":{"type":"exec","command":["pwd"]}},
            {"type":"shell_call_output","call_id":"shell_2","output":"/tmp"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
    accept_pair(
        "file_search_call",
        json!([
            {"type":"file_search_call","id":"fs_1","queries":["docs"]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
    accept_pair(
        "code_interpreter_call",
        json!([
            {"type":"code_interpreter_call","id":"ci_1","action":{"code":"1+1"}},
            {"type":"code_interpreter_call_output","call_id":"ci_1","output":"2"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
    accept_pair(
        "image_generation_call",
        json!([
            {"type":"image_generation_call","id":"img_1","action":{"prompt":"cat"}},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
    accept_pair(
        "computer_call",
        json!([
            {"type":"computer_call","call_id":"comp_1","action":{"type":"screenshot"}},
            {"type":"computer_call_output","call_id":"comp_1","output":"ok"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
    accept_pair(
        "apply_patch_call",
        json!([
            {"type":"apply_patch_call","call_id":"patch_1","operation":{"type":"update_file","path":"a.txt"}},
            {"type":"apply_patch_call_output","call_id":"patch_1","status":"completed","output":"Done"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
    accept_pair(
        "program",
        json!([
            {"type":"program","call_id":"program_1","code":"return 1","fingerprint":"fp"},
            {"type":"program_output","call_id":"program_1","status":"completed","result":"1"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
    accept_pair(
        "tool_search_call",
        json!([
            {"type":"tool_search_call","call_id":"search_1","arguments":{"query":"git"}},
            {"type":"tool_search_output","call_id":"search_1","tools":[]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
    accept_pair(
        "mcp_tool_call",
        json!([
            {"type":"mcp_tool_call","call_id":"mcp_1","name":"list","arguments":{"x":1}},
            {"type":"mcp_tool_call_output","call_id":"mcp_1","output":"[]"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
    accept_pair(
        "agent_message",
        json!([
            {
                "type":"agent_message",
                "id":"agent_1",
                "author":"/root/reviewer",
                "recipient":"/root",
                "content":[{"type":"input_text","text":"finding"}]
            },
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
    accept_pair(
        "compaction_trigger_dropped",
        json!([
            {"type":"compaction_trigger"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
    accept_pair(
        "compaction_summary_skipped_or_kept",
        json!([
            {"type":"compaction_summary","encrypted_content":"x"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
    accept_pair(
        "namespaced_custom_tool",
        json!([
            {"type":"custom_tool_call","call_id":"c1","name":"exec","namespace":"functions","input":"pwd"},
            {"type":"custom_tool_call_output","call_id":"c1","output":"/tmp"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]),
    );
}

#[test]
fn grok_and_antigravity_reject_the_same_unsafe_history_items() {
    reject_pair(
        "item_reference",
        json!([{"type":"item_reference","id":"msg_1"},{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]),
        "item_reference",
    );
    for item_type in [
        "mcp_call",
        "mcp_list_tools",
        "mcp_approval_request",
        "mcp_approval_response",
        "context_compaction",
    ] {
        reject_pair(
            item_type,
            json!([{ "type": item_type }]),
            "cannot safely replay",
        );
    }
    reject_pair(
        "file_search_call_with_results",
        json!([{"type":"file_search_call","id":"fs_1","queries":["docs"],"results":[{"file_id":"f1"}]}]),
        "results",
    );
    reject_pair(
        "tool_search_call_without_call_id",
        json!([{"type":"tool_search_call","arguments":{"query":"git"}}]),
        "without a call_id",
    );
}

#[test]
fn namespaced_custom_tool_keeps_qualified_name_on_both_drivers() {
    let input = json!([
        {"type":"additional_tools","tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"exec"}]}]},
        {"type":"custom_tool_call","call_id":"c1","name":"exec","namespace":"functions","input":"pwd"},
        {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
    ]);
    let grok = prepare_grok(request("grok-4.6", input.clone())).expect("grok");
    let grok_body: Value = serde_json::from_slice(&grok.payload).expect("grok json");
    assert_eq!(grok_body["input"][0]["name"], "functions__exec");

    let anti = prepare_antigravity(&request("gemini-3.7-flash", input), "project-1").expect("anti");
    let anti_body: Value = serde_json::from_slice(&anti).expect("anti json");
    let texts: Vec<String> = anti_body["request"]["contents"]
        .as_array()
        .expect("contents")
        .iter()
        .flat_map(|content| content["parts"].as_array().cloned().unwrap_or_default())
        .filter_map(|part| part.get("text").and_then(Value::as_str).map(str::to_owned))
        .collect();
    assert!(
        texts.iter().any(|text| text.contains("`functions__exec`")),
        "{texts:?}"
    );
}
