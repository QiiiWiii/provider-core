use std::fmt;

#[cfg(test)]
use std::io::Read;

use secrecy::{ExposeSecret, SecretString};
use serde_json::{Map, Value};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::contract::{OIDC_CLIENT_ID, OIDC_ISSUER};
use super::refresh::RefreshedGrokTokens;

/// Validated credentials for one Grok provider account.
#[derive(Clone)]
pub struct GrokCredentials {
    document: Map<String, Value>,
    access_token: SecretString,
    refresh_token: Option<SecretString>,
    upstream_user_id: Option<String>,
    oidc_issuer: Option<String>,
    oidc_client_id: Option<String>,
    token_endpoint: Option<String>,
}

impl GrokCredentials {
    pub fn from_json(credential_json: &SecretString) -> Result<Self, GrokAuthError> {
        let document: Value = serde_json::from_str(credential_json.expose_secret())?;
        let document = document
            .as_object()
            .cloned()
            .ok_or(GrokAuthError::NotObject)?;
        Self::from_document(document)
    }

    #[must_use]
    pub(crate) const fn access_token(&self) -> &SecretString {
        &self.access_token
    }

    #[must_use]
    pub(crate) fn refresh_token(&self) -> Option<&SecretString> {
        self.refresh_token.as_ref()
    }

    #[must_use]
    pub(crate) fn upstream_user_id(&self) -> Option<&str> {
        self.upstream_user_id.as_deref()
    }

    #[must_use]
    pub(crate) fn oidc_issuer(&self) -> Option<&str> {
        self.oidc_issuer.as_deref()
    }

    #[must_use]
    pub(crate) fn oidc_client_id(&self) -> Option<&str> {
        self.oidc_client_id.as_deref()
    }

    #[must_use]
    pub(crate) fn has_supported_refresh_provenance(&self) -> bool {
        self.refresh_token.is_some()
            && self
                .oidc_issuer()
                .is_some_and(|issuer| issuer.trim_end_matches('/') == OIDC_ISSUER)
            && self.oidc_client_id() == Some(OIDC_CLIENT_ID)
    }

    pub(crate) fn with_upstream_user_id(&self, user_id: &str) -> Result<Self, GrokAuthError> {
        let user_id = user_id.trim();
        if user_id.is_empty() {
            return Err(GrokAuthError::MissingUpstreamUserId);
        }
        let mut document = self.document.clone();
        document.insert(
            "upstream_user_id".to_owned(),
            Value::String(user_id.to_owned()),
        );
        Self::from_document(document)
    }

    #[must_use]
    pub(crate) fn token_endpoint(&self) -> Option<&str> {
        self.token_endpoint.as_deref()
    }

    pub(crate) fn expires_at(&self) -> Result<Option<i64>, GrokAuthError> {
        timestamp_field(&self.document, "expired")
    }

    pub(crate) fn last_refreshed_at(&self) -> Result<Option<i64>, GrokAuthError> {
        timestamp_field(&self.document, "last_refresh")
    }

    pub(crate) fn refreshed(
        &self,
        tokens: &RefreshedGrokTokens,
        refreshed_at: i64,
    ) -> Result<(Self, i64), GrokAuthError> {
        let expires_in = i64::from(tokens.expires_in);
        let expires_at = refreshed_at
            .checked_add(expires_in)
            .ok_or(GrokAuthError::TimestampOutOfRange)?;
        let expired = timestamp_rfc3339(expires_at)?;
        let last_refresh = timestamp_rfc3339(refreshed_at)?;
        let mut document = self.document.clone();

        document.insert("type".to_owned(), Value::String("xai".to_owned()));
        document.insert("auth_kind".to_owned(), Value::String("oauth".to_owned()));
        document.insert(
            "access_token".to_owned(),
            Value::String(tokens.access_token.expose_secret().to_owned()),
        );
        if let Some(refresh_token) = tokens.refresh_token.as_ref() {
            document.insert(
                "refresh_token".to_owned(),
                Value::String(refresh_token.expose_secret().to_owned()),
            );
        }
        if let Some(id_token) = tokens.id_token.as_ref() {
            document.insert(
                "id_token".to_owned(),
                Value::String(id_token.expose_secret().to_owned()),
            );
        }
        if let Some(token_type) = tokens.token_type.as_ref() {
            document.insert("token_type".to_owned(), Value::String(token_type.clone()));
        }
        document.insert("expires_in".to_owned(), Value::from(tokens.expires_in));
        document.insert(
            "token_endpoint".to_owned(),
            Value::String(tokens.token_endpoint.clone()),
        );
        document.insert("expired".to_owned(), Value::String(expired));
        document.insert("last_refresh".to_owned(), Value::String(last_refresh));
        document.insert("disabled".to_owned(), Value::Bool(false));

        Ok((Self::from_document(document)?, expires_at))
    }

