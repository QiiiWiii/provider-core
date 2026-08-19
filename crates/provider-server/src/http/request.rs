use std::collections::HashSet;

use axum::{
    body::Bytes,
    http::{HeaderMap, header},
};
use provider_auth::{ApiKeyAuthenticator, AuthenticatedApiKey};
use provider_core::{AccountId, ProxyRequest, RequestMetadata, WireFormat};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::{HttpError, resolve_claude_model_id};

pub(super) const CLAUDE_CODE_SESSION_HEADER: &str = "x-claude-code-session-id";
const GROK_CONVERSATION_ID_HEADER: &str = "x-grok-conv-id";
const CODEX_INSTALLATION_ID_HEADER: &str = "x-codex-installation-id";
const CODEX_WINDOW_ID_HEADER: &str = "x-codex-window-id";
const CODEX_TURN_METADATA_HEADER: &str = "x-codex-turn-metadata";
const MAX_CODEX_TURN_METADATA_LENGTH: usize = 8 * 1024;

#[cfg(test)]
pub(super) fn proxy_request(
    protocol: WireFormat,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<ProxyRequest, HttpError> {
    let payload = parse_payload(protocol, &body)?;
    proxy_request_from_payload(protocol, headers, body, payload)
}

pub(super) fn parse_payload(protocol: WireFormat, body: &[u8]) -> Result<Value, HttpError> {
    serde_json::from_slice(body)
        .map_err(|_| HttpError::invalid_request(protocol, "request body must be valid JSON"))
}

fn proxy_request_from_payload(
    protocol: WireFormat,
    headers: &HeaderMap,
    body: Bytes,
    mut payload: Value,
) -> Result<ProxyRequest, HttpError> {
    let model = payload
        .as_object()
        .and_then(|payload| payload.get("model"))
        .and_then(Value::as_str)
        .ok_or_else(|| HttpError::invalid_request(protocol, "model must be a non-empty string"))?
        .to_owned();

    let model = if protocol == WireFormat::ClaudeMessages {
        resolve_claude_model_id(&model)
    } else {
        model
    };
    let body = if payload["model"].as_str() == Some(model.as_str()) {
        body
    } else {
        payload["model"] = Value::String(model.clone());
        Bytes::from(serde_json::to_vec(&payload).map_err(|_| HttpError::internal(protocol))?)
    };

    let request = ProxyRequest::new(protocol, model, body)
        .map_err(|error| HttpError::from_proxy_request(protocol, error))?;
    Ok(request.with_metadata(request_metadata(headers, protocol)?))
}

pub(super) fn proxy_request_for_key(
    protocol: WireFormat,
    headers: &HeaderMap,
    body: Bytes,
    key: &AuthenticatedApiKey,
) -> Result<ProxyRequest, HttpError> {
    let payload = parse_payload(protocol, &body)?;
    proxy_request_for_key_from_payload(protocol, headers, body, payload, key)
}

pub(super) fn proxy_request_for_key_from_payload(
    protocol: WireFormat,
    headers: &HeaderMap,
    body: Bytes,
    payload: Value,
    key: &AuthenticatedApiKey,
) -> Result<ProxyRequest, HttpError> {
    let mut request = proxy_request_from_payload(protocol, headers, body, payload)?;
    request.metadata.routing_scope = Some(key.key_id.to_string());
    if protocol == WireFormat::OpenAiResponses {
        normalize_responses_linkage(&mut request, headers, key)?;
    } else if protocol == WireFormat::OpenAiChatCompletions {
        normalize_chat_session(&mut request, key);
    } else if protocol == WireFormat::ClaudeMessages {
        let mut payload: Value = serde_json::from_slice(&request.payload)
            .map_err(|_| HttpError::invalid_request(protocol, "request body must be valid JSON"))?;
        let Some(root) = payload.as_object_mut() else {
            return Err(HttpError::invalid_request(
                protocol,
                "request body must be a JSON object",
            ));
        };
        if let Some(session_id) = claude_code_session_id(headers, root, protocol)? {
            request.metadata.session_id =
                Some(claude_code_cache_key(key, &request.model, &session_id));
        }
        root.remove("metadata");
        request.payload = serde_json::to_vec(&payload)
            .map(Bytes::from)
            .map_err(|_| HttpError::internal(protocol))?;
    }
    Ok(request)
}

fn normalize_responses_linkage(
    request: &mut ProxyRequest,
    headers: &HeaderMap,
    key: &AuthenticatedApiKey,
) -> Result<(), HttpError> {
    let mut payload: Value = serde_json::from_slice(&request.payload).map_err(|_| {
        HttpError::invalid_request(request.format, "request body must be valid JSON")
    })?;
    let root = payload.as_object_mut().ok_or_else(|| {
        HttpError::invalid_request(request.format, "request body must be a JSON object")
    })?;
    let codex_identity = codex_identity_input(&request.metadata, root, request.format)?;
    if is_grok_model(&request.model) && request.metadata.session_id.is_none() {
        if let Some(value) = headers
            .get(GROK_CONVERSATION_ID_HEADER)
            .and_then(|value| value.to_str().ok())
        {
            request.metadata.session_id = validated_session_id(value, request.format)?;
        }
    }

    request.metadata.previous_response_id = match root.get("previous_response_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => normalized_string(value),
        Some(_) => {
            return Err(HttpError::invalid_request(
                request.format,
                "previous_response_id must be a string",
            ));
        }
    };
    if root
        .get("previous_response_id")
        .and_then(Value::as_str)
        .is_some_and(|value| value.trim().is_empty())
    {
        root.remove("previous_response_id");
    }
    if root
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .is_some_and(|value| value.trim().is_empty())
    {
        root.remove("prompt_cache_key");
    }

    let session_seed = request
        .metadata
        .session_id
        .as_deref()
        .and_then(normalized_string)
        .or_else(|| {
            root.get("prompt_cache_key")
                .and_then(Value::as_str)
                .and_then(normalized_string)
        })
        .or_else(|| {
            request
                .metadata
                .thread_id
                .as_deref()
                .and_then(normalized_string)
        })
        .or_else(|| {
            request
                .metadata
                .client_request_id
                .as_deref()
                .and_then(normalized_string)
        })
        .or_else(|| codex_identity.session_seed());
    if let Some(session_seed) = session_seed {
        let session_id = responses_cache_key(key, &request.model, &session_seed);
        request.metadata.session_id = Some(session_id.clone());
        if request.metadata.thread_id.is_some() {
            request.metadata.thread_id = Some(session_id.clone());
        }
        if request.metadata.client_request_id.is_some() {
            request.metadata.client_request_id = Some(session_id.clone());
        }
        root.insert(
            "prompt_cache_key".to_owned(),
            Value::String(session_id.clone()),
        );
        normalize_codex_identity(request, root, &session_id, codex_identity)?;
    } else {
        remove_unlinked_codex_identity(root);
        clear_codex_identity_headers(&mut request.metadata);
    }
    request.payload = serde_json::to_vec(&payload)
        .map(Bytes::from)
        .map_err(|_| HttpError::internal(request.format))?;
    Ok(())
}

fn is_grok_model(model: &str) -> bool {
    model.trim().to_ascii_lowercase().starts_with("grok")
}

#[derive(Default)]
struct CodexIdentityInput {
    installation_id: Option<String>,
    window_id: Option<String>,
    turn_id: Option<String>,
    turn_metadata: Option<Map<String, Value>>,
}

impl CodexIdentityInput {
    fn session_seed(&self) -> Option<String> {
        self.window_id
            .clone()
            .or_else(|| self.installation_id.clone())
            .or_else(|| self.turn_id.clone())
            .or_else(|| {
                self.turn_metadata.as_ref().and_then(|metadata| {
                    ["prompt_cache_key", "session_id", "thread_id"]
                        .into_iter()
                        .find_map(|field| normalized_identity_field(metadata, field))
                })
            })
    }
}

fn codex_identity_input(
    metadata: &RequestMetadata,
    root: &Map<String, Value>,
    protocol: WireFormat,
) -> Result<CodexIdentityInput, HttpError> {
    let client_metadata = match root.get("client_metadata") {
        None | Some(Value::Null) => None,
        Some(Value::Object(metadata)) => Some(metadata),
        Some(_)
            if metadata.codex_installation_id.is_some()
                || metadata.codex_window_id.is_some()
                || metadata.codex_turn_metadata.is_some() =>
        {
            return Err(HttpError::invalid_request(
                protocol,
                "client_metadata must be a JSON object",
            ));
        }
        Some(_) => None,
    };

    let body_installation_id =
        optional_identity_field(client_metadata, CODEX_INSTALLATION_ID_HEADER, protocol)?;
    let legacy_installation_id =
        optional_identity_field(client_metadata, "installation_id", protocol)?;
    let body_window_id =
        optional_identity_field(client_metadata, CODEX_WINDOW_ID_HEADER, protocol)?;
    let top_level_turn_id = optional_identity_field(client_metadata, "turn_id", protocol)?;
    let body_turn_metadata = optional_turn_metadata(
        client_metadata.and_then(|metadata| metadata.get(CODEX_TURN_METADATA_HEADER)),
        protocol,
    )?;
    let header_turn_metadata = metadata
        .codex_turn_metadata
        .as_deref()
        .map(|value| parse_turn_metadata(value, protocol))
        .transpose()?;
    // HTTP headers are the canonical source when a Codex client supplied the
    // same identity on both surfaces. Every chosen value is rewritten below
    // before either surface can cross into a provider adapter.
    let turn_metadata = header_turn_metadata.or(body_turn_metadata);
    let nested_installation_id = turn_metadata
        .as_ref()
        .map(|metadata| {
            optional_identity_field(Some(metadata), CODEX_INSTALLATION_ID_HEADER, protocol)
                .and_then(|value| {
                    if value.is_some() {
                        Ok(value)
                    } else {
                        optional_identity_field(Some(metadata), "installation_id", protocol)
                    }
                })
        })
        .transpose()?
        .flatten();
    let nested_window_id = turn_metadata
        .as_ref()
        .map(|metadata| optional_identity_field(Some(metadata), "window_id", protocol))
        .transpose()?
        .flatten();
    let nested_turn_id = turn_metadata
        .as_ref()
        .map(|metadata| optional_identity_field(Some(metadata), "turn_id", protocol))
        .transpose()?
        .flatten();

    Ok(CodexIdentityInput {
        installation_id: metadata
            .codex_installation_id
            .clone()
            .or(body_installation_id)
            .or(legacy_installation_id)
            .or(nested_installation_id),
        window_id: metadata
            .codex_window_id
            .clone()
            .or(body_window_id)
            .or(nested_window_id),
        turn_id: top_level_turn_id.or(nested_turn_id),
        turn_metadata,
    })
}

fn optional_identity_field(
    metadata: Option<&Map<String, Value>>,
    field: &str,
    protocol: WireFormat,
) -> Result<Option<String>, HttpError> {
    let Some(value) = metadata.and_then(|metadata| metadata.get(field)) else {
        return Ok(None);
    };
    match value {
        Value::Null => Ok(None),
        Value::String(value) => validated_session_id(value, protocol),
        _ => Err(HttpError::invalid_request(
            protocol,
            "Codex identity must be a string",
        )),
    }
}

fn optional_turn_metadata(
    value: Option<&Value>,
    protocol: WireFormat,
) -> Result<Option<Map<String, Value>>, HttpError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let raw = match value {
        Value::Null => return Ok(None),
        Value::String(value) => value.trim(),
        _ => {
            return Err(HttpError::invalid_request(
                protocol,
                "x-codex-turn-metadata must be a JSON string",
            ));
        }
    };
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.len() > MAX_CODEX_TURN_METADATA_LENGTH {
        return Err(HttpError::invalid_request(
            protocol,
            "x-codex-turn-metadata is too large",
        ));
    }
    parse_turn_metadata(raw, protocol).map(Some)
}

