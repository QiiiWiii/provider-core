use provider_core::{ProviderError, ProviderRequest};
use sha2::{Digest, Sha256};

use super::invalid;

pub(super) fn fallback_billing(
    request: &ProviderRequest,
    payload: &[u8],
) -> Result<String, ProviderError> {
    let user_agent = request
        .metadata
        .user_agent
        .as_deref()
        .ok_or_else(|| invalid("Claude OAuth request is missing the measured User-Agent"))?;
    let version = user_agent
        .strip_prefix("claude-cli/")
        .and_then(|value| value.split_once(' '))
        .map(|(version, _)| version)
        .ok_or_else(|| invalid("Claude OAuth User-Agent is invalid"))?;
    let entrypoint = user_agent
        .split_once("(external,")
        .and_then(|(_, value)| value.strip_suffix(')'))
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid("Claude OAuth entrypoint is invalid"))?;
    let payload: serde_json::Value = serde_json::from_slice(payload)
        .map_err(|_| invalid("Claude OAuth request body is invalid"))?;
    let message = payload
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|message| message.get("role").and_then(serde_json::Value::as_str) == Some("user"))
        .filter_map(|message| message.get("content"))
        .filter_map(message_text)
        .rfind(|value| !value.is_empty())
        .unwrap_or_default();
    let characters = message.chars().collect::<Vec<_>>();
    let mut fingerprint_input = "59cf53e54c78".to_owned();
    for index in [4, 7, 20] {
        fingerprint_input.push(characters.get(index).copied().unwrap_or('0'));
    }
    fingerprint_input.push_str(version);
    let fingerprint = Sha256::digest(fingerprint_input.as_bytes());
    Ok(format!(
        "x-anthropic-billing-header: cc_version={version}.{:02x}{:01x}; cc_entrypoint={entrypoint}; cch=00000;",
        fingerprint[0],
        fingerprint[1] >> 4
    ))
}

fn message_text(content: &serde_json::Value) -> Option<String> {
    if let Some(content) = content.as_str() {
        return Some(content.to_owned());
    }
    content.as_array().and_then(|blocks| {
        blocks
            .iter()
            .filter_map(|block| {
                (block.get("type").and_then(serde_json::Value::as_str) == Some("text"))
                    .then(|| block.get("text").and_then(serde_json::Value::as_str))
                    .flatten()
                    .map(str::to_owned)
            })
            .next_back()
    })
}
