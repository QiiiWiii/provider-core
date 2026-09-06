mod quota;
#[cfg(all(test, feature = "test-util"))]
mod tests;

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use super::driver::AntigravityDriver;
use super::{
    contract::{CREDENTIAL_FORMAT_VERSION, PERSISTENCE_RETRY_SECONDS, REFRESH_LEAD_SECONDS},
    credentials::{AntigravityAuthError, AntigravityCredentials},
    models::antigravity_models,
};
use async_trait::async_trait;
use provider_core::{
    AccountAuthState, AccountId, AccountRepository, AccountRuntimeState, CredentialKind,
    CredentialUpdate, CredentialWriteOutcome, ProviderAccount, ProviderConfigurationError,
    ProviderError, ProviderErrorKind, ProviderKind, ProviderModel, ProviderQuotaSource,
    ProviderRequest, ProviderStream, RefreshError, RefreshErrorKind, RefreshOutcome,
    RefreshTrigger, StoredProviderAccount, WireFormat,
    usage::{CacheEligibility, PricingMode, ProviderUsageProfile},
};
use secrecy::ExposeSecret;
use tokio::sync::Mutex;
pub(super) struct AntigravityAccount {
    driver: Arc<AntigravityDriver>,
    account_id: AccountId,
    repository: Option<Arc<dyn AccountRepository>>,
    state: RwLock<AntigravityState>,
    project_gate: Mutex<()>,
}

#[derive(Clone)]
pub(super) struct AntigravityState {
    credentials: AntigravityCredentials,
    revision: u64,
    quota_identity_revision: u64,
    generation: u64,
    format_version: u32,
    expires_at: Option<i64>,
    next_refresh_at: Option<i64>,
    auth_state: AccountAuthState,
    pending_update: Option<CredentialUpdate>,
}

#[cfg(feature = "test-util")]
impl AntigravityState {
    pub(super) fn for_test(access_token: impl Into<String>) -> Self {
        Self {
            credentials: AntigravityCredentials::for_test(access_token),
            revision: 0,
            quota_identity_revision: 0,
            generation: 0,
            format_version: CREDENTIAL_FORMAT_VERSION,
            expires_at: None,
            next_refresh_at: None,
            auth_state: AccountAuthState::Active,
            pending_update: None,
        }
    }
}
impl AntigravityAccount {
    pub(super) fn from_stored(
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
                quota_identity_revision: account.credential.quota_identity_revision,
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

    pub(super) fn build(
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
            Err(error) => {
                let mut state = self.state_mut();
                state.next_refresh_at = unix_timestamp().checked_add(PERSISTENCE_RETRY_SECONDS);
                Err(RefreshError::new(
                    RefreshErrorKind::Transient,
                    format!("Antigravity credential persistence failed; retry scheduled: {error}"),
                ))
            }
        }
    }

    async fn mark_reauth_required(
        &self,
        repository: &Arc<dyn AccountRepository>,
    ) -> Result<(), RefreshError> {
        let now = unix_timestamp();
        if let Err(error) = repository
            .update_auth_state(
                &self.account_id,
                AccountAuthState::ReauthRequired,
                Some("refresh_reauth_required"),
                now,
            )
            .await
        {
            let mut state = self.state_mut();
            state.next_refresh_at = now.checked_add(PERSISTENCE_RETRY_SECONDS);
            return Err(RefreshError::new(
                RefreshErrorKind::Transient,
                format!(
                    "Antigravity reauthentication state persistence failed; retry scheduled: {error}"
                ),
            ));
        }
        let mut state = self.state_mut();
        state.auth_state = AccountAuthState::ReauthRequired;
        state.next_refresh_at = None;
        Ok(())
    }

    async fn credentials_for_discovery(&self) -> Result<AntigravityCredentials, ProviderError> {
        if self.repository.is_some() && self.refresh_due() {
            match self.refresh_credentials(RefreshTrigger::Scheduled).await {
                Ok(_) => {}
                Err(error) if error.kind() == RefreshErrorKind::ReauthRequired => {
                    return Err(ProviderError::new(
                        ProviderErrorKind::Authentication,
                        error.to_string(),
                    ));
                }
                Err(error) => {
                    tracing::warn!(
                        account_id = self.account_id.as_str(),
                        error = %error,
                        "Antigravity credential refresh before model discovery failed"
                    );
                }
            }
        }
        match self.credentials_for_request().await {
            Ok(credentials) => Ok(credentials),
            Err(error) => {
                tracing::warn!(
                    account_id = self.account_id.as_str(),
                    error = %error,
                    "Antigravity project discovery before model listing failed"
                );
                Ok(self.state().credentials.clone())
            }
        }
    }

    fn refresh_due(&self) -> bool {
        self.state()
            .next_refresh_at
            .is_some_and(|at| at <= unix_timestamp())
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
            Err(error) => {
                let mut state = self.state_mut();
                state.credentials = credentials;
                state.pending_update = Some(update);
                return Err(ProviderError::new(
                    ProviderErrorKind::Internal,
                    format!("Antigravity project_id persistence failed; retry pending: {error}"),
                ));
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

    fn model_pricing_alias(&self, upstream_model: &str) -> Option<&'static str> {
        super::models::model_pricing_alias(upstream_model)
    }

    fn usage_profile(&self) -> Option<ProviderUsageProfile> {
        Some(ProviderUsageProfile {
            provider: ProviderKind::Antigravity,
            contract: super::usage::antigravity_usage_contract(
                CacheEligibility::Eligible,
                PricingMode::Default,
            ),
        })
    }

    fn runtime_state(&self) -> AccountRuntimeState {
        runtime_state(&self.state())
    }

    fn credential_revision(&self) -> u64 {
        self.state().revision
    }

    fn credential_identity_revision(&self) -> u64 {
        self.state().quota_identity_revision
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
        let credentials = self.credentials_for_discovery().await?;
        self.driver.model_client.discover(&credentials).await
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
                self.mark_reauth_required(repository).await?;
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
            Err(error) => {
                let mut state = self.state_mut();
                state.credentials = credentials;
                state.generation = state.generation.saturating_add(1);
                state.expires_at = Some(expires_at);
                state.next_refresh_at = refreshed_at.checked_add(PERSISTENCE_RETRY_SECONDS);
                state.auth_state = AccountAuthState::Active;
                state.pending_update = Some(update);
                Err(RefreshError::new(
                    RefreshErrorKind::Transient,
                    format!("Antigravity credential persistence failed; retry scheduled: {error}"),
                ))
            }
        }
    }
}

pub(super) fn validate_imported_credentials(
    credentials: &AntigravityCredentials,
) -> Result<(), ProviderConfigurationError> {
    if credentials.refresh_token().is_none() {
        return Err(ProviderConfigurationError::new(
            "Antigravity credential is missing refresh_token",
        ));
    }
    Ok(())
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
