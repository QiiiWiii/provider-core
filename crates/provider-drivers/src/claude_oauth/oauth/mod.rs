use std::{sync::Arc, time::Duration};

mod callback;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use provider_core::{
    BoundedBodyError, PendingProviderOAuth, ProviderConfigurationError, ProviderOAuthCallback,
    ProviderOAuthChallenge, StartedProviderOAuth, collect_bounded_body,
};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    net::TcpListener,
    sync::{Mutex, mpsc},
    time::{Instant, sleep_until, timeout_at},
};

use super::credentials::{
    ClaudeOAuthCredentials, ClaudeOAuthIdentity, generate_device_id, unix_timestamp,
};
use super::{response::response_stream, transport::claude_http_client};
#[cfg(test)]
use callback::parse_callback_url;
use callback::{
    ClaudeCallbackResult, ClaudeCallbackSubmitter, parse_callback, random_base64, random_hex,
    read_callback_request, write_callback_response,
};

pub(crate) const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub(crate) const SCOPE: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const AUTH_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const ROLES_URL: &str = "https://api.anthropic.com/api/oauth/claude_cli/roles";
const REDIRECT_URI: &str = "http://localhost:54545/callback";
const CALLBACK_ADDRESS: &str = "0.0.0.0:54545";
const FLOW_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const RESPONSE_LIMIT: usize = 64 * 1024;

#[derive(Clone)]
pub(crate) struct ClaudeOAuthClient {
    http: reqwest::Client,
    token_url: String,
}

impl ClaudeOAuthClient {
    pub(crate) fn new() -> Self {
        Self {
            http: claude_http_client(),
            token_url: TOKEN_URL.to_owned(),
        }
    }

    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn with_token_url(token_url: &str) -> Self {
        Self {
            http: claude_http_client(),
            token_url: token_url.to_owned(),
        }
    }

    pub(crate) async fn start(&self) -> Result<StartedProviderOAuth, ProviderConfigurationError> {
        let listener = TcpListener::bind(CALLBACK_ADDRESS).await.ok();
        let (callback_tx, callback_rx) = mpsc::channel(1);
        let verifier = random_base64(96)?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_hex(16)?;
        let mut authorization_url = reqwest::Url::parse(AUTH_URL).map_err(|_| {
            ProviderConfigurationError::new("Claude OAuth authorization URL is invalid")
        })?;
        authorization_url.query_pairs_mut().extend_pairs([
            ("client_id", CLIENT_ID),
            ("code", "true"),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("redirect_uri", REDIRECT_URI),
            ("response_type", "code"),
            ("scope", SCOPE),
            ("state", state.as_str()),
        ]);
        let expires_at = unix_timestamp() + i64::try_from(FLOW_TIMEOUT.as_secs()).unwrap_or(300);
        let authorization_url = authorization_url.to_string();
        Ok(StartedProviderOAuth {
            challenge: ProviderOAuthChallenge {
                verification_uri: AUTH_URL.to_owned(),
                verification_uri_complete: Some(authorization_url),
                user_code: String::new(),
                expires_at,
                interval_seconds: 1,
            },
            pending: Box::new(ClaudePendingOAuth {
                http: self.http.clone(),
                token_url: self.token_url.clone(),
                listener,
                callback_rx: Mutex::new(callback_rx),
                callback: Arc::new(ClaudeCallbackSubmitter {
                    callback_tx,
                    state: state.clone(),
                }),
                verifier,
                state,
                deadline: Instant::now() + FLOW_TIMEOUT,
            }),
        })
    }
}

struct ClaudePendingOAuth {
    http: reqwest::Client,
    token_url: String,
    listener: Option<TcpListener>,
    callback_rx: Mutex<mpsc::Receiver<ClaudeCallbackResult>>,
    callback: Arc<ClaudeCallbackSubmitter>,
    verifier: String,
    state: String,
    deadline: Instant,
}

