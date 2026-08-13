use std::collections::HashSet;

use axum::{
    body::Bytes,
    http::{HeaderMap, header},
};
use provider_auth::{ApiKeyAuthenticator, AuthenticatedApiKey};
use provider_core::{AccountId, ProxyRequest, RequestMetadata, WireFormat};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::{HttpError, resolve_claude_model_id};

pub(super) const CLAUDE_CODE_SESSION_HEADER: &str = "x-claude-code-session-id";

#[cfg(test)]
pub(super) fn proxy_request(
    protocol: WireFormat,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<ProxyRequest, HttpError> {
    let payload = parse_payload(protocol, &body)?;
    proxy_request_from_payload(protocol, headers, body, payload)
}

pub(super) fn parse_payload(protocol: WireFormat, body: &[u8]) -> Result<Value, HttpError> {
    serde_json::from_slice(body)
        .map_err(|_| HttpError::invalid_request(protocol, "request body must be valid JSON"))
}

fn proxy_request_from_payload(
    protocol: WireFormat,
    headers: &HeaderMap,
    body: Bytes,
    mut payload: Value,
) -> Result<ProxyRequest, HttpError> {
    let model = payload
        .as_object()
        .and_then(|payload| payload.get("model"))
        .and_then(Value::as_str)
        .ok_or_else(|| HttpError::invalid_request(protocol, "model must be a non-empty string"))?
        .to_owned();

    let model = if protocol == WireFormat::ClaudeMessages {
        resolve_claude_model_id(&model)
    } else {
        model
    };
    let body = if payload["model"].as_str() == Some(model.as_str()) {
        body
    } else {
        payload["model"] = Value::String(model.clone());
        Bytes::from(serde_json::to_vec(&payload).map_err(|_| HttpError::internal(protocol))?)
    };

    let request = ProxyRequest::new(protocol, model, body)
        .map_err(|error| HttpError::from_proxy_request(protocol, error))?;
    Ok(request.with_metadata(request_metadata(headers, protocol)?))
}

pub(super) fn proxy_request_for_key(
    protocol: WireFormat,
    headers: &HeaderMap,
    body: Bytes,
    key: &AuthenticatedApiKey,
) -> Result<ProxyRequest, HttpError> {
    let payload = parse_payload(protocol, &body)?;
    proxy_request_for_key_from_payload(protocol, headers, body, payload, key)
}

pub(super) fn proxy_request_for_key_from_payload(
    protocol: WireFormat,
    headers: &HeaderMap,
    body: Bytes,
    payload: Value,
    key: &AuthenticatedApiKey,
) -> Result<ProxyRequest, HttpError> {
    let mut request = proxy_request_from_payload(protocol, headers, body, payload)?;
    request.metadata.routing_scope = Some(key.key_id.to_string());
    request.metadata.previous_response_id = serde_json::from_slice::<Value>(&request.payload)
        .ok()
        .and_then(|payload| {
            payload
                .get("previous_response_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    if protocol == WireFormat::ClaudeMessages {
        let mut payload: Value = serde_json::from_slice(&request.payload)
            .map_err(|_| HttpError::invalid_request(protocol, "request body must be valid JSON"))?;
        let Some(root) = payload.as_object_mut() else {
            return Err(HttpError::invalid_request(
                protocol,
                "request body must be a JSON object",
            ));
        };
        if let Some(session_id) = claude_code_session_id(headers, root, protocol)? {
            request.metadata.session_id =
                Some(claude_code_cache_key(key, &request.model, &session_id));
        }
        root.remove("metadata");
        request.payload = serde_json::to_vec(&payload)
            .map(Bytes::from)
            .map_err(|_| HttpError::internal(protocol))?;
    }
    Ok(request)
}

fn request_metadata(
    headers: &HeaderMap,
    protocol: WireFormat,
) -> Result<RequestMetadata, HttpError> {
    let mut metadata = RequestMetadata::default();
    metadata.session_id = metadata_header(headers, "session-id", protocol)?;
    metadata.thread_id = metadata_header(headers, "thread-id", protocol)?;
    metadata.client_request_id = metadata_header(headers, "x-client-request-id", protocol)?;
    Ok(metadata)
}

pub(super) fn claude_code_session_id(
    headers: &HeaderMap,
    body: &Map<String, Value>,
    protocol: WireFormat,
) -> Result<Option<String>, HttpError> {
    if let Some(session_id) = metadata_header(headers, CLAUDE_CODE_SESSION_HEADER, protocol)? {
        return Ok(Some(session_id));
    }
    if let Some(user_id) = body
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(Value::as_str)
        && let Ok(metadata) = serde_json::from_str::<Value>(user_id)
        && let Some(session_id) = metadata.get("session_id").and_then(Value::as_str)
    {
        return validated_session_id(session_id, protocol);
    }
    metadata_header(headers, "session-id", protocol)
}

fn validated_session_id(value: &str, protocol: WireFormat) -> Result<Option<String>, HttpError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if !valid_metadata_value(value) {
        return Err(HttpError::invalid_request(
            protocol,
            "request metadata header is invalid",
        ));
    }
    Ok(Some(value.to_owned()))
}

pub(super) fn claude_code_cache_key(
    key: &AuthenticatedApiKey,
    model: &str,
    session_id: &str,
) -> String {
    let mut digest = Sha256::new();
    for value in [
        "claude-code-cache-v1",
        key.key_id.as_str(),
        key.owner_user_id.as_str(),
        model,
        session_id,
    ] {
        digest.update(
            u64::try_from(value.len())
                .expect("request metadata length must fit u64")
                .to_be_bytes(),
        );
        digest.update(value.as_bytes());
    }
    let digest = digest.finalize();
    let mut encoded = String::with_capacity(35);
    encoded.push_str("cc_");
    for byte in digest.iter().take(16) {
        use std::fmt::Write;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn metadata_header(
    headers: &HeaderMap,
    name: &'static str,
    protocol: WireFormat,
) -> Result<Option<String>, HttpError> {
    let Some(value) = headers.get(name) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| HttpError::invalid_request(protocol, "request metadata header is invalid"))?
        .trim();
    if value.is_empty() {
        return Ok(None);
    }
    if !valid_metadata_value(value) {
        return Err(HttpError::invalid_request(
            protocol,
            "request metadata header is invalid",
        ));
    }
    Ok(Some(value.to_owned()))
}

fn valid_metadata_value(value: &str) -> bool {
    value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}

pub(super) async fn load_key_account_filter(
    api_keys: &ApiKeyAuthenticator,
    key: &AuthenticatedApiKey,
    protocol: WireFormat,
) -> Result<HashSet<AccountId>, HttpError> {
    let account_ids = api_keys
        .account_ids_for_key(&key.owner_user_id, &key.group_label)
        .await
        .map_err(|_| HttpError::internal(protocol))?;
    let mut set = HashSet::new();
    for account_id in account_ids {
        let id = AccountId::new(account_id).map_err(|_| HttpError::internal(protocol))?;
        set.insert(id);
    }
    Ok(set)
}

pub(super) fn authenticate_api_key(
    authenticator: &ApiKeyAuthenticator,
    headers: &HeaderMap,
    protocol: WireFormat,
) -> Result<AuthenticatedApiKey, HttpError> {
    let key = downstream_api_key(headers).ok_or_else(|| HttpError::authentication(protocol))?;
    authenticator
        .authenticate(key, unix_timestamp())
        .map_err(|_| HttpError::authentication(protocol))
}

fn downstream_api_key(headers: &HeaderMap) -> Option<&str> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer_value);
    let x_api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty());
    match (bearer, x_api_key) {
        (Some(bearer), Some(x_api_key)) if bearer != x_api_key => None,
        (Some(bearer), _) => Some(bearer),
        (None, Some(x_api_key)) => Some(x_api_key),
        (None, None) => None,
    }
}

fn bearer_value(value: &str) -> Option<&str> {
    let mut parts = value.split_whitespace();
    let scheme = parts.next()?;
    let token = parts.next()?;
    if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() && parts.next().is_none() {
        Some(token)
    } else {
        None
    }
}

pub(super) fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock must be after unix epoch")
        .as_secs()
        .try_into()
        .expect("unix timestamp must fit i64")
}