fn parse_turn_metadata(raw: &str, protocol: WireFormat) -> Result<Map<String, Value>, HttpError> {
    let metadata = serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .ok_or_else(|| {
            HttpError::invalid_request(protocol, "x-codex-turn-metadata must contain a JSON object")
        })?;
    for field in [
        "prompt_cache_key",
        "session_id",
        "thread_id",
        "window_id",
        CODEX_INSTALLATION_ID_HEADER,
        "installation_id",
        "turn_id",
    ] {
        optional_identity_field(Some(&metadata), field, protocol)?;
    }
    Ok(metadata)
}

fn remove_unlinked_codex_identity(root: &mut Map<String, Value>) {
    let Some(metadata) = root
        .get_mut("client_metadata")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for field in [
        "session_id",
        "thread_id",
        "turn_id",
        "installation_id",
        "x-codex-window-id",
        "x-codex-installation-id",
    ] {
        metadata.remove(field);
    }
    metadata.remove("x-codex-turn-metadata");
}

fn normalize_chat_session(request: &mut ProxyRequest, key: &AuthenticatedApiKey) {
    let session_seed = request
        .metadata
        .session_id
        .as_deref()
        .and_then(normalized_string)
        .or_else(|| {
            request
                .metadata
                .thread_id
                .as_deref()
                .and_then(normalized_string)
        })
        .or_else(|| {
            request
                .metadata
                .client_request_id
                .as_deref()
                .and_then(normalized_string)
        });
    let Some(session_seed) = session_seed else {
        return;
    };
    let session_id = responses_cache_key(key, &request.model, &session_seed);
    request.metadata.session_id = Some(session_id.clone());
    if request.metadata.thread_id.is_some() {
        request.metadata.thread_id = Some(session_id.clone());
    }
    if request.metadata.client_request_id.is_some() {
        request.metadata.client_request_id = Some(session_id);
    }
}

