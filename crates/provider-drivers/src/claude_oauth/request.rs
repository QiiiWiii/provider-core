use std::ops::Range;

use bytes::Bytes;
use provider_core::{ProviderError, ProviderErrorKind, ProviderRequest, RequestClient};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use secrecy::ExposeSecret;
use serde::Serialize;
use sha2::{Digest, Sha256};
use xxhash_rust::xxh64::xxh64;

use super::credentials::ClaudeOAuthCredentials;

const CCH_SEED: u64 = 0x4D659218E32A3268;

pub(crate) fn prepare_request(
    request: &ProviderRequest,
    credentials: &ClaudeOAuthCredentials,
) -> Result<(Bytes, HeaderMap), ProviderError> {
    if request.metadata.client != RequestClient::ClaudeCode {
        return Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            "Claude OAuth requires a measured Claude Code client",
        ));
    }
    let session_id = request
        .metadata
        .claude_code_session_id
        .as_deref()
        .ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "Claude OAuth request is missing session identity",
            )
        })?;
    let parent_session_id = request
        .metadata
        .claude_code_user_id
        .as_deref()
        .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
        .and_then(|value| {
            value
                .get("parent_session_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    let identity = serde_json::to_string(&ClaudeIdentity {
        device_id: credentials.device_id(),
        account_uuid: credentials.account_uuid(),
        session_id,
        parent_session_id: parent_session_id.as_deref(),
    })
    .map_err(|_| internal("failed to encode Claude OAuth identity"))?;
    let encoded_identity = serde_json::to_vec(&identity)
        .map_err(|_| internal("failed to encode Claude OAuth metadata"))?;
    let payload = request
        .metadata
        .claude_code_payload
        .as_deref()
        .ok_or_else(|| invalid("Claude OAuth request is missing the native payload"))?;
    let metadata = unique_object_member(payload, 0, "metadata")
        .ok_or_else(|| invalid("Claude OAuth request is missing metadata"))?;
    let user_id = unique_object_member(payload, metadata.start, "user_id")
        .ok_or_else(|| invalid("Claude OAuth request is missing metadata.user_id"))?;
    let mut body = splice(payload, user_id, &encoded_identity);
    let billing = (!request.metadata.claude_code_helper_profile)
        .then(|| fallback_billing(request, payload))
        .transpose()?;
    body = ensure_and_sign_cch(body, billing.as_deref())?;

    let mut headers = HeaderMap::new();
    for (name, value) in &request.metadata.claude_code_headers {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(value) else {
            continue;
        };
        headers.append(name, value);
    }
    insert(
        &mut headers,
        "authorization",
        &format!("Bearer {}", credentials.access_token().expose_secret()),
    )?;
    insert(&mut headers, "content-type", "application/json")?;
    insert(&mut headers, "anthropic-version", "2023-06-01")?;
    insert(
        &mut headers,
        "anthropic-dangerous-direct-browser-access",
        "true",
    )?;
    insert(&mut headers, "x-app", "cli")?;
    insert(&mut headers, "x-stainless-retry-count", "0")?;
    insert(&mut headers, "x-stainless-runtime", "node")?;
    insert(&mut headers, "x-stainless-lang", "js")?;
    insert_default(&mut headers, "x-stainless-timeout", "600")?;
    insert_default(&mut headers, "x-stainless-package-version", "0.94.0")?;
    insert_default(&mut headers, "x-stainless-runtime-version", "v26.3.0")?;
    insert_default(&mut headers, "x-stainless-os", "MacOS")?;
    insert_default(&mut headers, "x-stainless-arch", "arm64")?;
    insert(&mut headers, "x-claude-code-session-id", session_id)?;
    insert(&mut headers, "accept", "application/json")?;
    if request.metadata.claude_code_helper_profile {
        insert_default(&mut headers, "accept-encoding", "gzip")?;
    } else {
        insert(&mut headers, "accept-encoding", "gzip, deflate, br, zstd")?;
    }
    insert(&mut headers, "connection", "keep-alive")?;
    if !headers.contains_key("x-client-request-id") {
        insert(
            &mut headers,
            "x-client-request-id",
            &uuid::Uuid::new_v4().to_string(),
        )?;
    }
    let beta = oauth_betas(
        request
            .metadata
            .claude_code_beta
            .as_deref()
            .unwrap_or_default(),
        request.metadata.claude_code_helper_profile,
    );
    insert(&mut headers, "anthropic-beta", &beta)?;
    Ok((Bytes::from(body), headers))
}

