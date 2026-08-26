use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use async_trait::async_trait;
use provider_core::{
    AccountAuthState, AccountId, AccountProvisioningInput, AccountRepository, AccountRuntimeState,
    CredentialKind, CredentialUpdate, CredentialWriteOutcome, DiscoveredProviderModel,
    ManagedProviderDriver, NewCredential, NewProviderAccount, ProviderAccount,
    ProviderAccountUpdate, ProviderConfigurationError, ProviderDriver, ProviderError,
    ProviderErrorKind, ProviderKind, ProviderModel, ProviderRequest, ProviderStream, RefreshError,
    RefreshErrorKind, RefreshOutcome, RefreshTrigger, StartedProviderOAuth, StoredProviderAccount,
    WireFormat, collect_bounded_body, parse_provider_retry_after, usage::ProviderUsageProfile,
};

use super::{
    count_tokens::{parse_count_tokens_response, prepare_count_tokens_request},
    credentials::{ClaudeOAuthCredentialError, ClaudeOAuthCredentials, unix_timestamp},
    oauth::{ClaudeOAuthClient, claude_http_client},
    refresh::ClaudeRefreshClient,
    request::prepare_request,
    response::response_stream,
};

const CREDENTIAL_FORMAT_VERSION: u32 = 1;
const REFRESH_LEAD_SECONDS: i64 = 5 * 60;
const PERSISTENCE_RETRY_SECONDS: i64 = 30;
const COUNT_TOKENS_RESPONSE_LIMIT: usize = 64 * 1024;
const ERROR_RESPONSE_LIMIT: usize = 64 * 1024;
const API_ROOT: &str = "https://api.anthropic.com/v1";
const CLAUDE_OAUTH_MODELS: &[&str] = &[
    "claude-haiku-4-5-20251001",
    "claude-sonnet-4-5-20250929",
    "claude-sonnet-4-6",
    "claude-opus-4-6",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-fable-5",
    "claude-opus-4-5-20251101",
    "claude-opus-4-1-20250805",
    "claude-opus-4-20250514",
    "claude-sonnet-4-20250514",
    "claude-3-7-sonnet-20250219",
    "claude-3-5-haiku-20241022",
];

pub struct ClaudeOAuthDriver {
    inference: reqwest::Client,
    oauth: ClaudeOAuthClient,
    refresh: ClaudeRefreshClient,
    api_root: String,
}

struct ClaudeOAuthAccount {
    driver: Arc<ClaudeOAuthDriver>,
    account_id: AccountId,
    repository: Arc<dyn AccountRepository>,
    state: RwLock<ClaudeOAuthState>,
    refresh_gate: tokio::sync::Mutex<()>,
}

#[derive(Clone)]
struct ClaudeOAuthState {
    credentials: ClaudeOAuthCredentials,
    revision: u64,
    generation: u64,
    auth_state: AccountAuthState,
    next_refresh_at: Option<i64>,
    pending_update: Option<CredentialUpdate>,
}

impl Default for ClaudeOAuthDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeOAuthDriver {
    pub fn new() -> Self {
        Self {
            inference: claude_http_client(),
            oauth: ClaudeOAuthClient::new(),
            refresh: ClaudeRefreshClient::new(),
            api_root: API_ROOT.to_owned(),
        }
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn for_test(api_root: &str, token_url: &str) -> Arc<Self> {
        Arc::new(Self {
            inference: reqwest::Client::new(),
            oauth: ClaudeOAuthClient::with_token_url(token_url),
            refresh: ClaudeRefreshClient::with_token_url(token_url),
            api_root: api_root.trim_end_matches('/').to_owned(),
        })
    }
}

impl ProviderDriver for ClaudeOAuthDriver {
    fn name(&self) -> &'static str {
        "claude_oauth"
    }

    fn native_format(&self) -> WireFormat {
        WireFormat::ClaudeMessages
    }

    fn models(&self) -> &[ProviderModel] {
        &[]
    }
}

