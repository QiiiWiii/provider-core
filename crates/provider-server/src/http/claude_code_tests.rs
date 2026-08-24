use super::*;
use axum::http::{HeaderName, HeaderValue};

fn headers(beta: &str, structured: bool) -> HeaderMap {
    let mut headers = HeaderMap::from_iter([
        (
            header::USER_AGENT,
            HeaderValue::from_static("claude-cli/2.1.220 (external, cli)"),
        ),
        (
            "x-app".parse().expect("x-app"),
            HeaderValue::from_static("cli"),
        ),
        (
            "anthropic-beta".parse().expect("anthropic-beta"),
            HeaderValue::from_str(beta).expect("beta"),
        ),
        (
            "accept".parse().expect("accept"),
            HeaderValue::from_static("application/json"),
        ),
        (
            "content-type".parse().expect("content-type"),
            HeaderValue::from_static("application/json"),
        ),
        (
            "x-stainless-lang".parse().expect("lang"),
            HeaderValue::from_static("js"),
        ),
        (
            "x-stainless-runtime".parse().expect("runtime"),
            HeaderValue::from_static("node"),
        ),
        (
            "x-stainless-retry-count".parse().expect("retry"),
            HeaderValue::from_static("0"),
        ),
        (
            "x-stainless-timeout".parse().expect("timeout"),
            HeaderValue::from_static("600"),
        ),
        (
            "anthropic-version".parse().expect("version"),
            HeaderValue::from_static("2023-06-01"),
        ),
        (
            "anthropic-dangerous-direct-browser-access"
                .parse()
                .expect("dangerous"),
            HeaderValue::from_static("true"),
        ),
        (
            "x-stainless-package-version".parse().expect("package"),
            HeaderValue::from_static(CLAUDE_CODE_PACKAGE_VERSION),
        ),
        (
            "x-stainless-runtime-version"
                .parse()
                .expect("runtime version"),
            HeaderValue::from_static(CLAUDE_CODE_RUNTIME_VERSION),
        ),
        (
            "x-stainless-os".parse().expect("os"),
            HeaderValue::from_static("Darwin"),
        ),
        (
            "x-stainless-arch".parse().expect("arch"),
            HeaderValue::from_static("arm64"),
        ),
        (
            "x-client-request-id".parse().expect("request id"),
            HeaderValue::from_static("66666666-7777-4888-8999-aaaaaaaaaaaa"),
        ),
        (
            CLAUDE_CODE_SESSION_HEADER.parse().expect("session header"),
            HeaderValue::from_static("11111111-2222-4333-8444-555555555555"),
        ),
    ]);
    headers.insert(
        HeaderName::from_static("accept-encoding"),
        HeaderValue::from_static(if structured {
            "gzip, deflate, br, zstd"
        } else {
            "gzip"
        }),
    );
    if structured {
        headers.insert(
            HeaderName::from_static("x-stainless-async"),
            HeaderValue::from_static("async"),
        );
    }
    headers
}

fn user_id() -> &'static str {
    r#"{"device_id":"0000000000000000000000000000000000000000000000000000000000000000","account_uuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","session_id":"11111111-2222-4333-8444-555555555555"}"#
}

fn minimal_payload_with_user_id(user_id: &str) -> Vec<u8> {
    format!(
            r#"{{"model":"{}","max_tokens":1,"messages":[{{"role":"user","content":"helper probe"}}],"metadata":{{"user_id":{}}}}}"#,
            HELPER_MODEL,
            serde_json::to_string(user_id).expect("user ID")
        )
        .into_bytes()
}

fn minimal_payload() -> Vec<u8> {
    minimal_payload_with_user_id(user_id())
}

fn structured_payload() -> Vec<u8> {
    format!(
            r#"{{"model":"{}","messages":[{{"role":"user","content":[{{"type":"text","text":"helper probe"}}]}}],"system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220; cc_entrypoint=cli; cch=00000;"}},{{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."}},{{"type":"text","text":"Return a short title."}}],"tools":[],"metadata":{{"user_id":{}}},"max_tokens":32000,"thinking":{{"type":"disabled"}},"temperature":1,"output_config":{{"format":{{"type":"json_schema","schema":{{"type":"object","properties":{{"title":{{"type":"string"}}}},"required":["title"],"additionalProperties":false}}}}}},"stream":true}}"#,
            HELPER_MODEL,
            serde_json::to_string(user_id()).expect("user ID")
        )
        .into_bytes()
}