#[derive(Serialize)]
struct ClaudeIdentity<'a> {
    device_id: &'a str,
    account_uuid: &'a str,
    session_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_session_id: Option<&'a str>,
}

fn oauth_betas(incoming: &str, helper: bool) -> String {
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

fn fallback_billing(request: &ProviderRequest, payload: &[u8]) -> Result<String, ProviderError> {
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

fn ensure_and_sign_cch(
    mut body: Vec<u8>,
    fallback_billing: Option<&str>,
) -> Result<Vec<u8>, ProviderError> {
    let system = match object_member(&body, 0, "system") {
        Some(system) => system,
        None => {
            let Some(fallback_billing) = fallback_billing else {
                return Ok(body);
            };
            body = append_system(&body, fallback_billing)?;
            object_member(&body, 0, "system").ok_or_else(|| internal("system vanished"))?
        }
    };
    if !first_system_block_is_billing(&body, &system) {
        let fallback_billing =
            fallback_billing.ok_or_else(|| invalid("Claude OAuth billing header is missing"))?;
        body = prepend_billing(&body, system, fallback_billing)?;
    }
    let system = object_member(&body, 0, "system").ok_or_else(|| internal("system vanished"))?;
    let first = array_item(&body, system.start, 0)
        .ok_or_else(|| invalid("Claude OAuth system profile is invalid"))?;
    let text = object_member(&body, first.start, "text")
        .ok_or_else(|| invalid("Claude OAuth billing header is missing"))?;
    let mut billing: String = serde_json::from_slice(&body[text.clone()])
        .map_err(|_| invalid("Claude OAuth billing header is invalid"))?;
    if !billing.starts_with("x-anthropic-billing-header:") {
        return Err(invalid("Claude OAuth billing header is missing"));
    }
    if !billing.contains(" cch=") {
        let entrypoint = billing
            .find("cc_entrypoint=")
            .and_then(|start| billing[start..].find(';').map(|end| start + end + 1))
            .ok_or_else(|| invalid("Claude OAuth billing entrypoint is invalid"))?;
        billing.insert_str(entrypoint, " cch=00000;");
        let encoded = serde_json::to_vec(&billing)
            .map_err(|_| internal("failed to encode Claude OAuth billing header"))?;
        body = splice(&body, text, &encoded);
    }
    let system = object_member(&body, 0, "system").ok_or_else(|| internal("system vanished"))?;
    let first = array_item(&body, system.start, 0).ok_or_else(|| internal("system vanished"))?;
    let text =
        object_member(&body, first.start, "text").ok_or_else(|| internal("billing vanished"))?;
    let relative = body[text.clone()]
        .windows(4)
        .position(|window| window == b"cch=")
        .ok_or_else(|| invalid("Claude OAuth billing CCH is missing"))?;
    let digits = text.start + relative + 4;
    if body.get(digits + 5) != Some(&b';') {
        return Err(invalid("Claude OAuth billing CCH is invalid"));
    }
    body[digits..digits + 5].copy_from_slice(b"00000");
    let normalized = normalize_cch(&body)?;
    let signature = format!("{:05x}", xxh64(&normalized, CCH_SEED) & 0xFFFFF);
    body[digits..digits + 5].copy_from_slice(signature.as_bytes());
    Ok(body)
}

fn first_system_block_is_billing(body: &[u8], system: &Range<usize>) -> bool {
    array_item(body, system.start, 0)
        .and_then(|first| object_member(body, first.start, "text"))
        .and_then(|text| serde_json::from_slice::<String>(&body[text]).ok())
        .is_some_and(|text| text.starts_with("x-anthropic-billing-header:"))
}

fn billing_block(billing: &str) -> Result<Vec<u8>, ProviderError> {
    serde_json::to_vec(&serde_json::json!({ "type": "text", "text": billing }))
        .map_err(|_| internal("failed to encode Claude OAuth billing block"))
}

fn append_system(body: &[u8], billing: &str) -> Result<Vec<u8>, ProviderError> {
    let end = body
        .iter()
        .rposition(|byte| *byte == b'}')
        .ok_or_else(|| invalid("Claude OAuth request body is invalid"))?;
    let prefix = body[..end].iter().rfind(|byte| !byte.is_ascii_whitespace());
    let separator = if prefix == Some(&b'{') {
        b"".as_slice()
    } else {
        b",".as_slice()
    };
    let block = billing_block(billing)?;
    let mut member = Vec::with_capacity(block.len() + 12);
    member.extend_from_slice(separator);
    member.extend_from_slice(b"\"system\":[");
    member.extend_from_slice(&block);
    member.push(b']');
    Ok(splice(body, end..end, &member))
}

fn prepend_billing(
    body: &[u8],
    system: Range<usize>,
    billing: &str,
) -> Result<Vec<u8>, ProviderError> {
    let block = billing_block(billing)?;
    match body.get(skip_ws(body, system.start)) {
        Some(b'[') => {
            let insert_at = skip_ws(body, system.start) + 1;
            let empty = body.get(skip_ws(body, insert_at)) == Some(&b']');
            let mut prefix = block;
            if !empty {
                prefix.push(b',');
            }
            Ok(splice(body, insert_at..insert_at, &prefix))
        }
        Some(b'"') => {
            let text: String = serde_json::from_slice(&body[system.clone()])
                .map_err(|_| invalid("Claude OAuth system text is invalid"))?;
            let original = billing_block(&text)?;
            let mut array = Vec::with_capacity(block.len() + original.len() + 3);
            array.push(b'[');
            array.extend_from_slice(&block);
            array.push(b',');
            array.extend_from_slice(&original);
            array.push(b']');
            Ok(splice(body, system, &array))
        }
        _ => Err(invalid("Claude OAuth system profile is invalid")),
    }
}

#[derive(Clone)]
struct Member {
    start: usize,
    end: usize,
    comma_before: Option<usize>,
    comma_after: Option<usize>,
    excluded: bool,
}

struct Scanner<'a> {
    body: &'a [u8],
    pos: usize,
    edits: Vec<Range<usize>>,
}

fn normalize_cch(body: &[u8]) -> Result<Vec<u8>, ProviderError> {
    let mut scanner = Scanner {
        body,
        pos: 0,
        edits: Vec::new(),
    };
    scanner.parse_value(true)?;
    scanner.skip_whitespace();
    if scanner.pos != body.len() {
        return Err(invalid("Claude OAuth body has trailing JSON"));
    }
    scanner.edits.sort_by_key(|edit| edit.start);
    let mut normalized = Vec::with_capacity(body.len());
    let mut last = 0;
    for edit in scanner.edits {
        if edit.start < last || edit.end > body.len() {
            return Err(internal("Claude OAuth CCH edits overlap"));
        }
        normalized.extend_from_slice(&body[last..edit.start]);
        last = edit.end;
    }
    normalized.extend_from_slice(&body[last..]);
    Ok(normalized)
}

impl Scanner<'_> {
    fn parse_value(&mut self, collect: bool) -> Result<(), ProviderError> {
        self.skip_whitespace();
        match self.body.get(self.pos) {
            Some(b'{') => self.parse_object(collect),
            Some(b'[') => self.parse_array(collect),
            Some(b'"') => self.parse_string().map(|_| ()),
            Some(_) => {
                let start = self.pos;
                while self.body.get(self.pos).is_some_and(|byte| {
                    !matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n')
                }) {
                    self.pos += 1;
                }
                if self.pos == start {
                    return Err(invalid("Claude OAuth JSON value is invalid"));
                }
                Ok(())
            }
            None => Err(invalid("Claude OAuth JSON value is missing")),
        }
    }

    fn parse_object(&mut self, collect: bool) -> Result<(), ProviderError> {
        self.pos += 1;
        self.skip_whitespace();
        if self.consume(b'}') {
            return Ok(());
        }
        let mut members = Vec::new();
        let mut comma_before = None;
        loop {
            self.skip_whitespace();
            let member_start = self.pos;
            let key = self.parse_string()?;
            self.skip_whitespace();
            if !self.consume(b':') {
                return Err(invalid("Claude OAuth JSON object is invalid"));
            }
            self.skip_whitespace();
            let excluded = collect
                && matches!(
                    &self.body[key.clone()],
                    b"\"max_tokens\"" | b"\"fallbacks\"" | b"\"fallback_credit_token\""
                );
            if collect
                && &self.body[key.clone()] == b"\"model\""
                && self.body.get(self.pos) == Some(&b'"')
            {
                let value = self.parse_string()?;
                self.edits.push(value.start + 1..value.end - 1);
            } else {
                self.parse_value(collect && !excluded)?;
            }
            let member_end = self.pos;
            self.skip_whitespace();
            let comma_after = self.consume(b',').then_some(self.pos - 1);
            members.push(Member {
                start: member_start,
                end: member_end,
                comma_before,
                comma_after,
                excluded,
            });
            if let Some(comma) = comma_after {
                comma_before = Some(comma);
                continue;
            }
            if !self.consume(b'}') {
                return Err(invalid("Claude OAuth JSON object is invalid"));
            }
            break;
        }
        if collect {
            self.exclude_members(&members);
        }
        Ok(())
    }

    fn parse_array(&mut self, collect: bool) -> Result<(), ProviderError> {
        self.pos += 1;
        self.skip_whitespace();
        if self.consume(b']') {
            return Ok(());
        }
        loop {
            self.parse_value(collect)?;
            self.skip_whitespace();
            if self.consume(b',') {
                continue;
            }
            if !self.consume(b']') {
                return Err(invalid("Claude OAuth JSON array is invalid"));
            }
            return Ok(());
        }
    }

    fn parse_string(&mut self) -> Result<Range<usize>, ProviderError> {
        if self.body.get(self.pos) != Some(&b'"') {
            return Err(invalid("Claude OAuth JSON string is invalid"));
        }
        let start = self.pos;
        self.pos += 1;
        while let Some(byte) = self.body.get(self.pos) {
            match byte {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    return Ok(start..self.pos);
                }
                _ => self.pos += 1,
            }
        }
        Err(invalid("Claude OAuth JSON string is unterminated"))
    }

    fn exclude_members(&mut self, members: &[Member]) {
        let mut start = 0;
        while start < members.len() {
            if !members[start].excluded {
                start += 1;
                continue;
            }
            let mut end = start;
            while end + 1 < members.len() && members[end + 1].excluded {
                end += 1;
            }
            let range = if end + 1 < members.len() {
                members[start].start..members[end].comma_after.unwrap_or(members[end].end) + 1
            } else if start > 0 && end > start {
                members[start].start..members[end].end
            } else if start > 0 {
                members[start].comma_before.unwrap_or(members[start].start)..members[end].end
            } else {
                members[start].start..members[end].end
            };
            self.edits.push(range);
            start = end + 1;
        }
    }

    fn skip_whitespace(&mut self) {
        while self
            .body
            .get(self.pos)
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.pos += 1;
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.body.get(self.pos) != Some(&byte) {
            return false;
        }
        self.pos += 1;
        true
    }
}

