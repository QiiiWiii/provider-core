#[path = "callback.rs"]
mod callback;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use provider_core::{
    BoundedBodyError, PendingProviderOAuth, ProviderConfigurationError, ProviderOAuthCallback,
    ProviderOAuthChallenge, StartedProviderOAuth, collect_bounded_body,
};
use secrecy::SecretString;
use serde::Deserialize;
use serde_json::Value;
use tokio::{
    net::TcpListener,
    sync::{Mutex, mpsc},
    time::{Instant, sleep, sleep_until, timeout_at},
};

use self::callback::{
    parse_callback_request, parse_callback_url, read_callback_request, write_callback_response,
};
use super::{
    contract::{
        API_BASE_URL, API_VERSION, AUTH_ENDPOINT, CALLBACK_ADDRESS, DAILY_API_BASE_URL,
        GOOG_API_CLIENT, REDIRECT_URI, SCOPES, TOKEN_ENDPOINT, USERINFO_ENDPOINT,
    },
    credentials::AntigravityCredentials,
    oauth_config::AntigravityOAuthConfig,
    version,
};

const FLOW_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_LIMIT: usize = 1024 * 1024;

#[derive(Clone)]
pub(crate) struct AntigravityOAuthClient {
    http: reqwest::Client,
    token_endpoint: String,
    userinfo_endpoint: String,
    api_base_url: String,
    daily_api_base_url: String,
    dynamic_version: bool,
    oauth_config: AntigravityOAuthConfig,
}

impl AntigravityOAuthClient {
    pub(crate) fn new(oauth_config: AntigravityOAuthConfig) -> Result<Self, reqwest::Error> {
        Ok(Self {
            http: reqwest::Client::builder().http1_only().build()?,
            token_endpoint: TOKEN_ENDPOINT.to_owned(),
            userinfo_endpoint: USERINFO_ENDPOINT.to_owned(),
            api_base_url: API_BASE_URL.to_owned(),
            daily_api_base_url: DAILY_API_BASE_URL.to_owned(),
            dynamic_version: true,
            oauth_config,
        })
    }