#[async_trait]
impl ManagedProviderDriver for ClaudeOAuthDriver {
    fn kind(&self) -> ProviderKind {
        ProviderKind::ClaudeOAuth
    }

    fn prepare_account(
        &self,
        input: AccountProvisioningInput,
    ) -> Result<NewProviderAccount, ProviderConfigurationError> {
        let AccountProvisioningInput::CredentialJson {
            id,
            label,
            group_label,
            credential_json,
        } = input
        else {
            return Err(ProviderConfigurationError::new(
                "Claude OAuth accounts require OAuth credential JSON",
            ));
        };
        let label = normalize_label(&label)?;
        let credentials =
            ClaudeOAuthCredentials::from_json(&credential_json).map_err(configuration_error)?;
        let credential_json = credentials.to_json().map_err(configuration_error)?;
        Ok(NewProviderAccount {
            id,
            provider: ProviderKind::ClaudeOAuth,
            label,
            group_label,
            priority: 0,
            config_json: "{}".to_owned(),
            enabled: true,
            credential: NewCredential {
                kind: CredentialKind::Oauth,
                format_version: CREDENTIAL_FORMAT_VERSION,
                credential_json,
                expires_at: Some(credentials.expires_at()),
                last_refreshed_at: Some(credentials.last_refreshed_at()),
            },
        })
    }

    fn prepare_account_update(
        &self,
        mut update: ProviderAccountUpdate,
    ) -> Result<ProviderAccountUpdate, ProviderConfigurationError> {
        update.label = normalize_label(&update.label)?;
        if serde_json::from_str::<serde_json::Value>(&update.config_json)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .is_none_or(|value| !value.is_empty())
        {
            return Err(ProviderConfigurationError::new(
                "Claude OAuth account configuration must be empty",
            ));
        }
        update.config_json = "{}".to_owned();
        Ok(update)
    }

    fn validate_credential_replacement(
        &self,
        credential: &provider_core::StoredCredential,
    ) -> Result<(), ProviderConfigurationError> {
        if credential.kind != CredentialKind::Oauth
            || credential.format_version != CREDENTIAL_FORMAT_VERSION
        {
            return Err(ProviderConfigurationError::new(
                "Claude OAuth credential format is unsupported",
            ));
        }
        ClaudeOAuthCredentials::from_json(&credential.credential_json)
            .map(|_| ())
            .map_err(configuration_error)
    }

    async fn start_oauth(&self) -> Result<StartedProviderOAuth, ProviderConfigurationError> {
        self.oauth.start().await
    }

    fn build_account(
        self: Arc<Self>,
        account: StoredProviderAccount,
        repository: Arc<dyn AccountRepository>,
    ) -> Result<Arc<dyn ProviderAccount>, ProviderConfigurationError> {
        if account.provider != ProviderKind::ClaudeOAuth
            || account.credential.kind != CredentialKind::Oauth
            || account.credential.format_version != CREDENTIAL_FORMAT_VERSION
        {
            return Err(ProviderConfigurationError::new(
                "stored account is not Claude OAuth",
            ));
        }
        let credentials = ClaudeOAuthCredentials::from_json(&account.credential.credential_json)
            .map_err(configuration_error)?;
        Ok(Arc::new(ClaudeOAuthAccount {
            driver: self,
            account_id: account.id,
            repository,
            state: RwLock::new(ClaudeOAuthState {
                next_refresh_at: (account.auth_state == AccountAuthState::Active)
                    .then_some(credentials.expires_at() - REFRESH_LEAD_SECONDS),
                credentials,
                revision: account.credential.revision,
                generation: 0,
                auth_state: account.auth_state,
                pending_update: None,
            }),
            refresh_gate: tokio::sync::Mutex::new(()),
        }))
    }
}

impl ClaudeOAuthAccount {
    fn state(&self) -> RwLockReadGuard<'_, ClaudeOAuthState> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn state_mut(&self) -> RwLockWriteGuard<'_, ClaudeOAuthState> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn snapshot(&self) -> ClaudeOAuthState {
        self.state().clone()
    }

