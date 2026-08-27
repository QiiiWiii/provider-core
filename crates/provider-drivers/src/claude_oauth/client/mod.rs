use bytes::Bytes;
use provider_core::{
    ProviderError, ProviderErrorKind, ProviderStream, collect_bounded_body,
    parse_provider_retry_after,
};
use reqwest::header::HeaderMap;

use super::{
    contract::{API_ROOT, COUNT_TOKENS_RESPONSE_LIMIT, ERROR_RESPONSE_LIMIT},
    count_tokens::parse_count_tokens_response,
    response::response_stream,
    transport::claude_http_client,
};

pub(super) struct ClaudeInferenceClient {
    http: reqwest::Client,
    api_root: String,
}

impl ClaudeInferenceClient {
    pub(super) fn new() -> Self {
        Self {
            http: claude_http_client(),
            api_root: API_ROOT.to_owned(),
        }
    }

    #[cfg(any(test, feature = "test-util"))]
    pub(super) fn with_api_root(api_root: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_root: api_root.trim_end_matches('/').to_owned(),
        }
    }

    pub(super) async fn execute_stream(
        &self,
        body: Bytes,
        headers: HeaderMap,
    ) -> Result<ProviderStream, ProviderError> {
        let response = self
            .http
            .post(format!("{}/messages?beta=true", self.api_root))
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|error| {
                upstream_request_error(error, "Claude OAuth upstream request failed")
            })?;
        if !response.status().is_success() {
            return Err(status_error(response).await);
        }
        response_stream(response).await
    }

    pub(super) async fn count_tokens(
        &self,
        body: Bytes,
        headers: HeaderMap,
    ) -> Result<u64, ProviderError> {
        let response = self
            .http
            .post(format!("{}/messages/count_tokens?beta=true", self.api_root))
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|error| {
                upstream_request_error(error, "Claude OAuth token count request failed")
            })?;
        if !response.status().is_success() {
            return Err(status_error(response).await);
        }
        let body = collect_bounded_body(
            response_stream(response).await?,
            COUNT_TOKENS_RESPONSE_LIMIT,
        )
        .await
        .map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Upstream,
                "Claude OAuth token count response could not be read",
            )
        })?;
        parse_count_tokens_response(&body)
    }
}

fn upstream_request_error(error: reqwest::Error, message: &'static str) -> ProviderError {
    let provider_error = ProviderError::new(ProviderErrorKind::Upstream, message);
    if error.is_connect() {
        provider_error
            .with_failover_reason(provider_core::ProviderFailoverReason::PreconnectFailure)
    } else {
        provider_error
    }
}

pub(super) async fn status_error(response: reqwest::Response) -> ProviderError {
    let status = response.status();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_provider_retry_after);
    let kind = match status.as_u16() {
        400 | 422 => ProviderErrorKind::InvalidRequest,
        401 | 403 => ProviderErrorKind::Authentication,
        429 => ProviderErrorKind::RateLimited,
        _ => ProviderErrorKind::Upstream,
    };
    let body = match response_stream(response).await {
        Ok(stream) => collect_bounded_body(stream, ERROR_RESPONSE_LIMIT)
            .await
            .ok()
            .filter(|body| !body.is_empty()),
        Err(_) => None,
    };
    let error = ProviderError::new(kind, format!("Claude OAuth returned HTTP {status}"))
        .with_upstream_status(status.as_u16());
    let error = match body {
        Some(body) => error.with_upstream_body(body),
        None => error,
    };
    let error = match status.as_u16() {
        402 => error.with_failover_reason(provider_core::ProviderFailoverReason::QuotaExhausted),
        429 => error.with_failover_reason(provider_core::ProviderFailoverReason::RateLimited),
        _ => error,
    };
    match retry_after {
        Some(value) => error.with_retry_after(value),
        None => error,
    }
}
