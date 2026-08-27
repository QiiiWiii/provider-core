use secrecy::{ExposeSecret, SecretString};

pub(crate) const CLIENT_ID_ENV: &str = "ANTIGRAVITY_OAUTH_CLIENT_ID";
pub(crate) const CLIENT_SECRET_ENV: &str = "ANTIGRAVITY_OAUTH_CLIENT_SECRET";

#[derive(Clone)]
pub(crate) struct AntigravityOAuthConfig {
    client_id: Option<String>,
    client_secret: Option<SecretString>,
}

impl AntigravityOAuthConfig {
    pub(crate) fn from_environment() -> Self {
        Self::from_values(
            environment_value(CLIENT_ID_ENV),
            environment_value(CLIENT_SECRET_ENV),
        )
    }

    pub(crate) fn credentials(&self) -> Result<(&str, &str), &'static str> {
        let client_id = self
            .client_id
            .as_deref()
            .ok_or("Antigravity OAuth client ID is not configured")?;
        let client_secret = self
            .client_secret
            .as_ref()
            .map(ExposeSecret::expose_secret)
            .ok_or("Antigravity OAuth client secret is not configured")?;
        Ok((client_id, client_secret))
    }

    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn for_test() -> Self {
        Self::from_values(
            Some("test-antigravity-client".to_owned()),
            Some("test-antigravity-secret".to_owned()),
        )
    }

    fn from_values(client_id: Option<String>, client_secret: Option<String>) -> Self {
        Self {
            client_id: client_id.filter(|value| !value.trim().is_empty()),
            client_secret: client_secret
                .filter(|value| !value.trim().is_empty())
                .map(SecretString::from),
        }
    }
}

fn environment_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_configuration_supplies_both_credentials() {
        let config = AntigravityOAuthConfig::for_test();

        assert_eq!(
            config.credentials().expect("test OAuth credentials").0,
            "test-antigravity-client"
        );
    }

    #[test]
    fn incomplete_configuration_is_rejected_when_used() {
        let config = AntigravityOAuthConfig::from_values(Some("client".to_owned()), None);

        assert_eq!(
            config.credentials().expect_err("missing client secret"),
            "Antigravity OAuth client secret is not configured"
        );
    }
}
