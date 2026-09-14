use std::collections::BTreeMap;

use bytes::Bytes;
use provider_core::{ProviderStream, ResponseTranslator};
use serde_json::Value;

use super::{ToolTarget, ToolTargets};
use stream::adapt_chat_stream;

mod stream;

#[derive(Debug)]
pub(crate) struct ResponsesResponseTranslator {
    context: ResponsesResponseContext,
}

#[derive(Debug)]
struct ResponsesResponseContext {
    model: String,
    targets: ToolTargets,
}

impl ResponsesResponseTranslator {
    pub(crate) fn new(model: String, targets: ToolTargets) -> Self {
        Self {
            context: ResponsesResponseContext { model, targets },
        }
    }
}

impl ResponseTranslator for ResponsesResponseTranslator {
    fn translate_stream(self: Box<Self>, stream: ProviderStream) -> ProviderStream {
        adapt_chat_stream(stream, self.context)
    }
}

struct ChatEventConverter {
    id: String,
    model: String,
    created_at: u64,
    targets: ToolTargets,
    sequence: u64,
    next_output_index: u64,
    created: bool,
    terminal: bool,
    text: Option<TextOutput>,
    reasoning: Option<ReasoningOutput>,
    tools: BTreeMap<u64, ToolOutput>,
    output: BTreeMap<u64, Value>,
    usage: Option<Value>,
    finish_reason: Option<String>,
    failure: Option<String>,
}

struct TextOutput {
    index: u64,
    item_id: String,
    text: String,
}

struct ReasoningOutput {
    index: u64,
    item_id: String,
    text: String,
}

struct ToolOutput {
    output_index: u64,
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    target: ToolTarget,
    started: bool,
}

impl ChatEventConverter {
    fn new(context: ResponsesResponseContext) -> Self {
        Self {
            id: String::new(),
            model: context.model,
            created_at: 0,
            targets: context.targets,
            sequence: 0,
            next_output_index: 0,
            created: false,
            terminal: false,
            text: None,
            reasoning: None,
            tools: BTreeMap::new(),
            output: BTreeMap::new(),
            usage: None,
            finish_reason: None,
            failure: None,
        }
    }