#[async_trait]
impl PendingProviderOAuth for ClaudePendingOAuth {
    async fn complete(self: Box<Self>) -> Result<SecretString, ProviderConfigurationError> {
        let callback = self.receive_callback().await?;
        let (code, state) = match callback {
            ClaudeCallbackResult::Success { code, state } => (code, state),
            ClaudeCallbackResult::Error { code, description } => {
                let detail = description
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| format!(": {value}"))
                    .unwrap_or_default();
                return Err(ProviderConfigurationError::new(format!(
                    "Claude OAuth authorization failed with {code}{detail}"
                )));
            }
        };
        let request = AuthorizationCodeRequest {
            grant_type: "authorization_code",
            code: &code,
            redirect_uri: REDIRECT_URI,
            client_id: CLIENT_ID,
            code_verifier: &self.verifier,
            state: &state,
        };
        let body = serde_json::to_vec(&request).map_err(|_| {
            ProviderConfigurationError::new("failed to encode Claude OAuth token request")
        })?;
        let response = axios_headers(
            self.http
                .post(&self.token_url)
                .timeout(Duration::from_secs(30))
                .body(body),
        )
        .send()
        .await
        .map_err(|_| ProviderConfigurationError::new("Claude OAuth token exchange failed"))?;
        let mut tokens: TokenResponse =
            response_json(response, "Claude OAuth token exchange").await?;
        if let Some(access_token) = tokens.access_token.as_deref() {
            if let Ok(profile) = control_plane_json::<ProfileResponse>(
                &self.http,
                PROFILE_URL,
                access_token,
                "Claude OAuth profile",
            )
            .await
            {
                if profile
                    .account
                    .uuid
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
                {
                    tokens.account.uuid = profile.account.uuid;
                }
                if profile
                    .account
                    .email
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
                {
                    tokens.account.email_address = profile.account.email;
                }
                if profile
                    .organization
                    .uuid
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
                {
                    tokens.organization.uuid = profile.organization.uuid;
                }
                if profile
                    .organization
                    .name
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
                {
                    tokens.organization.name = profile.organization.name;
                }
            }
            let _ = control_plane_json::<serde_json::Value>(
                &self.http,
                ROLES_URL,
                access_token,
                "Claude OAuth roles",
            )
            .await;
        }
        let refreshed_at = unix_timestamp();
        let credentials = ClaudeOAuthCredentials::from_parts_with_identity(
            required(tokens.access_token, "access_token")?,
            required(tokens.refresh_token, "refresh_token")?,
            ClaudeOAuthIdentity {
                account_uuid: Some(required(tokens.account.uuid, "account.uuid")?),
                email: tokens.account.email_address.and_then(normalized),
                organization_uuid: tokens.organization.uuid.and_then(normalized),
                organization_name: tokens.organization.name.and_then(normalized),
            },
            generate_device_id()
                .map_err(|error| ProviderConfigurationError::new(error.to_string()))?,
            refreshed_at + tokens.expires_in.unwrap_or(3600).max(60),
            refreshed_at,
        )
        .map_err(|error| ProviderConfigurationError::new(error.to_string()))?;
        credentials
            .to_json()
            .map_err(|error| ProviderConfigurationError::new(error.to_string()))
    }

    fn callback_handle(&self) -> Option<Arc<dyn ProviderOAuthCallback>> {
        Some(self.callback.clone())
    }
}

impl ClaudePendingOAuth {
    async fn receive_callback(&self) -> Result<ClaudeCallbackResult, ProviderConfigurationError> {
        let mut receiver = self.callback_rx.lock().await;
        loop {
            if let Some(listener) = &self.listener {
                tokio::select! {
                    submitted = receiver.recv() => {
                        return submitted.ok_or_else(|| ProviderConfigurationError::new("Claude OAuth callback is unavailable"));
                    }
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted.map_err(|_| ProviderConfigurationError::new("Claude OAuth callback failed"))?;
                        let callback = match read_callback_request(&mut stream, self.deadline).await {
                            Ok(request) => parse_callback(&request, &self.state),
                            Err(error) => Err(error),
                        };
                        let accepted = matches!(callback, Ok(ClaudeCallbackResult::Success { .. }));
                        write_callback_response(&mut stream, accepted).await;
                        if let Ok(callback) = callback {
                            return Ok(callback);
                        }
                    }
                    _ = sleep_until(self.deadline) => {
                        return Err(ProviderConfigurationError::new("Claude OAuth authorization expired"));
                    }
                }
            } else {
                return timeout_at(self.deadline, receiver.recv())
                    .await
                    .map_err(|_| {
                        ProviderConfigurationError::new("Claude OAuth authorization expired")
                    })?
                    .ok_or_else(|| {
                        ProviderConfigurationError::new("Claude OAuth callback is unavailable")
                    });
            }
        }
    }
}