#[test]
fn accepts_standard_and_count_tokens_signals() {
    let standard = HeaderMap::from_iter([
        (
            header::USER_AGENT,
            HeaderValue::from_static("claude-cli/2.1.220 (external, cli)"),
        ),
        (
            "x-app".parse().expect("x-app"),
            HeaderValue::from_static("cli"),
        ),
        (
            "anthropic-beta".parse().expect("beta"),
            HeaderValue::from_static(CLAUDE_CODE_BETA),
        ),
    ]);
    assert_eq!(
        request_metadata(&standard, &minimal_payload(), false).client,
        RequestClient::ClaudeCode
    );
    assert_eq!(
        request_metadata(&standard, br#"{"model":"x"}"#, true).client,
        RequestClient::ClaudeCode
    );
}

#[test]
fn accepts_measured_markerless_helpers_and_rejects_near_misses() {
    let beta = "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05";
    let headers = headers(beta, false);
    assert_eq!(
        request_metadata(&headers, &minimal_payload(), false).client,
        RequestClient::ClaudeCode
    );
    let user_id_without_account_uuid = r#"{"device_id":"0000000000000000000000000000000000000000000000000000000000000000","session_id":"11111111-2222-4333-8444-555555555555"}"#;
    assert_eq!(
        request_metadata(
            &headers,
            &minimal_payload_with_user_id(user_id_without_account_uuid),
            false,
        )
        .client,
        RequestClient::Unknown
    );
    let mut invalid = headers.clone();
    invalid.insert(
        HeaderName::from_static("x-client-request-id"),
        HeaderValue::from_static("not-a-uuid"),
    );
    assert_eq!(
        request_metadata(&invalid, &minimal_payload(), false).client,
        RequestClient::Unknown
    );
    let reordered = String::from_utf8(minimal_payload())
        .expect("helper JSON")
        .replacen(
            &format!(r#"{{"model":"{HELPER_MODEL}","max_tokens":1"#),
            &format!(r#"{{"max_tokens":1,"model":"{HELPER_MODEL}""#),
            1,
        );
    assert_eq!(
        request_metadata(&headers, reordered.as_bytes(), false).client,
        RequestClient::Unknown
    );
}

#[test]
fn accepts_structured_markerless_helpers() {
    let beta = "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,structured-outputs-2025-12-15";
    let headers = headers(beta, true);
    let payload = structured_payload();
    assert!(matches!(helper_shape(beta), Some(HelperShape::Structured)));
    assert!(matches!(
        helper_body_shape(&payload, HelperShape::Structured),
        Some(HelperShape::Structured)
    ));
    let user_id = claude_code_user_id(&payload).map(|(user_id, _)| user_id);
    assert!(helper_headers_match(&headers, HelperShape::Structured));
    assert!(helper_session_matches(&headers, user_id.as_deref()));
    assert_eq!(
        request_metadata(&headers, &payload, false).client,
        RequestClient::ClaudeCode
    );
}

#[test]
fn allows_missing_account_uuid() {
    let headers = HeaderMap::from_iter([
        (
            header::USER_AGENT,
            HeaderValue::from_static("claude-cli/2.1.220 (external, cli)"),
        ),
        (
            "x-app".parse().expect("x-app"),
            HeaderValue::from_static("cli"),
        ),
        (
            "anthropic-beta".parse().expect("beta"),
            HeaderValue::from_static(CLAUDE_CODE_BETA),
        ),
    ]);
    let body = br#"{"metadata":{"user_id":"{\"device_id\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"session_id\":\"11111111-2222-4333-8444-555555555555\"}"}}"#;
    assert_eq!(
        request_metadata(&headers, body, false).client,
        RequestClient::ClaudeCode
    );
}

#[test]
fn rejects_unmeasured_user_agents_and_helper_software_tuples() {
    let beta = "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05";
    let payload = minimal_payload();
    let mut unmeasured_version = headers(beta, false);
    unmeasured_version.insert(
        header::USER_AGENT,
        HeaderValue::from_static("claude-cli/2.1.221 (external, cli)"),
    );
    assert_eq!(
        request_metadata(&unmeasured_version, &payload, false).client,
        RequestClient::Unknown
    );

    let mut extra_identity = headers(beta, false);
    extra_identity.insert(
        header::USER_AGENT,
        HeaderValue::from_static("claude-cli/2.1.220 (external, cli, forged/1.0.0)"),
    );
    assert_eq!(
        request_metadata(&extra_identity, &payload, false).client,
        RequestClient::Unknown
    );

    let mut foreign_package = headers(beta, false);
    foreign_package.insert(
        HeaderName::from_static("x-stainless-package-version"),
        HeaderValue::from_static("9.9.9"),
    );
    assert_eq!(
        request_metadata(&foreign_package, &payload, false).client,
        RequestClient::Unknown
    );
}

#[test]
fn rejects_mismatched_session_identity_and_captures_auxiliary_headers() {
    let mut standard = HeaderMap::from_iter([
        (
            header::USER_AGENT,
            HeaderValue::from_static("claude-cli/2.1.220 (external, cli)"),
        ),
        (
            HeaderName::from_static("x-app"),
            HeaderValue::from_static("cli"),
        ),
        (
            HeaderName::from_static("anthropic-beta"),
            HeaderValue::from_static(CLAUDE_CODE_BETA),
        ),
        (
            HeaderName::from_static("x-claude-code-agent-id"),
            HeaderValue::from_static("agent-123"),
        ),
    ]);
    standard.insert(
        HeaderName::from_static(CLAUDE_CODE_SESSION_HEADER),
        HeaderValue::from_static("22222222-3333-4444-8555-666666666666"),
    );
    assert_eq!(
        request_metadata(&standard, &minimal_payload(), false).client,
        RequestClient::Unknown
    );

    standard.insert(
        HeaderName::from_static(CLAUDE_CODE_SESSION_HEADER),
        HeaderValue::from_static("11111111-2222-4333-8444-555555555555"),
    );
    let metadata = request_metadata(&standard, &minimal_payload(), false);
    assert_eq!(metadata.client, RequestClient::ClaudeCode);
    assert!(
        metadata
            .claude_code_headers
            .iter()
            .any(|(name, value)| { name == "x-claude-code-agent-id" && value == "agent-123" })
    );
}

#[test]
fn rejects_duplicate_sensitive_identity_members() {
    let headers = HeaderMap::from_iter([
        (
            header::USER_AGENT,
            HeaderValue::from_static("claude-cli/2.1.220 (external, cli)"),
        ),
        (
            HeaderName::from_static("x-app"),
            HeaderValue::from_static("cli"),
        ),
        (
            HeaderName::from_static("anthropic-beta"),
            HeaderValue::from_static(CLAUDE_CODE_BETA),
        ),
    ]);
    let encoded = serde_json::to_string(user_id()).expect("user ID");
    for payload in [
        format!(
            r#"{{"model":"claude-sonnet-4-6","metadata":{{"user_id":{encoded}}},"metadata":{{"user_id":{encoded}}}}}"#
        ),
        format!(
            r#"{{"model":"claude-sonnet-4-6","metadata":{{"user_id":{encoded},"user_id":{encoded}}}}}"#
        ),
    ] {
        assert_eq!(
            request_metadata(&headers, payload.as_bytes(), false).client,
            RequestClient::Unknown
        );
    }
}

#[test]
fn model_catalog_requires_the_measured_user_agent() {
    let measured = HeaderMap::from_iter([(
        header::USER_AGENT,
        HeaderValue::from_static("claude-cli/2.1.220 (external, cli)"),
    )]);
    assert_eq!(models_request(&measured).client, RequestClient::ClaudeCode);

    let unmeasured = HeaderMap::from_iter([(
        header::USER_AGENT,
        HeaderValue::from_static("claude-cli/2.1.221 (external, cli)"),
    )]);
    assert_eq!(models_request(&unmeasured).client, RequestClient::Unknown);
}
