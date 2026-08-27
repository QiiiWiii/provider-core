use std::collections::HashSet;

use provider_core::ProviderError;

use super::super::credentials::ClaudeOAuthCredentials;
use super::json::{skip_value, skip_ws, string_range};
use super::{internal, invalid};

pub(super) fn oauth_betas(incoming: &str, helper: bool) -> String {
    let mut values = Vec::new();
    for value in incoming
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !values.iter().any(|existing| existing == value) {
            values.push(value.to_owned());
        }
    }
    if !values.iter().any(|value| value == "oauth-2025-04-20") {
        let position = usize::from(
            values
                .first()
                .is_some_and(|value| value == "claude-code-20250219"),
        );
        values.insert(position, "oauth-2025-04-20".to_owned());
    }
    if !helper
        && !values
            .iter()
            .any(|value| value == "extended-cache-ttl-2025-04-11")
    {
        values.push("extended-cache-ttl-2025-04-11".to_owned());
    }
    values.join(",")
}

pub(super) fn rebuild_identity(
    existing: &str,
    credentials: &ClaudeOAuthCredentials,
    session_id: &str,
) -> Result<String, ProviderError> {
    let raw = existing.as_bytes();
    let mut pos = skip_ws(raw, 0);
    if raw.get(pos) != Some(&b'{') {
        return Err(invalid("Claude OAuth metadata.user_id is invalid"));
    }
    pos += 1;
    let mut seen = HashSet::new();
    let mut extras = Vec::new();
    loop {
        pos = skip_ws(raw, pos);
        if raw.get(pos) == Some(&b'}') {
            pos += 1;
            break;
        }
        let member_start = pos;
        let key = string_range(raw, pos)
            .ok_or_else(|| invalid("Claude OAuth metadata.user_id is invalid"))?;
        let decoded: String = serde_json::from_slice(&raw[key.clone()])
            .map_err(|_| invalid("Claude OAuth metadata.user_id is invalid"))?;
        if !seen.insert(decoded.clone()) {
            return Err(invalid(
                "Claude OAuth metadata.user_id has duplicate members",
            ));
        }
        pos = skip_ws(raw, key.end);
        if raw.get(pos) != Some(&b':') {
            return Err(invalid("Claude OAuth metadata.user_id is invalid"));
        }
        let value_start = skip_ws(raw, pos + 1);
        let value_end = skip_value(raw, value_start)
            .ok_or_else(|| invalid("Claude OAuth metadata.user_id is invalid"))?;
        if !matches!(
            decoded.as_str(),
            "device_id" | "account_uuid" | "session_id"
        ) {
            extras.push(member_start..value_end);
        }
        pos = skip_ws(raw, value_end);
        match raw.get(pos) {
            Some(b',') => pos += 1,
            Some(b'}') => {
                pos += 1;
                break;
            }
            _ => return Err(invalid("Claude OAuth metadata.user_id is invalid")),
        }
    }
    if skip_ws(raw, pos) != raw.len() {
        return Err(invalid("Claude OAuth metadata.user_id is invalid"));
    }

    let mut rebuilt = Vec::new();
    rebuilt.extend_from_slice(b"{\"device_id\":");
    rebuilt.extend_from_slice(
        &serde_json::to_vec(credentials.device_id())
            .map_err(|_| internal("failed to encode Claude OAuth identity"))?,
    );
    rebuilt.extend_from_slice(b",\"account_uuid\":");
    rebuilt.extend_from_slice(
        &serde_json::to_vec(credentials.account_uuid())
            .map_err(|_| internal("failed to encode Claude OAuth identity"))?,
    );
    rebuilt.extend_from_slice(b",\"session_id\":");
    rebuilt.extend_from_slice(
        &serde_json::to_vec(session_id)
            .map_err(|_| internal("failed to encode Claude OAuth identity"))?,
    );
    for extra in extras {
        rebuilt.push(b',');
        rebuilt.extend_from_slice(&raw[extra]);
    }
    rebuilt.push(b'}');
    String::from_utf8(rebuilt).map_err(|_| internal("failed to encode Claude OAuth identity"))
}
