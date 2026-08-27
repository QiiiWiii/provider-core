use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use async_trait::async_trait;
use provider_core::{
    AccountAuthState, AccountId, AccountRepository, AccountRuntimeState, CredentialKind,
    CredentialUpdate, CredentialWriteOutcome, DiscoveredProviderModel, ProviderAccount,
    ProviderError, ProviderKind, ProviderRequest, ProviderStream, RefreshError, RefreshErrorKind,
    RefreshOutcome, RefreshTrigger, StoredProviderAccount, WireFormat, usage::ProviderUsageProfile,
};

use super::{
    contract::{CREDENTIAL_FORMAT_VERSION, PERSISTENCE_RETRY_SECONDS, REFRESH_LEAD_SECONDS},
    count_tokens::prepare_count_tokens_request,
    credentials::{ClaudeOAuthCredentialError, ClaudeOAuthCredentials, unix_timestamp},
    driver::ClaudeOAuthDriver,
    models::discover_models,
    request::prepare_request,
};

pub(super) struct ClaudeOAuthAccount {
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

impl ClaudeOAuthAccount {
    pub(super) fn from_stored(
        driver: Arc<ClaudeOAuthDriver>,
        account: StoredProviderAccount,
        repository: Arc<dyn AccountRepository>,
    ) -> Result<Self, ClaudeOAuthCredentialError> {
        let credentials = ClaudeOAuthCredentials::from_json(&account.credential.credential_json)?;
        Ok(Self {
            driver,
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
        })
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
        self.driver.client.execute_stream(body, headers).await
    }

    async fn count_tokens(&self, request: ProviderRequest) -> Result<u64, ProviderError> {
        let (body, headers) = prepare_count_tokens_request(&request, &self.snapshot().credentials)?;
        self.driver.client.count_tokens(body, headers).await
    }

    async fn discover_models(&self) -> Result<Vec<DiscoveredProviderModel>, ProviderError> {
        discover_models()
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

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod transport_tests;
