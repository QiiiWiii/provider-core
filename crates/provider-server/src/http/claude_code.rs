use axum::http::{HeaderMap, header};
use provider_core::{RequestClient, RequestMetadata};
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value};
use uuid::Uuid;

pub(super) const CLAUDE_CODE_SESSION_HEADER: &str = "x-claude-code-session-id";

const CLAUDE_CODE_BETA: &str = "claude-code-20250219";
const CLAUDE_CODE_VERSION: &str = "2.1.220";
const CLAUDE_CODE_PACKAGE_VERSION: &str = "0.94.0";
const CLAUDE_CODE_RUNTIME_VERSION: &str = "v26.3.0";
const HELPER_MODEL: &str = "claude-haiku-4-5-20251001";
const HELPER_TIMEOUT: &str = "600";

#[derive(Clone, Copy, Eq, PartialEq)]
enum HelperShape {
    Minimal,
    Structured,
}

struct Detection {
    user_agent: String,
    beta: String,
    user_id: Option<String>,
    session_id: Option<String>,
    headers: Vec<(String, String)>,
    helper_profile: bool,
}

pub(super) fn request_metadata(
    headers: &HeaderMap,
    body: &[u8],
    count_tokens: bool,
) -> RequestMetadata {
    let mut metadata = RequestMetadata::default();
    let Some(detection) = detect(headers, body, count_tokens) else {
        return metadata;
    };
    metadata.client = RequestClient::ClaudeCode;
    metadata.user_agent = Some(detection.user_agent);
    metadata.claude_code_beta = Some(detection.beta);
    metadata.claude_code_user_id = detection.user_id;
    metadata.claude_code_session_id = detection.session_id;
    metadata.claude_code_headers = detection.headers;
    metadata.claude_code_helper_profile = detection.helper_profile;
    metadata
}

pub(super) fn models_request(headers: &HeaderMap) -> RequestMetadata {
    let mut metadata = RequestMetadata::default();
    let Some(user_agent) = user_agent(headers) else {
        return metadata;
    };
    if is_claude_code_user_agent(&user_agent) {
        metadata.client = RequestClient::ClaudeCode;
        metadata.user_agent = Some(user_agent);
    }
    metadata
}

pub(super) fn non_streaming_helper(headers: &HeaderMap, body: &[u8]) -> bool {
    detect(headers, body, false).is_some_and(|detection| {
        detection.helper_profile
            && helper_body_shape(body, HelperShape::Minimal) == Some(HelperShape::Minimal)
    })
}

fn detect(headers: &HeaderMap, body: &[u8], count_tokens: bool) -> Option<Detection> {
    let user_agent = user_agent(headers)?;
    if !is_claude_code_user_agent(&user_agent) {
        return None;
    }
    let beta = normalized_beta_header(headers)?;
    let identity = claude_code_user_id(body);
    let user_id = identity.as_ref().map(|(user_id, _)| user_id.clone());
    let identity_session_id = identity.as_ref().map(|(_, session_id)| session_id.clone());
    let header_session_id = claude_code_session_id(headers);
    if header_session_id
        .as_ref()
        .zip(identity_session_id.as_ref())
        .is_some_and(|(header, identity)| header != identity)
    {
        return None;
    }
    let x_app_cli = header_equals(headers, "x-app", "cli");
    let standard =
        x_app_cli && beta_contains_claude_code(&beta) && (count_tokens || user_id.is_some());
    let helper = !count_tokens
        && x_app_cli
        && user_id.is_some()
        && matches_helper_profile(headers, body, &user_agent, &beta, user_id.as_deref());
    if !standard && !helper {
        return None;
    }
    Some(Detection {
        user_agent,
        beta,
        user_id,
        session_id: header_session_id.or(identity_session_id),
        headers: claude_code_headers(headers),
        helper_profile: helper,
    })
}

fn user_agent(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .map(ToOwned::to_owned)
}

fn is_claude_code_user_agent(user_agent: &str) -> bool {
    claude_code_entrypoint(user_agent).is_some()
}

fn claude_code_entrypoint(user_agent: &str) -> Option<&str> {
    let (name, version_and_details) = user_agent.split_once('/')?;
    if !name.eq_ignore_ascii_case("claude-cli") {
        return None;
    }
    let (version, details) = version_and_details.split_once(' ')?;
    if version != CLAUDE_CODE_VERSION {
        return None;
    }
    let details = details
        .strip_prefix("(external, ")
        .and_then(|details| details.strip_suffix(')'))?;
    let mut parts = details.split(',').map(str::trim);
    let entrypoint = match parts.next()?.to_ascii_lowercase().as_str() {
        "cli" => "cli",
        "sdk-cli" => "sdk-cli",
        "claude-vscode" => "claude-vscode",
        _ => return None,
    };
    match (parts.next(), parts.next()) {
        (None, None) => Some(entrypoint),
        (Some(agent_sdk), None) => agent_sdk
            .strip_prefix("agent-sdk/")
            .filter(|version| is_semver(version))
            .map(|_| entrypoint),
        _ => None,
    }
}

