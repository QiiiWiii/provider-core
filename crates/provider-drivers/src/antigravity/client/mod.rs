mod error;

#[cfg(test)]
mod tests;

use std::time::Duration;

use futures_util::TryStreamExt;
use provider_core::{
    ProviderError, ProviderErrorKind, ProviderFailoverReason, ProviderRequest, ProviderStream,
    collect_bounded_body, parse_provider_retry_after,
};
use secrecy::ExposeSecret;

use self::error::status_error;
use super::{
    contract::{API_BASE_URL, API_VERSION, DAILY_API_BASE_URL},
    credentials::AntigravityCredentials,
    request::{prepare_count_tokens_request, prepare_request},
    response::translate_stream,
    version,
};

const MAX_ERROR_RESPONSE_SIZE: usize = 256 * 1024;
const RESPONSE_HEADERS_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(crate) struct AntigravityClient {
    http: reqwest::Client,
    base_urls: Vec<String>,
    dynamic_version: bool,
}

impl AntigravityClient {
    pub(crate) fn new() -> Result<Self, reqwest::Error> {
        Self::with_base_urls_and_version(
            [DAILY_API_BASE_URL.to_owned(), API_BASE_URL.to_owned()],
            true,
        )
    }

    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn with_base_url(base_url: impl Into<String>) -> Result<Self, reqwest::Error> {
        Self::with_base_urls([base_url.into()])
    }

    #[cfg(any(test, feature = "test-util"))]
    fn with_base_urls<const N: usize>(base_urls: [String; N]) -> Result<Self, reqwest::Error> {
        Self::with_base_urls_and_version(base_urls, false)
    }

