use std::fmt;

use getrandom::fill;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Clone)]
pub(crate) struct ClaudeOAuthCredentials {
    access_token: SecretString,
    refresh_token: SecretString,
    account_uuid: String,
    email: Option<String>,
    device_id: String,
    expires_at: i64,
    last_refreshed_at: i64,
}

#[derive(Debug, Error)]
pub(crate) enum ClaudeOAuthCredentialError {
    #[error("Claude OAuth credential JSON is invalid")]
    InvalidJson,
    #[error("Claude OAuth credential is missing {0}")]
    Missing(&'static str),
    #[error("Claude OAuth credential has an invalid {0}")]
    Invalid(&'static str),
    #[error("failed to generate Claude OAuth device identity")]
    DeviceIdentity,
}

#[derive(Deserialize)]
struct ImportedCredential {
    #[serde(default, rename = "type")]
    credential_type: Option<String>,
    #[serde(default)]
    auth_kind: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    #[serde(default)]
    account_uuid: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    claude_device_ids: Vec<String>,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default)]
    expired: Option<String>,
    #[serde(default)]
    last_refreshed_at: Option<i64>,
    #[serde(default)]
    last_refresh: Option<String>,
}

#[derive(Serialize)]
struct StoredCredential<'a> {
    #[serde(rename = "type")]
    credential_type: &'static str,
    auth_kind: &'static str,
    access_token: &'a str,
    refresh_token: &'a str,
    account_uuid: &'a str,
    email: Option<&'a str>,
    claude_device_ids: [&'a str; 1],
    expires_at: i64,
    last_refreshed_at: i64,
}

impl ClaudeOAuthCredentials {
    pub(crate) fn from_json(value: &SecretString) -> Result<Self, ClaudeOAuthCredentialError> {
        let imported: ImportedCredential = serde_json::from_str(value.expose_secret())
            .map_err(|_| ClaudeOAuthCredentialError::InvalidJson)?;
        if imported
            .credential_type
            .as_deref()
            .is_some_and(|value| value != "claude")
        {
            return Err(ClaudeOAuthCredentialError::Invalid("type"));
        }
        if imported
            .auth_kind
            .as_deref()
            .is_some_and(|value| value != "oauth")
        {
            return Err(ClaudeOAuthCredentialError::Invalid("auth_kind"));
        }
        let expires_at = imported
            .expires_at
            .or_else(|| imported.expired.as_deref().and_then(parse_timestamp))
            .ok_or(ClaudeOAuthCredentialError::Missing("expires_at"))?;
        let last_refreshed_at = imported
            .last_refreshed_at
            .or_else(|| imported.last_refresh.as_deref().and_then(parse_timestamp))
            .unwrap_or_else(unix_timestamp);
        let device_id = match imported
            .claude_device_ids
            .into_iter()
            .map(|value| value.trim().to_ascii_lowercase())
            .find(|value| valid_device_id(value))
        {
            Some(value) => value,
            None => generate_device_id()?,
        };
        Self::from_parts(
            required(imported.access_token, "access_token")?,
            required(imported.refresh_token, "refresh_token")?,
            required(imported.account_uuid, "account_uuid")?,
            imported.email,
            device_id,
            expires_at,
            last_refreshed_at,
        )
    }

    pub(crate) fn from_parts(
        access_token: String,
        refresh_token: String,
        account_uuid: String,
        email: Option<String>,
        device_id: String,
        expires_at: i64,
        last_refreshed_at: i64,
    ) -> Result<Self, ClaudeOAuthCredentialError> {
        if access_token.trim().is_empty() {
            return Err(ClaudeOAuthCredentialError::Missing("access_token"));
        }
        if refresh_token.trim().is_empty() {
            return Err(ClaudeOAuthCredentialError::Missing("refresh_token"));
        }
        if uuid::Uuid::parse_str(account_uuid.trim()).is_err() {
            return Err(ClaudeOAuthCredentialError::Invalid("account_uuid"));
        }
        if !valid_device_id(&device_id) {
            return Err(ClaudeOAuthCredentialError::Invalid("claude_device_ids"));
        }
        Ok(Self {
            access_token: SecretString::from(access_token.trim().to_owned()),
            refresh_token: SecretString::from(refresh_token.trim().to_owned()),
            account_uuid: account_uuid.trim().to_owned(),
            email: email.and_then(normalized),
            device_id,
            expires_at,
            last_refreshed_at,
        })
    }

