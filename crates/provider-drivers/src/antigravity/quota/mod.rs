mod error;
mod parser;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

use std::time::Duration;

use provider_core::{
    BoundedBodyError, ProviderQuotaError, ProviderQuotaErrorKind, ProviderQuotaSnapshot,
    collect_bounded_body,
};
use secrecy::ExposeSecret;
use serde_json::{Map, Value};

use self::{
    error::{status_error, upstream_error},
    parser::{normalize_quota, parse_available_model_quotas},
};
use super::{contract::API_BASE_URL, credentials::AntigravityCredentials, version};

const MAX_RESPONSE_SIZE: usize = 256 * 1024;
const QUOTA_TIMEOUT: Duration = Duration::from_secs(15);
const FETCH_AVAILABLE_MODELS_PATH: &str = "/v1internal:fetchAvailableModels";

#[derive(Clone)]
pub(crate) struct AntigravityQuotaClient {
    http: reqwest::Client,
    base_url: String,
}

impl AntigravityQuotaClient {
    pub(crate) fn new() -> Result<Self, reqwest::Error> {
        Self::with_base_url(API_BASE_URL)
    }

    pub(crate) fn with_base_url(base_url: impl Into<String>) -> Result<Self, reqwest::Error> {
        Ok(Self {
            http: reqwest::Client::builder().http1_only().build()?,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        })
    }

    pub(crate) async fn fetch(
        &self,
        account_id: &str,
        credentials: &AntigravityCredentials,
    ) -> Result<ProviderQuotaSnapshot, ProviderQuotaError> {
        let payload = self
            .fetch_payload(
                FETCH_AVAILABLE_MODELS_PATH,
                project_request_body(credentials.project_id()),
                credentials,
            )
            .await?;
        normalize_quota(account_id, parse_available_model_quotas(&payload)?)
    }

    async fn fetch_payload(
        &self,
        path: &str,
        request_body: Value,
        credentials: &AntigravityCredentials,
    ) -> Result<Value, ProviderQuotaError> {
        let body = serde_json::to_vec(&request_body)
            .map_err(|_| upstream_error("failed to encode Antigravity quota request"))?;
        let response = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .timeout(QUOTA_TIMEOUT)
            .header(reqwest::header::ACCEPT, "*/*")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::USER_AGENT, version::user_agent())
            .bearer_auth(credentials.access_token().expose_secret())
            .body(body)
            .send()
            .await
            .map_err(|_| upstream_error("Antigravity quota request failed"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(response, status).await);
        }
        let body = collect_bounded_body(response.bytes_stream(), MAX_RESPONSE_SIZE)
            .await
            .map_err(|error| match error {
                BoundedBodyError::Read(_) => {
                    upstream_error("failed to read Antigravity quota response")
                }
                BoundedBodyError::TooLarge => ProviderQuotaError::new(
                    ProviderQuotaErrorKind::InvalidResponse,
                    "Antigravity quota response was too large",
                ),
            })?;
        serde_json::from_slice::<Value>(&body).map_err(|error| {
            ProviderQuotaError::new(
                ProviderQuotaErrorKind::InvalidResponse,
                format!("Antigravity quota returned invalid JSON: {error}"),
            )
        })
    }
}

fn project_request_body(project_id: &str) -> Value {
    let project_id = project_id.trim();
    if project_id.is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::json!({"project": project_id})
    }
}