    fn with_base_urls_and_version<const N: usize>(
        base_urls: [String; N],
        dynamic_version: bool,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            http: reqwest::Client::builder()
                .http1_only()
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            base_urls: base_urls
                .into_iter()
                .map(|value| value.trim_end_matches('/').to_owned())
                .filter(|value| !value.is_empty())
                .collect(),
            dynamic_version,
        })
    }

    fn user_agent(&self) -> String {
        if self.dynamic_version {
            version::user_agent()
        } else {
            version::fallback_user_agent().to_owned()
        }
    }

    pub(crate) async fn execute_stream(
        &self,
        credentials: &AntigravityCredentials,
        request: &ProviderRequest,
    ) -> Result<ProviderStream, ProviderError> {
        let payload = prepare_request(request, credentials.project_id())?;
        let user_agent = self.user_agent();
        let mut last_error = None;
        for (index, base_url) in self.base_urls.iter().enumerate() {
            let url = format!("{base_url}/{API_VERSION}:streamGenerateContent?alt=sse");
            let response = tokio::time::timeout(
                RESPONSE_HEADERS_TIMEOUT,
                self.http
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .header(reqwest::header::ACCEPT, "text/event-stream")
                    .header(reqwest::header::USER_AGENT, &user_agent)
                    .bearer_auth(credentials.access_token().expose_secret())
                    .body(payload.clone())
                    .send(),
            )
            .await;
            let response = match response {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    let provider_error = ProviderError::new(
                        ProviderErrorKind::Upstream,
                        format!("Antigravity upstream request failed: {error}"),
                    );
                    if error.is_connect() && index + 1 < self.base_urls.len() {
                        last_error = Some(provider_error);
                        continue;
                    }
                    return Err(if error.is_connect() {
                        provider_error
                            .with_failover_reason(ProviderFailoverReason::PreconnectFailure)
                    } else {
                        provider_error
                    });
                }
                Err(_) => {
                    let error = ProviderError::new(
                        ProviderErrorKind::Upstream,
                        "Antigravity upstream response headers timed out",
                    )
                    .with_failover_reason(ProviderFailoverReason::CapacityExhausted);
                    if index + 1 < self.base_urls.len() {
                        last_error = Some(error);
                        continue;
                    }
                    return Err(error);
                }
            };
            let status = response.status();
            if !status.is_success() {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_provider_retry_after);
                let error = status_error(response, status).await;
                if index + 1 < self.base_urls.len() && should_try_fallback(status, &error) {
                    last_error = Some(error);
                    continue;
                }
                return Err(match retry_after {
                    Some(value) if error.retry_after().is_none() => error.with_retry_after(value),
                    _ => error,
                });
            }
            let stream = response.bytes_stream().map_err(|error| {
                ProviderError::new(
                    ProviderErrorKind::Upstream,
                    format!("Antigravity upstream stream failed: {error}"),
                )
            });
            return Ok(translate_stream(
                Box::pin(stream),
                request.model.clone(),
                &request.payload,
            ));
        }
        Err(last_error.unwrap_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Upstream,
                "Antigravity has no upstream endpoint",
            )
        }))
    }

    pub(crate) async fn count_tokens(
        &self,
        credentials: &AntigravityCredentials,
        request: &ProviderRequest,
    ) -> Result<u64, ProviderError> {
        let payload = prepare_count_tokens_request(request)?;
        let user_agent = self.user_agent();
        let mut last_error = None;
        for (index, base_url) in self.base_urls.iter().enumerate() {
            let url = format!("{base_url}/{API_VERSION}:countTokens");
            let response = tokio::time::timeout(
                RESPONSE_HEADERS_TIMEOUT,
                self.http
                    .post(url)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .header(reqwest::header::USER_AGENT, &user_agent)
                    .bearer_auth(credentials.access_token().expose_secret())
                    .body(payload.clone())
                    .send(),
            )
            .await;
            let response = match response {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    let provider_error = ProviderError::new(
                        ProviderErrorKind::Upstream,
                        format!("Antigravity countTokens request failed: {error}"),
                    );
                    if error.is_connect() && index + 1 < self.base_urls.len() {
                        last_error = Some(provider_error);
                        continue;
                    }
                    return Err(if error.is_connect() {
                        provider_error
                            .with_failover_reason(ProviderFailoverReason::PreconnectFailure)
                    } else {
                        provider_error
                    });
                }
                Err(_) => {
                    let error = ProviderError::new(
                        ProviderErrorKind::Upstream,
                        "Antigravity countTokens response headers timed out",
                    )
                    .with_failover_reason(ProviderFailoverReason::CapacityExhausted);
                    if index + 1 < self.base_urls.len() {
                        last_error = Some(error);
                        continue;
                    }
                    return Err(error);
                }
            };
            let status = response.status();
            if !status.is_success() {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_provider_retry_after);
                let error = status_error(response, status).await;
                if index + 1 < self.base_urls.len() && should_try_fallback(status, &error) {
                    last_error = Some(error);
                    continue;
                }
                return Err(match retry_after {
                    Some(value) if error.retry_after().is_none() => error.with_retry_after(value),
                    _ => error,
                });
            }
            let body = collect_bounded_body(response.bytes_stream(), MAX_ERROR_RESPONSE_SIZE)
                .await
                .map_err(|_| {
                    ProviderError::new(
                        ProviderErrorKind::Upstream,
                        "Antigravity countTokens response could not be read",
                    )
                })?;
            let value = serde_json::from_slice::<serde_json::Value>(&body).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Upstream,
                    "Antigravity countTokens response was not valid JSON",
                )
            })?;
            return value
                .get("totalTokens")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    ProviderError::new(
                        ProviderErrorKind::Upstream,
                        "Antigravity countTokens response is missing totalTokens",
                    )
                });
        }
        Err(last_error.unwrap_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Upstream,
                "Antigravity has no upstream endpoint",
            )
        }))
    }
}

fn should_try_fallback(status: reqwest::StatusCode, error: &ProviderError) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
        || error.failover_reason() == Some(ProviderFailoverReason::CapacityExhausted)
}
