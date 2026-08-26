use provider_core::{
    ProviderError, ProviderErrorKind, ProviderFailoverReason, collect_bounded_body,
};

use super::super::errors::error_markers;
use super::MAX_ERROR_RESPONSE_SIZE;

pub(super) async fn status_error(
    response: reqwest::Response,
    status: reqwest::StatusCode,
) -> ProviderError {
    let body = match collect_bounded_body(response.bytes_stream(), MAX_ERROR_RESPONSE_SIZE).await {
        Ok(body) => body,
        Err(_) => {
            return ProviderError::new(
                ProviderErrorKind::Upstream,
                "failed to read Antigravity upstream error response",
            )
            .with_upstream_status(status.as_u16());
        }
    };
    let parsed = serde_json::from_slice::<serde_json::Value>(&body).ok();
    let markers = parsed.as_ref().map(error_markers).unwrap_or_default();
    let message = parsed
        .as_ref()
        .and_then(|value| value.get("message").or_else(|| value.get("error")))
        .and_then(|value| value.as_str().or_else(|| value.get("message")?.as_str()))
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("Antigravity upstream request failed");
    let mut error = match status.as_u16() {
        401 => ProviderError::new(ProviderErrorKind::Authentication, message)
            .with_failover_reason(ProviderFailoverReason::AuthenticationExhausted),
        403 if markers.iter().any(|marker| {
            marker.contains("subscription")
                || marker.contains("unauthenticated")
                || marker.contains("permission_denied")
                || marker.contains("invalid_credentials")
        }) =>
        {
            ProviderError::new(ProviderErrorKind::Authentication, message)
                .with_failover_reason(ProviderFailoverReason::AuthenticationExhausted)
        }
        429 if markers.iter().any(|marker| {
            marker.contains("quota_exhausted")
                || marker.contains("resource_exhausted")
                || marker.contains("insufficient_g1_credits_balance")
                || marker.contains("quota exhausted")
        }) =>
        {
            ProviderError::new(ProviderErrorKind::Capacity, message)
                .with_failover_reason(ProviderFailoverReason::QuotaExhausted)
        }
        429 => ProviderError::new(ProviderErrorKind::RateLimited, message)
            .with_failover_reason(ProviderFailoverReason::RateLimited),
        500..=599 => ProviderError::new(ProviderErrorKind::Capacity, message)
            .with_failover_reason(ProviderFailoverReason::CapacityExhausted),
        400..=499 => ProviderError::new(ProviderErrorKind::InvalidRequest, message),
        _ => ProviderError::new(ProviderErrorKind::Upstream, message),
    };
    error = error.with_upstream_status(status.as_u16());
    error
}