    pub(crate) fn to_json(&self) -> Result<SecretString, GrokAuthError> {
        serde_json::to_string(&Value::Object(self.document.clone()))
            .map(SecretString::from)
            .map_err(GrokAuthError::Json)
    }

    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn from_access_token(access_token: impl Into<String>) -> Self {
        let access_token = access_token.into();
        let mut document = Map::new();
        document.insert("type".to_owned(), Value::String("xai".to_owned()));
        document.insert("auth_kind".to_owned(), Value::String("oauth".to_owned()));
        document.insert("access_token".to_owned(), Value::String(access_token));
        document.insert(
            "upstream_user_id".to_owned(),
            Value::String("test-user".to_owned()),
        );
        document.insert("disabled".to_owned(), Value::Bool(false));
        match Self::from_document(document) {
            Ok(credentials) => credentials,
            Err(_) => unreachable!("test credential must be valid"),
        }
    }

    #[cfg(test)]
    fn from_reader(mut reader: impl Read) -> Result<Self, GrokAuthError> {
        let mut credential_json = String::new();
        reader.read_to_string(&mut credential_json)?;
        Self::from_json(&SecretString::from(credential_json))
    }

    fn from_document(document: Map<String, Value>) -> Result<Self, GrokAuthError> {
        if !string_field(&document, "type")
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("xai"))
        {
            return Err(GrokAuthError::InvalidProviderType);
        }
        if !string_field(&document, "auth_kind")
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("oauth"))
        {
            return Err(GrokAuthError::InvalidAuthKind);
        }
        if document
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or_default()
        {
            return Err(GrokAuthError::Disabled);
        }
        let access_token =
            required_secret(&document, "access_token").ok_or(GrokAuthError::MissingAccessToken)?;
        let refresh_token = optional_secret(&document, "refresh_token");
        let upstream_user_id = string_field(&document, "upstream_user_id")
            .or_else(|| string_field(&document, "user_id"))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let token_endpoint = string_field(&document, "token_endpoint")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let oidc_issuer = normalized_string_field(&document, "oidc_issuer");
        let oidc_client_id = normalized_string_field(&document, "oidc_client_id");
        Ok(Self {
            document,
            access_token,
            refresh_token,
            upstream_user_id,
            oidc_issuer,
            oidc_client_id,
            token_endpoint,
        })
    }
}

impl fmt::Debug for GrokCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GrokCredentials")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("upstream_user_id", &self.upstream_user_id)
            .field("oidc_issuer", &self.oidc_issuer)
            .field("oidc_client_id", &self.oidc_client_id)
            .field("token_endpoint", &self.token_endpoint)
            .finish_non_exhaustive()
    }
}

fn string_field<'a>(document: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    document.get(field).and_then(Value::as_str)
}

fn normalized_string_field(document: &Map<String, Value>, field: &str) -> Option<String> {
    string_field(document, field)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn required_secret(document: &Map<String, Value>, field: &str) -> Option<SecretString> {
    optional_secret(document, field).filter(|value| !value.expose_secret().trim().is_empty())
}

fn optional_secret(document: &Map<String, Value>, field: &str) -> Option<SecretString> {
    string_field(document, field)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| SecretString::from(value.to_owned()))
}

fn timestamp_rfc3339(timestamp: i64) -> Result<String, GrokAuthError> {
    OffsetDateTime::from_unix_timestamp(timestamp)
        .map_err(|_| GrokAuthError::TimestampOutOfRange)?
        .format(&Rfc3339)
        .map_err(|_| GrokAuthError::TimestampOutOfRange)
}

