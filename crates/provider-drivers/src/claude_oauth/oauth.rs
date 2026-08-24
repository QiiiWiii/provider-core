use std::{sync::Arc, time::Duration};

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
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Mutex, mpsc},
    time::{Instant, sleep_until, timeout_at},
};

use super::credentials::{ClaudeOAuthCredentials, generate_device_id, unix_timestamp};
use super::response::response_stream;

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
            http: oauth_http_client(),
            token_url: TOKEN_URL.to_owned(),
        }
    }

    #[cfg(feature = "test-util")]
    pub(crate) fn with_token_url(token_url: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
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
    callback_rx: Mutex<mpsc::Receiver<String>>,
    callback: Arc<ClaudeCallbackSubmitter>,
    verifier: String,
    state: String,
    deadline: Instant,
}

struct ClaudeCallbackSubmitter {
    callback_tx: mpsc::Sender<String>,
    state: String,
}

#[async_trait]
impl PendingProviderOAuth for ClaudePendingOAuth {
    async fn complete(self: Box<Self>) -> Result<SecretString, ProviderConfigurationError> {
        let code = self.receive_callback().await?;
        let request = AuthorizationCodeRequest {
            grant_type: "authorization_code",
            code: &code,
            redirect_uri: REDIRECT_URI,
            client_id: CLIENT_ID,
            code_verifier: &self.verifier,
            state: &self.state,
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
        let credentials = ClaudeOAuthCredentials::from_parts(
            required(tokens.access_token, "access_token")?,
            required(tokens.refresh_token, "refresh_token")?,
            required(tokens.account.uuid, "account.uuid")?,
            tokens.account.email_address.and_then(normalized),
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

impl ProviderOAuthCallback for ClaudeCallbackSubmitter {
    fn submit(&self, callback_url: &str) -> Result<(), ProviderConfigurationError> {
        let code = parse_callback_url(callback_url, &self.state)?;
        self.callback_tx
            .try_send(code)
            .map_err(|_| ProviderConfigurationError::new("Claude OAuth callback is unavailable"))
    }
}

impl ClaudePendingOAuth {
    async fn receive_callback(&self) -> Result<String, ProviderConfigurationError> {
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
                        write_callback_response(&mut stream, callback.is_ok()).await;
                        if let Ok(code) = callback {
                            return Ok(code);
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

async fn write_callback_response(stream: &mut tokio::net::TcpStream, accepted: bool) {
    let (status, response_body) = if accepted {
        ("200 OK", "Claude OAuth complete. You may close this tab.")
    } else {
        ("400 Bad Request", "Claude OAuth callback rejected.")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{response_body}",
        response_body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

async fn read_callback_request(
    stream: &mut tokio::net::TcpStream,
    deadline: Instant,
) -> Result<Vec<u8>, ProviderConfigurationError> {
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 512];
    loop {
        let read = timeout_at(deadline, stream.read(&mut chunk))
            .await
            .map_err(|_| ProviderConfigurationError::new("Claude OAuth authorization expired"))?
            .map_err(|_| ProviderConfigurationError::new("Claude OAuth callback failed"))?;
        if read == 0 || request.len().saturating_add(read) > 16 * 1024 {
            return Err(ProviderConfigurationError::new(
                "Claude OAuth callback is invalid",
            ));
        }
        request.extend_from_slice(&chunk[..read]);
        if request.contains(&b'\n') {
            return Ok(request);
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
}

#[derive(Deserialize, Default)]
struct TokenAccount {
    uuid: Option<String>,
    email_address: Option<String>,
}

#[derive(Deserialize, Default)]
struct ProfileResponse {
    #[serde(default)]
    account: ProfileAccount,
}

#[derive(Deserialize, Default)]
struct ProfileAccount {
    uuid: Option<String>,
    email: Option<String>,
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

pub(crate) async fn inspect_profile(http: &reqwest::Client, url: &str, access_token: &str) {
    let _ =
        control_plane_json::<serde_json::Value>(http, url, access_token, "Claude OAuth profile")
            .await;
}

fn parse_callback(
    request: &[u8],
    expected_state: &str,
) -> Result<String, ProviderConfigurationError> {
    let line = std::str::from_utf8(request)
        .ok()
        .and_then(|request| request.lines().next())
        .ok_or_else(|| ProviderConfigurationError::new("Claude OAuth callback is invalid"))?;
    let mut request_line = line.split_whitespace();
    if request_line.next() != Some("GET") {
        return Err(ProviderConfigurationError::new(
            "Claude OAuth callback is invalid",
        ));
    }
    let path = request_line
        .next()
        .ok_or_else(|| ProviderConfigurationError::new("Claude OAuth callback is invalid"))?;
    if request_line.next() != Some("HTTP/1.1") || request_line.next().is_some() {
        return Err(ProviderConfigurationError::new(
            "Claude OAuth callback is invalid",
        ));
    }
    let url = reqwest::Url::parse(&format!("http://localhost{path}"))
        .map_err(|_| ProviderConfigurationError::new("Claude OAuth callback is invalid"))?;
    if url.path() != "/callback" {
        return Err(ProviderConfigurationError::new(
            "Claude OAuth callback is invalid",
        ));
    }
    parse_callback_parameters(&url, expected_state)
}

fn parse_callback_url(
    callback_url: &str,
    expected_state: &str,
) -> Result<String, ProviderConfigurationError> {
    let url = reqwest::Url::parse(callback_url.trim())
        .map_err(|_| ProviderConfigurationError::new("Claude OAuth callback URL is invalid"))?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
        || url.port_or_known_default() != Some(54545)
        || url.path() != "/callback"
    {
        return Err(ProviderConfigurationError::new(
            "Claude OAuth callback URL is invalid",
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
    for (name, value) in url.query_pairs() {
        match name.as_ref() {
            "code" if code.is_none() => code = Some(value.into_owned()),
            "state" if state.is_none() => state = Some(value.into_owned()),
            "code" | "state" => {
                return Err(ProviderConfigurationError::new(
                    "Claude OAuth callback has duplicate parameters",
                ));
            }
            _ => {}
        }
    }
    if state.as_deref() != Some(expected_state) {
        return Err(ProviderConfigurationError::new(
            "Claude OAuth callback state does not match",
        ));
    }
    code.map(|value| {
        value
            .split('#')
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned()
    })
    .filter(|value| !value.is_empty())
    .ok_or_else(|| ProviderConfigurationError::new("Claude OAuth callback is missing code"))
}

fn random_base64(length: usize) -> Result<String, ProviderConfigurationError> {
    let mut bytes = vec![0_u8; length];
    getrandom::fill(&mut bytes).map_err(|_| {
        ProviderConfigurationError::new("failed to generate Claude OAuth PKCE state")
    })?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn random_hex(length: usize) -> Result<String, ProviderConfigurationError> {
    let mut bytes = vec![0_u8; length];
    getrandom::fill(&mut bytes)
        .map_err(|_| ProviderConfigurationError::new("failed to generate Claude OAuth state"))?;
    let mut encoded = String::with_capacity(length.saturating_mul(2));
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(encoded, "{byte:02x}");
    }
    Ok(encoded)
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

fn oauth_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .build()
        .expect("Claude OAuth HTTP client configuration must be valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::get};
    use serde_json::json;

    #[test]
    fn callback_requires_matching_state_and_extracts_code_fragment() {
        let request = b"GET /callback?code=auth-code%23embedded-state&state=expected HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(
            parse_callback(request, "expected").expect("callback"),
            "auth-code"
        );
        assert!(parse_callback(request, "different").is_err());
        assert_eq!(
            parse_callback_url(
                "http://localhost:54545/callback?code=auth-code&state=expected",
                "expected"
            )
            .expect("submitted callback"),
            "auth-code"
        );
        assert!(
            parse_callback_url(
                "https://attacker.example/callback?code=auth-code&state=expected",
                "expected"
            )
            .is_err()
        );
        assert!(
            parse_callback_url(
                "http://localhost:54545/callback?code=auth-code&state=expected&state=expected",
                "expected"
            )
            .is_err()
        );
    }

    #[test]
    fn token_exchange_body_matches_cpa_field_order() {
        let body = serde_json::to_string(&AuthorizationCodeRequest {
            grant_type: "authorization_code",
            code: "auth-code",
            redirect_uri: REDIRECT_URI,
            client_id: CLIENT_ID,
            code_verifier: "verifier",
            state: "state",
        })
        .expect("token body");
        assert_eq!(
            body,
            format!(
                r#"{{"grant_type":"authorization_code","code":"auth-code","redirect_uri":"{REDIRECT_URI}","client_id":"{CLIENT_ID}","code_verifier":"verifier","state":"state"}}"#
            )
        );
    }

    #[tokio::test]
    async fn control_plane_headers_match_cpa_axios_profile() {
        let app = Router::new().route(
            "/profile",
            get(|headers: axum::http::HeaderMap| async move {
                assert_eq!(headers["accept"], "application/json, text/plain, */*");
                assert_eq!(headers["content-type"], "application/json");
                assert_eq!(headers["authorization"], "Bearer access-token");
                assert_eq!(headers["cache-control"], "no-cache");
                assert_eq!(headers["user-agent"], "axios/1.15.2");
                assert_eq!(headers["accept-encoding"], "gzip, compress, deflate, br");
                Json(json!({"ok": true}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("profile listener");
        let address = listener.local_addr().expect("profile address");
        let server = tokio::spawn(axum::serve(listener, app).into_future());
        let result: serde_json::Value = control_plane_json(
            &reqwest::Client::new(),
            &format!("http://{address}/profile"),
            "access-token",
            "profile",
        )
        .await
        .expect("profile response");
        assert_eq!(result, json!({"ok": true}));
        server.abort();
    }
}