    fn convert(&mut self, event: &Value) -> Vec<Bytes> {
        let mut events = Vec::new();
        if let Some(error) = event.get("error").filter(|value| !value.is_null()) {
            self.emit_created(&mut events);
            events.push(self.event("error", response_error(error)));
            self.terminal = true;
            return events;
        }
        if let Err(message) = self.capture_envelope(event) {
            self.emit_created(&mut events);
            events.extend(self.fail(message));
            return events;
        }
        if let Some(usage) = event.get("usage").filter(|value| value.is_object()) {
            self.usage = Some(responses_usage(usage));
        }
        let Some(choices) = event.get("choices").and_then(Value::as_array) else {
            return events;
        };
        for choice in choices {
            if choice.get("index").and_then(Value::as_u64).unwrap_or(0) != 0 {
                continue;
            }
            self.emit_created(&mut events);
            if let Some(delta) = choice.get("delta").and_then(Value::as_object) {
                if let Some(reasoning) = delta
                    .get("reasoning_content")
                    .or_else(|| delta.get("reasoning"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    self.emit_reasoning(reasoning, &mut events);
                }
                if let Some(content) = delta
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    self.emit_text(content, &mut events);
                }
                if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for tool_call in tool_calls {
                        self.emit_tool_delta(tool_call, &mut events);
                    }
                }
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(reason.to_owned());
            }
        }
        if let Some(message) = self.failure.take() {
            events.extend(self.fail(&message));
        }
        events
    }

    fn capture_envelope(&mut self, event: &Value) -> Result<(), &'static str> {
        if let Some(id) = event.get("id").filter(|value| !value.is_null()) {
            let id = id
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or("Chat Completions response id must be non-empty text")?;
            let id = if id.starts_with("resp_") {
                id.to_owned()
            } else {
                format!("resp_{id}")
            };
            if self.created && id != self.response_id() {
                return Err("Chat Completions upstream changed response id within one stream");
            }
            self.id = id;
        }
        if let Some(model) = event.get("model").filter(|value| !value.is_null()) {
            let model = model
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or("Chat Completions response model must be non-empty text")?;
            if self.created && model != self.model {
                return Err("Chat Completions upstream changed response model within one stream");
            }
            self.model = model.to_owned();
        }
        if let Some(created) = event.get("created").filter(|value| !value.is_null()) {
            let created = created
                .as_u64()
                .ok_or("Chat Completions response created must be a non-negative integer")?;
            if self.created && created != self.created_at {
                return Err(
                    "Chat Completions upstream changed response creation time within one stream",
                );
            }
            self.created_at = created;
        }
        Ok(())
    }

    fn emit_created(&mut self, events: &mut Vec<Bytes>) {
        if self.created {
            return;
        }
        self.created = true;
        events.push(self.event("response.created", serde_json::json!({
            "response":self.response("in_progress", Value::Array(Vec::new()), Value::Null, Value::Null)
        })));
    }

    fn emit_text(&mut self, delta: &str, events: &mut Vec<Bytes>) {
        if self.text.is_none() {
            let index = self.take_output_index();
            let item_id = format!("msg_{}", self.id_suffix());
            self.text = Some(TextOutput {
                index,
                item_id: item_id.clone(),
                text: String::new(),
            });
            events.push(self.event("response.output_item.added", serde_json::json!({
                "output_index":index,
                "item":{"id":item_id,"type":"message","status":"in_progress","role":"assistant","content":[]}
            })));
            events.push(self.event(
                "response.content_part.added",
                serde_json::json!({
                    "item_id":item_id,"output_index":index,"content_index":0,
                    "part":{"type":"output_text","text":"","annotations":[]}
                }),
            ));
        }
        let text = self.text.as_mut().expect("text output exists");
        text.text.push_str(delta);
        let item_id = text.item_id.clone();
        let index = text.index;
        events.push(self.event(
            "response.output_text.delta",
            serde_json::json!({
                "item_id":item_id,"output_index":index,"content_index":0,"delta":delta
            }),
        ));
    }

    fn emit_reasoning(&mut self, delta: &str, events: &mut Vec<Bytes>) {
        if self.reasoning.is_none() {
            let index = self.take_output_index();
            let item_id = format!("rs_{}", self.id_suffix());
            self.reasoning = Some(ReasoningOutput {
                index,
                item_id: item_id.clone(),
                text: String::new(),
            });
            events.push(self.event(
                "response.output_item.added",
                serde_json::json!({
                    "output_index":index,
                    "item":{"id":item_id,"type":"reasoning","status":"in_progress","summary":[]}
                }),
            ));
            events.push(self.event(
                "response.reasoning_summary_part.added",
                serde_json::json!({
                    "item_id":item_id,"output_index":index,"summary_index":0,
                    "part":{"type":"summary_text","text":""}
                }),
            ));
        }
        let reasoning = self.reasoning.as_mut().expect("reasoning output exists");
        reasoning.text.push_str(delta);
        let item_id = reasoning.item_id.clone();
        let index = reasoning.index;
        events.push(self.event(
            "response.reasoning_summary_text.delta",
            serde_json::json!({
                "item_id":item_id,"output_index":index,"summary_index":0,"delta":delta
            }),
        ));
    }

    fn emit_tool_delta(&mut self, tool_call: &Value, events: &mut Vec<Bytes>) {
        let index = tool_call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let function = tool_call.get("function").unwrap_or(&Value::Null);
        let incoming_name = function
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !incoming_name.is_empty() && !self.targets.contains_key(incoming_name) {
            self.failure = Some(format!(
                "Chat Completions upstream called undeclared tool {incoming_name}"
            ));
            return;
        }
        if !self.tools.contains_key(&index) {
            let call_id = tool_call
                .get("id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .unwrap_or_default();
            let name = incoming_name.to_owned();
            let target = self
                .targets
                .get(&name)
                .cloned()
                .unwrap_or(ToolTarget::Function);
            let output_index = self.take_output_index();
            let item_id = format!("fc_{}_{index}", self.id_suffix());
            let tool = ToolOutput {
                output_index,
                item_id: item_id.clone(),
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: String::new(),
                target,
                started: false,
            };
            self.tools.insert(index, tool);
        }
        let tool = self.tools.get_mut(&index).expect("tool output exists");
        if let Some(call_id) = tool_call
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            if !tool.call_id.is_empty() && tool.call_id != call_id {
                self.failure = Some(
                    "Chat Completions upstream changed a tool call id within one index".to_owned(),
                );
                return;
            }
            tool.call_id = call_id.to_owned();
        }
        if !incoming_name.is_empty() {
            if !tool.name.is_empty() && tool.name != incoming_name {
                self.failure = Some(
                    "Chat Completions upstream changed a tool name within one index".to_owned(),
                );
                return;
            }
            tool.name = incoming_name.to_owned();
            tool.target = self
                .targets
                .get(incoming_name)
                .cloned()
                .unwrap_or(ToolTarget::Function);
        }
        let arguments = function
            .get("arguments")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or_default();
        if !arguments.is_empty() {
            tool.arguments.push_str(arguments);
        }
        let ready = !tool.call_id.is_empty() && !tool.name.is_empty();
        let started = tool.started;
        if ready && !started {
            tool.started = true;
            let (output_index, item, argument_event) = {
                let tool = self.tools.get(&index).expect("tool output exists");
                let argument_event = if function_target(&tool.target) && !tool.arguments.is_empty()
                {
                    Some(serde_json::json!({
                        "item_id":tool.item_id,"output_index":tool.output_index,"delta":tool.arguments
                    }))
                } else {
                    None
                };
                (
                    tool.output_index,
                    tool_item(tool, "in_progress", false),
                    argument_event,
                )
            };
            events.push(self.event(
                "response.output_item.added",
                serde_json::json!({"output_index":output_index,"item":item}),
            ));
            if let Some(argument_event) = argument_event {
                events.push(self.event("response.function_call_arguments.delta", argument_event));
            }
        } else if started && function_target(&tool.target) && !arguments.is_empty() {
            let item_id = tool.item_id.clone();
            let output_index = tool.output_index;
            events.push(self.event(
                "response.function_call_arguments.delta",
                serde_json::json!({
                    "item_id":item_id,"output_index":output_index,"delta":arguments
                }),
            ));
        }
    }

    fn complete(&mut self) -> Vec<Bytes> {
        if self.terminal {
            return Vec::new();
        }
        let mut events = Vec::new();
        self.emit_created(&mut events);
        if self.tools.values().any(|tool| !tool.started) {
            events.extend(
                self.fail("Chat Completions upstream ended a tool call without both id and name"),
            );
            return events;
        }
        let finish_reason = self.finish_reason.as_deref().unwrap_or("stop");
        let incomplete_reason = match finish_reason {
            "length" => Some("max_output_tokens"),
            "content_filter" => Some("content_filter"),
            "stop" | "tool_calls" | "function_call" => None,
            reason => {
                events.extend(self.fail(&format!(
                    "Chat Completions upstream returned unsupported finish_reason {reason}"
                )));
                return events;
            }
        };
        self.finish_reasoning(&mut events);
        self.finish_text(&mut events);
        self.finish_tools(&mut events);
        let status = if incomplete_reason.is_some() {
            "incomplete"
        } else {
            "completed"
        };
        let event_type = format!("response.{status}");
        let details =
            incomplete_reason.map_or(Value::Null, |reason| serde_json::json!({"reason":reason}));
        let output = Value::Array(self.output.values().cloned().collect());
        let usage = self.usage.clone().unwrap_or(Value::Null);
        events.push(self.event(
            &event_type,
            serde_json::json!({
                "response":self.response(status, output, usage, details)
            }),
        ));
        self.terminal = true;
        events
    }

    fn fail(&mut self, message: &str) -> Vec<Bytes> {
        if self.terminal {
            return Vec::new();
        }
        let response = serde_json::json!({
            "id":self.response_id(),"object":"response","created_at":self.created_at,
            "status":"failed","model":self.model,"output":[],"usage":self.usage,
            "error":{"code":"upstream_protocol_error","message":message,"param":null},
            "incomplete_details":null
        });
        let event = self.event("response.failed", serde_json::json!({"response":response}));
        self.terminal = true;
        vec![event]
    }

    fn finish_reasoning(&mut self, events: &mut Vec<Bytes>) {
        let Some(reasoning) = self.reasoning.take() else {
            return;
        };
        events.push(self.event("response.reasoning_summary_text.done", serde_json::json!({
            "item_id":reasoning.item_id,"output_index":reasoning.index,"summary_index":0,"text":reasoning.text
        })));
        events.push(self.event(
            "response.reasoning_summary_part.done",
            serde_json::json!({
                "item_id":reasoning.item_id,"output_index":reasoning.index,"summary_index":0,
                "part":{"type":"summary_text","text":reasoning.text}
            }),
        ));
        let item = serde_json::json!({
            "id":reasoning.item_id,"type":"reasoning","status":"completed",
            "summary":[{"type":"summary_text","text":reasoning.text}]
        });
        events.push(self.event(
            "response.output_item.done",
            serde_json::json!({"output_index":reasoning.index,"item":item}),
        ));
        self.output.insert(reasoning.index, item);
    }

    fn finish_text(&mut self, events: &mut Vec<Bytes>) {
        let Some(text) = self.text.take() else { return };
        events.push(self.event(
            "response.output_text.done",
            serde_json::json!({
                "item_id":text.item_id,"output_index":text.index,"content_index":0,"text":text.text
            }),
        ));
        let part = serde_json::json!({"type":"output_text","text":text.text,"annotations":[]});
        events.push(self.event(
            "response.content_part.done",
            serde_json::json!({
                "item_id":text.item_id,"output_index":text.index,"content_index":0,"part":part
            }),
        ));
        let item = serde_json::json!({
            "id":text.item_id,"type":"message","status":"completed","role":"assistant","content":[part]
        });
        events.push(self.event(
            "response.output_item.done",
            serde_json::json!({"output_index":text.index,"item":item}),
        ));
        self.output.insert(text.index, item);
    }

    fn finish_tools(&mut self, events: &mut Vec<Bytes>) {
        for (_, tool) in std::mem::take(&mut self.tools) {
            let custom = matches!(
                tool.target,
                ToolTarget::Custom | ToolTarget::Namespace { custom: true, .. }
            );
            if custom {
                events.push(self.event(
                    "response.custom_tool_call_input.delta",
                    serde_json::json!({
                        "item_id":tool.item_id,"output_index":tool.output_index,
                        "delta":custom_input(&tool.arguments)
                    }),
                ));
            }
            let event_type = if custom {
                "response.custom_tool_call_input.done"
            } else {
                "response.function_call_arguments.done"
            };
            let payload = if custom {
                serde_json::json!({
                    "item_id":tool.item_id,"output_index":tool.output_index,"input":custom_input(&tool.arguments)
                })
            } else {
                serde_json::json!({
                    "item_id":tool.item_id,"output_index":tool.output_index,"arguments":tool.arguments
                })
            };
            events.push(self.event(event_type, payload));
            let item = tool_item(&tool, "completed", true);
            events.push(self.event(
                "response.output_item.done",
                serde_json::json!({"output_index":tool.output_index,"item":item}),
            ));
            self.output.insert(tool.output_index, item);
        }
    }

    fn response(
        &self,
        status: &str,
        output: Value,
        usage: Value,
        incomplete_details: Value,
    ) -> Value {
        serde_json::json!({
            "id":self.response_id(),"object":"response","created_at":self.created_at,
            "status":status,"model":self.model,"output":output,"usage":usage,
            "error":null,"incomplete_details":incomplete_details
        })
    }

    fn event(&mut self, event_type: &str, fields: Value) -> Bytes {
        let mut event = fields.as_object().cloned().unwrap_or_default();
        event.insert("type".to_owned(), Value::String(event_type.to_owned()));
        event.insert(
            "sequence_number".to_owned(),
            Value::Number(self.sequence.into()),
        );
        self.sequence += 1;
        let value = Value::Object(event);
        Bytes::from(format!("event: {event_type}\ndata: {value}\n\n"))
    }

    fn take_output_index(&mut self) -> u64 {
        let index = self.next_output_index;
        self.next_output_index += 1;
        index
    }

    fn response_id(&self) -> &str {
        if self.id.is_empty() {
            "resp_provider"
        } else {
            &self.id
        }
    }

    fn id_suffix(&self) -> String {
        self.response_id()
            .trim_start_matches("resp_")
            .replace(|character: char| !character.is_ascii_alphanumeric(), "_")
    }
}