fn timestamp_field(
    document: &Map<String, Value>,
    field: &str,
) -> Result<Option<i64>, GrokAuthError> {
    let Some(value) = string_field(document, field)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    OffsetDateTime::parse(value, &Rfc3339)
        .map(|timestamp| Some(timestamp.unix_timestamp()))
        .map_err(|_| GrokAuthError::InvalidTimestamp(field.to_owned()))
}

#[derive(Debug, Error)]
pub enum GrokAuthError {
    #[error("failed to read Grok auth JSON: {0}")]
    Read(#[from] std::io::Error),
    #[error("failed to parse Grok auth JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Grok auth JSON must be an object")]
    NotObject,
    #[error("Grok auth JSON must have type xai")]
    InvalidProviderType,
    #[error("Grok auth JSON must have auth_kind oauth")]
    InvalidAuthKind,
    #[error("Grok credential is disabled")]
    Disabled,
    #[error("Grok auth JSON is missing access_token")]
    MissingAccessToken,
    #[error("Grok auth JSON is missing upstream user ID")]
    MissingUpstreamUserId,
    #[error("stored provider account is not a Grok account")]
    InvalidStoredProvider,
    #[error("unsupported Grok credential format version {0}")]
    UnsupportedCredentialFormat(u32),
    #[error("Grok credential timestamp is out of range")]
    TimestampOutOfRange,
    #[error("Grok auth JSON has invalid {0} timestamp")]
    InvalidTimestamp(String),
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    const VALID_AUTH: &str = r#"{
        "type": "xai",
        "auth_kind": "oauth",
        "access_token": "secret-token",
        "disabled": false
    }"#;

    #[test]
    fn parses_valid_auth_without_exposing_token_in_debug() {
        let credentials =
            GrokCredentials::from_reader(Cursor::new(VALID_AUTH)).expect("valid credentials");
        let debug = format!("{credentials:?}");

        assert_eq!(credentials.access_token().expose_secret(), "secret-token");
        assert!(!debug.contains("secret-token"));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn rejects_disabled_credential() {
        let auth = VALID_AUTH.replace("false", "true");
        let result = GrokCredentials::from_reader(Cursor::new(auth));

        assert!(matches!(result, Err(GrokAuthError::Disabled)));
    }

    #[test]
    fn rejects_empty_access_token() {
        let auth = VALID_AUTH.replace("secret-token", "   ");
        let result = GrokCredentials::from_reader(Cursor::new(auth));

        assert!(matches!(result, Err(GrokAuthError::MissingAccessToken)));
    }

    #[test]
    fn parse_error_does_not_echo_secret_value() {
        let error = GrokCredentials::from_reader(Cursor::new(
            r#"{"type":"xai","auth_kind":"oauth","access_token":"do-not-log""#,
        ))
        .expect_err("invalid JSON");

        assert!(!error.to_string().contains("do-not-log"));
    }

    #[test]
    fn refresh_keeps_provenance_and_unrotated_refresh_token() {
        let credentials = GrokCredentials::from_json(&SecretString::from(
            serde_json::json!({
                "type": "xai",
                "auth_kind": "oauth",
                "access_token": "old-access",
                "refresh_token": "old-refresh",
                "oidc_issuer": OIDC_ISSUER,
                "oidc_client_id": OIDC_CLIENT_ID,
                "token_endpoint": "https://auth.x.ai/old-token"
            })
            .to_string(),
        ))
        .expect("credentials");
        let tokens = RefreshedGrokTokens {
            access_token: SecretString::from("new-access"),
            refresh_token: None,
            id_token: None,
            token_type: Some("Bearer".to_owned()),
            expires_in: 3600,
            token_endpoint: "https://auth.x.ai/current-token".to_owned(),
        };

        let (refreshed, _) = credentials
            .refreshed(&tokens, 1_700_000_000)
            .expect("refreshed credential");
        let document: Value = serde_json::from_str(
            refreshed
                .to_json()
                .expect("credential JSON")
                .expose_secret(),
        )
        .expect("credential document");

        assert_eq!(document["refresh_token"], "old-refresh");
        assert_eq!(document["oidc_issuer"], OIDC_ISSUER);
        assert_eq!(document["oidc_client_id"], OIDC_CLIENT_ID);
        assert_eq!(
            document["token_endpoint"],
            "https://auth.x.ai/current-token"
        );
    }
}