fn object_member(body: &[u8], object_start: usize, wanted: &str) -> Option<Range<usize>> {
    let mut pos = skip_ws(body, object_start);
    if body.get(pos) != Some(&b'{') {
        return None;
    }
    pos += 1;
    loop {
        pos = skip_ws(body, pos);
        if body.get(pos) == Some(&b'}') {
            return None;
        }
        let key = string_range(body, pos)?;
        let decoded: String = serde_json::from_slice(&body[key.clone()]).ok()?;
        pos = skip_ws(body, key.end);
        if body.get(pos) != Some(&b':') {
            return None;
        }
        let value_start = skip_ws(body, pos + 1);
        let value_end = skip_value(body, value_start)?;
        if decoded == wanted {
            return Some(value_start..value_end);
        }
        pos = skip_ws(body, value_end);
        match body.get(pos) {
            Some(b',') => pos += 1,
            Some(b'}') => return None,
            _ => return None,
        }
    }
}

fn unique_object_member(body: &[u8], object_start: usize, wanted: &str) -> Option<Range<usize>> {
    let mut pos = skip_ws(body, object_start);
    if body.get(pos) != Some(&b'{') {
        return None;
    }
    pos += 1;
    let mut found = None;
    loop {
        pos = skip_ws(body, pos);
        if body.get(pos) == Some(&b'}') {
            return found;
        }
        let key = string_range(body, pos)?;
        let decoded: String = serde_json::from_slice(&body[key.clone()]).ok()?;
        pos = skip_ws(body, key.end);
        if body.get(pos) != Some(&b':') {
            return None;
        }
        let value_start = skip_ws(body, pos + 1);
        let value_end = skip_value(body, value_start)?;
        if decoded == wanted {
            if found.is_some() {
                return None;
            }
            found = Some(value_start..value_end);
        }
        pos = skip_ws(body, value_end);
        match body.get(pos) {
            Some(b',') => pos += 1,
            Some(b'}') => return found,
            _ => return None,
        }
    }
}

