use std::collections::HashSet;

use serde_json::{Map, Value, json};

pub(super) fn sanitize_schema(value: Value, require_validated_placeholder: bool) -> Value {
    let value = sanitize_schema_inner(inline_local_refs(value), false);
    if require_validated_placeholder {
        add_validated_placeholders(value)
    } else {
        value
    }
}

pub(super) fn sanitize_response_schema(value: Value) -> Value {
    sanitize_schema_inner(inline_local_refs(value), true)
}

fn inline_local_refs(value: Value) -> Value {
    let definitions = value
        .get("$defs")
        .or_else(|| value.get("definitions"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    resolve_local_refs(value, &definitions, &mut HashSet::new())
}

fn resolve_local_refs(
    value: Value,
    definitions: &Map<String, Value>,
    resolving: &mut HashSet<String>,
) -> Value {
    match value {
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| resolve_local_refs(value, definitions, resolving))
                .collect(),
        ),
        Value::Object(mut object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str)
                && let Some(name) = reference
                    .strip_prefix("#/$defs/")
                    .or_else(|| reference.strip_prefix("#/definitions/"))
                && let Some(target) = definitions.get(name)
                && resolving.insert(name.to_owned())
            {
                let mut resolved = resolve_local_refs(target.clone(), definitions, resolving);
                resolving.remove(name);
                if let Value::Object(resolved_object) = &mut resolved {
                    object.remove("$ref");
                    for (key, value) in object {
                        resolved_object
                            .insert(key, resolve_local_refs(value, definitions, resolving));
                    }
                    return resolved;
                }
                return resolved;
            }
            Value::Object(
                object
                    .into_iter()
                    .map(|(key, value)| (key, resolve_local_refs(value, definitions, resolving)))
                    .collect(),
            )
        }
        value => value,
    }
}

fn sanitize_schema_inner(value: Value, response: bool) -> Value {
    let Value::Object(mut object) = value else {
        return value;
    };

    if let Some(constant) = object.remove("const")
        && !object.contains_key("enum")
    {
        object.insert("enum".to_owned(), Value::Array(vec![constant]));
    }

    if let Some(schema_type) = object.remove("type") {
        match schema_type {
            Value::Array(types) => {
                let nullable = types.iter().any(|value| value.as_str() == Some("null"));
                if let Some(value) = types
                    .iter()
                    .find(|value| value.as_str().is_some_and(|value| value != "null"))
                    .cloned()
                {
                    object.insert("type".to_owned(), value);
                }
                if nullable {
                    object.insert("nullable".to_owned(), Value::Bool(true));
                }
            }
            schema_type => {
                object.insert("type".to_owned(), schema_type);
            }
        }
    }

    for key in ["anyOf", "oneOf"] {
        if let Some(Value::Array(branches)) = object.remove(key) {
            let nullable = branches.iter().any(is_null_schema);
            let selected = branches
                .into_iter()
                .find(|branch| !is_null_schema(branch))
                .unwrap_or_else(|| json!({"type": "string"}));
            let mut selected = match sanitize_schema_inner(selected, response) {
                Value::Object(selected) => selected,
                value => Map::from_iter([(String::from("type"), value)]),
            };
            if nullable {
                selected.insert("nullable".to_owned(), Value::Bool(true));
            }
            merge_schema_fields(&mut selected, &mut object);
            object = selected;
            break;
        }
    }

    if let Some(Value::Array(branches)) = object.remove("allOf") {
        let mut merged = Map::new();
        for branch in branches {
            if let Value::Object(branch) = sanitize_schema_inner(branch, response) {
                merge_schema_fields(&mut merged, &mut branch.clone());
            }
        }
        merge_schema_fields(&mut merged, &mut object);
        object = merged;
    }

    if !object.contains_key("type")
        && !object.contains_key("properties")
        && object
            .values()
            .all(|value| value.is_object() || value.is_null())
        && !object.is_empty()
    {
        let properties = std::mem::take(&mut object);
        object.insert("type".to_owned(), Value::String("object".to_owned()));
        object.insert("properties".to_owned(), Value::Object(properties));
    }

    if let Some(Value::Object(properties)) = object.remove("properties") {
        object.insert(
            "properties".to_owned(),
            Value::Object(
                properties
                    .into_iter()
                    .map(|(name, schema)| (name, sanitize_schema_inner(schema, response)))
                    .collect(),
            ),
        );
    }
    if let Some(items) = object.remove("items") {
        object.insert("items".to_owned(), sanitize_schema_inner(items, response));
    }

    if !response {
        if let Some(enum_values) = object.remove("enum") {
            append_description(&mut object, format!("Allowed values: {}", enum_values));
        }
        if let Some(additional) = object.remove("additionalProperties")
            && additional == Value::Bool(false)
        {
            append_description(&mut object, "No extra properties allowed".to_owned());
        }
    } else {
        let remove_enum = object
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|values| values.iter().all(Value::is_boolean));
        if remove_enum {
            object.remove("enum");
        }
        if object.get("additionalProperties") != Some(&Value::Bool(false)) {
            object.remove("additionalProperties");
        }
    }

    for key in [
        "$schema",
        "$id",
        "$comment",
        "default",
        "examples",
        "format",
        "patternProperties",
        "unevaluatedProperties",
        "additionalItems",
        "propertyNames",
        "dependentRequired",
        "dependentSchemas",
        "uniqueItems",
        "not",
        "if",
        "then",
        "else",
        "$defs",
        "definitions",
        "$ref",
        "discriminator",
        "xml",
        "externalDocs",
    ] {
        object.remove(key);
    }
    if !response {
        object.remove("title");
    }

    let properties = object
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(Value::Array(required)) = object.get_mut("required") {
        required.retain(|value| {
            value
                .as_str()
                .is_some_and(|name| properties.contains_key(name))
        });
        required.dedup();
        if required.is_empty() {
            object.remove("required");
        }
    }

    Value::Object(object)
}