fn is_semver(value: &str) -> bool {
    value.split('.').count() == 3
        && value
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn normalized_beta_header(headers: &HeaderMap) -> Option<String> {
    let values = headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.join(","))
}

fn beta_contains_claude_code(beta: &str) -> bool {
    beta.split(',')
        .any(|value| value.trim() == CLAUDE_CODE_BETA)
}

fn header_equals(headers: &HeaderMap, name: &str, expected: &str) -> bool {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim() == expected)
}

fn claude_code_user_id(body: &[u8]) -> Option<(String, String)> {
    let shape = serde_json::from_slice::<JsonShape>(body).ok()?;
    let metadata = unique_object_value(&shape, "metadata")?;
    unique_object_value(metadata, "user_id")?;
    let payload = serde_json::from_slice::<Value>(body).ok()?;
    let user_id = payload
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(Value::as_str)?;
    let identity = serde_json::from_str::<Value>(user_id).ok()?;
    let identity = identity.as_object()?;
    let device_id = identity.get("device_id").and_then(Value::as_str)?;
    let session_id = identity.get("session_id").and_then(Value::as_str)?;
    let account_uuid = match identity.get("account_uuid") {
        None => None,
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => return None,
    };
    if device_id.len() != 64
        || !device_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || Uuid::parse_str(session_id).is_err()
        || account_uuid.is_some_and(|value| !value.is_empty() && Uuid::parse_str(value).is_err())
    {
        return None;
    }
    Some((user_id.to_owned(), session_id.to_owned()))
}

fn claude_code_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut captured = Vec::new();
    for (name, value) in headers {
        let name = name.as_str();
        if !claude_code_header_allowed(name) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            captured.push((name.to_owned(), value.to_owned()));
        }
    }
    captured
}

fn claude_code_header_allowed(name: &str) -> bool {
    matches!(
        name,
        "accept"
            | "accept-encoding"
            | "user-agent"
            | "x-app"
            | "x-client-request-id"
            | "x-client-app"
            | "x-anthropic-additional-protection"
    ) || name.starts_with("anthropic-")
        || name.starts_with("x-stainless-")
        || name.starts_with("x-claude-code-")
        || name.starts_with("x-claude-remote-")
}

fn claude_code_session_id(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(CLAUDE_CODE_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    Uuid::parse_str(value).ok().map(|_| value.to_owned())
}

fn matches_helper_profile(
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

fn helper_shape(beta: &str) -> Option<HelperShape> {
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

fn helper_headers_match(headers: &HeaderMap, shape: HelperShape) -> bool {
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

fn helper_session_matches(headers: &HeaderMap, user_id: Option<&str>) -> bool {
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

fn helper_body_shape(body: &[u8], expected: HelperShape) -> Option<HelperShape> {
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

enum JsonShape {
    Object(Vec<(String, JsonShape)>),
    Array(Vec<JsonShape>),
    Scalar,
}

impl<'de> Deserialize<'de> for JsonShape {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(JsonShapeVisitor)
    }
}

struct JsonShapeVisitor;

impl<'de> Visitor<'de> for JsonShapeVisitor {
    type Value = JsonShape;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JSON")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some((key, value)) = map.next_entry()? {
            values.push((key, value));
        }
        Ok(JsonShape::Object(values))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element()? {
            values.push(value);
        }
        Ok(JsonShape::Array(values))
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(JsonShape::Scalar)
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(JsonShape::Scalar)
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(JsonShape::Scalar)
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(JsonShape::Scalar)
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(JsonShape::Scalar)
    }

    fn visit_string<E>(self, _value: String) -> Result<Self::Value, E> {
        Ok(JsonShape::Scalar)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(JsonShape::Scalar)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(JsonShape::Scalar)
    }
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

fn keys_match(value: &JsonShape, expected: &[&str]) -> bool {
    let JsonShape::Object(values) = value else {
        return false;
    };
    values
        .iter()
        .map(|(key, _)| key.as_str())
        .eq(expected.iter().copied())
}

fn object_value<'a>(value: &'a JsonShape, key: &str) -> Option<&'a JsonShape> {
    let JsonShape::Object(values) = value else {
        return None;
    };
    values
        .iter()
        .find_map(|(candidate, value)| (candidate == key).then_some(value))
}

fn unique_object_value<'a>(value: &'a JsonShape, key: &str) -> Option<&'a JsonShape> {
    let JsonShape::Object(values) = value else {
        return None;
    };
    let mut matches = values
        .iter()
        .filter_map(|(candidate, value)| (candidate == key).then_some(value));
    let value = matches.next()?;
    matches.next().is_none().then_some(value)
}

fn first_array_item(value: &JsonShape) -> Option<&JsonShape> {
    array_items(value)?.first()
}

fn array_items(value: &JsonShape) -> Option<&[JsonShape]> {
    let JsonShape::Array(values) = value else {
        return None;
    };
    Some(values)
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

#[cfg(test)]
#[path = "claude_code_tests.rs"]
mod tests;
