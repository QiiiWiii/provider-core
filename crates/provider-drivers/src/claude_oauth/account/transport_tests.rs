use std::sync::{Arc, Mutex, atomic::AtomicUsize};

use axum::{
    Json, Router,
    body::Bytes,
    http::StatusCode,
    routing::{get, post},
};
use provider_core::{ProviderRequest, WireFormat};
use serde_json::json;

use super::super::client::status_error;
use super::test_support::FailingOnceRepository;
use super::*;

#[tokio::test]
async fn count_tokens_uses_the_measured_anthropic_oauth_contract() {
    let app = Router::new().route(
        "/messages/count_tokens",
        post(|uri: axum::http::Uri, headers: axum::http::HeaderMap, body: Bytes| async move {
            assert_eq!(uri.query(), Some("beta=true"));
            assert_eq!(headers["authorization"], "Bearer access-token");
            assert_eq!(
                headers["anthropic-beta"],
                "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,context-management-2025-06-27,token-counting-2024-11-01"
            );
            assert_eq!(
                headers["x-claude-code-session-id"],
                "11111111-2222-4333-8444-555555555555"
            );
            assert!(!headers.contains_key("x-stainless-timeout"));
            assert_eq!(
                body.as_ref(),
                br#"{"model":"claude-opus-5","messages":[{"role":"user","content":"hello"}],"tools":[]}"#
            );
            Json(json!({"input_tokens": 34}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("count tokens listener");
    let address = listener.local_addr().expect("count tokens address");
    let server = tokio::spawn(axum::serve(listener, app).into_future());
    let repository = Arc::new(FailingOnceRepository {
        writes: AtomicUsize::new(0),
        update: Mutex::new(None),
        auth_state: Mutex::new(None),
    });
    let credentials = ClaudeOAuthCredentials::from_parts(
        "access-token".to_owned(),
        "refresh-token".to_owned(),
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_owned(),
        None,
        "f".repeat(64),
        unix_timestamp() + 3600,
        unix_timestamp(),
    )
    .expect("credentials");
    let account = ClaudeOAuthAccount {
        driver: ClaudeOAuthDriver::for_test(
            &format!("http://{address}"),
            "http://unused.invalid/token",
        ),
        account_id: AccountId::new("claude-oauth-count").expect("account ID"),
        repository,
        state: RwLock::new(ClaudeOAuthState {
            next_refresh_at: None,
            credentials,
            revision: 1,
            generation: 0,
            auth_state: AccountAuthState::Active,
            pending_update: None,
        }),
        refresh_gate: tokio::sync::Mutex::new(()),
    };
    let body = Bytes::from_static(
        br#"{"model":"claude-opus-5","messages":[{"role":"user","content":"hello"}],"tools":[]}"#,
    );
    let mut metadata = provider_core::RequestMetadata::default();
    metadata.client = provider_core::RequestClient::ClaudeCode;
    metadata.user_agent = Some("claude-cli/2.1.220 (external, cli)".to_owned());
    metadata.claude_code_session_id = Some("11111111-2222-4333-8444-555555555555".to_owned());
    metadata.claude_code_payload = Some(body.clone());
    metadata.claude_code_headers = vec![
        (
            "user-agent".to_owned(),
            "claude-cli/2.1.220 (external, cli)".to_owned(),
        ),
        ("x-stainless-timeout".to_owned(), "600".to_owned()),
    ];
    let count = account
        .count_tokens(ProviderRequest {
            format: WireFormat::ClaudeMessages,
            model: "claude-opus-5".to_owned(),
            payload: body,
            metadata,
        })
        .await
        .expect("upstream token count");
    assert_eq!(count, 34);
    server.abort();
}

#[tokio::test]
async fn messages_uses_beta_endpoint_and_retains_upstream_error() {
    let app = Router::new().route(
        "/messages",
        post(|uri: axum::http::Uri| async move {
            assert_eq!(uri.query(), Some("beta=true"));
            (
                StatusCode::from_u16(529).expect("overloaded status"),
                [(axum::http::header::RETRY_AFTER, "7")],
                Json(json!({
                    "type": "error",
                    "error": {"type": "overloaded_error", "message": "Overloaded"}
                })),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("messages listener");
    let address = listener.local_addr().expect("messages address");
    let server = tokio::spawn(axum::serve(listener, app).into_future());
    let repository = Arc::new(FailingOnceRepository {
        writes: AtomicUsize::new(0),
        update: Mutex::new(None),
        auth_state: Mutex::new(None),
    });
    let credentials = ClaudeOAuthCredentials::from_parts(
        "access-token".to_owned(),
        "refresh-token".to_owned(),
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_owned(),
        None,
        "f".repeat(64),
        unix_timestamp() + 3600,
        unix_timestamp(),
    )
    .expect("credentials");
    let account = ClaudeOAuthAccount {
        driver: ClaudeOAuthDriver::for_test(
            &format!("http://{address}"),
            "http://unused.invalid/token",
        ),
        account_id: AccountId::new("claude-oauth-messages").expect("account ID"),
        repository,
        state: RwLock::new(ClaudeOAuthState {
            next_refresh_at: None,
            credentials,
            revision: 1,
            generation: 0,
            auth_state: AccountAuthState::Active,
            pending_update: None,
        }),
        refresh_gate: tokio::sync::Mutex::new(()),
    };
    let session = "11111111-2222-4333-8444-555555555555";
    let user_id = format!(
        r#"{{"device_id":"{}","account_uuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","session_id":"{session}"}}"#,
        "e".repeat(64)
    );
    let body = Bytes::from(format!(
        r#"{{"model":"claude-opus-5","messages":[{{"role":"user","content":"hello"}}],"system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=cli; cch=00000;"}}],"metadata":{{"user_id":{}}},"max_tokens":1,"stream":true}}"#,
        serde_json::to_string(&user_id).expect("user ID")
    ));
    let mut metadata = provider_core::RequestMetadata::default();
    metadata.client = provider_core::RequestClient::ClaudeCode;
    metadata.user_agent = Some("claude-cli/2.1.220 (external, cli)".to_owned());
    metadata.claude_code_beta = Some("claude-code-20250219".to_owned());
    metadata.claude_code_user_id = Some(user_id);
    metadata.claude_code_session_id = Some(session.to_owned());
    metadata.claude_code_payload = Some(body.clone());
    let error = match account
        .execute_stream(ProviderRequest {
            format: WireFormat::ClaudeMessages,
            model: "claude-opus-5".to_owned(),
            payload: body,
            metadata,
        })
        .await
    {
        Ok(_) => panic!("overloaded response was accepted"),
        Err(error) => error,
    };
    assert_eq!(error.upstream_status(), Some(529));
    assert_eq!(error.retry_after(), Some(std::time::Duration::from_secs(7)));
    let body: serde_json::Value =
        serde_json::from_slice(error.upstream_body().expect("upstream error body"))
            .expect("upstream error JSON");
    assert_eq!(body["error"]["type"], "overloaded_error");
    assert_eq!(body["error"]["message"], "Overloaded");
    server.abort();
}

#[tokio::test]
async fn quota_and_rate_limit_statuses_are_failover_eligible() {
    let app = Router::new()
        .route("/quota", get(|| async { StatusCode::PAYMENT_REQUIRED }))
        .route("/rate", get(|| async { StatusCode::TOO_MANY_REQUESTS }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("status listener");
    let address = listener.local_addr().expect("status address");
    let server = tokio::spawn(axum::serve(listener, app).into_future());
    for (path, reason) in [
        (
            "quota",
            provider_core::ProviderFailoverReason::QuotaExhausted,
        ),
        ("rate", provider_core::ProviderFailoverReason::RateLimited),
    ] {
        let response = reqwest::get(format!("http://{address}/{path}"))
            .await
            .expect("status response");
        let error = status_error(response).await;
        assert_eq!(error.failover_reason(), Some(reason));
        assert!(error.upstream_body().is_none());
    }
    server.abort();
}
