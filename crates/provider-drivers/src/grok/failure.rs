use provider_core::{
    BoundedBodyError, ProviderError, ProviderErrorKind, ProviderRetryHint, collect_bounded_body,
};
use reqwest::StatusCode;
use serde_json::Value;
use std::time::Duration;

const MAX_ERROR_RESPONSE_SIZE: usize = 64 * 1024;
const MAX_ERROR_DETAIL_CHARS: usize = 512;

pub(super) async fn status_error(response: reqwest::Response, status: StatusCode) -> ProviderError {
    let (code, detail, corpus, body_issue) = match read_error_body(response).await {
        Ok(body) => {
            let (code, detail) = grok_error_fields(&body);
            let corpus = String::from_utf8_lossy(&body).to_ascii_lowercase();
            (code, detail, corpus, None)
        }
        Err(issue) => (None, None, String::new(), Some(issue)),
    };
    let bad_credentials = status == StatusCode::FORBIDDEN
        && (code.as_deref() == Some("unauthenticated:bad-credentials")
            || detail.as_deref().is_some_and(|message| {
                message
                    .to_ascii_lowercase()
                    .contains("oauth2 access token could not be validated")
            }));
    let free_usage_exhausted = contains_any(
        &corpus,
        &[
            "free-usage-exhausted",
            "free_usage_exhausted",
            "included free usage",
            "used all the included free",
            "usage-limit-exceeded",
            "free tier limit",
        ],
    );
    let billing_or_entitlement =
        is_billing_or_entitlement(status, code.as_deref(), detail.as_deref(), &corpus);
    let capacity = contains_any(
        &corpus,
        &[
            "model capacity",
            "overloaded",
            "temporarily unavailable",
            "empty model output",
            "no content/tool_calls",
        ],
    );
    let body_rate_limit = contains_any(&corpus, &["rate limit", "rate_limit", "too many requests"]);
    let effective_status = if bad_credentials {
        StatusCode::UNAUTHORIZED
    } else {
        status
    };
    let kind = if free_usage_exhausted || billing_or_entitlement || body_rate_limit {
        ProviderErrorKind::RateLimited
    } else if capacity {
        ProviderErrorKind::Upstream
    } else {
        match effective_status {
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
                ProviderErrorKind::InvalidRequest
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ProviderErrorKind::Authentication,
            StatusCode::TOO_MANY_REQUESTS => ProviderErrorKind::RateLimited,
            _ => ProviderErrorKind::Upstream,
        }
    };

    let message = match body_issue {
        Some(ErrorBodyIssue::ReadFailed) => {
            format!("Grok upstream returned HTTP {status} with an unreadable error response")
        }
        Some(ErrorBodyIssue::TooLarge) => {
            format!("Grok upstream returned HTTP {status} with an oversized error response")
        }
        None if !matches!(kind, ProviderErrorKind::Authentication) => {
            detail.as_deref().map_or_else(
                || format!("Grok upstream returned HTTP {status}"),
                |detail| format!("Grok upstream returned HTTP {status}: {detail}"),
            )
        }
        None => format!("Grok upstream returned HTTP {status}"),
    };
    let mut error =
        ProviderError::new(kind, message).with_upstream_status(effective_status.as_u16());
    if is_invalid_encrypted_content(status, code.as_deref(), detail.as_deref()) {
        error = error.with_retry_hint(ProviderRetryHint::StripEncryptedReasoning);
    }
    if free_usage_exhausted {
        return error
            .with_failover_reason(provider_core::ProviderFailoverReason::QuotaExhausted)
            .with_reviewed_retry_after(Duration::from_secs(24 * 60 * 60));
    }
    if billing_or_entitlement {
        return error
            .with_failover_reason(provider_core::ProviderFailoverReason::QuotaExhausted)
            .with_reviewed_retry_after(Duration::from_secs(30 * 60));
    }
    if status == StatusCode::TOO_MANY_REQUESTS || body_rate_limit {
        return error.with_failover_reason(provider_core::ProviderFailoverReason::RateLimited);
    }
    if status.is_server_error() || capacity {
        return error
            .with_failover_reason(provider_core::ProviderFailoverReason::CapacityExhausted);
    }
    error
}

