use std::fmt;

use secrecy::{ExposeSecret, SecretString};
use serde_json::{Map, Value};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use super::refresh::RefreshedTokens;

#[derive(Clone)]
pub(crate) struct AntigravityCredentials {
    document: Map<String, Value>,
    access_token: SecretString,
    refresh_token: Option<SecretString>,
    project_id: String,
    expires_at: Option<i64>,
    last_refreshed_at: Option<i64>,
}

impl AntigravityCredentials {
    pub(crate) fn from_json(credential_json: &SecretString) -> Result<Self, AntigravityAuthError> {
        let document: Value = serde_json::from_str(credential_json.expose_secret())?;
        let document = document
            .as_object()
            .cloned()
            .ok_or(AntigravityAuthError::NotObject)?;
        Self::from_document(document)
    }

    pub(crate) fn from_parts(
        access_token: String,
        refresh_token: String,
        email: String,
        project_id: String,
        expires_in: i64,
        refreshed_at: i64,
    ) -> Result<Self, AntigravityAuthError> {
        let expires_at = refreshed_at
            .checked_add(expires_in.max(60))
            .ok_or(AntigravityAuthError::TimestampOutOfRange)?;
        let mut document = Map::new();
        document.insert("type".to_owned(), Value::String("antigravity".to_owned()));
        document.insert("auth_kind".to_owned(), Value::String("oauth".to_owned()));
        document.insert("access_token".to_owned(), Value::String(access_token));
        document.insert("refresh_token".to_owned(), Value::String(refresh_token));
        document.insert("email".to_owned(), Value::String(email));
        document.insert("project_id".to_owned(), Value::String(project_id));
        document.insert("expires_in".to_owned(), Value::from(expires_in.max(60)));
        document.insert(
            "timestamp".to_owned(),
            Value::from(refreshed_at.saturating_mul(1000)),
        );
        document.insert(
            "expired".to_owned(),
            Value::String(timestamp_rfc3339(expires_at)?),
        );
        document.insert(
            "last_refresh".to_owned(),
            Value::String(timestamp_rfc3339(refreshed_at)?),
        );
        document.insert("disabled".to_owned(), Value::Bool(false));
        Self::from_document(document)
    }

    pub(crate) fn access_token(&self) -> &SecretString {
        &self.access_token
    }

    pub(crate) fn refresh_token(&self) -> Option<&SecretString> {
        self.refresh_token.as_ref()
    }

    pub(crate) fn project_id(&self) -> &str {
        &self.project_id
    }

    pub(crate) fn with_project_id(
        &self,
        project_id: impl Into<String>,
    ) -> Result<Self, AntigravityAuthError> {
        let project_id = project_id.into().trim().to_owned();
        if project_id.is_empty() {
            return Err(AntigravityAuthError::MissingProjectId);
        }
        let mut document = self.document.clone();
        document.insert("project_id".to_owned(), Value::String(project_id));
        Self::from_document(document)
    }

    pub(crate) fn expires_at(&self) -> Option<i64> {
        self.expires_at
    }

    pub(crate) fn last_refreshed_at(&self) -> Option<i64> {
        self.last_refreshed_at
    }

    pub(crate) fn refreshed(
        &self,
        tokens: &RefreshedTokens,
        refreshed_at: i64,
    ) -> Result<(Self, i64), AntigravityAuthError> {
        let expires_at = refreshed_at
            .checked_add(tokens.expires_in)
            .ok_or(AntigravityAuthError::TimestampOutOfRange)?;
        let mut document = self.document.clone();
        document.insert("type".to_owned(), Value::String("antigravity".to_owned()));
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
        document.insert("expires_in".to_owned(), Value::from(tokens.expires_in));
        document.insert(
            "timestamp".to_owned(),
            Value::from(refreshed_at.saturating_mul(1000)),
        );
        document.insert(
            "expired".to_owned(),
            Value::String(timestamp_rfc3339(expires_at)?),
        );
        document.insert(
            "last_refresh".to_owned(),
            Value::String(timestamp_rfc3339(refreshed_at)?),
        );
        document.insert("disabled".to_owned(), Value::Bool(false));
        Ok((Self::from_document(document)?, expires_at))
    }

    pub(crate) fn to_json(&self) -> Result<SecretString, AntigravityAuthError> {
        serde_json::to_string(&Value::Object(self.document.clone()))
            .map(SecretString::from)
            .map_err(AntigravityAuthError::Json)
    }

    #[cfg(feature = "test-util")]
    pub(crate) fn for_test(access_token: impl Into<String>) -> Self {
        Self::from_parts(
            access_token.into(),
            "refresh-token".to_owned(),
            "test@example.com".to_owned(),
            "test-project".to_owned(),
            3600,
            1_700_000_000,
        )
        .expect("test credentials")
    }