    async fn persist_pending(
        &self,
        pending: CredentialUpdate,
    ) -> Result<RefreshOutcome, RefreshError> {
        match self
            .repository
            .compare_and_swap_credential(&self.account_id, pending.clone())
            .await
        {
            Ok(CredentialWriteOutcome::Updated { revision }) => {
                let mut state = self.state_mut();
                state.revision = revision;
                state.pending_update = None;
                state.next_refresh_at = Some(state.credentials.expires_at() - REFRESH_LEAD_SECONDS);
                Ok(RefreshOutcome {
                    state: runtime_state(&state),
                })
            }
            Ok(CredentialWriteOutcome::Conflict) => Err(RefreshError::new(
                RefreshErrorKind::Internal,
                "Claude OAuth credential revision conflict",
            )),
            Err(_) => {
                let mut state = self.state_mut();
                state.next_refresh_at = unix_timestamp().checked_add(PERSISTENCE_RETRY_SECONDS);
                Ok(RefreshOutcome {
                    state: runtime_state(&state),
                })
            }
        }
    }

    async fn mark_reauth_required(&self) {
        let now = unix_timestamp();
        let _ = self
            .repository
            .update_auth_state(
                &self.account_id,
                AccountAuthState::ReauthRequired,
                Some("refresh_reauth_required"),
                now,
            )
            .await;
        let mut state = self.state_mut();
        state.auth_state = AccountAuthState::ReauthRequired;
        state.next_refresh_at = None;
        state.pending_update = None;
    }
}

fn runtime_state(state: &ClaudeOAuthState) -> AccountRuntimeState {
    AccountRuntimeState {
        generation: state.generation,
        next_refresh_at: state.next_refresh_at,
        auth_state: state.auth_state,
        persistence_pending: state.pending_update.is_some(),
    }
}

