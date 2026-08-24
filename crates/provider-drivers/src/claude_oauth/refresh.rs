use std::time::Duration;

use provider_core::{ProviderConfigurationError, RefreshError, RefreshErrorKind};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};

use super::{
    credentials::{ClaudeOAuthCredentials, unix_timestamp},
    oauth::{CLIENT_ID, SCOPE, axios_headers, inspect_profile, response_json},
};

const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";

#[derive(Clone)]
pub(crate) struct ClaudeRefreshClient {
    http: reqwest::Client,
    token_url: String,
    profile_url: String,
}

impl ClaudeRefreshClient {
    pub(crate) fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .http1_only()
                .build()
                .expect("Claude refresh HTTP client configuration must be valid"),
            token_url: TOKEN_URL.to_owned(),
            profile_url: PROFILE_URL.to_owned(),
        }
    }

    #[cfg(feature = "test-util")]
    pub(crate) fn with_token_url(token_url: &str) -> Self {
        let profile_url = format!("{}/profile", token_url.trim_end_matches("/token"));
        Self {
            http: reqwest::Client::new(),
            token_url: token_url.to_owned(),
            profile_url,
        }
    }

    pub(crate) async fn refresh(
        &self,
        credentials: &ClaudeOAuthCredentials,
    ) -> Result<ClaudeOAuthCredentials, RefreshError> {
        let request = RefreshRequest {
            client_id: CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token: credentials.refresh_token().expose_secret(),
            scope: SCOPE,
        };
        let body = serde_json::to_vec(&request).map_err(|_| {
            RefreshError::new(
                RefreshErrorKind::Internal,
                "failed to encode Claude OAuth refresh request",
            )
        })?;
        let response = axios_headers(
            self.http
                .post(&self.token_url)
                .timeout(Duration::from_secs(30))
                .body(body),
        )
        .send()
        .await
        .map_err(|_| {
            RefreshError::new(RefreshErrorKind::Transient, "Claude OAuth refresh failed")
        })?;
        let status = response.status();
        let tokens: RefreshResponse = response_json(response, "Claude OAuth refresh")
            .await
            .map_err(|error| {
                RefreshError::new(
                    if matches!(
                        status,
                        reqwest::StatusCode::BAD_REQUEST
                            | reqwest::StatusCode::UNAUTHORIZED
                            | reqwest::StatusCode::FORBIDDEN
                    ) {
                        RefreshErrorKind::ReauthRequired
                    } else {
                        RefreshErrorKind::Transient
                    },
                    error.to_string(),
                )
            })?;
        let refreshed_at = unix_timestamp();
        let access_token = required(tokens.access_token, "access_token")?;
        inspect_profile(&self.http, &self.profile_url, &access_token).await;
        credentials
            .refreshed(
                access_token,
                tokens.refresh_token.and_then(normalized),
                refreshed_at + tokens.expires_in.unwrap_or(3600).max(60),
                refreshed_at,
            )
            .map_err(|error| RefreshError::new(RefreshErrorKind::Internal, error.to_string()))
    }
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    client_id: &'static str,
    grant_type: &'static str,
    refresh_token: &'a str,
    scope: &'static str,
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

fn required(value: Option<String>, field: &str) -> Result<String, RefreshError> {
    value.and_then(normalized).ok_or_else(|| {
        RefreshError::new(
            RefreshErrorKind::Internal,
            ProviderConfigurationError::new(format!("Claude OAuth response is missing {field}"))
                .to_string(),
        )
    })
}

fn normalized(value: String) -> Option<String> {
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_body_matches_cpa_field_order() {
        let body = serde_json::to_string(&RefreshRequest {
            client_id: CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token: "refresh",
            scope: SCOPE,
        })
        .expect("refresh body");
        assert_eq!(
            body,
            format!(
                r#"{{"client_id":"{CLIENT_ID}","grant_type":"refresh_token","refresh_token":"refresh","scope":"{SCOPE}"}}"#
            )
        );
    }
}