fn normalize_codex_identity(
    request: &mut ProxyRequest,
    root: &mut Map<String, Value>,
    session_id: &str,
    mut identity: CodexIdentityInput,
) -> Result<(), HttpError> {
    let canonical_turn_id = identity
        .turn_id
        .as_deref()
        .map(|turn_id| isolated_codex_identity_id("codex-turn-v1", "rt_", session_id, turn_id));
    let canonical_installation_id = identity.installation_id.as_deref().map(|installation_id| {
        isolated_codex_identity_id("codex-installation-v1", "ri_", session_id, installation_id)
    });
    let canonical_window_id = identity
        .window_id
        .as_ref()
        .map(|_| format!("{session_id}:0"));
    let has_codex_identity = canonical_installation_id.is_some()
        || canonical_window_id.is_some()
        || identity.turn_metadata.is_some();
    if !root.get("client_metadata").is_some_and(Value::is_object) && !has_codex_identity {
        clear_codex_identity_headers(&mut request.metadata);
        return Ok(());
    }
    if !root.get("client_metadata").is_some_and(Value::is_object) {
        root.insert("client_metadata".to_owned(), Value::Object(Map::new()));
    }
    let client_metadata = root
        .get_mut("client_metadata")
        .expect("Codex client_metadata exists")
        .as_object_mut()
        .expect("Codex client_metadata was validated as an object");
    normalize_codex_identity_fields(
        client_metadata,
        session_id,
        canonical_turn_id.as_deref(),
        canonical_installation_id.as_deref(),
    );
    match canonical_installation_id.as_deref() {
        Some(value) => {
            client_metadata.insert(
                CODEX_INSTALLATION_ID_HEADER.to_owned(),
                Value::String(value.to_owned()),
            );
        }
        None => {
            client_metadata.remove(CODEX_INSTALLATION_ID_HEADER);
        }
    }
    match canonical_window_id.as_deref() {
        Some(value) => {
            client_metadata.insert(
                CODEX_WINDOW_ID_HEADER.to_owned(),
                Value::String(value.to_owned()),
            );
        }
        None => {
            client_metadata.remove(CODEX_WINDOW_ID_HEADER);
        }
    }
    let canonical_turn_metadata = if let Some(turn_metadata) = identity.turn_metadata.as_mut() {
        normalize_codex_identity_fields(
            turn_metadata,
            session_id,
            canonical_turn_id.as_deref(),
            canonical_installation_id.as_deref(),
        );
        if turn_metadata.contains_key("prompt_cache_key") {
            turn_metadata.insert(
                "prompt_cache_key".to_owned(),
                Value::String(session_id.to_owned()),
            );
        }
        if turn_metadata.contains_key("window_id") {
            turn_metadata.insert(
                "window_id".to_owned(),
                Value::String(format!("{session_id}:0")),
            );
        }
        Some(
            serde_json::to_string(turn_metadata)
                .map_err(|_| HttpError::internal(request.format))?,
        )
    } else {
        None
    };
    match canonical_turn_metadata.as_deref() {
        Some(value) => {
            client_metadata.insert(
                CODEX_TURN_METADATA_HEADER.to_owned(),
                Value::String(value.to_owned()),
            );
        }
        None => {
            client_metadata.remove(CODEX_TURN_METADATA_HEADER);
        }
    }
    request.metadata.codex_installation_id = canonical_installation_id;
    request.metadata.codex_window_id = canonical_window_id;
    request.metadata.codex_turn_metadata = canonical_turn_metadata;
    Ok(())
}