fn array_item(body: &[u8], array_start: usize, wanted: usize) -> Option<Range<usize>> {
    let mut pos = skip_ws(body, array_start);
    if body.get(pos) != Some(&b'[') {
        return None;
    }
    pos += 1;
    for index in 0..=wanted {
        let start = skip_ws(body, pos);
        let end = skip_value(body, start)?;
        if index == wanted {
            return Some(start..end);
        }
        pos = skip_ws(body, end);
        if body.get(pos) != Some(&b',') {
            return None;
        }
        pos += 1;
    }
    None
}

fn skip_value(body: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    match body.get(pos)? {
        b'"' => string_range(body, pos).map(|range| range.end),
        b'{' | b'[' => {
            let open = body[pos];
            let close = if open == b'{' { b'}' } else { b']' };
            let mut depth = 0_usize;
            while pos < body.len() {
                match body[pos] {
                    b'"' => pos = string_range(body, pos)?.end,
                    byte if byte == open => {
                        depth += 1;
                        pos += 1;
                    }
                    byte if byte == close => {
                        depth = depth.checked_sub(1)?;
                        pos += 1;
                        if depth == 0 {
                            return Some(pos);
                        }
                    }
                    _ => pos += 1,
                }
            }
            None
        }
        _ => {
            while body.get(pos).is_some_and(|byte| {
                !matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n')
            }) {
                pos += 1;
            }
            (pos > start).then_some(pos)
        }
    }
}

