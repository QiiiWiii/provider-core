use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Json, Router,
    http::StatusCode,
    routing::{get, post},
};
use provider_core::RefreshTrigger;
use secrecy::ExposeSecret;
use serde_json::json;

use super::super::refresh::ClaudeRefreshClient;
use super::test_support::FailingOnceRepository;
use super::*;

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
        .route(
            "/profile",
            get(|| async {
                Json(json!({
                    "account": {
                        "uuid": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                        "email": "user@example.com"
                    },
                    "organization": {
                        "uuid": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                        "name": "Example Org"
                    }
                }))
            }),
        );
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
        "old-refresh-persistence".to_owned(),
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
        refresh_gate: tokio::sync::Mutex::new(()),
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
    assert!(
        stored
            .expose_secret()
            .contains("\"organization_uuid\":\"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb\"")
    );
    assert!(
        stored
            .expose_secret()
            .contains("\"organization_name\":\"Example Org\"")
    );
    assert!(stored.expose_secret().contains("\"expired\":\""));
    assert!(stored.expose_secret().contains("\"last_refresh\":\""));
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
        "old-refresh-reauth".to_owned(),
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
        refresh_gate: tokio::sync::Mutex::new(()),
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
async fn shares_concurrent_refreshes_and_cools_down_after_429() {
    let refresh_calls = Arc::new(AtomicUsize::new(0));
    let refresh_calls_for_server = refresh_calls.clone();
    let app = Router::new()
        .route(
            "/token",
            post(move || {
                let refresh_calls = refresh_calls_for_server.clone();
                async move {
                    refresh_calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    Json(json!({
                        "access_token": "new-access",
                        "refresh_token": "new-refresh",
                        "expires_in": 3600
                    }))
                }
            }),
        )
        .route(
            "/profile",
            get(|| async {
                Json(json!({
                    "account": {
                        "uuid": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                        "email": "user@example.com"
                    },
                    "organization": {
                        "uuid": "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                        "name": "Example Org"
                    }
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("token listener");
    let address = listener.local_addr().expect("token address");
    let server = tokio::spawn(axum::serve(listener, app).into_future());
    let client = ClaudeRefreshClient::with_token_url(&format!("http://{address}/token"));
    let first_credentials = ClaudeOAuthCredentials::from_parts(
        "old-access".to_owned(),
        format!("refresh-{address}"),
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_owned(),
        None,
        "f".repeat(64),
        unix_timestamp() + 60,
        unix_timestamp(),
    )
    .expect("credentials");
    let second_credentials = ClaudeOAuthCredentials::from_parts(
        "old-access".to_owned(),
        format!("refresh-{address}"),
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_owned(),
        None,
        "e".repeat(64),
        unix_timestamp() + 60,
        unix_timestamp(),
    )
    .expect("credentials");

    let (first, second) = tokio::join!(
        client.refresh(&first_credentials),
        client.refresh(&second_credentials)
    );
    let first = first.expect("leader refresh");
    let second = second.expect("follower refresh");
    assert_eq!(first.device_id(), "f".repeat(64));
    assert_eq!(second.device_id(), "e".repeat(64));
    assert_eq!(first.account_uuid(), "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
    assert_eq!(
        second.account_uuid(),
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    );
    assert_eq!(refresh_calls.load(Ordering::SeqCst), 1);
    server.abort();

    let cooldown_calls = Arc::new(AtomicUsize::new(0));
    let cooldown_calls_for_server = cooldown_calls.clone();
    let app = Router::new().route(
        "/token",
        post(move || {
            let cooldown_calls = cooldown_calls_for_server.clone();
            async move {
                cooldown_calls.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(axum::http::header::RETRY_AFTER, "30")],
                    Json(json!({"error": "rate_limited"})),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("cooldown listener");
    let address = listener.local_addr().expect("cooldown address");
    let server = tokio::spawn(axum::serve(listener, app).into_future());
    let client = ClaudeRefreshClient::with_token_url(&format!("http://{address}/token"));
    let credentials = ClaudeOAuthCredentials::from_parts(
        "old-access".to_owned(),
        format!("cooldown-{address}"),
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".to_owned(),
        None,
        "e".repeat(64),
        unix_timestamp() + 60,
        unix_timestamp(),
    )
    .expect("credentials");
    let first = client.refresh(&credentials).await.expect_err("429 refresh");
    assert_eq!(first.kind(), RefreshErrorKind::Transient);
    let second = client
        .refresh(&credentials)
        .await
        .expect_err("cooldown refresh");
    assert_eq!(second.kind(), RefreshErrorKind::Transient);
    assert!(second.message().contains("cooling down"));
    assert_eq!(cooldown_calls.load(Ordering::SeqCst), 1);
    server.abort();
}