fn function_target(target: &ToolTarget) -> bool {
    matches!(
        target,
        ToolTarget::Function | ToolTarget::Namespace { custom: false, .. }
    )
}

fn tool_item(tool: &ToolOutput, status: &str, completed: bool) -> Value {
    match &tool.target {
        ToolTarget::Custom => serde_json::json!({
            "id":tool.item_id,"type":"custom_tool_call","status":status,
            "call_id":tool.call_id,"name":tool.name,"input":if completed { custom_input(&tool.arguments) } else { String::new() }
        }),
        ToolTarget::Namespace {
            namespace,
            name,
            custom,
        } if *custom => serde_json::json!({
            "id":tool.item_id,"type":"custom_tool_call","status":status,
            "call_id":tool.call_id,"namespace":namespace,"name":name,
            "input":if completed { custom_input(&tool.arguments) } else { String::new() }
        }),
        ToolTarget::Namespace {
            namespace, name, ..
        } => serde_json::json!({
            "id":tool.item_id,"type":"function_call","status":status,
            "call_id":tool.call_id,"namespace":namespace,"name":name,
            "arguments":if completed { tool.arguments.clone() } else { String::new() }
        }),
        ToolTarget::LocalShell => {
            hosted_item(tool, status, "local_shell_call", "action", completed)
        }
        ToolTarget::ApplyPatch => {
            hosted_item(tool, status, "apply_patch_call", "operation", completed)
        }
        ToolTarget::ToolSearch => serde_json::json!({
            "id":tool.item_id,"type":"tool_search_call","status":status,
            "call_id":tool.call_id,"arguments":if completed { tool.arguments.clone() } else { String::new() }
        }),
        ToolTarget::Function => serde_json::json!({
            "id":tool.item_id,"type":"function_call","status":status,
            "call_id":tool.call_id,"name":tool.name,
            "arguments":if completed { tool.arguments.clone() } else { String::new() }
        }),
    }
}