fn string_range(body: &[u8], start: usize) -> Option<Range<usize>> {
    if body.get(start) != Some(&b'"') {
        return None;
    }
    let mut pos = start + 1;
    while let Some(byte) = body.get(pos) {
        match byte {
            b'\\' => pos += 2,
            b'"' => return Some(start..pos + 1),
            _ => pos += 1,
        }
    }
    None
}

fn skip_ws(body: &[u8], mut pos: usize) -> usize {
    while body
        .get(pos)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    {
        pos += 1;
    }
    pos
}

fn splice(body: &[u8], range: Range<usize>, value: &[u8]) -> Vec<u8> {
    let mut updated = Vec::with_capacity(body.len() - range.len() + value.len());
    updated.extend_from_slice(&body[..range.start]);
    updated.extend_from_slice(value);
    updated.extend_from_slice(&body[range.end..]);
    updated
}

fn insert(headers: &mut HeaderMap, name: &'static str, value: &str) -> Result<(), ProviderError> {
    let value =
        HeaderValue::from_str(value).map_err(|_| internal("invalid Claude OAuth header"))?;
    headers.insert(HeaderName::from_static(name), value);
    Ok(())
}

fn insert_default(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &str,
) -> Result<(), ProviderError> {
    if !headers.contains_key(name) {
        insert(headers, name, value)?;
    }
    Ok(())
}

fn invalid(message: &'static str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::InvalidRequest, message)
}

fn internal(message: &'static str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Internal, message)
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
