use std::time::Duration;

use provider_core::{BoundedBodyError, RefreshError, RefreshErrorKind, collect_bounded_body};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use super::{
    contract::TOKEN_ENDPOINT, credentials::AntigravityCredentials,
    oauth_config::AntigravityOAuthConfig,
};

const MAX_RESPONSE_SIZE: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct RefreshedTokens {
    pub(crate) access_token: SecretString,
    pub(crate) refresh_token: Option<SecretString>,
    pub(crate) expires_in: i64,
}

#[derive(Clone)]
pub(crate) struct AntigravityRefreshClient {
    http: reqwest::Client,
    token_endpoint: String,
    oauth_config: AntigravityOAuthConfig,
}

impl AntigravityRefreshClient {
    pub(crate) fn new(oauth_config: AntigravityOAuthConfig) -> Result<Self, reqwest::Error> {
        Ok(Self {
            http: reqwest::Client::builder().http1_only().build()?,
            token_endpoint: TOKEN_ENDPOINT.to_owned(),
            oauth_config,
        })
    }

    pub(crate) async fn refresh(
        &self,
        credentials: &AntigravityCredentials,
    ) -> Result<RefreshedTokens, RefreshError> {
        let refresh_token = credentials.refresh_token().ok_or_else(|| {
            RefreshError::new(
                RefreshErrorKind::ReauthRequired,
                "Antigravity credential is missing refresh_token",
            )
        })?;
        let (client_id, client_secret) = self
            .oauth_config
            .credentials()
            .map_err(|message| RefreshError::new(RefreshErrorKind::Internal, message))?;
        let response = self
            .http
            .post(&self.token_endpoint)
            .timeout(Duration::from_secs(30))
            .form(&[
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("refresh_token", refresh_token.expose_secret()),
                ("grant_type", "refresh_token"),
            ])
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, "Go-http-client/2.0")
            .send()
            .await
            .map_err(|_| {
                RefreshError::new(
                    RefreshErrorKind::Transient,
                    "Antigravity token refresh request failed",
                )
            })?;
        let status = response.status();
        let body = collect_bounded_body(response.bytes_stream(), MAX_RESPONSE_SIZE)
            .await
            .map_err(|error| match error {
                BoundedBodyError::Read(_) => RefreshError::new(
                    RefreshErrorKind::Transient,
                    "failed to read Antigravity token refresh response",
                ),
                BoundedBodyError::TooLarge => RefreshError::new(
                    RefreshErrorKind::Transient,
                    "Antigravity token refresh response was too large",
                ),
            })?;
        if !status.is_success() {
            let error_code = serde_json::from_slice::<OAuthErrorResponse>(&body)
                .ok()
                .and_then(|value| value.error);
            let kind = match error_code.as_deref() {
                Some("invalid_grant" | "invalid_token" | "unauthorized_client") | None
                    if status == reqwest::StatusCode::UNAUTHORIZED
                        || status == reqwest::StatusCode::BAD_REQUEST =>
                {
                    RefreshErrorKind::ReauthRequired
                }
                _ if status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status.is_server_error() =>
                {
                    RefreshErrorKind::Transient
                }
                _ => RefreshErrorKind::Internal,
            };
            return Err(RefreshError::new(
                kind,
                format!("Antigravity token refresh returned HTTP {status}"),
            ));
        }
        let response: TokenResponse = serde_json::from_slice(&body).map_err(|_| {
            RefreshError::new(
                RefreshErrorKind::Transient,
                "Antigravity token refresh returned invalid JSON",
            )
        })?;
        let access_token = non_empty_secret(response.access_token).ok_or_else(|| {
            RefreshError::new(
                RefreshErrorKind::Transient,
                "Antigravity token refresh response is missing access_token",
            )
        })?;
        let expires_in = response
            .expires_in
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                RefreshError::new(
                    RefreshErrorKind::Transient,
                    "Antigravity token refresh response has invalid expires_in",
                )
            })?;
        Ok(RefreshedTokens {
            access_token,
            refresh_token: non_empty_secret(response.refresh_token),
            expires_in,
        })
    }
}

fn non_empty_secret(value: Option<String>) -> Option<SecretString> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(SecretString::from)
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

#[derive(Deserialize)]
struct OAuthErrorResponse {
    error: Option<String>,
}
