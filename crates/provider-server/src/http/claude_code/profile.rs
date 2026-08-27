use axum::http::HeaderMap;
use serde_json::{Map, Value};
use uuid::Uuid;

use super::json_shape::{JsonShape, array_items, first_array_item, keys_match, object_value};
use super::{
    CLAUDE_CODE_PACKAGE_VERSION, CLAUDE_CODE_RUNTIME_VERSION, CLAUDE_CODE_SESSION_HEADER,
    HELPER_MODEL, HELPER_TIMEOUT, claude_code_entrypoint, header_equals,
};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum HelperShape {
    Minimal,
    Structured,
}

pub(super) fn matches_helper_profile(
    headers: &HeaderMap,
    body: &[u8],
    user_agent: &str,
    beta: &str,
    user_id: Option<&str>,
) -> bool {
    if claude_code_entrypoint(user_agent) != Some("cli") {
        return false;
    }
    let Some(shape) = helper_shape(beta) else {
        return false;
    };
    if helper_body_shape(body, shape) != Some(shape) {
        return false;
    }
    if !helper_order_matches(body, user_id, shape) {
        return false;
    }
    if !helper_headers_match(headers, shape) {
        return false;
    }
    helper_session_matches(headers, user_id)
}

pub(super) fn helper_shape(beta: &str) -> Option<HelperShape> {
    match beta {
        "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05"
        | "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05" => {
            Some(HelperShape::Minimal)
        }
        "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,structured-outputs-2025-12-15,cache-diagnosis-2026-04-07"
        | "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,structured-outputs-2025-12-15,fallback-credit-2026-06-01"
        | "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,structured-outputs-2025-12-15"
        | "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,structured-outputs-2025-12-15" => {
            Some(HelperShape::Structured)
        }
        _ => None,
    }
}

pub(super) fn helper_headers_match(headers: &HeaderMap, shape: HelperShape) -> bool {
    [
        ("accept", "application/json"),
        ("content-type", "application/json"),
        ("x-stainless-lang", "js"),
        ("x-stainless-runtime", "node"),
        ("x-stainless-retry-count", "0"),
        ("x-stainless-timeout", HELPER_TIMEOUT),
        ("x-stainless-package-version", CLAUDE_CODE_PACKAGE_VERSION),
        ("x-stainless-runtime-version", CLAUDE_CODE_RUNTIME_VERSION),
        ("anthropic-version", "2023-06-01"),
        ("anthropic-dangerous-direct-browser-access", "true"),
    ]
    .into_iter()
    .all(|(name, expected)| header_equals(headers, name, expected))
        && ["x-stainless-os", "x-stainless-arch"]
            .into_iter()
            .all(|name| header_present(headers, name))
        && match shape {
            HelperShape::Minimal => {
                !header_present(headers, "x-stainless-async")
                    && header_equals(headers, "accept-encoding", "gzip")
            }
            HelperShape::Structured => {
                header_equals(headers, "x-stainless-async", "async")
                    && header_equals(headers, "accept-encoding", "gzip, deflate, br, zstd")
            }
        }
        && headers
            .get("x-client-request-id")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| Uuid::parse_str(value.trim()).is_ok())
}

fn header_present(headers: &HeaderMap, name: &str) -> bool {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.trim().is_empty())
}

pub(super) fn helper_session_matches(headers: &HeaderMap, user_id: Option<&str>) -> bool {
    let Some(user_id) = user_id else {
        return false;
    };
    let Ok(identity) = serde_json::from_str::<Value>(user_id) else {
        return false;
    };
    let Some(identity) = identity.as_object() else {
        return false;
    };
    if !identity_keys_match(identity) {
        return false;
    }
    let Some(session_id) = identity.get("session_id").and_then(Value::as_str) else {
        return false;
    };
    headers
        .get(CLAUDE_CODE_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim() == session_id)
}

