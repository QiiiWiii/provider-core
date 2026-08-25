use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use super::{
    client::AntigravityClient,
    contract::{CREDENTIAL_FORMAT_VERSION, PERSISTENCE_RETRY_SECONDS, REFRESH_LEAD_SECONDS},
    credentials::{AntigravityAuthError, AntigravityCredentials},
    models::{antigravity_models, discovered_models},
    oauth::AntigravityOAuthClient,
    quota::AntigravityQuotaClient,
    refresh::AntigravityRefreshClient,
    version,
};
use async_trait::async_trait;
use provider_core::{
    AccountAuthState, AccountId, AccountProvisioningInput, AccountRepository, AccountRuntimeState,
    CredentialKind, CredentialUpdate, CredentialWriteOutcome, ManagedProviderDriver, NewCredential,
    NewProviderAccount, ProviderAccount, ProviderAccountUpdate, ProviderConfigurationError,
    ProviderDriver, ProviderError, ProviderErrorKind, ProviderKind, ProviderModel,
    ProviderQuotaError, ProviderQuotaErrorKind, ProviderQuotaFetch, ProviderQuotaSource,
    ProviderRequest, ProviderStream, RefreshError, RefreshErrorKind, RefreshOutcome,
    RefreshTrigger, StartedProviderOAuth, StoredProviderAccount, WireFormat,
};
use secrecy::ExposeSecret;
use tokio::sync::Mutex;

pub struct AntigravityDriver {
    client: AntigravityClient,
    refresh_client: AntigravityRefreshClient,
    oauth_client: AntigravityOAuthClient,
    quota_client: AntigravityQuotaClient,
}

struct AntigravityAccount {
    driver: Arc<AntigravityDriver>,
    account_id: AccountId,
    repository: Option<Arc<dyn AccountRepository>>,
    state: RwLock<AntigravityState>,
    project_gate: Mutex<()>,
}

#[derive(Clone)]
struct AntigravityState {
    credentials: AntigravityCredentials,
    revision: u64,
    generation: u64,
    format_version: u32,
    expires_at: Option<i64>,
    next_refresh_at: Option<i64>,
    auth_state: AccountAuthState,
    pending_update: Option<CredentialUpdate>,
}

impl Default for AntigravityDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl AntigravityDriver {
    #[must_use]
    pub fn new() -> Self {
        version::start_background_refresh();
        Self {
            client: AntigravityClient::new(),
            refresh_client: AntigravityRefreshClient::new(),
            oauth_client: AntigravityOAuthClient::new(),
            quota_client: AntigravityQuotaClient::new(),
        }
    }

    #[cfg(feature = "test-util")]
    #[must_use]
    pub fn for_test(base_url: impl Into<String>) -> Arc<Self> {
        let base_url = base_url.into();
        Arc::new(Self {
            client: AntigravityClient::with_base_url(base_url.clone()),
            refresh_client: AntigravityRefreshClient::new(),
            oauth_client: AntigravityOAuthClient::new(),
            quota_client: AntigravityQuotaClient::with_base_url(base_url),
        })
    }

    #[cfg(feature = "test-util")]
    #[must_use]
    pub fn test_account(
        self: &Arc<Self>,
        access_token: impl Into<String>,
    ) -> Arc<dyn ProviderAccount> {
        let account_id = AccountId::new("test-antigravity").expect("account ID");
        Arc::new(AntigravityAccount::build(
            self.clone(),
            account_id,
            AntigravityState {
                credentials: AntigravityCredentials::for_test(access_token),
                revision: 0,
                generation: 0,
                format_version: CREDENTIAL_FORMAT_VERSION,
                expires_at: None,
                next_refresh_at: None,
                auth_state: AccountAuthState::Active,
                pending_update: None,
            },
            None,
        ))
    }
}

impl ProviderDriver for AntigravityDriver {
    fn name(&self) -> &'static str {
        "antigravity"
    }

    fn native_format(&self) -> WireFormat {
        WireFormat::OpenAiResponses
    }

    fn models(&self) -> &[ProviderModel] {
        antigravity_models()
    }
}

