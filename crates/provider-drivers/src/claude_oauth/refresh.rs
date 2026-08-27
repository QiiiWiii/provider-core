use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError},
    time::{Duration, Instant, SystemTime},
};

use provider_core::{ProviderConfigurationError, RefreshError, RefreshErrorKind};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use super::{
    credentials::{ClaudeOAuthCredentials, ClaudeOAuthIdentity, unix_timestamp},
    oauth::{CLIENT_ID, SCOPE, axios_headers, inspect_profile, response_json},
    transport::claude_http_client,
};

const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const MIN_REFRESH_COOLDOWN: Duration = Duration::from_secs(5);
const MAX_REFRESH_COOLDOWN: Duration = Duration::from_secs(5 * 60);

type SharedRefreshResult = Result<SharedRefreshSuccess, SharedRefreshFailure>;

#[derive(Clone)]
struct SharedRefreshSuccess {
    access_token: String,
    refresh_token: Option<String>,
    identity: ClaudeOAuthIdentity,
    expires_at: i64,
    refreshed_at: i64,
}

#[derive(Clone)]
struct SharedRefreshFailure {
    kind: RefreshErrorKind,
    message: String,
}

struct RefreshFlight {
    result: Mutex<Option<SharedRefreshResult>>,
    notify: Notify,
}

struct LeaderFlightGuard {
    key: String,
    flight: Arc<RefreshFlight>,
    completed: bool,
}

impl LeaderFlightGuard {
    fn new(key: String, flight: Arc<RefreshFlight>) -> Self {
        Self {
            key,
            flight,
            completed: false,
        }
    }

    fn finish(mut self, result: SharedRefreshResult) {
        self.completed = true;
        self.flight.complete(result);
        release_flight(&self.key, &self.flight);
    }
}

impl Drop for LeaderFlightGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        self.flight.complete(Err(SharedRefreshFailure {
            kind: RefreshErrorKind::Transient,
            message: "Claude OAuth refresh leader was cancelled".to_owned(),
        }));
        release_flight(&self.key, &self.flight);
    }
}

struct RefreshState {
    flights: Mutex<HashMap<String, Arc<RefreshFlight>>>,
    blocked_until: Mutex<HashMap<String, Instant>>,
}

static REFRESH_STATE: OnceLock<RefreshState> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct ClaudeRefreshClient {
    http: reqwest::Client,
    token_url: String,
    profile_url: String,
}

impl ClaudeRefreshClient {
    pub(crate) fn new() -> Self {
        Self {
            http: claude_http_client(),
            token_url: TOKEN_URL.to_owned(),
            profile_url: PROFILE_URL.to_owned(),
        }
    }

    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn with_token_url(token_url: &str) -> Self {
        let profile_url = format!("{}/profile", token_url.trim_end_matches("/token"));
        Self {
            http: claude_http_client(),
            token_url: token_url.to_owned(),
            profile_url,
        }
    }

    pub(crate) async fn refresh(
        &self,
        credentials: &ClaudeOAuthCredentials,
    ) -> Result<ClaudeOAuthCredentials, RefreshError> {
        let refresh_token = credentials.refresh_token().expose_secret().to_owned();
        if let Some(blocked_until) = blocked_until(&refresh_token) {
            return Err(RefreshError::new(
                RefreshErrorKind::Transient,
                format!(
                    "Claude OAuth refresh is cooling down until {}",
                    blocked_until
                        .saturating_duration_since(Instant::now())
                        .as_secs()
                ),
            ));
        }
        let (flight, leader) = acquire_flight(&refresh_token);
        let refreshed = if leader {
            let leader = LeaderFlightGuard::new(refresh_token.clone(), flight);
            let result = self.refresh_once(&refresh_token).await;
            let shared = result
                .as_ref()
                .map(Clone::clone)
                .map_err(SharedRefreshFailure::from);
            leader.finish(shared);
            result?
        } else {
            flight.wait().await?
        };
        credentials
            .refreshed_with_identity(
                refreshed.access_token,
                refreshed.refresh_token,
                refreshed.identity,
                refreshed.expires_at,
                refreshed.refreshed_at,
            )
            .map_err(|error| RefreshError::new(RefreshErrorKind::Internal, error.to_string()))
    }