fn identity_keys_match(identity: &Map<String, Value>) -> bool {
    let has_parent_session_id = identity.contains_key("parent_session_id");
    let expected_len = 3 + usize::from(has_parent_session_id);
    identity.len() == expected_len
        && identity.contains_key("device_id")
        && identity.contains_key("account_uuid")
        && identity.contains_key("session_id")
}

pub(super) fn helper_body_shape(body: &[u8], expected: HelperShape) -> Option<HelperShape> {
    let payload = serde_json::from_slice::<Value>(body).ok()?;
    let root = payload.as_object()?;
    let minimal = ["model", "max_tokens", "messages", "metadata"];
    let structured = [
        "model",
        "messages",
        "system",
        "tools",
        "metadata",
        "max_tokens",
        "thinking",
        "temperature",
        "output_config",
        "stream",
    ];
    let shape = if object_has_exact_keys(root, &minimal) {
        HelperShape::Minimal
    } else if object_has_exact_keys(root, &structured) {
        HelperShape::Structured
    } else {
        return None;
    };
    if shape != expected || root.get("model").and_then(Value::as_str) != Some(HELPER_MODEL) {
        return None;
    }
    let messages = root.get("messages")?.as_array()?;
    if messages.len() != 1 {
        return None;
    }
    let message = messages.first()?.as_object()?;
    if !object_has_exact_keys(message, &["role", "content"])
        || message.get("role").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    match shape {
        HelperShape::Minimal => {
            if root.get("max_tokens").and_then(Value::as_u64) != Some(1)
                || !message.get("content").is_some_and(Value::is_string)
            {
                return None;
            }
        }
        HelperShape::Structured => {
            if root.get("max_tokens").and_then(Value::as_u64) != Some(32000)
                || root.get("temperature").and_then(Value::as_f64) != Some(1.0)
                || root.get("stream").and_then(Value::as_bool) != Some(true)
                || !structured_message(message)
                || !structured_system(root.get("system")?)
                || !root.get("tools").and_then(Value::as_array)?.is_empty()
                || !structured_thinking(root.get("thinking")?)
                || !structured_output_config(root.get("output_config")?)
            {
                return None;
            }
        }
    }
    let metadata = root.get("metadata")?.as_object()?;
    metadata.get("user_id").and_then(Value::as_str)?;
    Some(shape)
}

fn object_has_exact_keys(object: &Map<String, Value>, expected: &[&str]) -> bool {
    object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
}

fn helper_order_matches(body: &[u8], user_id: Option<&str>, shape: HelperShape) -> bool {
    let Ok(root) = serde_json::from_slice::<JsonShape>(body) else {
        return false;
    };
    let root_keys = match shape {
        HelperShape::Minimal => ["model", "max_tokens", "messages", "metadata"].as_slice(),
        HelperShape::Structured => [
            "model",
            "messages",
            "system",
            "tools",
            "metadata",
            "max_tokens",
            "thinking",
            "temperature",
            "output_config",
            "stream",
        ]
        .as_slice(),
    };
    if !keys_match(&root, root_keys)
        || !object_value(&root, "messages")
            .and_then(first_array_item)
            .is_some_and(|message| keys_match(message, &["role", "content"]))
        || !object_value(&root, "metadata")
            .is_some_and(|metadata| keys_match(metadata, &["user_id"]))
    {
        return false;
    }
    let Some(user_id) = user_id else {
        return false;
    };
    let Ok(identity) = serde_json::from_str::<JsonShape>(user_id) else {
        return false;
    };
    if !keys_match(&identity, &["device_id", "account_uuid", "session_id"])
        && !keys_match(
            &identity,
            &[
                "device_id",
                "account_uuid",
                "session_id",
                "parent_session_id",
            ],
        )
    {
        return false;
    }
    if shape == HelperShape::Minimal {
        return true;
    }
    object_value(&root, "messages")
        .and_then(first_array_item)
        .and_then(|message| object_value(message, "content"))
        .and_then(first_array_item)
        .is_some_and(|content| keys_match(content, &["type", "text"]))
        && object_value(&root, "system")
            .and_then(array_items)
            .is_some_and(|blocks| {
                blocks
                    .iter()
                    .all(|block| keys_match(block, &["type", "text"]))
            })
        && object_value(&root, "thinking").is_some_and(|thinking| keys_match(thinking, &["type"]))
        && structured_output_order(&root)
}