    pub(crate) fn refreshed(
        &self,
        access_token: String,
        refresh_token: Option<String>,
        expires_at: i64,
        refreshed_at: i64,
    ) -> Result<Self, ClaudeOAuthCredentialError> {
        Self::from_parts(
            access_token,
            refresh_token.unwrap_or_else(|| self.refresh_token.expose_secret().to_owned()),
            self.account_uuid.clone(),
            self.email.clone(),
            self.device_id.clone(),
            expires_at,
            refreshed_at,
        )
    }

    pub(crate) fn to_json(&self) -> Result<SecretString, ClaudeOAuthCredentialError> {
        serde_json::to_string(&StoredCredential {
            credential_type: "claude",
            auth_kind: "oauth",
            access_token: self.access_token.expose_secret(),
            refresh_token: self.refresh_token.expose_secret(),
            account_uuid: &self.account_uuid,
            email: self.email.as_deref(),
            claude_device_ids: [&self.device_id],
            expires_at: self.expires_at,
            last_refreshed_at: self.last_refreshed_at,
        })
        .map(SecretString::from)
        .map_err(|_| ClaudeOAuthCredentialError::InvalidJson)
    }

    pub(crate) fn access_token(&self) -> &SecretString {
        &self.access_token
    }

    pub(crate) fn refresh_token(&self) -> &SecretString {
        &self.refresh_token
    }

    pub(crate) fn account_uuid(&self) -> &str {
        &self.account_uuid
    }

    pub(crate) fn device_id(&self) -> &str {
        &self.device_id
    }

    pub(crate) const fn expires_at(&self) -> i64 {
        self.expires_at
    }

    pub(crate) const fn last_refreshed_at(&self) -> i64 {
        self.last_refreshed_at
    }
}

impl fmt::Debug for ClaudeOAuthCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaudeOAuthCredentials")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("account_uuid", &"[REDACTED]")
            .field("email", &self.email.as_ref().map(|_| "[REDACTED]"))
            .field("device_id", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field("last_refreshed_at", &self.last_refreshed_at)
            .finish()
    }
}

pub(crate) fn generate_device_id() -> Result<String, ClaudeOAuthCredentialError> {
    let mut bytes = [0_u8; 32];
    fill(&mut bytes).map_err(|_| ClaudeOAuthCredentialError::DeviceIdentity)?;
    let mut encoded = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(encoded, "{byte:02x}");
    }
    Ok(encoded)
}

fn required(
    value: Option<String>,
    field: &'static str,
) -> Result<String, ClaudeOAuthCredentialError> {
    value
        .and_then(normalized)
        .ok_or(ClaudeOAuthCredentialError::Missing(field))
}

fn normalized(value: String) -> Option<String> {
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn valid_device_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn parse_timestamp(value: &str) -> Option<i64> {
    OffsetDateTime::parse(value.trim(), &Rfc3339)
        .ok()
        .map(OffsetDateTime::unix_timestamp)
}

pub(crate) fn unix_timestamp() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_cpa_credential_and_normalizes_storage() {
        let device_id = format!(" {} ", "F".repeat(64));
        let imported = SecretString::from(format!(
            r#"{{"type":"claude","access_token":"access","refresh_token":"refresh","account_uuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","claude_device_ids":["{}"],"expired":"2030-01-01T00:00:00Z","last_refresh":"2026-08-24T00:00:00Z"}}"#,
            device_id
        ));
        let credentials = ClaudeOAuthCredentials::from_json(&imported).expect("CPA credential");
        let stored = credentials.to_json().expect("stored credential");
        let value: serde_json::Value =
            serde_json::from_str(stored.expose_secret()).expect("stored JSON");
        assert_eq!(value["type"], "claude");
        assert_eq!(value["auth_kind"], "oauth");
        assert_eq!(value["claude_device_ids"][0], "f".repeat(64));
        assert!(value["expires_at"].as_i64().is_some());
    }
}
