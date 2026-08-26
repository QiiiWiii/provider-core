use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use provider_core::{ProviderError, ProviderErrorKind};
use serde_json::{Value, json};

use super::super::errors::error_markers;

pub(super) fn convert_usage(value: &Value) -> Value {
    let input = value
        .get("promptTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let output = value
        .get("candidatesTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let reasoning = value
        .get("thoughtsTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    json!({
        "input_tokens": input,
        "output_tokens": output.saturating_add(reasoning),
        "total_tokens": value
            .get("totalTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| input.saturating_add(output).saturating_add(reasoning)),
        "input_tokens_details": {"cached_tokens": value.get("cachedContentTokenCount").and_then(Value::as_u64).unwrap_or_default()},
        "output_tokens_details": {"reasoning_tokens": reasoning}
    })
}

pub(super) fn normalize_response_id(value: &str) -> String {
    if value.starts_with("resp_") {
        value.to_owned()
    } else {
        format!("resp_{value}")
    }
}

pub(super) fn format_response_signature(signature: &str, target_claude: bool) -> String {
    if !target_claude || !signature.starts_with('R') {
        return signature.to_owned();
    }
    STANDARD
        .decode(signature)
        .ok()
        .and_then(|decoded| String::from_utf8(decoded).ok())
        .filter(|decoded| decoded.starts_with('E'))
        .unwrap_or_else(|| signature.to_owned())
}

pub(super) fn encode_reasoning_carrier(signature: &str, direction: &str, target: &str) -> String {
    format!(
        "cpa-gemini-responses-carrier-v1:{direction}:{target}:{}",
        STANDARD_NO_PAD.encode(signature.as_bytes())
    )
}

pub(super) fn custom_tool_input(value: &Value) -> String {
    let Some(input) = value.get("input") else {
        return value.to_string();
    };
    input
        .as_str()
        .map_or_else(|| input.to_string(), ToOwned::to_owned)
}

pub(super) fn incomplete_reason(finish_reason: Option<&str>) -> Option<&'static str> {
    let finish_reason = finish_reason
        .unwrap_or_default()
        .trim()
        .to_ascii_uppercase();
    match finish_reason.as_str() {
        "STOP" | "TOOL_CALLS" => None,
        "MAX_TOKENS" | "MAX_OUTPUT_TOKENS" | "LENGTH" => Some("max_output_tokens"),
        "SAFETY"
        | "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "RECITATION"
        | "SPII"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION"
        | "MODEL_ARMOR" => Some("content_filter"),
        "MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL" => Some("tool_error"),
        _ => Some("other"),
    }
}

pub(super) fn stream_error(error: &Value) -> ProviderError {
    let markers = error_markers(error);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("Antigravity upstream returned an error");
    let status = markers.iter().find_map(|marker| marker.parse::<u16>().ok());
    let quota_exhausted = markers
        .iter()
        .any(|marker| marker.contains("resource_exhausted") || marker.contains("quota_exhausted"));
    let authentication_error = markers.iter().any(|marker| {
        marker.contains("unauthenticated")
            || marker.contains("permission_denied")
            || marker.contains("invalid_credentials")
    });
    let mut provider_error = match status {
        Some(401 | 403) => ProviderError::new(ProviderErrorKind::Authentication, message)
            .with_failover_reason(provider_core::ProviderFailoverReason::AuthenticationExhausted),
        Some(429) if quota_exhausted => ProviderError::new(ProviderErrorKind::Capacity, message)
            .with_failover_reason(provider_core::ProviderFailoverReason::QuotaExhausted),
        Some(429) => ProviderError::new(ProviderErrorKind::RateLimited, message)
            .with_failover_reason(provider_core::ProviderFailoverReason::RateLimited),
        _ if quota_exhausted => ProviderError::new(ProviderErrorKind::Capacity, message)
            .with_failover_reason(provider_core::ProviderFailoverReason::QuotaExhausted),
        _ if authentication_error => ProviderError::new(ProviderErrorKind::Authentication, message)
            .with_failover_reason(provider_core::ProviderFailoverReason::AuthenticationExhausted),
        _ => ProviderError::new(ProviderErrorKind::Upstream, message),
    };
    if let Some(status) = status {
        provider_error = provider_error.with_upstream_status(status);
    }
    provider_error
}