#[async_trait]
impl ManagedProviderDriver for AntigravityDriver {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Antigravity
    }

    fn supports_quota(&self) -> bool {
        true
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
                "Antigravity accounts require OAuth credential JSON",
            ));
        };
        let label = label.trim().to_owned();
        if label.is_empty() {
            return Err(ProviderConfigurationError::new(
                "Antigravity account label must not be empty",
            ));
        }
        let credentials = AntigravityCredentials::from_json(&credential_json)
            .map_err(|error| ProviderConfigurationError::new(error.to_string()))?;
        validate_imported_credentials(&credentials)?;
        let credential_json = credentials
            .to_json()
            .map_err(|error| ProviderConfigurationError::new(error.to_string()))?;
        Ok(NewProviderAccount {
            id,
            provider: ProviderKind::Antigravity,
            label,
            group_label,
            priority: 0,
            config_json: "{}".to_owned(),
            enabled: true,
            credential: NewCredential {
                kind: CredentialKind::Oauth,
                format_version: CREDENTIAL_FORMAT_VERSION,
                credential_json,
                expires_at: credentials.expires_at(),
                last_refreshed_at: credentials.last_refreshed_at(),
            },
        })
    }

    fn prepare_account_update(
        &self,
        mut update: ProviderAccountUpdate,
    ) -> Result<ProviderAccountUpdate, ProviderConfigurationError> {
        update.label = update.label.trim().to_owned();
        if update.label.is_empty() {
            return Err(ProviderConfigurationError::new(
                "Antigravity account label must not be empty",
            ));
        }
        let config =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&update.config_json)
                .map_err(|_| {
                    ProviderConfigurationError::new(
                        "Antigravity configuration must be a JSON object",
                    )
                })?;
        if !config.is_empty() {
            return Err(ProviderConfigurationError::new(
                "Antigravity upstream URL is managed by the driver",
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
                "unsupported Antigravity credential format",
            ));
        }
        let credentials = AntigravityCredentials::from_json(&credential.credential_json)
            .map_err(|error| ProviderConfigurationError::new(error.to_string()))?;
        validate_imported_credentials(&credentials)
    }

    async fn start_oauth(&self) -> Result<StartedProviderOAuth, ProviderConfigurationError> {
        self.oauth_client.start().await
    }

    fn build_account(
        self: Arc<Self>,
        account: StoredProviderAccount,
        repository: Arc<dyn AccountRepository>,
    ) -> Result<Arc<dyn ProviderAccount>, ProviderConfigurationError> {
        AntigravityAccount::from_stored(self, account, repository)
            .map(|account| Arc::new(account) as Arc<dyn ProviderAccount>)
            .map_err(|error| ProviderConfigurationError::new(error.to_string()))
    }
}

impl AntigravityAccount {
    fn from_stored(
        driver: Arc<AntigravityDriver>,
        account: StoredProviderAccount,
        repository: Arc<dyn AccountRepository>,
    ) -> Result<Self, AntigravityAuthError> {
        if account.provider != ProviderKind::Antigravity {
            return Err(AntigravityAuthError::InvalidStoredProvider);
        }
        if account.credential.format_version != CREDENTIAL_FORMAT_VERSION {
            return Err(AntigravityAuthError::UnsupportedCredentialFormat(
                account.credential.format_version,
            ));
        }
        let credentials = AntigravityCredentials::from_json(&account.credential.credential_json)?;
        let account_id = account.id;
        let next_refresh_at = initial_refresh_at(
            account.auth_state,
            credentials.refresh_token().is_some(),
            credentials.expires_at(),
            &account_id,
        );
        Ok(Self::build(
            driver,
            account_id,
            AntigravityState {
                credentials,
                revision: account.credential.revision,
                generation: 0,
                format_version: account.credential.format_version,
                expires_at: account.credential.expires_at,
                next_refresh_at,
                auth_state: account.auth_state,
                pending_update: None,
            },
            Some(repository),
        ))
    }

    fn build(
        driver: Arc<AntigravityDriver>,
        account_id: AccountId,
        state: AntigravityState,
        repository: Option<Arc<dyn AccountRepository>>,
    ) -> Self {
        Self {
            driver,
            account_id,
            repository,
            state: RwLock::new(state),
            project_gate: Mutex::new(()),
        }
    }