fn clear_codex_identity_headers(metadata: &mut RequestMetadata) {
    metadata.codex_installation_id = None;
    metadata.codex_window_id = None;
    metadata.codex_turn_metadata = None;
}

fn normalize_codex_identity_fields(
    metadata: &mut Map<String, Value>,
    session_id: &str,
    turn_id: Option<&str>,
    installation_id: Option<&str>,
) {
    for field in ["session_id", "thread_id"] {
        if metadata.contains_key(field) {
            metadata.insert(field.to_owned(), Value::String(session_id.to_owned()));
        }
    }
    if metadata.contains_key("turn_id") {
        match turn_id {
            Some(turn_id) => {
                metadata.insert("turn_id".to_owned(), Value::String(turn_id.to_owned()));
            }
            None => {
                metadata.remove("turn_id");
            }
        }
    }
    for field in ["x-codex-installation-id", "installation_id"] {
        if metadata.contains_key(field) {
            match installation_id {
                Some(installation_id) => {
                    metadata.insert(field.to_owned(), Value::String(installation_id.to_owned()));
                }
                None => {
                    metadata.remove(field);
                }
            }
        }
    }
}

fn normalized_identity_field(metadata: &Map<String, Value>, field: &str) -> Option<String> {
    metadata
        .get(field)
        .and_then(Value::as_str)
        .and_then(normalized_string)
}

