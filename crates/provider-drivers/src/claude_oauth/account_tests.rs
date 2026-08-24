use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use axum::{
    Json, Router,
    body::Bytes,
    http::StatusCode,
    routing::{get, post},
};
use provider_core::{
    AccountRepositoryError, CredentialUpdate, RefreshTrigger, StoredProviderAccount,
};
use secrecy::ExposeSecret;
use serde_json::json;

use super::*;

struct FailingOnceRepository {
    writes: AtomicUsize,
    update: Mutex<Option<CredentialUpdate>>,
    auth_state: Mutex<Option<AccountAuthState>>,
}

#[async_trait]
impl AccountRepository for FailingOnceRepository {
    async fn load_enabled_accounts(
        &self,
    ) -> Result<Vec<StoredProviderAccount>, AccountRepositoryError> {
        Ok(Vec::new())
    }

    async fn compare_and_swap_credential(
        &self,
        _account_id: &AccountId,
        update: CredentialUpdate,
    ) -> Result<CredentialWriteOutcome, AccountRepositoryError> {
        let write = self.writes.fetch_add(1, Ordering::SeqCst);
        *self.update.lock().expect("update lock") = Some(update);
        if write == 0 {
            Err(AccountRepositoryError::new("temporary storage failure"))
        } else {
            Ok(CredentialWriteOutcome::Updated { revision: 2 })
        }
    }

    async fn update_auth_state(
        &self,
        _account_id: &AccountId,
        state: AccountAuthState,
        _safe_error_code: Option<&str>,
        _updated_at: i64,
    ) -> Result<(), AccountRepositoryError> {
        *self.auth_state.lock().expect("auth state lock") = Some(state);
        Ok(())
    }
}

#[tokio::test]
async fn retains_rotated_refresh_token_until_persistence_recovers() {
    let refreshes = Arc::new(AtomicUsize::new(0));
    let refresh_count = refreshes.clone();
    let app = Router::new()
        .route(
            "/token",
            post(move |headers: axum::http::HeaderMap| {
                let refresh_count = refresh_count.clone();
                async move {
                    assert_eq!(headers["user-agent"], "axios/1.15.2");
                    assert_eq!(headers["accept-encoding"], "gzip, compress, deflate, br");
                    refresh_count.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "access_token": "new-access",
                        "refresh_token": "new-refresh",
                        "expires_in": 3600
                    }))
                }
            }),
        )
        .route("/profile", get(|| async { Json(json!({"account": {}})) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("token listener");
    let address = listener.local_addr().expect("token address");
    let server = tokio::spawn(axum::serve(listener, app).into_future());
    let repository = Arc::new(FailingOnceRepository {
        writes: AtomicUsize::new(0),
        update: Mutex::new(None),
        auth_state: Mutex::new(None),
    });
    let credentials = ClaudeOAuthCredentials::from_parts(
        "old-access".to_owned(),
        "old-refresh".to_owned(),
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_owned(),
        None,
        "f".repeat(64),
        unix_timestamp() + 60,
        1,
    )
    .expect("credentials");
    let account = ClaudeOAuthAccount {
        driver: ClaudeOAuthDriver::for_test(
            "http://unused.invalid",
            &format!("http://{address}/token"),
        ),
        account_id: AccountId::new("claude-oauth-test").expect("account ID"),
        repository: repository.clone(),
        state: RwLock::new(ClaudeOAuthState {
            next_refresh_at: Some(0),
            credentials,
            revision: 1,
            generation: 0,
            auth_state: AccountAuthState::Active,
            pending_update: None,
        }),
    };

    let first = account
        .refresh_credentials(RefreshTrigger::Scheduled)
        .await
        .expect("refresh remains usable");
    assert!(first.state.persistence_pending);
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);

    let second = account
        .refresh_credentials(RefreshTrigger::Scheduled)
        .await
        .expect("pending persistence retry");
    assert!(!second.state.persistence_pending);
    assert_eq!(account.credential_revision(), 2);
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    let stored = repository
        .update
        .lock()
        .expect("update lock")
        .as_ref()
        .expect("stored update")
        .credential_json
        .clone();
    assert!(stored.expose_secret().contains("new-refresh"));
    server.abort();
}

#[tokio::test]
async fn marks_account_reauth_required_after_refresh_rejection() {
    let app = Router::new()
        .route(
            "/token",
            post(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "invalid_grant"})),
                )
            }),
        )
        .route("/profile", get(|| async { Json(json!({"account": {}})) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("token listener");
    let address = listener.local_addr().expect("token address");
    let server = tokio::spawn(axum::serve(listener, app).into_future());
    let repository = Arc::new(FailingOnceRepository {
        writes: AtomicUsize::new(0),
        update: Mutex::new(None),
        auth_state: Mutex::new(None),
    });
    let credentials = ClaudeOAuthCredentials::from_parts(
        "old-access".to_owned(),
        "old-refresh".to_owned(),
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_owned(),
        None,
        "f".repeat(64),
        unix_timestamp() + 60,
        1,
    )
    .expect("credentials");
    let account = ClaudeOAuthAccount {
        driver: ClaudeOAuthDriver::for_test(
            "http://unused.invalid",
            &format!("http://{address}/token"),
        ),
        account_id: AccountId::new("claude-oauth-test").expect("account ID"),
        repository: repository.clone(),
        state: RwLock::new(ClaudeOAuthState {
            next_refresh_at: Some(0),
            credentials,
            revision: 1,
            generation: 0,
            auth_state: AccountAuthState::Active,
            pending_update: None,
        }),
    };

    let error = account
        .refresh_credentials(RefreshTrigger::Scheduled)
        .await
        .expect_err("refresh must require reauthorization");
    assert_eq!(error.kind(), RefreshErrorKind::ReauthRequired);
    assert_eq!(
        account.runtime_state().auth_state,
        AccountAuthState::ReauthRequired
    );
    assert_eq!(account.runtime_state().next_refresh_at, None);
    assert_eq!(
        *repository.auth_state.lock().expect("auth state lock"),
        Some(AccountAuthState::ReauthRequired)
    );
    server.abort();
}

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
        assert_eq!(status_error(response).failover_reason(), Some(reason));
    }
    server.abort();
}