fn add_validated_placeholders(value: Value) -> Value {
    let Value::Object(mut object) = value else {
        return value;
    };

    if let Some(Value::Object(properties)) = object.remove("properties") {
        object.insert(
            "properties".to_owned(),
            Value::Object(
                properties
                    .into_iter()
                    .map(|(name, schema)| (name, add_validated_placeholders(schema)))
                    .collect(),
            ),
        );
    }
    if let Some(items) = object.remove("items") {
        object.insert("items".to_owned(), add_validated_placeholders(items));
    }

    if object.get("type").and_then(Value::as_str) == Some("object") {
        let properties = object
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let has_required = object
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|required| !required.is_empty());
        if properties.is_empty() {
            let mut properties = properties;
            properties.insert(
                "reason".to_owned(),
                json!({
                    "type": "string",
                    "description": "Brief explanation of why you are calling this tool"
                }),
            );
            object.insert("properties".to_owned(), Value::Object(properties));
            object.insert("required".to_owned(), json!(["reason"]));
        } else if !has_required {
            let mut properties = properties;
            properties
                .entry("_".to_owned())
                .or_insert_with(|| json!({"type": "boolean"}));
            object.insert("properties".to_owned(), Value::Object(properties));
            object.insert("required".to_owned(), json!(["_"]));
        }
    }

    Value::Object(object)
}

fn is_null_schema(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("null")
}

fn merge_schema_fields(target: &mut Map<String, Value>, source: &mut Map<String, Value>) {
    if let (Some(Value::Object(target_props)), Some(Value::Object(source_props))) =
        (target.get_mut("properties"), source.remove("properties"))
    {
        target_props.extend(source_props);
    }
    if let (Some(Value::Array(target_required)), Some(Value::Array(source_required))) =
        (target.get_mut("required"), source.remove("required"))
    {
        target_required.extend(source_required);
    }
    let source = std::mem::take(source);
    for (key, value) in source {
        target.entry(key).or_insert(value);
    }
}

fn append_description(object: &mut Map<String, Value>, hint: String) {
    let hint = hint.trim().to_owned();
    if hint.is_empty() {
        return;
    }
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let description = match description {
        Some(description) if !description.is_empty() => format!("{description}. {hint}"),
        _ => hint,
    };
    object.insert("description".to_owned(), Value::String(description));
}
