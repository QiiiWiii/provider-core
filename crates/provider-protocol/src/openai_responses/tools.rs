use std::collections::HashSet;

use provider_core::{ProviderError, ProviderErrorKind};
use serde_json::{Map, Value};

use super::{ToolTarget, ToolTargets};

pub(super) fn convert_tools(tools: &[Value]) -> Result<(Vec<Value>, ToolTargets), ProviderError> {
    let mut converted = Vec::new();
    let mut targets = ToolTargets::new();
    for tool in tools {
        convert_tool(tool, None, &mut converted, &mut targets)?;
    }
    Ok((converted, targets))
}

fn convert_tool(
    tool: &Value,
    namespace: Option<&str>,
    converted: &mut Vec<Value>,
    targets: &mut ToolTargets,
) -> Result<(), ProviderError> {
    let tool = tool
        .as_object()
        .ok_or_else(|| invalid("Responses tools must contain objects"))?;
    let tool_type = required_string(tool, "type", "Responses tool requires a type")?;
    if tool_type == "namespace" {
        let namespace = required_string(tool, "name", "Responses namespace tool requires a name")?;
        let nested = tool
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("Responses namespace tool requires tools"))?;
        for nested_tool in nested {
            convert_tool(nested_tool, Some(namespace), converted, targets)?;
        }
        return Ok(());
    }
    let (name, target, parameters) = match tool_type {
        "function" => {
            let parameters = match tool.get("parameters") {
                None | Some(Value::Null) => empty_schema(),
                Some(parameters) if parameters.is_object() => parameters.clone(),
                Some(_) => return Err(invalid("Responses function parameters must be an object")),
            };
            (
                required_string(tool, "name", "Responses function tool requires a name")?
                    .to_owned(),
                ToolTarget::Function,
                parameters,
            )
        }
        "custom" => {
            validate_custom_format(tool.get("format"))?;
            (
                required_string(tool, "name", "Responses custom tool requires a name")?.to_owned(),
                ToolTarget::Custom,
                custom_schema(),
            )
        }
        "local_shell" => (
            "local_shell".to_owned(),
            ToolTarget::LocalShell,
            open_schema(),
        ),
        "apply_patch" => (
            "apply_patch".to_owned(),
            ToolTarget::ApplyPatch,
            open_schema(),
        ),
        "tool_search" => (
            "tool_search".to_owned(),
            ToolTarget::ToolSearch,
            open_schema(),
        ),
        "web_search" | "file_search" | "computer" | "code_interpreter" | "image_generation" => {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!("Responses hosted tool {tool_type} has no Chat Completions equivalent"),
            ));
        }
        _ => {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                format!("Responses tool type {tool_type} cannot be converted to Chat Completions"),
            ));
        }
    };
    let chat_name = namespace.map_or_else(
        || name.clone(),
        |namespace| format!("{}__{name}", namespace.trim_end_matches("__")),
    );
    let custom = matches!(target, ToolTarget::Custom);
    let target = namespace.map_or(target, |namespace| ToolTarget::Namespace {
        namespace: namespace.to_owned(),
        name: name.clone(),
        custom,
    });
    if targets.insert(chat_name.clone(), target).is_some() {
        return Err(invalid(
            "Responses tool names must remain unique after namespace conversion",
        ));
    }
    let mut function = Map::new();
    function.insert("name".to_owned(), Value::String(chat_name));
    if let Some(description) = optional_string(tool, "description")? {
        function.insert(
            "description".to_owned(),
            Value::String(description.to_owned()),
        );
    }
    function.insert("parameters".to_owned(), parameters);
    if tool_type == "function" {
        match tool.get("strict") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(strict)) => {
                function.insert("strict".to_owned(), Value::Bool(*strict));
            }
            Some(_) => {
                return Err(invalid(
                    "Responses function strict must be a boolean or null",
                ));
            }
        }
    }
    converted.push(serde_json::json!({"type":"function","function":function}));
    Ok(())
}

pub(super) fn convert_tool_choice(
    choice: Option<&Value>,
    targets: &ToolTargets,
) -> Result<Option<Value>, ProviderError> {
    let Some(choice) = choice.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    if let Some(mode) = choice.as_str() {
        return match mode {
            "none" | "auto" | "required" => Ok(Some(Value::String(mode.to_owned()))),
            _ => Err(invalid("unsupported Responses tool_choice")),
        };
    }
    let choice = choice
        .as_object()
        .ok_or_else(|| invalid("Responses tool_choice must be text or an object"))?;
    let choice_type = required_string(choice, "type", "Responses tool_choice requires a type")?;
    if choice_type == "allowed_tools" {
        let mode = match choice.get("mode") {
            None | Some(Value::Null) => "auto",
            Some(Value::String(mode)) if matches!(mode.as_str(), "auto" | "required") => mode,
            Some(_) => return Err(invalid("unsupported Responses allowed_tools mode")),
        };
        let allowed = choice
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("Responses allowed_tools choice requires tools"))?;
        if mode == "required" && allowed.len() == 1 {
            let allowed = allowed[0]
                .as_object()
                .ok_or_else(|| invalid("Responses allowed tool must be an object"))?;
            let allowed_type =
                required_string(allowed, "type", "Responses allowed tool requires a type")?;
            let name = tool_choice_name(allowed_type, allowed)?;
            return forced_tool_choice(allowed_type, &name, targets).map(Some);
        }
        if mode == "auto" {
            let mut allowed_names = HashSet::with_capacity(allowed.len());
            for allowed in allowed {
                let allowed = allowed
                    .as_object()
                    .ok_or_else(|| invalid("Responses allowed tool must be an object"))?;
                let allowed_type =
                    required_string(allowed, "type", "Responses allowed tool requires a type")?;
                let name = tool_choice_name(allowed_type, allowed)?;
                if !tool_choice_target_matches(allowed_type, &name, targets)
                    || !allowed_names.insert(name)
                {
                    return Err(invalid(
                        "Responses allowed_tools choice cannot be represented by Chat Completions",
                    ));
                }
            }
            if allowed_names.len() == targets.len() {
                return Ok(Some(Value::String("auto".to_owned())));
            }
        }
        return Err(invalid(
            "Responses allowed_tools choice cannot be represented by Chat Completions",
        ));
    }
    let name = tool_choice_name(choice_type, choice)?;
    forced_tool_choice(choice_type, &name, targets).map(Some)
}

