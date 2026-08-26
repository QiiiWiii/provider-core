use std::time::{SystemTime, UNIX_EPOCH};

use provider_core::{ProviderQuotaError, ProviderQuotaErrorKind, collect_bounded_body};
use serde_json::Value;

use super::MAX_RESPONSE_SIZE;

pub(super) async fn status_error(
    response: reqwest::Response,
    status: reqwest::StatusCode,
) -> ProviderQuotaError {
    let body = match collect_bounded_body(response.bytes_stream(), MAX_RESPONSE_SIZE).await {
        Ok(body) => body,
        Err(_) => {
            return ProviderQuotaError::new(
                ProviderQuotaErrorKind::Upstream,
                "failed to read Antigravity quota error response",
            )
            .with_upstream_status(status.as_u16());
        }
    };
    let message = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| value.pointer("/error/message").and_then(Value::as_str))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| {
            std::str::from_utf8(&body)
                .ok()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| format!("Antigravity quota returned HTTP {status}"));
    let kind = match status.as_u16() {
        401 | 403 => ProviderQuotaErrorKind::Authentication,
        429 => ProviderQuotaErrorKind::RateLimited,
        _ => ProviderQuotaErrorKind::Upstream,
    };
    ProviderQuotaError::new(kind, message).with_upstream_status(status.as_u16())
}

pub(super) fn invalid_response(message: impl Into<String>) -> ProviderQuotaError {
    ProviderQuotaError::new(ProviderQuotaErrorKind::InvalidResponse, message)
}

pub(super) fn upstream_error(message: impl Into<String>) -> ProviderQuotaError {
    ProviderQuotaError::new(ProviderQuotaErrorKind::Upstream, message)
}

pub(super) fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}