fn isolated_codex_identity_id(
    namespace: &str,
    prefix: &str,
    session_id: &str,
    identity: &str,
) -> String {
    let mut digest = Sha256::new();
    for value in [namespace, session_id, identity] {
        digest.update(
            u64::try_from(value.len())
                .expect("Codex identity length must fit u64")
                .to_be_bytes(),
        );
        digest.update(value.as_bytes());
    }
    let digest = digest.finalize();
    let mut encoded = String::with_capacity(prefix.len() + 32);
    encoded.push_str(prefix);
    for byte in digest.iter().take(16) {
        use std::fmt::Write;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn normalized_string(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

pub(super) fn responses_cache_key(
    key: &AuthenticatedApiKey,
    model: &str,
    session_id: &str,
) -> String {
    isolated_cache_key("responses-cache-v1", "rs_", key, model, session_id)
}

fn request_metadata(
    headers: &HeaderMap,
    protocol: WireFormat,
) -> Result<RequestMetadata, HttpError> {
    let mut metadata = RequestMetadata::default();
    metadata.session_id = metadata_header(headers, "session-id", protocol)?;
    metadata.thread_id = metadata_header(headers, "thread-id", protocol)?;
    metadata.client_request_id = metadata_header(headers, "x-client-request-id", protocol)?;
    if protocol == WireFormat::OpenAiResponses {
        metadata.codex_installation_id =
            metadata_header(headers, CODEX_INSTALLATION_ID_HEADER, protocol)?;
        metadata.codex_window_id = metadata_header(headers, CODEX_WINDOW_ID_HEADER, protocol)?;
        metadata.codex_turn_metadata = codex_turn_metadata_header(headers, protocol)?;
    }
    Ok(metadata)
}

fn codex_turn_metadata_header(
    headers: &HeaderMap,
    protocol: WireFormat,
) -> Result<Option<String>, HttpError> {
    let Some(value) = headers.get(CODEX_TURN_METADATA_HEADER) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| {
        HttpError::invalid_request(protocol, "x-codex-turn-metadata header is invalid")
    })?;
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > MAX_CODEX_TURN_METADATA_LENGTH {
        return Err(HttpError::invalid_request(
            protocol,
            "x-codex-turn-metadata is too large",
        ));
    }
    Ok(Some(value.to_owned()))
}

pub(super) fn claude_code_session_id(
    headers: &HeaderMap,
    body: &Map<String, Value>,
    protocol: WireFormat,
) -> Result<Option<String>, HttpError> {
    if let Some(session_id) = metadata_header(headers, CLAUDE_CODE_SESSION_HEADER, protocol)? {
        return Ok(Some(session_id));
    }
    if let Some(user_id) = body
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(Value::as_str)
        && let Ok(metadata) = serde_json::from_str::<Value>(user_id)
        && let Some(session_id) = metadata.get("session_id").and_then(Value::as_str)
    {
        return validated_session_id(session_id, protocol);
    }
    metadata_header(headers, "session-id", protocol)
}

fn validated_session_id(value: &str, protocol: WireFormat) -> Result<Option<String>, HttpError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if !valid_metadata_value(value) {
        return Err(HttpError::invalid_request(
            protocol,
            "request metadata header is invalid",
        ));
    }
    Ok(Some(value.to_owned()))
}

pub(super) fn claude_code_cache_key(
    key: &AuthenticatedApiKey,
    model: &str,
    session_id: &str,
) -> String {
    isolated_cache_key("claude-code-cache-v1", "cc_", key, model, session_id)
}

fn isolated_cache_key(
    namespace: &str,
    prefix: &str,
    key: &AuthenticatedApiKey,
    model: &str,
    session_id: &str,
) -> String {
    let mut digest = Sha256::new();
    for value in [
        namespace,
        key.key_id.as_str(),
        key.owner_user_id.as_str(),
        model,
        session_id,
    ] {
        digest.update(
            u64::try_from(value.len())
                .expect("request metadata length must fit u64")
                .to_be_bytes(),
        );
        digest.update(value.as_bytes());
    }
    let digest = digest.finalize();
    let mut encoded = String::with_capacity(prefix.len() + 32);
    encoded.push_str(prefix);
    for byte in digest.iter().take(16) {
        use std::fmt::Write;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn metadata_header(
    headers: &HeaderMap,
    name: &'static str,
    protocol: WireFormat,
) -> Result<Option<String>, HttpError> {
    let Some(value) = headers.get(name) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| HttpError::invalid_request(protocol, "request metadata header is invalid"))?
        .trim();
    if value.is_empty() {
        return Ok(None);
    }
    if !valid_metadata_value(value) {
        return Err(HttpError::invalid_request(
            protocol,
            "request metadata header is invalid",
        ));
    }
    Ok(Some(value.to_owned()))
}

fn valid_metadata_value(value: &str) -> bool {
    value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}

pub(super) async fn load_key_account_filter(
    api_keys: &ApiKeyAuthenticator,
    key: &AuthenticatedApiKey,
    protocol: WireFormat,
) -> Result<HashSet<AccountId>, HttpError> {
    let account_ids = api_keys
        .account_ids_for_key(&key.owner_user_id, &key.group_label)
        .await
        .map_err(|_| HttpError::internal(protocol))?;
    let mut set = HashSet::new();
    for account_id in account_ids {
        let id = AccountId::new(account_id).map_err(|_| HttpError::internal(protocol))?;
        set.insert(id);
    }
    Ok(set)
}

pub(super) fn authenticate_api_key(
    authenticator: &ApiKeyAuthenticator,
    headers: &HeaderMap,
    protocol: WireFormat,
) -> Result<AuthenticatedApiKey, HttpError> {
    let key = downstream_api_key(headers).ok_or_else(|| HttpError::authentication(protocol))?;
    authenticator
        .authenticate(key, unix_timestamp())
        .map_err(|_| HttpError::authentication(protocol))
}

fn downstream_api_key(headers: &HeaderMap) -> Option<&str> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer_value);
    let x_api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty());
    match (bearer, x_api_key) {
        (Some(bearer), Some(x_api_key)) if bearer != x_api_key => None,
        (Some(bearer), _) => Some(bearer),
        (None, Some(x_api_key)) => Some(x_api_key),
        (None, None) => None,
    }
}

fn bearer_value(value: &str) -> Option<&str> {
    let mut parts = value.split_whitespace();
    let scheme = parts.next()?;
    let token = parts.next()?;
    if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() && parts.next().is_none() {
        Some(token)
    } else {
        None
    }
}

pub(super) fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock must be after unix epoch")
        .as_secs()
        .try_into()
        .expect("unix timestamp must fit i64")
}
