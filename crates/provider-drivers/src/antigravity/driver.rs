use std::sync::Arc;

#[cfg(feature = "test-util")]
use super::account::AntigravityState;
use super::{
    account::{AntigravityAccount, validate_imported_credentials},
    client::AntigravityClient,
    contract::CREDENTIAL_FORMAT_VERSION,
    credentials::AntigravityCredentials,
    models::antigravity_models,
    oauth::AntigravityOAuthClient,
    quota::AntigravityQuotaClient,
    refresh::AntigravityRefreshClient,
    version,
};
use async_trait::async_trait;
#[cfg(feature = "test-util")]
use provider_core::AccountId;
use provider_core::{
    AccountProvisioningInput, AccountRepository, CredentialKind, ManagedProviderDriver,
    NewCredential, NewProviderAccount, ProviderAccount, ProviderAccountUpdate,
    ProviderConfigurationError, ProviderDriver, ProviderKind, ProviderModel, StartedProviderOAuth,
    StoredProviderAccount, WireFormat,
};
pub struct AntigravityDriver {
    pub(super) client: AntigravityClient,
    pub(super) refresh_client: AntigravityRefreshClient,
    pub(super) oauth_client: AntigravityOAuthClient,
    pub(super) quota_client: AntigravityQuotaClient,
}
impl AntigravityDriver {
    pub fn new() -> Result<Self, ProviderConfigurationError> {
        version::start_background_refresh();
        Ok(Self {
            client: AntigravityClient::new()
                .map_err(|error| ProviderConfigurationError::new(error.to_string()))?,
            refresh_client: AntigravityRefreshClient::new()
                .map_err(|error| ProviderConfigurationError::new(error.to_string()))?,
            oauth_client: AntigravityOAuthClient::new()
                .map_err(|error| ProviderConfigurationError::new(error.to_string()))?,
            quota_client: AntigravityQuotaClient::new()
                .map_err(|error| ProviderConfigurationError::new(error.to_string()))?,
        })
    }

    #[cfg(feature = "test-util")]
    #[must_use]
    pub fn for_test(base_url: impl Into<String>) -> Arc<Self> {
        let base_url = base_url.into();
        Arc::new(Self {
            client: AntigravityClient::with_base_url(base_url.clone()).expect("test client"),
            refresh_client: AntigravityRefreshClient::new().expect("test refresh client"),
            oauth_client: AntigravityOAuthClient::new().expect("test OAuth client"),
            quota_client: AntigravityQuotaClient::with_base_url(base_url)
                .expect("test quota client"),
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
            AntigravityState::for_test(access_token),
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
