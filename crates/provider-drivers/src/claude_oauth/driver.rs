use std::sync::Arc;

use async_trait::async_trait;
use provider_core::{
    AccountProvisioningInput, AccountRepository, CredentialKind, ManagedProviderDriver,
    NewCredential, NewProviderAccount, ProviderAccount, ProviderAccountUpdate,
    ProviderConfigurationError, ProviderDriver, ProviderKind, ProviderModel, StartedProviderOAuth,
    StoredProviderAccount, WireFormat,
};

use super::{
    account::ClaudeOAuthAccount,
    client::ClaudeInferenceClient,
    contract::CREDENTIAL_FORMAT_VERSION,
    credentials::{ClaudeOAuthCredentialError, ClaudeOAuthCredentials},
    oauth::ClaudeOAuthClient,
    refresh::ClaudeRefreshClient,
};

pub struct ClaudeOAuthDriver {
    pub(super) client: ClaudeInferenceClient,
    pub(super) oauth: ClaudeOAuthClient,
    pub(super) refresh: ClaudeRefreshClient,
}

impl Default for ClaudeOAuthDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeOAuthDriver {
    pub fn new() -> Self {
        Self {
            client: ClaudeInferenceClient::new(),
            oauth: ClaudeOAuthClient::new(),
            refresh: ClaudeRefreshClient::new(),
        }
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn for_test(api_root: &str, token_url: &str) -> Arc<Self> {
        Arc::new(Self {
            client: ClaudeInferenceClient::with_api_root(api_root),
            oauth: ClaudeOAuthClient::with_token_url(token_url),
            refresh: ClaudeRefreshClient::with_token_url(token_url),
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
        ClaudeOAuthAccount::from_stored(self, account, repository)
            .map(|account| Arc::new(account) as Arc<dyn ProviderAccount>)
            .map_err(|error| ProviderConfigurationError::new(error.to_string()))
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
