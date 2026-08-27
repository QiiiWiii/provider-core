use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use provider_core::{
    AccountAuthState, AccountId, AccountRepository, AccountRepositoryError, CredentialUpdate,
    CredentialWriteOutcome, StoredProviderAccount,
};

pub(super) struct FailingOnceRepository {
    pub(super) writes: AtomicUsize,
    pub(super) update: Mutex<Option<CredentialUpdate>>,
    pub(super) auth_state: Mutex<Option<AccountAuthState>>,
}

#[async_trait]
impl AccountRepository for FailingOnceRepository {
    async fn load_enabled_accounts(
        &self,
    ) -> Result<Vec<StoredProviderAccount>, AccountRepositoryError> {
        Ok(Vec::new())
    }

    async fn compare_and_swap_credential(
        &self,
        _account_id: &AccountId,
        update: CredentialUpdate,
    ) -> Result<CredentialWriteOutcome, AccountRepositoryError> {
        let write = self.writes.fetch_add(1, Ordering::SeqCst);
        *self.update.lock().expect("update lock") = Some(update);
        if write == 0 {
            Err(AccountRepositoryError::new("temporary storage failure"))
        } else {
            Ok(CredentialWriteOutcome::Updated { revision: 2 })
        }
    }

    async fn update_auth_state(
        &self,
        _account_id: &AccountId,
        state: AccountAuthState,
        _safe_error_code: Option<&str>,
        _updated_at: i64,
    ) -> Result<(), AccountRepositoryError> {
        *self.auth_state.lock().expect("auth state lock") = Some(state);
        Ok(())
    }
}