#[async_trait]
impl ProviderAccount for ClaudeOAuthAccount {
    fn provider_name(&self) -> &'static str {
        "claude_oauth"
    }

    fn account_id(&self) -> &AccountId {
        &self.account_id
    }

    fn native_format(&self) -> WireFormat {
        WireFormat::ClaudeMessages
    }

    fn usage_profile(&self) -> Option<ProviderUsageProfile> {
        Some(ProviderUsageProfile {
            provider: ProviderKind::ClaudeOAuth,
            contract: super::usage::claude_oauth_usage_contract(),
        })
    }

    fn runtime_state(&self) -> AccountRuntimeState {
        runtime_state(&self.state())
    }

    fn credential_revision(&self) -> u64 {
        self.state().revision
    }

    async fn execute_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderStream, ProviderError> {
        let state = self.snapshot();
        let (body, headers) = prepare_request(&request, &state.credentials)?;
        let response = self
            .driver
            .inference
            .post(format!("{}/messages?beta=true", self.driver.api_root))
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|error| {
                let provider_error = ProviderError::new(
                    ProviderErrorKind::Upstream,
                    "Claude OAuth upstream request failed",
                );
                if error.is_connect() {
                    provider_error.with_failover_reason(
                        provider_core::ProviderFailoverReason::PreconnectFailure,
                    )
                } else {
                    provider_error
                }
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(response).await);
        }
        response_stream(response).await
    }

    async fn count_tokens(&self, request: ProviderRequest) -> Result<u64, ProviderError> {
        let (body, headers) = prepare_count_tokens_request(&request, &self.snapshot().credentials)?;
        let response = self
            .driver
            .inference
            .post(format!(
                "{}/messages/count_tokens?beta=true",
                self.driver.api_root
            ))
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|error| {
                let provider_error = ProviderError::new(
                    ProviderErrorKind::Upstream,
                    "Claude OAuth token count request failed",
                );
                if error.is_connect() {
                    provider_error.with_failover_reason(
                        provider_core::ProviderFailoverReason::PreconnectFailure,
                    )
                } else {
                    provider_error
                }
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

    async fn discover_models(&self) -> Result<Vec<DiscoveredProviderModel>, ProviderError> {
        CLAUDE_OAUTH_MODELS
            .iter()
            .map(|id| {
                let metadata_json = serde_json::to_string(&ProviderModel::new(*id, "anthropic"))
                    .map_err(|_| {
                        ProviderError::new(
                            ProviderErrorKind::Internal,
                            "failed to normalize Claude OAuth model",
                        )
                    })?;
                Ok(DiscoveredProviderModel {
                    upstream_model: (*id).to_owned(),
                    input_modalities: None,
                    metadata_json,
                    routable: true,
                    pricing: None,
                })
            })
            .collect()
    }

    async fn refresh_credentials(
        &self,
        _trigger: RefreshTrigger,
    ) -> Result<RefreshOutcome, RefreshError> {
        let observed_generation = self.state().generation;
        let _refresh_guard = self.refresh_gate.lock().await;
        if self.state().generation != observed_generation {
            let pending = { self.state().pending_update.clone() };
            if let Some(pending) = pending {
                return self.persist_pending(pending).await;
            }
            let state = { runtime_state(&self.state()) };
            return Ok(RefreshOutcome { state });
        }
        let pending_update = { self.state().pending_update.clone() };
        if let Some(pending) = pending_update {
            return self.persist_pending(pending).await;
        }
        let current = self.snapshot();
        if current.auth_state == AccountAuthState::ReauthRequired {
            return Err(RefreshError::new(
                RefreshErrorKind::ReauthRequired,
                "Claude OAuth account requires authorization",
            ));
        }
        let credentials = match self.driver.refresh.refresh(&current.credentials).await {
            Ok(credentials) => credentials,
            Err(error) if error.kind() == RefreshErrorKind::ReauthRequired => {
                self.mark_reauth_required().await;
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        let credential_json = credentials
            .to_json()
            .map_err(|error| RefreshError::new(RefreshErrorKind::Internal, error.to_string()))?;
        let updated_at = unix_timestamp();
        let update = CredentialUpdate {
            expected_revision: current.revision,
            kind: CredentialKind::Oauth,
            format_version: CREDENTIAL_FORMAT_VERSION,
            credential_json,
            expires_at: Some(credentials.expires_at()),
            last_refreshed_at: Some(credentials.last_refreshed_at()),
            updated_at,
        };
        let outcome = self
            .repository
            .compare_and_swap_credential(&self.account_id, update.clone())
            .await;
        match outcome {
            Ok(CredentialWriteOutcome::Updated { revision }) => {
                let mut state = self.state_mut();
                state.credentials = credentials;
                state.revision = revision;
                state.generation = state.generation.saturating_add(1);
                state.auth_state = AccountAuthState::Active;
                state.next_refresh_at = Some(state.credentials.expires_at() - REFRESH_LEAD_SECONDS);
                state.pending_update = None;
                Ok(RefreshOutcome {
                    state: runtime_state(&state),
                })
            }
            Ok(CredentialWriteOutcome::Conflict) => Err(RefreshError::new(
                RefreshErrorKind::Internal,
                "Claude OAuth credential revision conflict",
            )),
            Err(_) => {
                let mut state = self.state_mut();
                state.credentials = credentials;
                state.generation = state.generation.saturating_add(1);
                state.auth_state = AccountAuthState::Active;
                state.next_refresh_at = updated_at.checked_add(PERSISTENCE_RETRY_SECONDS);
                state.pending_update = Some(update);
                Ok(RefreshOutcome {
                    state: runtime_state(&state),
                })
            }
        }
    }
}

async fn status_error(response: reqwest::Response) -> ProviderError {
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

fn normalize_label(value: &str) -> Result<String, ProviderConfigurationError> {
    let value = value.trim().to_owned();
    if value.is_empty() {
        return Err(ProviderConfigurationError::new(
            "Claude OAuth account label must not be empty",
        ));
    }
    Ok(value)
}

fn configuration_error(error: ClaudeOAuthCredentialError) -> ProviderConfigurationError {
    ProviderConfigurationError::new(error.to_string())
}

#[cfg(test)]
#[path = "account_tests.rs"]
mod tests;
