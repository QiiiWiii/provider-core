use std::collections::{BTreeSet, HashMap};

use provider_core::ProviderError;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::schema::sanitize_schema;
use super::validation::{invalid, required_string};
use super::{ToolCatalog, ToolIdentity};

struct ToolDescriptor {
    name: String,
    local_name: Option<String>,
    namespace: Option<String>,
    description: Option<Value>,
    parameters: Option<Value>,
    custom: bool,
}

pub(super) fn convert_tools(
    root: &Map<String, Value>,
    require_validated_placeholder: bool,
) -> Result<ToolCatalog, ProviderError> {
    let mut descriptors = Vec::new();
    let mut web_search = false;
    append_tool_source(root.get("tools"), &mut descriptors, &mut web_search)?;
    if let Some(Value::Array(items)) = root.get("input") {
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
                append_tool_source(item.get("tools"), &mut descriptors, &mut web_search)?;
            }
        }
    }

    let names = build_tool_name_map(&descriptors);
    let identities = descriptors
        .iter()
        .filter_map(|descriptor| {
            let upstream_name = names.get(&descriptor.name)?.clone();
            Some((
                upstream_name,
                ToolIdentity {
                    name: descriptor
                        .local_name
                        .clone()
                        .unwrap_or_else(|| descriptor.name.clone()),
                    namespace: descriptor.namespace.clone(),
                    custom: descriptor.custom,
                },
            ))
        })
        .collect();
    let mut declarations = Vec::new();
    let mut seen = BTreeSet::new();
    for descriptor in descriptors {
        if !seen.insert(descriptor.name.clone()) {
            continue;
        }
        let name = names
            .get(&descriptor.name)
            .cloned()
            .unwrap_or_else(|| sanitize_function_name(&descriptor.name));
        let parameters = if descriptor.custom {
            json!({
                "type": "object",
                "properties": {"input": {"type": "string"}},
                "required": ["input"]
            })
        } else {
            descriptor
                .parameters
                .unwrap_or_else(|| json!({"type":"object","properties":{}}))
        };
        let mut declaration = Map::new();
        declaration.insert("name".to_owned(), Value::String(name));
        if let Some(description) = descriptor.description {
            declaration.insert("description".to_owned(), description);
        }
        declaration.insert(
            "parameters".to_owned(),
            sanitize_schema(parameters, require_validated_placeholder),
        );
        declarations.push(Value::Object(declaration));
    }
    Ok(ToolCatalog {
        declarations,
        web_search,
        names,
        identities,
    })
}

pub(crate) fn response_tool_identities(payload: &[u8]) -> HashMap<String, ToolIdentity> {
    let Ok(Value::Object(root)) = serde_json::from_slice(payload) else {
        return HashMap::new();
    };
    convert_tools(&root, false)
        .map(|catalog| catalog.identities)
        .unwrap_or_default()
}

fn append_tool_source(
    value: Option<&Value>,
    descriptors: &mut Vec<ToolDescriptor>,
    web_search: &mut bool,
) -> Result<(), ProviderError> {
    let Some(value) = value else {
        return Ok(());
    };
    let tools = value
        .as_array()
        .ok_or_else(|| invalid("OpenAI Responses tools must be an array"))?;
    for tool in tools {
        let object = tool
            .as_object()
            .ok_or_else(|| invalid("OpenAI Responses tools must contain objects"))?;
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "function" | "custom" | "" => {
                descriptors.push(tool_descriptor(tool, None, kind == "custom")?);
            }
            "namespace" => {
                let namespace =
                    required_string(object, "name", "OpenAI namespace tool requires a name")?;
                let children = object
                    .get("tools")
                    .and_then(Value::as_array)
                    .ok_or_else(|| invalid("OpenAI namespace tool requires a tools array"))?;
                for child in children {
                    let child_object = child
                        .as_object()
                        .ok_or_else(|| invalid("OpenAI namespace tools must contain objects"))?;
                    let child_kind = child_object
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("function");
                    if !matches!(child_kind, "function" | "custom" | "") {
                        continue;
                    }
                    let child_name = tool_name(child_object)
                        .ok_or_else(|| invalid("OpenAI namespace tool requires child names"))?;
                    let qualified = qualify_tool_name(namespace, child_name);
                    descriptors.push(tool_descriptor(
                        child,
                        Some((qualified, child_name.to_owned(), namespace.to_owned())),
                        child_kind == "custom",
                    )?);
                }
            }
            "web_search_preview" | "web_search_preview_2025_03_11" | "web_search" => {
                *web_search = true;
            }
            _ => return Err(invalid("unsupported OpenAI Responses tool type")),
        }
    }
    Ok(())
}

fn tool_descriptor(
    tool: &Value,
    qualified_name: Option<(String, String, String)>,
    custom: bool,
) -> Result<ToolDescriptor, ProviderError> {
    let object = tool
        .as_object()
        .ok_or_else(|| invalid("OpenAI Responses tools must contain objects"))?;
    let (name, local_name, namespace) = if let Some((name, local_name, namespace)) = qualified_name
    {
        (name, Some(local_name), Some(namespace))
    } else {
        (
            tool_name(object)
                .ok_or_else(|| invalid("OpenAI function tool requires a name"))?
                .to_owned(),
            None,
            None,
        )
    };
    let function = object.get("function").and_then(Value::as_object);
    let description = object
        .get("description")
        .or_else(|| function.and_then(|value| value.get("description")))
        .cloned();
    let parameters = [
        object.get("parameters"),
        object.get("parametersJsonSchema"),
        object.get("input_schema"),
        function.and_then(|value| value.get("parameters")),
        function.and_then(|value| value.get("parametersJsonSchema")),
    ]
    .into_iter()
    .flatten()
    .next()
    .cloned();
    Ok(ToolDescriptor {
        name,
        local_name,
        namespace,
        description,
        parameters,
        custom,
    })
}

