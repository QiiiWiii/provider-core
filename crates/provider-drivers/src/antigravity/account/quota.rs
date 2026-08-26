use async_trait::async_trait;
use provider_core::{
    ProviderError, ProviderErrorKind, ProviderQuotaError, ProviderQuotaErrorKind,
    ProviderQuotaFetch, ProviderQuotaSource,
};

use super::AntigravityAccount;

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