fn tool_choice_name(
    choice_type: &str,
    choice: &Map<String, Value>,
) -> Result<String, ProviderError> {
    match choice_type {
        "function" | "custom" => qualified_name(choice),
        "local_shell" => Ok("local_shell".to_owned()),
        "apply_patch" => Ok("apply_patch".to_owned()),
        "tool_search" => Ok("tool_search".to_owned()),
        _ => Err(invalid("unsupported Responses tool_choice")),
    }
}

fn forced_tool_choice(
    choice_type: &str,
    name: &str,
    targets: &ToolTargets,
) -> Result<Value, ProviderError> {
    if !tool_choice_target_matches(choice_type, name, targets) {
        return Err(invalid(
            "Responses tool_choice references an unavailable tool",
        ));
    }
    Ok(serde_json::json!({"type":"function","function":{"name":name}}))
}

fn tool_choice_target_matches(choice_type: &str, name: &str, targets: &ToolTargets) -> bool {
    matches!(
        (choice_type, targets.get(name)),
        ("function", Some(ToolTarget::Function))
            | (
                "function",
                Some(ToolTarget::Namespace { custom: false, .. })
            )
            | ("custom", Some(ToolTarget::Custom))
            | ("custom", Some(ToolTarget::Namespace { custom: true, .. }))
            | ("local_shell", Some(ToolTarget::LocalShell))
            | ("apply_patch", Some(ToolTarget::ApplyPatch))
            | ("tool_search", Some(ToolTarget::ToolSearch))
    )
}

pub(super) fn reject_required_tool_choice(choice: Option<&Value>) -> Result<(), ProviderError> {
    match choice {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(mode)) => match mode.as_str() {
            "auto" | "none" => Ok(()),
            "required" => Err(invalid(
                "Responses tool_choice requires at least one convertible tool",
            )),
            _ => Err(invalid("unsupported Responses tool_choice")),
        },
        Some(Value::Object(choice))
            if matches!(
                choice.get("type").and_then(Value::as_str),
                Some("auto" | "none")
            ) =>
        {
            Ok(())
        }
        Some(Value::Object(_)) => Err(invalid(
            "Responses tool_choice requires at least one convertible tool",
        )),
        Some(_) => Err(invalid("unsupported Responses tool_choice")),
    }
}

fn qualified_name(object: &Map<String, Value>) -> Result<String, ProviderError> {
    let name = required_string(object, "name", "Responses tool reference requires a name")?;
    Ok(object.get("namespace").and_then(Value::as_str).map_or_else(
        || name.to_owned(),
        |namespace| format!("{}__{name}", namespace.trim_end_matches("__")),
    ))
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    message: &'static str,
) -> Result<&'a str, ProviderError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid(message))
}

fn optional_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<Option<&'a str>, ProviderError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(invalid("Responses tool description must be text or null")),
    }
}

fn empty_schema() -> Value {
    serde_json::json!({"type":"object","properties":{}})
}

fn open_schema() -> Value {
    serde_json::json!({"type":"object","properties":{},"additionalProperties":true})
}

fn custom_schema() -> Value {
    serde_json::json!({
        "type":"object",
        "properties":{"input":{"type":"string"}},
        "required":["input"],
        "additionalProperties":false
    })
}

fn validate_custom_format(format: Option<&Value>) -> Result<(), ProviderError> {
    let Some(format) = format.filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let format = format
        .as_object()
        .ok_or_else(|| invalid("Responses custom tool format must be an object"))?;
    match format.get("type").and_then(Value::as_str) {
        Some("text") => Ok(()),
        Some("grammar") => {
            let syntax = required_string(
                format,
                "syntax",
                "Responses custom tool grammar requires a syntax",
            )?;
            if !matches!(syntax, "lark" | "regex") {
                return Err(invalid(
                    "Responses custom tool grammar syntax must be lark or regex",
                ));
            }
            required_string(
                format,
                "definition",
                "Responses custom tool grammar requires a definition",
            )?;
            Ok(())
        }
        _ => Err(invalid(
            "Responses custom tool format must be text or a supported grammar",
        )),
    }
}

fn invalid(message: &'static str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}