    async fn refresh_once(
        &self,
        refresh_token: &str,
    ) -> Result<SharedRefreshSuccess, RefreshError> {
        let request = RefreshRequest {
            client_id: CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token,
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
        let cooldown = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let cooldown = parse_retry_after(response.headers());
            set_blocked_until(refresh_token, Instant::now() + cooldown);
            Some(cooldown)
        } else {
            None
        };
        let tokens: RefreshResponse = response_json(response, "Claude OAuth refresh")
            .await
            .map_err(|error| {
                let kind = if matches!(
                    status,
                    reqwest::StatusCode::BAD_REQUEST
                        | reqwest::StatusCode::UNAUTHORIZED
                        | reqwest::StatusCode::FORBIDDEN
                ) {
                    RefreshErrorKind::ReauthRequired
                } else {
                    RefreshErrorKind::Transient
                };
                let message = if let Some(cooldown) = cooldown {
                    format!("{}; retry after {} seconds", error, cooldown.as_secs())
                } else {
                    error.to_string()
                };
                RefreshError::new(kind, message)
            })?;
        let refreshed_at = unix_timestamp();
        let access_token = required(tokens.access_token, "access_token")?;
        let profile = inspect_profile(&self.http, &self.profile_url, &access_token).await;
        let account_uuid = profile.as_ref().and_then(|profile| {
            profile
                .account
                .uuid
                .as_deref()
                .map(str::trim)
                .filter(|value| uuid::Uuid::parse_str(value).is_ok())
                .map(str::to_owned)
        });
        let email = profile
            .as_ref()
            .and_then(|profile| profile.account.email.clone());
        let organization_uuid = profile
            .as_ref()
            .and_then(|profile| profile.organization.uuid.clone());
        let organization_name = profile
            .as_ref()
            .and_then(|profile| profile.organization.name.clone());
        if cooldown.is_none() {
            clear_blocked_until(refresh_token);
        }
        Ok(SharedRefreshSuccess {
            access_token,
            refresh_token: tokens.refresh_token.and_then(normalized),
            identity: ClaudeOAuthIdentity {
                account_uuid,
                email,
                organization_uuid,
                organization_name,
            },
            expires_at: refreshed_at + tokens.expires_in.unwrap_or(3600).max(60),
            refreshed_at,
        })
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

fn refresh_state() -> &'static RefreshState {
    REFRESH_STATE.get_or_init(|| RefreshState {
        flights: Mutex::new(HashMap::new()),
        blocked_until: Mutex::new(HashMap::new()),
    })
}

fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn acquire_flight(key: &str) -> (Arc<RefreshFlight>, bool) {
    let mut flights = guard(&refresh_state().flights);
    if let Some(flight) = flights.get(key) {
        return (flight.clone(), false);
    }
    let flight = Arc::new(RefreshFlight {
        result: Mutex::new(None),
        notify: Notify::new(),
    });
    flights.insert(key.to_owned(), flight.clone());
    (flight, true)
}

fn release_flight(key: &str, flight: &Arc<RefreshFlight>) {
    let mut flights = guard(&refresh_state().flights);
    if flights
        .get(key)
        .is_some_and(|current| Arc::ptr_eq(current, flight))
    {
        flights.remove(key);
    }
}

impl RefreshFlight {
    fn complete(&self, result: SharedRefreshResult) {
        *guard(&self.result) = Some(result);
        self.notify.notify_waiters();
    }

    async fn wait(&self) -> Result<SharedRefreshSuccess, RefreshError> {
        loop {
            let notified = self.notify.notified();
            if let Some(result) = guard(&self.result).clone() {
                return result.map_err(SharedRefreshFailure::into_error);
            }
            notified.await;
        }
    }
}

impl From<&RefreshError> for SharedRefreshFailure {
    fn from(error: &RefreshError) -> Self {
        Self {
            kind: error.kind(),
            message: error.message().to_owned(),
        }
    }
}

impl SharedRefreshFailure {
    fn into_error(self) -> RefreshError {
        RefreshError::new(self.kind, self.message)
    }
}

fn blocked_until(key: &str) -> Option<Instant> {
    let mut blocked = guard(&refresh_state().blocked_until);
    let until = blocked.get(key).copied();
    if until.is_some_and(|value| value <= Instant::now()) {
        blocked.remove(key);
        None
    } else {
        until
    }
}

fn set_blocked_until(key: &str, until: Instant) {
    guard(&refresh_state().blocked_until).insert(key.to_owned(), until);
}

fn clear_blocked_until(key: &str) {
    guard(&refresh_state().blocked_until).remove(key);
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Duration {
    if let Some(raw) = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(seconds) = raw.trim().parse::<u64>() {
            return clamp_cooldown(Duration::from_secs(
                seconds.min(MAX_REFRESH_COOLDOWN.as_secs()),
            ));
        }
        if let Ok(when) = httpdate::parse_http_date(raw.trim()) {
            return clamp_cooldown(
                when.duration_since(SystemTime::now())
                    .unwrap_or(Duration::ZERO),
            );
        }
    }
    if let Some(raw) = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        && let Ok(milliseconds) = raw.trim().parse::<u64>()
    {
        return clamp_cooldown(Duration::from_millis(
            milliseconds.min(MAX_REFRESH_COOLDOWN.as_millis() as u64),
        ));
    }
    MIN_REFRESH_COOLDOWN
}

fn clamp_cooldown(value: Duration) -> Duration {
    value.clamp(MIN_REFRESH_COOLDOWN, MAX_REFRESH_COOLDOWN)
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

    #[test]
    fn retry_after_uses_cpa_headers_and_clamps_cooldown() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "2".parse().expect("header"));
        assert_eq!(parse_retry_after(&headers), MIN_REFRESH_COOLDOWN);
        headers.remove(reqwest::header::RETRY_AFTER);
        headers.insert("retry-after-ms", "12000".parse().expect("header"));
        assert_eq!(parse_retry_after(&headers), Duration::from_secs(12));
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "9999".parse().expect("header"),
        );
        assert_eq!(parse_retry_after(&headers), MAX_REFRESH_COOLDOWN);
    }

    #[tokio::test]
    async fn cancelled_leader_releases_and_fails_waiting_followers() {
        let key = "cancelled-leader-refresh".to_owned();
        let (flight, leader) = acquire_flight(&key);
        assert!(leader);
        let follower = tokio::spawn({
            let flight = flight.clone();
            async move { flight.wait().await }
        });

        drop(LeaderFlightGuard::new(key.clone(), flight));

        let result = tokio::time::timeout(Duration::from_secs(1), follower)
            .await
            .expect("follower must be notified")
            .expect("follower task");
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("cancelled leader must fail followers"),
        };
        assert_eq!(error.kind(), RefreshErrorKind::Transient);
        assert!(error.message().contains("leader was cancelled"));

        let (flight, leader) = acquire_flight(&key);
        assert!(leader);
        release_flight(&key, &flight);
    }
}
