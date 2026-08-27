mod json_shape;
mod profile;

use axum::http::{HeaderMap, header};
use provider_core::{RequestClient, RequestMetadata};
use serde_json::Value;
use uuid::Uuid;

use json_shape::{JsonShape, unique_object_value};
use profile::{HelperShape, helper_body_shape, matches_helper_profile};
#[cfg(test)]
use profile::{helper_headers_match, helper_session_matches, helper_shape};

pub(super) const CLAUDE_CODE_SESSION_HEADER: &str = "x-claude-code-session-id";

const CLAUDE_CODE_BETA: &str = "claude-code-20250219";
const CLAUDE_CODE_VERSION: &str = "2.1.220";
const CLAUDE_CODE_PACKAGE_VERSION: &str = "0.94.0";
const CLAUDE_CODE_RUNTIME_VERSION: &str = "v26.3.0";
const HELPER_MODEL: &str = "claude-haiku-4-5-20251001";
const HELPER_TIMEOUT: &str = "600";

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

#[cfg(test)]
mod tests;