    fn from_document(mut document: Map<String, Value>) -> Result<Self, AntigravityAuthError> {
        if !string_field(&document, "type")
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("antigravity"))
        {
            return Err(AntigravityAuthError::InvalidProviderType);
        }
        if let Some(auth_kind) = string_field(&document, "auth_kind")
            && !auth_kind.trim().eq_ignore_ascii_case("oauth")
        {
            return Err(AntigravityAuthError::InvalidAuthKind);
        }
        if document
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or_default()
        {
            return Err(AntigravityAuthError::Disabled);
        }
        let access_token = required_secret(&document, "access_token")
            .ok_or(AntigravityAuthError::MissingAccessToken)?;
        let refresh_token = optional_secret(&document, "refresh_token");
        let project_id = string_field(&document, "project_id")
            .or_else(|| string_field(&document, "cloudaicompanionProject"))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .unwrap_or_default();
        let expires_at = timestamp_field(&document, &["expires_at", "expired"])?;
        let last_refreshed_at = timestamp_field(&document, &["last_refreshed_at", "last_refresh"])?
            .or_else(|| {
                document
                    .get("timestamp")
                    .and_then(Value::as_i64)
                    .map(|value| {
                        if value > 10_000_000_000 {
                            value / 1000
                        } else {
                            value
                        }
                    })
            });
        document.insert("type".to_owned(), Value::String("antigravity".to_owned()));
        document.insert("auth_kind".to_owned(), Value::String("oauth".to_owned()));
        Ok(Self {
            document,
            access_token,
            refresh_token,
            project_id,
            expires_at,
            last_refreshed_at,
        })
    }
}

impl fmt::Debug for AntigravityCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AntigravityCredentials")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("project_id", &self.project_id)
            .field("expires_at", &self.expires_at)
            .field("last_refreshed_at", &self.last_refreshed_at)
            .finish()
    }
}

fn string_field<'a>(document: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    document.get(field).and_then(Value::as_str)
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

fn timestamp_field(
    document: &Map<String, Value>,
    fields: &[&str],
) -> Result<Option<i64>, AntigravityAuthError> {
    for field in fields {
        let Some(value) = document.get(*field) else {
            continue;
        };
        if let Some(timestamp) = value.as_i64() {
            return Ok(Some(timestamp));
        }
        if let Some(timestamp) = value.as_str() {
            let timestamp = OffsetDateTime::parse(timestamp.trim(), &Rfc3339)
                .map_err(|_| AntigravityAuthError::InvalidTimestamp((*field).to_owned()))?
                .unix_timestamp();
            return Ok(Some(timestamp));
        }
        return Err(AntigravityAuthError::InvalidTimestamp((*field).to_owned()));
    }
    Ok(None)
}

fn timestamp_rfc3339(timestamp: i64) -> Result<String, AntigravityAuthError> {
    OffsetDateTime::from_unix_timestamp(timestamp)
        .map_err(|_| AntigravityAuthError::TimestampOutOfRange)?
        .format(&Rfc3339)
        .map_err(|_| AntigravityAuthError::TimestampOutOfRange)
}

#[derive(Debug, Error)]
pub(crate) enum AntigravityAuthError {
    #[error("failed to parse Antigravity credential JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Antigravity credential JSON must be an object")]
    NotObject,
    #[error("Antigravity credential JSON must have type antigravity")]
    InvalidProviderType,
    #[error("Antigravity credential JSON must have auth_kind oauth")]
    InvalidAuthKind,
    #[error("Antigravity credential is disabled")]
    Disabled,
    #[error("Antigravity credential is missing access_token")]
    MissingAccessToken,
    #[error("Antigravity credential is missing project_id")]
    MissingProjectId,
    #[error("Antigravity credential timestamp is out of range")]
    TimestampOutOfRange,
    #[error("Antigravity credential has invalid {0} timestamp")]
    InvalidTimestamp(String),
    #[error("stored provider account is not an Antigravity account")]
    InvalidStoredProvider,
    #[error("unsupported Antigravity credential format version {0}")]
    UnsupportedCredentialFormat(u32),
}

#[cfg(test)]
mod tests {
    use secrecy::{ExposeSecret, SecretString};

    use super::*;

    #[test]
    fn imports_cpa_credential_and_normalizes_oauth_kind() {
        let credentials = AntigravityCredentials::from_json(&SecretString::from(
            serde_json::json!({
                "type": "antigravity",
                "access_token": "access",
                "refresh_token": "refresh",
                "project_id": "project-1",
                "expired": "2030-01-01T00:00:00Z",
                "timestamp": 1_700_000_000_000_i64
            })
            .to_string(),
        ))
        .expect("CPA credential");
        let stored = credentials.to_json().expect("stored credential");
        let value: Value = serde_json::from_str(stored.expose_secret()).expect("stored JSON");
        assert_eq!(value["type"], "antigravity");
        assert_eq!(value["auth_kind"], "oauth");
        assert_eq!(credentials.project_id(), "project-1");
        assert_eq!(credentials.expires_at(), Some(1_893_456_000));
    }

    #[test]
    fn accepts_cpa_credential_without_project_for_lazy_discovery() {
        let credentials = AntigravityCredentials::from_json(&SecretString::from(
            serde_json::json!({
                "type": "antigravity",
                "access_token": "access",
                "refresh_token": "refresh"
            })
            .to_string(),
        ))
        .expect("CPA credential without project");
        assert_eq!(credentials.project_id(), "");
        assert_eq!(
            credentials
                .with_project_id("project-1")
                .expect("project ID")
                .project_id(),
            "project-1"
        );
    }
}