fn hosted_item(
    tool: &ToolOutput,
    status: &str,
    item_type: &str,
    field: &str,
    completed: bool,
) -> Value {
    let mut item = serde_json::json!({
        "id":tool.item_id,"type":item_type,"status":status,"call_id":tool.call_id
    });
    item[field] = if completed {
        serde_json::from_str(&tool.arguments)
            .unwrap_or_else(|_| Value::String(tool.arguments.clone()))
    } else {
        Value::Null
    };
    item
}

fn custom_input(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("input")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| arguments.to_owned())
}

fn responses_usage(usage: &Value) -> Value {
    let input = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = usage
        .get("completion_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    serde_json::json!({
        "input_tokens":input,"output_tokens":output,
        "total_tokens":usage.get("total_tokens").and_then(Value::as_u64).unwrap_or(input.saturating_add(output)),
        "input_tokens_details":{"cached_tokens":cached},
        "output_tokens_details":{"reasoning_tokens":reasoning}
    })
}

fn response_error(error: &Value) -> Value {
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("upstream_error");
    let message = error.get("message").and_then(Value::as_str).unwrap_or(code);
    serde_json::json!({"code":code,"message":message,"param":error.get("param").cloned().unwrap_or(Value::Null)})
}

#[cfg(test)]
mod tests;
