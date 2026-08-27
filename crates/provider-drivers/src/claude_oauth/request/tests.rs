use super::*;
use provider_core::{RequestMetadata, WireFormat};

const BASE_BODY: &str = r#"{"model":"model-a","messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"},{"type":"text","text":"system-x"}],"tools":[],"metadata":{"user_id":"meta-x"},"max_tokens":1,"thinking":{"type":"adaptive","display":"omitted"},"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"output_config":{"effort":"high"},"stream":true}"#;

#[test]
fn cch_matches_cpa_known_vectors() {
    for (body, expected) in [
            (BASE_BODY.to_owned(), "7ee87"),
            (
                BASE_BODY.replacen(r#""text":"x""#, r#""text":"y""#, 1),
                "b9cc8",
            ),
            (
                BASE_BODY.replacen(r#""model":"model-a""#, r#""model":"model-b""#, 1),
                "7ee87",
            ),
            (
                BASE_BODY.replacen(r#""max_tokens":1"#, r#""max_tokens":2"#, 1),
                "7ee87",
            ),
            (
                BASE_BODY.replacen(r#""user_id":"meta-x""#, r#""user_id":"meta-y""#, 1),
                "7a89d",
            ),
            (
                BASE_BODY.replacen(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","max_tokens":2}"#,
                    1,
                ),
                "7ee87",
            ),
            (
                BASE_BODY.replacen(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","max_tokens":999,"fallbacks":[{"model":"fallback-model"}]}"#,
                    1,
                ),
                "4589b",
            ),
            (
                BASE_BODY.replacen(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","fallback_credit_token":"a"}"#,
                    1,
                ),
                "7ee87",
            ),
            (
                BASE_BODY.replacen(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","model":"nested-model","max_tokens":999,"fallbacks":[{"model":"fallback-model"}],"fallback_credit_token":"not-a-real-token"}"#,
                    1,
                ),
                "2d312",
            ),
            (
                BASE_BODY.replacen(
                    r#""metadata":{"user_id":"meta-x"}"#,
                    r#""metadata":{"user_id":"meta-x","max_tokens":999,"model":"nested-model","fallbacks":[{"model":"fallback-model"}]}"#,
                    1,
                ),
                "0601b",
            ),
            (
                r#"{"stream":true,"output_config":{"effort":"high"},"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"thinking":{"type":"adaptive","display":"omitted"},"max_tokens":1,"metadata":{"user_id":"meta-x"},"tools":[],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"},{"type":"text","text":"system-x"}],"messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],"model":"model-a"}"#.to_owned(),
                "e5b6c",
            ),
        ] {
        let signed = ensure_and_sign_cch(body.into_bytes(), None).expect("signed CCH");
            assert!(
                String::from_utf8(signed)
                    .expect("UTF-8 body")
                    .contains(&format!("cch={expected};"))
            );
        }
}

#[test]
fn prepares_oauth_identity_headers_and_preserves_body_order() {
    let session = "11111111-2222-4333-8444-555555555555";
    let caller_user_id = format!(
        r#"{{"device_id":"{}","account_uuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","session_id":"{session}","parent_session_id":"22222222-3333-4444-8555-666666666666","workload":{{"kind":"review"}},"future_flag":true}}"#,
        "f".repeat(64)
    );
    let body = BASE_BODY.replace(
        r#""user_id":"meta-x""#,
        &format!(
            r#""user_id":{}"#,
            serde_json::to_string(&caller_user_id).expect("user ID")
        ),
    );
    let credentials = ClaudeOAuthCredentials::from_parts(
        "access-token".to_owned(),
        "refresh-token".to_owned(),
        "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".to_owned(),
        None,
        "e".repeat(64),
        9_999_999_999,
        1,
    )
    .expect("credentials");
    let mut metadata = RequestMetadata::default();
    metadata.client = RequestClient::ClaudeCode;
    metadata.user_agent = Some("claude-cli/2.1.220 (external, cli)".to_owned());
    metadata.claude_code_beta = Some("claude-code-20250219".to_owned());
    metadata.claude_code_user_id = Some(caller_user_id);
    metadata.claude_code_session_id = Some(session.to_owned());
    metadata.claude_code_payload = Some(Bytes::from(body.clone()));
    let request = ProviderRequest {
        format: WireFormat::ClaudeMessages,
        model: "model-a".to_owned(),
        payload: Bytes::from(body),
        metadata,
    };
    let (body, headers) = prepare_request(&request, &credentials).expect("prepared request");
    assert!(body.starts_with(br#"{"model":"model-a","messages":"#));
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("prepared JSON");
    let identity: serde_json::Value =
        serde_json::from_str(parsed["metadata"]["user_id"].as_str().expect("user ID"))
            .expect("identity JSON");
    assert_eq!(identity["device_id"], "e".repeat(64));
    assert_eq!(
        identity["account_uuid"],
        "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
    );
    assert_eq!(
        identity["parent_session_id"],
        "22222222-3333-4444-8555-666666666666"
    );
    assert_eq!(identity["workload"]["kind"], "review");
    assert_eq!(identity["future_flag"], true);
    let identity_raw = parsed["metadata"]["user_id"].as_str().expect("user ID");
    let parent = identity_raw
        .find("parent_session_id")
        .expect("parent position");
    let workload = identity_raw.find("workload").expect("workload position");
    let future = identity_raw.find("future_flag").expect("future position");
    assert!(parent < workload && workload < future);
    assert_eq!(headers["authorization"], "Bearer access-token");
    assert_eq!(
        headers["anthropic-beta"],
        "claude-code-20250219,oauth-2025-04-20,extended-cache-ttl-2025-04-11"
    );
}

#[test]
fn injects_and_signs_missing_standard_billing_header() {
    let mut metadata = RequestMetadata::default();
    metadata.client = RequestClient::ClaudeCode;
    metadata.user_agent = Some("claude-cli/2.1.220 (external, cli)".to_owned());
    let request = ProviderRequest {
        format: WireFormat::ClaudeMessages,
        model: "claude-sonnet-4-6".to_owned(),
        payload: Bytes::from_static(
            br#"{"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"hello from Claude Code"}],"metadata":{"user_id":"meta"},"max_tokens":1}"#,
        ),
        metadata,
    };
    let billing = fallback_billing(&request, &request.payload).expect("fallback billing");
    let signed =
        ensure_and_sign_cch(request.payload.to_vec(), Some(&billing)).expect("signed body");
    let value: serde_json::Value = serde_json::from_slice(&signed).expect("signed JSON");
    let billing = value["system"][0]["text"].as_str().expect("billing text");
    assert!(billing.starts_with("x-anthropic-billing-header: cc_version=2.1.220."));
    assert!(!billing.contains("cch=00000"));
}

#[test]
fn preserves_minimal_helper_gzip_profile() {
    let session = "11111111-2222-4333-8444-555555555555";
    let caller_user_id = format!(
        r#"{{"device_id":"{}","account_uuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","session_id":"{session}"}}"#,
        "f".repeat(64)
    );
    let body = format!(
        r#"{{"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":[{{"role":"user","content":"title"}}],"metadata":{{"user_id":{}}}}}"#,
        serde_json::to_string(&caller_user_id).expect("user ID")
    );
    let credentials = ClaudeOAuthCredentials::from_parts(
        "access-token".to_owned(),
        "refresh-token".to_owned(),
        "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".to_owned(),
        None,
        "e".repeat(64),
        9_999_999_999,
        1,
    )
    .expect("credentials");
    let mut metadata = RequestMetadata::default();
    metadata.client = RequestClient::ClaudeCode;
    metadata.user_agent = Some("claude-cli/2.1.220 (external, cli)".to_owned());
    metadata.claude_code_beta = Some(
        "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05".to_owned(),
    );
    metadata.claude_code_user_id = Some(caller_user_id);
    metadata.claude_code_session_id = Some(session.to_owned());
    metadata.claude_code_headers = vec![("accept-encoding".to_owned(), "gzip".to_owned())];
    metadata.claude_code_helper_profile = true;
    metadata.claude_code_payload = Some(Bytes::from(body.clone()));
    let request = ProviderRequest {
        format: WireFormat::ClaudeMessages,
        model: "claude-haiku-4-5-20251001".to_owned(),
        payload: Bytes::from(body),
        metadata,
    };

    let (_, headers) = prepare_request(&request, &credentials).expect("prepared helper");
    assert_eq!(headers["accept-encoding"], "gzip");
    assert!(
        !headers["anthropic-beta"]
            .to_str()
            .expect("beta")
            .contains("extended-cache-ttl-2025-04-11")
    );
}

#[test]
fn rejects_duplicate_sensitive_identity_members() {
    let session = "11111111-2222-4333-8444-555555555555";
    let caller_user_id = format!(
        r#"{{"device_id":"{}","account_uuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","session_id":"{session}"}}"#,
        "f".repeat(64)
    );
    let encoded = serde_json::to_string(&caller_user_id).expect("user ID");
    let credentials = ClaudeOAuthCredentials::from_parts(
        "access-token".to_owned(),
        "refresh-token".to_owned(),
        "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".to_owned(),
        None,
        "e".repeat(64),
        9_999_999_999,
        1,
    )
    .expect("credentials");
    let duplicate_identity = format!(
        r#"{{"device_id":"{}","account_uuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","session_id":"{session}","session_id":"{session}"}}"#,
        "f".repeat(64)
    );
    for (body, user_id) in [
        (
            format!(
                r#"{{"model":"claude-sonnet-4-6","messages":[],"metadata":{{"user_id":{encoded}}},"metadata":{{"user_id":{encoded}}}}}"#
            ),
            caller_user_id.clone(),
        ),
        (
            format!(
                r#"{{"model":"claude-sonnet-4-6","messages":[],"metadata":{{"user_id":{encoded},"user_id":{encoded}}}}}"#
            ),
            caller_user_id.clone(),
        ),
        (
            format!(
                r#"{{"model":"claude-sonnet-4-6","messages":[],"metadata":{{"user_id":{}}}}}"#,
                serde_json::to_string(&duplicate_identity).expect("duplicate identity")
            ),
            duplicate_identity.clone(),
        ),
    ] {
        let mut metadata = RequestMetadata::default();
        metadata.client = RequestClient::ClaudeCode;
        metadata.user_agent = Some("claude-cli/2.1.220 (external, cli)".to_owned());
        metadata.claude_code_beta = Some("claude-code-20250219".to_owned());
        metadata.claude_code_user_id = Some(user_id);
        metadata.claude_code_session_id = Some(session.to_owned());
        metadata.claude_code_payload = Some(Bytes::from(body));
        let request = ProviderRequest {
            format: WireFormat::ClaudeMessages,
            model: "claude-sonnet-4-6".to_owned(),
            payload: Bytes::from_static(br#"{"model":"claude-sonnet-4-6","messages":[]}"#),
            metadata,
        };
        assert!(prepare_request(&request, &credentials).is_err());
    }
}
