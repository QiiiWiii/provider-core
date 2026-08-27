mod billing;
mod cch;
mod headers;
mod identity;
mod json;

use bytes::Bytes;
use provider_core::{ProviderError, ProviderErrorKind, ProviderRequest, RequestClient};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use secrecy::ExposeSecret;

use super::credentials::ClaudeOAuthCredentials;
use billing::fallback_billing;
use cch::ensure_and_sign_cch;
use headers::{insert, insert_default};
use identity::{oauth_betas, rebuild_identity};
use json::{splice, unique_object_member};

pub(crate) fn prepare_request(
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
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "Claude OAuth request is missing session identity",
            )
        })?;
    let caller_user_id = request
        .metadata
        .claude_code_user_id
        .as_deref()
        .ok_or_else(|| invalid("Claude OAuth request is missing metadata.user_id"))?;
    let identity = rebuild_identity(caller_user_id, credentials, session_id)?;
    let encoded_identity = serde_json::to_vec(&identity)
        .map_err(|_| internal("failed to encode Claude OAuth metadata"))?;
    let payload = request
        .metadata
        .claude_code_payload
        .as_deref()
        .ok_or_else(|| invalid("Claude OAuth request is missing the native payload"))?;
    let metadata = unique_object_member(payload, 0, "metadata")
        .ok_or_else(|| invalid("Claude OAuth request is missing metadata"))?;
    let user_id = unique_object_member(payload, metadata.start, "user_id")
        .ok_or_else(|| invalid("Claude OAuth request is missing metadata.user_id"))?;
    let mut body = splice(payload, user_id, &encoded_identity);
    let billing = (!request.metadata.claude_code_helper_profile)
        .then(|| fallback_billing(request, payload))
        .transpose()?;
    body = ensure_and_sign_cch(body, billing.as_deref())?;

    let mut headers = HeaderMap::new();
    for (name, value) in &request.metadata.claude_code_headers {
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
    insert_default(&mut headers, "x-stainless-timeout", "600")?;
    insert_default(&mut headers, "x-stainless-package-version", "0.94.0")?;
    insert_default(&mut headers, "x-stainless-runtime-version", "v26.3.0")?;
    insert_default(&mut headers, "x-stainless-os", "MacOS")?;
    insert_default(&mut headers, "x-stainless-arch", "arm64")?;
    insert(&mut headers, "x-claude-code-session-id", session_id)?;
    insert(&mut headers, "accept", "application/json")?;
    if request.metadata.claude_code_helper_profile {
        insert_default(&mut headers, "accept-encoding", "gzip")?;
    } else {
        insert(&mut headers, "accept-encoding", "gzip, deflate, br, zstd")?;
    }
    insert(&mut headers, "connection", "keep-alive")?;
    if !headers.contains_key("x-client-request-id") {
        insert(
            &mut headers,
            "x-client-request-id",
            &uuid::Uuid::new_v4().to_string(),
        )?;
    }
    let beta = oauth_betas(
        request
            .metadata
            .claude_code_beta
            .as_deref()
            .unwrap_or_default(),
        request.metadata.claude_code_helper_profile,
    );
    insert(&mut headers, "anthropic-beta", &beta)?;
    Ok((Bytes::from(body), headers))
}

fn invalid(message: &'static str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

fn internal(message: &'static str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Internal, message)
}

#[cfg(test)]
mod tests;