    pub(crate) async fn start(&self) -> Result<StartedProviderOAuth, ProviderConfigurationError> {
        let (client_id, _) = self
            .oauth_config
            .credentials()
            .map_err(ProviderConfigurationError::new)?;
        let listener = TcpListener::bind(CALLBACK_ADDRESS).await.ok();
        let (callback_tx, callback_rx) = mpsc::channel(1);
        let state = uuid::Uuid::new_v4().simple().to_string();
        let mut authorization_url = reqwest::Url::parse(AUTH_ENDPOINT).map_err(|_| {
            ProviderConfigurationError::new("Antigravity OAuth authorization URL is invalid")
        })?;
        authorization_url.query_pairs_mut().extend_pairs([
            ("access_type", "offline"),
            ("client_id", client_id),
            ("prompt", "consent"),
            ("redirect_uri", REDIRECT_URI),
            ("response_type", "code"),
            ("scope", SCOPES),
            ("state", state.as_str()),
        ]);
        let expires_at = unix_timestamp()
            .checked_add(i64::try_from(FLOW_TIMEOUT.as_secs()).unwrap_or(300))
            .ok_or_else(|| {
                ProviderConfigurationError::new("Antigravity OAuth expiry is too large")
            })?;
        let callback = Arc::new(AntigravityCallbackSubmitter {
            callback_tx,
            state: state.clone(),
        });
        Ok(StartedProviderOAuth {
            challenge: ProviderOAuthChallenge {
                verification_uri: AUTH_ENDPOINT.to_owned(),
                verification_uri_complete: Some(authorization_url.to_string()),
                user_code: String::new(),
                expires_at,
                interval_seconds: 1,
            },
            pending: Box::new(AntigravityPendingOAuth {
                client: self.clone(),
                listener,
                callback_rx: Mutex::new(callback_rx),
                callback,
                deadline: Instant::now() + FLOW_TIMEOUT,
            }),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        Self {
            http: reqwest::Client::builder()
                .http1_only()
                .build()
                .expect("test OAuth HTTP client"),
            token_endpoint: format!("{base_url}/token"),
            userinfo_endpoint: format!("{base_url}/userinfo"),
            api_base_url: format!("{base_url}/api"),
            daily_api_base_url: format!("{base_url}/daily"),
            dynamic_version: false,
            oauth_config: AntigravityOAuthConfig::for_test(),
        }
    }

    fn user_agent(&self) -> String {
        if self.dynamic_version {
            version::user_agent()
        } else {
            version::fallback_user_agent().to_owned()
        }
    }

    fn onboard_user_agent(&self) -> String {
        if self.dynamic_version {
            version::onboard_user_agent()
        } else {
            version::fallback_onboard_user_agent().to_owned()
        }
    }

    fn ide_version(&self) -> String {
        if self.dynamic_version {
            version::latest_version()
        } else {
            version::fallback_version().to_owned()
        }
    }
}

struct AntigravityPendingOAuth {
    client: AntigravityOAuthClient,
    listener: Option<TcpListener>,
    callback_rx: Mutex<mpsc::Receiver<String>>,
    callback: Arc<AntigravityCallbackSubmitter>,
    deadline: Instant,
}

struct AntigravityCallbackSubmitter {
    callback_tx: mpsc::Sender<String>,
    state: String,
}

#[async_trait]
impl PendingProviderOAuth for AntigravityPendingOAuth {
    async fn complete(self: Box<Self>) -> Result<SecretString, ProviderConfigurationError> {
        let code = self.receive_callback().await?;
        let token = self.client.exchange_code(&code).await?;
        let access_token = required(token.access_token, "access_token")?;
        let refresh_token = required(token.refresh_token, "refresh_token")?;
        let email = self.client.fetch_user_info(&access_token).await?;
        let project_id = self.client.fetch_project_id(&access_token).await?;
        let expires_in = token.expires_in.filter(|value| *value > 0).ok_or_else(|| {
            ProviderConfigurationError::new(
                "Antigravity OAuth token response has invalid expires_in",
            )
        })?;
        let credentials = AntigravityCredentials::from_parts(
            access_token,
            refresh_token,
            email,
            project_id,
            expires_in,
            unix_timestamp(),
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

impl ProviderOAuthCallback for AntigravityCallbackSubmitter {
    fn submit(&self, callback_url: &str) -> Result<(), ProviderConfigurationError> {
        let code = parse_callback_url(callback_url, &self.state)?;
        self.callback_tx.try_send(code).map_err(|_| {
            ProviderConfigurationError::new("Antigravity OAuth callback is unavailable")
        })
    }
}

impl AntigravityPendingOAuth {
    async fn receive_callback(&self) -> Result<String, ProviderConfigurationError> {
        let mut receiver = self.callback_rx.lock().await;
        loop {
            if let Some(listener) = &self.listener {
                tokio::select! {
                    submitted = receiver.recv() => {
                        return submitted.ok_or_else(|| ProviderConfigurationError::new("Antigravity OAuth callback is unavailable"));
                    }
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted.map_err(|_| ProviderConfigurationError::new("Antigravity OAuth callback failed"))?;
                        let callback = self.callback.clone();
                        let deadline = self.deadline;
                        tokio::spawn(async move {
                            let accepted = match read_callback_request(&mut stream, deadline).await {
                                Ok(request) => match parse_callback_request(&request, &callback.state) {
                                    Ok(code) => callback.callback_tx.try_send(code).is_ok(),
                                    Err(_) => false,
                                },
                                Err(_) => false,
                            };
                            write_callback_response(&mut stream, accepted).await;
                        });
                    }
                    _ = sleep_until(self.deadline) => {
                        return Err(ProviderConfigurationError::new("Antigravity OAuth authorization expired"));
                    }
                }
            } else {
                return timeout_at(self.deadline, receiver.recv())
                    .await
                    .map_err(|_| {
                        ProviderConfigurationError::new("Antigravity OAuth authorization expired")
                    })?
                    .ok_or_else(|| {
                        ProviderConfigurationError::new("Antigravity OAuth callback is unavailable")
                    });
            }
        }
    }
}

impl AntigravityOAuthClient {
    async fn exchange_code(&self, code: &str) -> Result<TokenResponse, ProviderConfigurationError> {
        let (client_id, client_secret) = self
            .oauth_config
            .credentials()
            .map_err(ProviderConfigurationError::new)?;
        let response = self
            .http
            .post(&self.token_endpoint)
            .timeout(Duration::from_secs(30))
            .form(&[
                ("code", code),
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("redirect_uri", REDIRECT_URI),
                ("grant_type", "authorization_code"),
            ])
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| {
                ProviderConfigurationError::new("Antigravity OAuth token exchange failed")
            })?;
        response_json(response, "Antigravity OAuth token exchange").await
    }

    async fn fetch_user_info(
        &self,
        access_token: &str,
    ) -> Result<String, ProviderConfigurationError> {
        let user_agent = self.user_agent();
        let response = self
            .http
            .get(&self.userinfo_endpoint)
            .timeout(Duration::from_secs(30))
            .bearer_auth(access_token)
            .header(reqwest::header::USER_AGENT, user_agent)
            .send()
            .await
            .map_err(|_| ProviderConfigurationError::new("Antigravity user info request failed"))?;
        let response: UserInfo = response_json(response, "Antigravity user info").await?;
        required(response.email, "email")
    }

    pub(crate) async fn fetch_project_id(
        &self,
        access_token: &str,
    ) -> Result<String, ProviderConfigurationError> {
        let user_agent = self.user_agent();
        let body = serde_json::json!({"metadata":{"ideType":"ANTIGRAVITY"}});
        let response = self
            .http
            .post(format!(
                "{}/{API_VERSION}:loadCodeAssist",
                self.api_base_url
            ))
            .timeout(Duration::from_secs(30))
            .bearer_auth(access_token)
            .header(reqwest::header::ACCEPT, "*/*")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::USER_AGENT, user_agent)
            .body(body.to_string())
            .send()
            .await
            .map_err(|_| ProviderConfigurationError::new("Antigravity project discovery failed"))?;
        let load: Value = response_json(response, "Antigravity project discovery").await?;
        if let Some(project_id) = project_id(&load) {
            return Ok(project_id);
        }
        let tier_id = default_tier(&load)?;
        self.onboard_user(access_token, &tier_id).await
    }

    async fn onboard_user(
        &self,
        access_token: &str,
        tier_id: &str,
    ) -> Result<String, ProviderConfigurationError> {
        let ide_version = self.ide_version();
        let user_agent = self.onboard_user_agent();
        let body = serde_json::json!({
            "tier_id": tier_id,
            "metadata": {
                "ide_type": "ANTIGRAVITY",
                "ide_version": ide_version,
                "ide_name": "antigravity"
            }
        });
        for attempt in 0..5 {
            let response = self
                .http
                .post(format!(
                    "{}/{API_VERSION}:onboardUser",
                    self.daily_api_base_url
                ))
                .timeout(Duration::from_secs(30))
                .bearer_auth(access_token)
                .header(reqwest::header::ACCEPT, "*/*")
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::USER_AGENT, &user_agent)
                .header("X-Goog-Api-Client", GOOG_API_CLIENT)
                .body(body.to_string())
                .send()
                .await
                .map_err(|_| ProviderConfigurationError::new("Antigravity onboarding failed"))?;
            let value: Value = response_json(response, "Antigravity onboarding").await?;
            if value.get("done").and_then(Value::as_bool).unwrap_or(false) {
                if let Some(project_id) = value.get("response").and_then(project_id) {
                    return Ok(project_id);
                }
                return Err(ProviderConfigurationError::new(
                    "Antigravity onboarding response is missing project_id",
                ));
            }
            if attempt < 4 {
                sleep(Duration::from_secs(2)).await;
            }
        }
        Err(ProviderConfigurationError::new(
            "Antigravity onboarding did not complete",
        ))
    }
}

async fn response_json<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
    operation: &str,
) -> Result<T, ProviderConfigurationError> {
    let status = response.status();
    let body = collect_bounded_body(response.bytes_stream(), RESPONSE_LIMIT)
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

fn project_id(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    for key in ["cloudaicompanionProject", "projectId", "project"] {
        match object.get(key) {
            Some(Value::String(value)) if !value.trim().is_empty() => {
                return Some(value.trim().to_owned());
            }
            Some(Value::Object(value)) => {
                if let Some(value) = value.get("id").and_then(Value::as_str)
                    && !value.trim().is_empty()
                {
                    return Some(value.trim().to_owned());
                }
            }
            _ => {}
        }
    }
    None
}

fn default_tier(value: &Value) -> Result<String, ProviderConfigurationError> {
    value
        .get("allowedTiers")
        .and_then(Value::as_array)
        .and_then(|tiers| {
            tiers.iter().find_map(|tier| {
                (tier.get("isDefault").and_then(Value::as_bool) == Some(true))
                    .then(|| tier.get("id").and_then(Value::as_str))
                    .flatten()
                    .map(str::to_owned)
            })
        })
        .or_else(|| {
            value
                .get("currentTier")
                .and_then(|tier| tier.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ProviderConfigurationError::new(
                "Antigravity project discovery response is missing an allowed tier",
            )
        })
}

fn required(value: Option<String>, field: &str) -> Result<String, ProviderConfigurationError> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ProviderConfigurationError::new(format!(
                "Antigravity OAuth response is missing {field}"
            ))
        })
}

fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

#[derive(Deserialize)]
struct UserInfo {
    email: Option<String>,
}