fn structured_output_order(root: &JsonShape) -> bool {
    let Some(output) = object_value(root, "output_config") else {
        return false;
    };
    let Some(format) = object_value(output, "format") else {
        return false;
    };
    let Some(schema) = object_value(format, "schema") else {
        return false;
    };
    let Some(properties) = object_value(schema, "properties") else {
        return false;
    };
    keys_match(output, &["format"])
        && keys_match(format, &["type", "schema"])
        && keys_match(
            schema,
            &["type", "properties", "required", "additionalProperties"],
        )
        && keys_match(properties, &["title"])
        && object_value(properties, "title").is_some_and(|title| keys_match(title, &["type"]))
}

fn structured_message(message: &Map<String, Value>) -> bool {
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return false;
    };
    if content.len() != 1 {
        return false;
    }
    let Some(content) = content.first().and_then(Value::as_object) else {
        return false;
    };
    object_has_exact_keys(content, &["type", "text"])
        && content.get("type").and_then(Value::as_str) == Some("text")
}

fn structured_system(value: &Value) -> bool {
    let Some(system) = value.as_array() else {
        return false;
    };
    if system.len() != 3
        || !system.iter().all(|block| {
            block
                .as_object()
                .is_some_and(|block| object_has_exact_keys(block, &["type", "text"]))
                && block.get("type").and_then(Value::as_str) == Some("text")
        })
    {
        return false;
    }
    let Some(billing) = system[0].get("text").and_then(Value::as_str) else {
        return false;
    };
    let Some(identity) = system[1].get("text").and_then(Value::as_str) else {
        return false;
    };
    billing.starts_with("x-anthropic-billing-header:")
        && valid_cch(billing)
        && identity.starts_with("You are Claude Code")
}

fn valid_cch(billing: &str) -> bool {
    let Some(start) = billing.find(" cch=").map(|index| index + 5) else {
        return false;
    };
    let Some(end) = start.checked_add(5) else {
        return false;
    };
    billing.as_bytes().get(end) == Some(&b';')
        && billing.as_bytes()[start..end]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn structured_thinking(value: &Value) -> bool {
    let Some(thinking) = value.as_object() else {
        return false;
    };
    object_has_exact_keys(thinking, &["type"])
        && thinking.get("type").and_then(Value::as_str) == Some("disabled")
}

fn structured_output_config(value: &Value) -> bool {
    let Some(output_config) = value.as_object() else {
        return false;
    };
    let Some(format) = output_config.get("format").and_then(Value::as_object) else {
        return false;
    };
    let Some(schema) = format.get("schema").and_then(Value::as_object) else {
        return false;
    };
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return false;
    };
    let Some(title) = properties.get("title").and_then(Value::as_object) else {
        return false;
    };
    object_has_exact_keys(output_config, &["format"])
        && object_has_exact_keys(format, &["type", "schema"])
        && format.get("type").and_then(Value::as_str) == Some("json_schema")
        && object_has_exact_keys(
            schema,
            &["type", "properties", "required", "additionalProperties"],
        )
        && schema.get("type").and_then(Value::as_str) == Some("object")
        && object_has_exact_keys(properties, &["title"])
        && object_has_exact_keys(title, &["type"])
        && title.get("type").and_then(Value::as_str) == Some("string")
        && schema
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|required| required.len() == 1 && required[0].as_str() == Some("title"))
        && schema.get("additionalProperties").and_then(Value::as_bool) == Some(false)
}