    fn state(&self) -> RwLockReadGuard<'_, AntigravityState> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn state_mut(&self) -> RwLockWriteGuard<'_, AntigravityState> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    async fn persist_pending(
        &self,
        repository: &Arc<dyn AccountRepository>,
        pending: CredentialUpdate,
    ) -> Result<RefreshOutcome, RefreshError> {
        match repository
            .compare_and_swap_credential(&self.account_id, pending)
            .await
        {
            Ok(CredentialWriteOutcome::Updated { revision }) => {
                let mut state = self.state_mut();
                state.revision = revision;
                state.pending_update = None;
                state.next_refresh_at = refresh_at(state.expires_at, &self.account_id);
                Ok(RefreshOutcome {
                    state: runtime_state(&state),
                })
            }
            Ok(CredentialWriteOutcome::Conflict) => Err(RefreshError::new(
                RefreshErrorKind::Internal,
                "Antigravity credential revision conflict",
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

    async fn mark_reauth_required(&self, repository: &Arc<dyn AccountRepository>) {
        let now = unix_timestamp();
        let _ = repository
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
    }

    async fn credentials_for_request(&self) -> Result<AntigravityCredentials, ProviderError> {
        let current = self.state().credentials.clone();
        if !current.project_id().is_empty() {
            return Ok(current);
        }

        let _gate = self.project_gate.lock().await;
        let current = self.state().credentials.clone();
        if !current.project_id().is_empty() {
            return Ok(current);
        }
        let project_id = self
            .driver
            .oauth_client
            .fetch_project_id(current.access_token().expose_secret())
            .await
            .map_err(|error| {
                ProviderError::new(
                    ProviderErrorKind::Authentication,
                    format!("Antigravity project discovery failed: {error}"),
                )
            })?;
        let credentials = current
            .with_project_id(project_id)
            .map_err(|error| ProviderError::new(ProviderErrorKind::Internal, error.to_string()))?;
        self.persist_project_id(credentials.clone()).await?;
        Ok(credentials)
    }

    async fn persist_project_id(
        &self,
        credentials: AntigravityCredentials,
    ) -> Result<(), ProviderError> {
        let updated_at = unix_timestamp();
        let credential_json = credentials
            .to_json()
            .map_err(|error| ProviderError::new(ProviderErrorKind::Internal, error.to_string()))?;
        let revision = self.state().revision;
        let update = CredentialUpdate {
            expected_revision: revision,
            kind: CredentialKind::Oauth,
            format_version: CREDENTIAL_FORMAT_VERSION,
            credential_json,
            expires_at: credentials.expires_at(),
            last_refreshed_at: credentials.last_refreshed_at(),
            updated_at,
        };
        let Some(repository) = self.repository.as_ref() else {
            self.state_mut().credentials = credentials;
            return Ok(());
        };
        match repository
            .compare_and_swap_credential(&self.account_id, update.clone())
            .await
        {
            Ok(CredentialWriteOutcome::Updated { revision }) => {
                let mut state = self.state_mut();
                state.credentials = credentials;
                state.revision = revision;
                state.pending_update = None;
            }
            Ok(CredentialWriteOutcome::Conflict) => {
                return Err(ProviderError::new(
                    ProviderErrorKind::Internal,
                    "Antigravity credential revision conflict while saving project_id",
                ));
            }
            Err(_) => {
                let mut state = self.state_mut();
                state.credentials = credentials;
                state.pending_update = Some(update);
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ProviderAccount for AntigravityAccount {
    fn provider_name(&self) -> &'static str {
        "antigravity"
    }

    fn account_id(&self) -> &AccountId {
        &self.account_id
    }

    fn native_format(&self) -> WireFormat {
        WireFormat::OpenAiResponses
    }

    fn runtime_state(&self) -> AccountRuntimeState {
        runtime_state(&self.state())
    }

    fn credential_revision(&self) -> u64 {
        self.state().revision
    }

    fn quota_source(&self) -> Option<&dyn ProviderQuotaSource> {
        Some(self)
    }

    async fn execute_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderStream, ProviderError> {
        let credentials = self.credentials_for_request().await?;
        let stream = self
            .driver
            .client
            .execute_stream(&credentials, &request)
            .await?;
        Ok(stream)
    }

    async fn count_tokens(&self, request: ProviderRequest) -> Result<u64, ProviderError> {
        let credentials = self.credentials_for_request().await?;
        self.driver
            .client
            .count_tokens(&credentials, &request)
            .await
    }

    async fn discover_models(
        &self,
    ) -> Result<Vec<provider_core::DiscoveredProviderModel>, ProviderError> {
        Ok(discovered_models())
    }

    fn fallback_models(&self) -> &[ProviderModel] {
        antigravity_models()
    }

    async fn refresh_credentials(
        &self,
        _trigger: RefreshTrigger,
    ) -> Result<RefreshOutcome, RefreshError> {
        let repository = self.repository.as_ref().ok_or_else(|| {
            RefreshError::new(
                RefreshErrorKind::Internal,
                "Antigravity account has no credential repository",
            )
        })?;
        let pending = self.state().pending_update.clone();
        if let Some(pending) = pending {
            return self.persist_pending(repository, pending).await;
        }
        let current = self.state().clone();
        if current.auth_state == AccountAuthState::ReauthRequired {
            return Err(RefreshError::new(
                RefreshErrorKind::ReauthRequired,
                "Antigravity account requires authorization",
            ));
        }
        let tokens = match self
            .driver
            .refresh_client
            .refresh(&current.credentials)
            .await
        {
            Ok(tokens) => tokens,
            Err(error) if error.kind() == RefreshErrorKind::ReauthRequired => {
                self.mark_reauth_required(repository).await;
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        let refreshed_at = unix_timestamp();
        let (credentials, expires_at) = current
            .credentials
            .refreshed(&tokens, refreshed_at)
            .map_err(|error| RefreshError::new(RefreshErrorKind::Internal, error.to_string()))?;
        let credential_json = credentials
            .to_json()
            .map_err(|error| RefreshError::new(RefreshErrorKind::Internal, error.to_string()))?;
        let update = CredentialUpdate {
            expected_revision: current.revision,
            kind: CredentialKind::Oauth,
            format_version: current.format_version,
            credential_json,
            expires_at: Some(expires_at),
            last_refreshed_at: Some(refreshed_at),
            updated_at: refreshed_at,
        };
        match repository
            .compare_and_swap_credential(&self.account_id, update.clone())
            .await
        {
            Ok(CredentialWriteOutcome::Updated { revision }) => {
                let mut state = self.state_mut();
                state.credentials = credentials;
                state.revision = revision;
                state.generation = state.generation.saturating_add(1);
                state.expires_at = Some(expires_at);
                state.next_refresh_at = refresh_at(Some(expires_at), &self.account_id);
                state.auth_state = AccountAuthState::Active;
                state.pending_update = None;
                Ok(RefreshOutcome {
                    state: runtime_state(&state),
                })
            }
            Ok(CredentialWriteOutcome::Conflict) => Err(RefreshError::new(
                RefreshErrorKind::Internal,
                "Antigravity credential revision conflict",
            )),
            Err(_) => {
                let mut state = self.state_mut();
                state.credentials = credentials;
                state.generation = state.generation.saturating_add(1);
                state.expires_at = Some(expires_at);
                state.next_refresh_at = refreshed_at.checked_add(PERSISTENCE_RETRY_SECONDS);
                state.auth_state = AccountAuthState::Active;
                state.pending_update = Some(update);
                Ok(RefreshOutcome {
                    state: runtime_state(&state),
                })
            }
        }
    }
}

#[async_trait]
impl ProviderQuotaSource for AntigravityAccount {
    async fn fetch_quota(&self) -> Result<ProviderQuotaFetch, ProviderQuotaError> {
        let credentials = self
            .credentials_for_request()
            .await
            .map_err(quota_provider_error)?;
        let revision = self.state().revision;
        let snapshot = self
            .driver
            .quota_client
            .fetch(self.account_id.as_str(), &credentials)
            .await?;
        Ok(ProviderQuotaFetch {
            snapshot,
            credential_revision: revision,
        })
    }
}

fn validate_imported_credentials(
    credentials: &AntigravityCredentials,
) -> Result<(), ProviderConfigurationError> {
    if credentials.refresh_token().is_none() {
        return Err(ProviderConfigurationError::new(
            "Antigravity credential is missing refresh_token",
        ));
    }
    Ok(())
}

fn quota_provider_error(error: ProviderError) -> ProviderQuotaError {
    let kind = match error.kind() {
        ProviderErrorKind::Authentication => ProviderQuotaErrorKind::Authentication,
        ProviderErrorKind::RateLimited | ProviderErrorKind::Capacity => {
            ProviderQuotaErrorKind::RateLimited
        }
        ProviderErrorKind::InvalidRequest | ProviderErrorKind::Upstream => {
            ProviderQuotaErrorKind::Upstream
        }
        ProviderErrorKind::Internal => ProviderQuotaErrorKind::Internal,
    };
    let status = error.upstream_status();
    let quota_error = ProviderQuotaError::new(kind, error.message());
    if let Some(status) = status {
        quota_error.with_upstream_status(status)
    } else {
        quota_error
    }
}

fn runtime_state(state: &AntigravityState) -> AccountRuntimeState {
    AccountRuntimeState {
        generation: state.generation,
        next_refresh_at: state.next_refresh_at,
        auth_state: state.auth_state,
        persistence_pending: state.pending_update.is_some(),
    }
}

fn refresh_at(expires_at: Option<i64>, account_id: &AccountId) -> Option<i64> {
    let expires_at = expires_at?;
    let mut hasher = DefaultHasher::new();
    account_id.hash(&mut hasher);
    let jitter = i64::try_from(hasher.finish() % 31).unwrap_or_default();
    expires_at.checked_sub(REFRESH_LEAD_SECONDS + jitter)
}

fn initial_refresh_at(
    auth_state: AccountAuthState,
    can_refresh: bool,
    expires_at: Option<i64>,
    account_id: &AccountId,
) -> Option<i64> {
    (auth_state == AccountAuthState::Active && can_refresh)
        .then(|| refresh_at(expires_at, account_id))
        .flatten()
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or_default()
}