fn contains_any(corpus: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| corpus.contains(needle))
}

fn is_billing_or_entitlement(
    status: StatusCode,
    code: Option<&str>,
    detail: Option<&str>,
    corpus: &str,
) -> bool {
    if status == StatusCode::PAYMENT_REQUIRED
        || matches!(
            code,
            Some(
                "subscription_required" | "entitlement_required" | "not_entitled" | "plan_required"
            )
        )
        || contains_any(
            corpus,
            &[
                "subscription required",
                "spending-limit",
                "spending limit",
                "payment required",
                "billing quota",
                "insufficient credits",
                "account suspended",
                "team suspended",
            ],
        )
    {
        return true;
    }
    if code != Some("permission_denied") {
        return false;
    }
    let detail = detail.unwrap_or_default().to_ascii_lowercase();
    contains_any(
        &detail,
        &[
            "account access",
            "access to this account",
            "model access",
            "access to this model",
            "plan entitlement",
            "entitled under your plan",
            "not included in your plan",
        ],
    )
}

#[derive(Clone, Copy)]
enum ErrorBodyIssue {
    ReadFailed,
    TooLarge,
}

async fn read_error_body(response: reqwest::Response) -> Result<Vec<u8>, ErrorBodyIssue> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ERROR_RESPONSE_SIZE as u64)
    {
        return Err(ErrorBodyIssue::TooLarge);
    }
    collect_bounded_body(response.bytes_stream(), MAX_ERROR_RESPONSE_SIZE)
        .await
        .map(|body| body.to_vec())
        .map_err(|error| match error {
            BoundedBodyError::Read(_) => ErrorBodyIssue::ReadFailed,
            BoundedBodyError::TooLarge => ErrorBodyIssue::TooLarge,
        })
}

fn grok_error_fields(body: &[u8]) -> (Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return (None, None);
    };
    let code = value
        .get("code")
        .or_else(|| value.pointer("/error/code"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);
    let detail = [
        value.pointer("/error/message"),
        value.pointer("/error/error"),
        value.get("error"),
        value.get("message"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str)
    .map(sanitize_error_detail)
    .filter(|value| !value.is_empty());
    (code, detail)
}

fn sanitize_error_detail(value: &str) -> String {
    let cleaned = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.chars().count() <= MAX_ERROR_DETAIL_CHARS {
        cleaned
    } else {
        let mut truncated = cleaned
            .chars()
            .take(MAX_ERROR_DETAIL_CHARS)
            .collect::<String>();
        truncated.push_str("...");
        truncated
    }
}

fn is_invalid_encrypted_content(
    status: StatusCode,
    code: Option<&str>,
    detail: Option<&str>,
) -> bool {
    if status != StatusCode::BAD_REQUEST {
        return false;
    }
    if code == Some("invalid_encrypted_content") {
        return true;
    }
    if code.is_some_and(|code| code != "invalid-argument") {
        return false;
    }
    let detail = detail.unwrap_or_default().to_ascii_lowercase();
    detail.contains("encrypted_content")
        && (detail.contains("decrypt") || detail.contains("unmodified"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_explicit_entitlement_codes() {
        for code in ["entitlement_required", "not_entitled", "plan_required"] {
            assert!(is_billing_or_entitlement(
                StatusCode::FORBIDDEN,
                Some(code),
                None,
                "",
            ));
        }
    }

    #[test]
    fn permission_denied_requires_explicit_entitlement_context() {
        assert!(is_billing_or_entitlement(
            StatusCode::FORBIDDEN,
            Some("permission_denied"),
            Some("Your plan does not include access to this model."),
            "",
        ));
        assert!(is_billing_or_entitlement(
            StatusCode::FORBIDDEN,
            Some("permission_denied"),
            Some("Missing account access for this model."),
            "",
        ));
        assert!(!is_billing_or_entitlement(
            StatusCode::FORBIDDEN,
            Some("permission_denied"),
            Some("The request was rejected by the content policy."),
            "permission_denied content policy violation",
        ));
    }
}