#[derive(Serialize)]
struct AuthorizationCodeRequest<'a> {
    grant_type: &'static str,
    code: &'a str,
    redirect_uri: &'static str,
    client_id: &'static str,
    code_verifier: &'a str,
    state: &'a str,
}

#[derive(Deserialize, Default)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    #[serde(default)]
    account: TokenAccount,
    #[serde(default)]
    organization: TokenOrganization,
}

#[derive(Deserialize, Default)]
struct TokenAccount {
    uuid: Option<String>,
    email_address: Option<String>,
}

#[derive(Deserialize, Default)]
struct TokenOrganization {
    uuid: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize, Default)]
pub(crate) struct ProfileResponse {
    #[serde(default)]
    pub(crate) account: ProfileAccount,
    #[serde(default)]
    pub(crate) organization: ProfileOrganization,
}

#[derive(Deserialize, Default)]
pub(crate) struct ProfileAccount {
    pub(crate) uuid: Option<String>,
    pub(crate) email: Option<String>,
}

#[derive(Deserialize, Default)]
pub(crate) struct ProfileOrganization {
    pub(crate) uuid: Option<String>,
    pub(crate) name: Option<String>,
}

pub(crate) fn axios_headers(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request
        .header(reqwest::header::ACCEPT, "application/json, text/plain, */*")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::USER_AGENT, "axios/1.15.2")
        .header(
            reqwest::header::ACCEPT_ENCODING,
            "gzip, compress, deflate, br",
        )
        .header(reqwest::header::CONNECTION, "close")
}

pub(crate) async fn response_json<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
    operation: &str,
) -> Result<T, ProviderConfigurationError> {
    let status = response.status();
    let stream = response_stream(response).await.map_err(|_| {
        ProviderConfigurationError::new(format!("failed to read {operation} response"))
    })?;
    let body = collect_bounded_body(stream, RESPONSE_LIMIT)
        .await
        .map_err(|error| match error {
            BoundedBodyError::Read(_) => {
                ProviderConfigurationError::new(format!("failed to read {operation} response"))
            }
            BoundedBodyError::TooLarge => {
                ProviderConfigurationError::new(format!("{operation} response was too large"))
            }
        })?;
    if !status.is_success() {
        return Err(ProviderConfigurationError::new(format!(
            "{operation} returned HTTP {status}"
        )));
    }
    serde_json::from_slice(&body)
        .map_err(|_| ProviderConfigurationError::new(format!("{operation} returned invalid JSON")))
}

async fn control_plane_json<T: for<'de> Deserialize<'de>>(
    http: &reqwest::Client,
    url: &str,
    access_token: &str,
    operation: &str,
) -> Result<T, ProviderConfigurationError> {
    let response = axios_headers(
        http.get(url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {access_token}"),
            )
            .header(reqwest::header::CACHE_CONTROL, "no-cache")
            .timeout(Duration::from_secs(15)),
    )
    .send()
    .await
    .map_err(|_| ProviderConfigurationError::new(format!("{operation} request failed")))?;
    response_json(response, operation).await
}

pub(crate) async fn inspect_profile(
    http: &reqwest::Client,
    url: &str,
    access_token: &str,
) -> Option<ProfileResponse> {
    control_plane_json::<ProfileResponse>(http, url, access_token, "Claude OAuth profile")
        .await
        .ok()
}

fn required(value: Option<String>, field: &str) -> Result<String, ProviderConfigurationError> {
    value.and_then(normalized).ok_or_else(|| {
        ProviderConfigurationError::new(format!("Claude OAuth response is missing {field}"))
    })
}

fn normalized(value: String) -> Option<String> {
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests;