fn tool_name(object: &Map<String, Value>) -> Option<&str> {
    object
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| {
            object
                .get("function")
                .and_then(|value| value.get("name"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn qualify_tool_name(namespace: &str, child: &str) -> String {
    let namespace = namespace.trim();
    let child = child.trim();
    if namespace.is_empty()
        || child.is_empty()
        || child.starts_with("mcp__")
        || child == namespace
        || child.starts_with(&format!("{namespace}__"))
    {
        return child.to_owned();
    }
    if namespace.ends_with("__") {
        format!("{namespace}{child}")
    } else {
        format!("{namespace}__{child}")
    }
}

fn build_tool_name_map(descriptors: &[ToolDescriptor]) -> HashMap<String, String> {
    let unique_names = descriptors
        .iter()
        .map(|descriptor| descriptor.name.clone())
        .collect::<BTreeSet<_>>();
    let mut base_counts = HashMap::new();
    for name in &unique_names {
        *base_counts
            .entry(sanitize_function_name(name))
            .or_insert(0_usize) += 1;
    }
    let mut used = HashMap::new();
    let mut result = HashMap::new();
    for name in unique_names {
        let base = sanitize_function_name(&name);
        let mapped = if base_counts.get(&base) == Some(&1) && !used.contains_key(&base) {
            base
        } else {
            disambiguate_tool_name(&base, &name, &used)
        };
        used.insert(mapped.clone(), name.clone());
        result.insert(name, mapped);
    }
    for descriptor in descriptors {
        if let Some(local_name) = descriptor.local_name.as_ref()
            && !result.contains_key(local_name)
            && let Some(mapped) = result.get(&descriptor.name).cloned()
        {
            result.insert(local_name.clone(), mapped);
        }
    }
    result
}

fn disambiguate_tool_name(base: &str, original: &str, used: &HashMap<String, String>) -> String {
    for attempt in 0_u32.. {
        let digest = Sha256::digest(format!("{original}\0{attempt}"));
        let suffix = digest[..6]
            .iter()
            .map(|value| format!("{value:02x}"))
            .collect::<String>();
        let suffix = format!("_{suffix}");
        let prefix = &base[..base.len().min(64 - suffix.len())];
        let candidate = format!("{prefix}{suffix}");
        if !used.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!("tool name disambiguation must find a candidate")
}

fn sanitize_function_name(name: &str) -> String {
    let mut sanitized = name
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() || matches!(value, '_' | '.' | ':' | '-') {
                value
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        sanitized.push('_');
    }
    if !sanitized
        .chars()
        .next()
        .is_some_and(|value| value.is_ascii_alphabetic() || value == '_')
    {
        sanitized.insert(0, '_');
    }
    sanitized.truncate(64);
    sanitized
}

pub(super) fn mapped_tool_name(
    object: &Map<String, Value>,
    name: &str,
    tool_names: &HashMap<String, String>,
) -> String {
    let qualified = object
        .get("namespace")
        .and_then(Value::as_str)
        .map(|namespace| qualify_tool_name(namespace, name));
    qualified
        .as_ref()
        .and_then(|value| tool_names.get(value))
        .or_else(|| tool_names.get(name))
        .cloned()
        .unwrap_or_else(|| sanitize_function_name(name))
}

pub(super) fn convert_tool_choice(
    value: &Value,
    tool_names: &HashMap<String, String>,
) -> Result<Value, ProviderError> {
    let (mode, allowed_name) = match value {
        Value::String(value) => match value.as_str() {
            "auto" => ("AUTO", None),
            "required" | "any" => ("ANY", None),
            "none" => ("NONE", None),
            _ => return Err(invalid("unsupported OpenAI tool_choice value")),
        },
        Value::Object(object) => {
            let kind = object
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match kind {
                "none" => ("NONE", None),
                "auto" => ("AUTO", None),
                "required" | "any" => ("ANY", None),
                "function" | "custom" | "tool" | "" => {
                    let name = object
                        .get("name")
                        .or_else(|| object.get("function").and_then(|value| value.get("name")))
                        .or_else(|| object.get("custom").and_then(|value| value.get("name")))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| invalid("OpenAI function tool_choice requires a name"))?;
                    let namespace = object
                        .get("namespace")
                        .or_else(|| {
                            object
                                .get("function")
                                .and_then(|value| value.get("namespace"))
                        })
                        .or_else(|| {
                            object
                                .get("custom")
                                .and_then(|value| value.get("namespace"))
                        })
                        .and_then(Value::as_str);
                    let name = namespace
                        .map(|namespace| qualify_tool_name(namespace, name))
                        .unwrap_or_else(|| name.to_owned());
                    let name = tool_names
                        .get(&name)
                        .cloned()
                        .unwrap_or_else(|| sanitize_function_name(&name));
                    ("ANY", Some(name))
                }
                _ => return Err(invalid("unsupported OpenAI tool_choice value")),
            }
        }
        _ => return Err(invalid("unsupported OpenAI tool_choice value")),
    };
    let mut config = Map::new();
    config.insert("mode".to_owned(), Value::String(mode.to_owned()));
    if let Some(allowed_name) = allowed_name {
        config.insert(
            "allowedFunctionNames".to_owned(),
            Value::Array(vec![Value::String(allowed_name)]),
        );
    }
    Ok(Value::Object(config))
}
