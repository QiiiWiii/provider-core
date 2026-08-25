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
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Mutex, mpsc},
    time::{Instant, sleep, sleep_until, timeout_at},
};

use super::{
    contract::{
        API_BASE_URL, API_VERSION, AUTH_ENDPOINT, CALLBACK_ADDRESS, CALLBACK_PATH, CALLBACK_PORT,
        CLIENT_ID, CLIENT_SECRET, DAILY_API_BASE_URL, GOOG_API_CLIENT, REDIRECT_URI, SCOPES,
        TOKEN_ENDPOINT, USERINFO_ENDPOINT,
    },
    credentials::AntigravityCredentials,
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
}

impl AntigravityOAuthClient {
    pub(crate) fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .http1_only()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            token_endpoint: TOKEN_ENDPOINT.to_owned(),
            userinfo_endpoint: USERINFO_ENDPOINT.to_owned(),
            api_base_url: API_BASE_URL.to_owned(),
            daily_api_base_url: DAILY_API_BASE_URL.to_owned(),
            dynamic_version: true,
        }
    }

    pub(crate) async fn start(&self) -> Result<StartedProviderOAuth, ProviderConfigurationError> {
        let listener = TcpListener::bind(CALLBACK_ADDRESS).await.ok();
        let (callback_tx, callback_rx) = mpsc::channel(1);
        let state = uuid::Uuid::new_v4().simple().to_string();
        let mut authorization_url = reqwest::Url::parse(AUTH_ENDPOINT).map_err(|_| {
            ProviderConfigurationError::new("Antigravity OAuth authorization URL is invalid")
        })?;
        authorization_url.query_pairs_mut().extend_pairs([
            ("access_type", "offline"),
            ("client_id", CLIENT_ID),
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
                .unwrap_or_else(|_| reqwest::Client::new()),
            token_endpoint: format!("{base_url}/token"),
            userinfo_endpoint: format!("{base_url}/userinfo"),
            api_base_url: format!("{base_url}/api"),
            daily_api_base_url: format!("{base_url}/daily"),
            dynamic_version: false,
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
        let response = self
            .http
            .post(&self.token_endpoint)
            .timeout(Duration::from_secs(30))
            .form(&[
                ("code", code),
                ("client_id", CLIENT_ID),
                ("client_secret", CLIENT_SECRET),
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
        let tier_id = default_tier(&load);
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

fn default_tier(value: &Value) -> String {
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
        .unwrap_or_else(|| "free-tier".to_owned())
}

async fn write_callback_response(stream: &mut tokio::net::TcpStream, accepted: bool) {
    let (status, body) = if accepted {
        (
            "200 OK",
            "Antigravity OAuth complete. You may close this tab.",
        )
    } else {
        ("400 Bad Request", "Antigravity OAuth callback rejected.")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

async fn read_callback_request(
    stream: &mut tokio::net::TcpStream,
    deadline: Instant,
) -> Result<Vec<u8>, ProviderConfigurationError> {
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 512];
    let read_deadline = std::cmp::min(deadline, Instant::now() + CALLBACK_READ_TIMEOUT);
    loop {
        let read = timeout_at(read_deadline, stream.read(&mut chunk))
            .await
            .map_err(|_| {
                ProviderConfigurationError::new("Antigravity OAuth authorization expired")
            })?
            .map_err(|_| ProviderConfigurationError::new("Antigravity OAuth callback failed"))?;
        if read == 0 || request.len().saturating_add(read) > 16 * 1024 {
            return Err(ProviderConfigurationError::new(
                "Antigravity OAuth callback is invalid",
            ));
        }
        request.extend_from_slice(&chunk[..read]);
        if request.contains(&b'\n') {
            return Ok(request);
        }
    }
}

fn parse_callback_request(
    request: &[u8],
    expected_state: &str,
) -> Result<String, ProviderConfigurationError> {
    let line = std::str::from_utf8(request)
        .ok()
        .and_then(|request| request.lines().next())
        .ok_or_else(|| ProviderConfigurationError::new("Antigravity OAuth callback is invalid"))?;
    let mut parts = line.split_whitespace();
    if parts.next() != Some("GET") {
        return Err(ProviderConfigurationError::new(
            "Antigravity OAuth callback is invalid",
        ));
    }
    let path = parts
        .next()
        .ok_or_else(|| ProviderConfigurationError::new("Antigravity OAuth callback is invalid"))?;
    if parts.next() != Some("HTTP/1.1") || parts.next().is_some() {
        return Err(ProviderConfigurationError::new(
            "Antigravity OAuth callback is invalid",
        ));
    }
    let url = reqwest::Url::parse(&format!("http://localhost{path}"))
        .map_err(|_| ProviderConfigurationError::new("Antigravity OAuth callback is invalid"))?;
    if url.path() != CALLBACK_PATH {
        return Err(ProviderConfigurationError::new(
            "Antigravity OAuth callback is invalid",
        ));
    }
    parse_callback_parameters(&url, expected_state)
}

fn parse_callback_url(
    callback_url: &str,
    expected_state: &str,
) -> Result<String, ProviderConfigurationError> {
    let url = reqwest::Url::parse(callback_url.trim()).map_err(|_| {
        ProviderConfigurationError::new("Antigravity OAuth callback URL is invalid")
    })?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
        || url.port_or_known_default() != Some(CALLBACK_PORT)
        || url.path() != CALLBACK_PATH
    {
        return Err(ProviderConfigurationError::new(
            "Antigravity OAuth callback URL is invalid",
        ));
    }
    parse_callback_parameters(&url, expected_state)
}

fn parse_callback_parameters(
    url: &reqwest::Url,
    expected_state: &str,
) -> Result<String, ProviderConfigurationError> {
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (name, value) in url.query_pairs() {
        match name.as_ref() {
            "code" if code.is_none() => code = Some(value.into_owned()),
            "state" if state.is_none() => state = Some(value.into_owned()),
            "error" if error.is_none() => error = Some(value.into_owned()),
            "code" | "state" | "error" => {
                return Err(ProviderConfigurationError::new(
                    "Antigravity OAuth callback has duplicate parameters",
                ));
            }
            _ => {}
        }
    }
    if let Some(error) = error.filter(|value| !value.trim().is_empty()) {
        return Err(ProviderConfigurationError::new(format!(
            "Antigravity OAuth authorization failed: {error}"
        )));
    }
    let state = state
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ProviderConfigurationError::new("Antigravity OAuth callback is missing state")
        })?;
    if state != expected_state {
        return Err(ProviderConfigurationError::new(
            "Antigravity OAuth callback state does not match",
        ));
    }
    code.filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ProviderConfigurationError::new("Antigravity OAuth callback is missing code")
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

#[cfg(test)]
mod tests {
    use axum::{Router, body::to_bytes, extract::Request, http::StatusCode, routing::post};
    use tokio::net::TcpListener;

    use super::*;

    async fn spawn(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(axum::serve(listener, router).into_future());
        format!("http://{address}")
    }

    #[test]
    fn accepts_local_callback_and_validates_state() {
        let code = parse_callback_url(
            "http://localhost:51121/oauth-callback?code=auth-code&state=state-1",
            "state-1",
        )
        .expect("callback");
        assert_eq!(code, "auth-code");
    }

    #[test]
    fn rejects_callback_with_wrong_origin_or_duplicate_state() {
        assert!(
            parse_callback_url(
                "https://localhost:51121/oauth-callback?code=auth-code&state=state-1",
                "state-1",
            )
            .is_err()
        );
        assert!(
            parse_callback_url(
                "http://localhost:51121/oauth-callback?code=auth-code&state=state-1&state=state-1",
                "state-1",
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn discovers_project_and_polls_onboarding_with_cpa_headers() {
        let app = Router::new()
            .route(
                "/api/v1internal:loadCodeAssist",
                post(|request: Request| async move {
                    assert_eq!(
                        request
                            .headers()
                            .get("authorization")
                            .and_then(|value| value.to_str().ok()),
                        Some("Bearer access")
                    );
                    assert_eq!(
                        request
                            .headers()
                            .get("user-agent")
                            .and_then(|value| value.to_str().ok()),
                        Some(version::fallback_user_agent())
                    );
                    (
                        StatusCode::OK,
                        r#"{"allowedTiers":[{"id":"free-tier","isDefault":true}]}"#,
                    )
                }),
            )
            .route(
                "/daily/v1internal:onboardUser",
                post(|request: Request| async move {
                    assert_eq!(
                        request
                            .headers()
                            .get("x-goog-api-client")
                            .and_then(|value| value.to_str().ok()),
                        Some(GOOG_API_CLIENT)
                    );
                    assert_eq!(
                        request
                            .headers()
                            .get("user-agent")
                            .and_then(|value| value.to_str().ok()),
                        Some(version::fallback_onboard_user_agent())
                    );
                    let body = to_bytes(request.into_body(), 4096).await.expect("body");
                    let body: Value = serde_json::from_slice(&body).expect("body JSON");
                    assert_eq!(
                        body["metadata"]["ide_version"],
                        version::fallback_version()
                    );
                    (
                        StatusCode::OK,
                        r#"{"done":true,"response":{"cloudaicompanionProject":{"id":"project-a"}}}"#,
                    )
                }),
            );
        let base = spawn(app).await;
        let client = AntigravityOAuthClient::for_test(base);
        assert_eq!(
            client.fetch_project_id("access").await.expect("project ID"),
            "project-a"
        );
    }
}
