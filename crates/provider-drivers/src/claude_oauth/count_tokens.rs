use bytes::Bytes;
use provider_core::{ProviderError, ProviderErrorKind, ProviderRequest, RequestClient};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use secrecy::ExposeSecret;
use serde_json::Value;

use super::credentials::ClaudeOAuthCredentials;

const COUNT_TOKENS_BETAS: &str = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,context-management-2025-06-27,token-counting-2024-11-01";

pub(crate) fn prepare_count_tokens_request(
    request: &ProviderRequest,
    credentials: &ClaudeOAuthCredentials,
) -> Result<(Bytes, HeaderMap), ProviderError> {
    if request.metadata.client != RequestClient::ClaudeCode {
        return Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            "Claude OAuth requires a measured Claude Code client",
        ));
    }
    let session_id = request
        .metadata
        .claude_code_session_id
        .as_deref()
        .ok_or_else(|| invalid("Claude OAuth token count is missing session identity"))?;
    let raw = request
        .metadata
        .claude_code_payload
        .as_deref()
        .ok_or_else(|| invalid("Claude OAuth token count is missing the native payload"))?;
    let mut payload: Value = serde_json::from_slice(raw)
        .map_err(|_| invalid("Claude OAuth token count body is invalid"))?;
    let root = payload
        .as_object_mut()
        .ok_or_else(|| invalid("Claude OAuth token count body must be an object"))?;
    if !root.get("messages").is_some_and(Value::is_array) {
        return Err(invalid("Claude OAuth token count messages are invalid"));
    }
    let mut changed = false;
    for field in [
        "metadata",
        "context_management",
        "diagnostics",
        "system",
        "betas",
    ] {
        changed |= root.remove(field).is_some();
    }
    if root.get("model").and_then(Value::as_str) != Some(request.model.as_str()) {
        root.insert("model".to_owned(), Value::String(request.model.clone()));
        changed = true;
    }
    let body = if changed {
        Bytes::from(
            serde_json::to_vec(&payload)
                .map_err(|_| internal("failed to encode Claude OAuth token count body"))?,
        )
    } else {
        Bytes::copy_from_slice(raw)
    };

    let mut headers = HeaderMap::new();
    for (name, value) in &request.metadata.claude_code_headers {
        if matches!(
            name.as_str(),
            "accept"
                | "accept-encoding"
                | "authorization"
                | "content-type"
                | "connection"
                | "anthropic-beta"
                | "x-api-key"
                | "x-stainless-async"
                | "x-stainless-timeout"
        ) {
            continue;
        }
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(value) else {
            continue;
        };
        headers.append(name, value);
    }
    insert(
        &mut headers,
        "authorization",
        &format!("Bearer {}", credentials.access_token().expose_secret()),
    )?;
    insert(&mut headers, "content-type", "application/json")?;
    insert(&mut headers, "anthropic-version", "2023-06-01")?;
    insert(
        &mut headers,
        "anthropic-dangerous-direct-browser-access",
        "true",
    )?;
    insert(&mut headers, "x-app", "cli")?;
    insert(&mut headers, "x-stainless-retry-count", "0")?;
    insert(&mut headers, "x-stainless-runtime", "node")?;
    insert(&mut headers, "x-stainless-lang", "js")?;
    insert_default(&mut headers, "x-stainless-package-version", "0.94.0")?;
    insert_default(&mut headers, "x-stainless-runtime-version", "v26.3.0")?;
    insert_default(&mut headers, "x-stainless-os", "MacOS")?;
    insert_default(&mut headers, "x-stainless-arch", "arm64")?;
    insert(&mut headers, "x-claude-code-session-id", session_id)?;
    insert(&mut headers, "accept", "application/json")?;
    insert(&mut headers, "accept-encoding", "gzip, deflate, br, zstd")?;
    insert(&mut headers, "connection", "keep-alive")?;
    if !headers.contains_key("user-agent") {
        insert(
            &mut headers,
            "user-agent",
            request
                .metadata
                .user_agent
                .as_deref()
                .ok_or_else(|| invalid("Claude OAuth token count is missing User-Agent"))?,
        )?;
    }
    if !headers.contains_key("x-client-request-id") {
        insert(
            &mut headers,
            "x-client-request-id",
            &uuid::Uuid::new_v4().to_string(),
        )?;
    }
    insert(&mut headers, "anthropic-beta", COUNT_TOKENS_BETAS)?;
    Ok((body, headers))
}

pub(crate) fn parse_count_tokens_response(body: &[u8]) -> Result<u64, ProviderError> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|payload| payload.get("input_tokens").and_then(Value::as_u64))
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Upstream,
                "Claude OAuth token count response is invalid",
            )
        })
}

fn insert(headers: &mut HeaderMap, name: &'static str, value: &str) -> Result<(), ProviderError> {
    let value = HeaderValue::from_str(value)
        .map_err(|_| internal("invalid Claude OAuth token count header"))?;
    headers.insert(HeaderName::from_static(name), value);
    Ok(())
}

fn insert_default(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &str,
) -> Result<(), ProviderError> {
    if !headers.contains_key(name) {
        insert(headers, name, value)?;
    }
    Ok(())
}

fn invalid(message: &'static str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

fn internal(message: &'static str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Internal, message)
}
