use std::{future::IntoFuture, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    http::{Response, StatusCode},
    routing::post,
};
use provider_auth::{ApiKeyAuthenticator, AuthService, CreateApiKeyInput};
use provider_core::{ProviderKind, ProviderVisibility, ProxyService, usage::TokenMetric};
use provider_drivers::claude_oauth::ClaudeOAuthDriver;
use provider_management::{CredentialProviderAccountInput, ProviderManager};
use provider_protocol::DefaultProtocolBridge;
use provider_runtime::ProviderRuntimeCatalog;
use provider_storage::SqliteAccountRepository;
use provider_usage::{
    DEFAULT_WRITE_QUEUE, DeliveryOutcome, ExecutionOutcome, LogicalStatus, TrackingState,
    UsageRepository, UsageTracking, UsageWriter,
};
use secrecy::{ExposeSecret, SecretString};
use serde_json::json;
use tokio::net::TcpListener;

const ACCOUNT_UUID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const SESSION_ID: &str = "11111111-2222-4333-8444-555555555555";
const DEVICE_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const USER_AGENT: &str = "claude-cli/2.1.220 (external, cli)";

async fn messages() -> Response<Body> {
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{",
        "\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",",
        "\"model\":\"claude-opus-5\",\"content\":[],\"stop_reason\":null,",
        "\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":50,",
        "\"cache_creation_input_tokens\":20,\"output_tokens\":0}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{",
        "\"stop_reason\":\"end_turn\",\"stop_sequence\":null},",
        "\"usage\":{\"output_tokens\":25,",
        "\"output_tokens_details\":{\"thinking_tokens\":4}}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from(body))
        .expect("Claude SSE response")
}

async fn spawn(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener");
    let address = listener.local_addr().expect("test address");
    tokio::spawn(axum::serve(listener, router).into_future());
    format!("http://{address}")
}

fn unix_timestamp() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs(),
    )
    .expect("timestamp fits")
}

#[tokio::test]
async fn claude_oauth_native_usage_reaches_sqlite_through_runtime() {
    let upstream_url = spawn(Router::new().route("/messages", post(messages))).await;
    let repository = Arc::new(
        SqliteAccountRepository::in_memory()
            .await
            .expect("repository"),
    );
    let auth = AuthService::new(repository.clone());
    let now = unix_timestamp();
    let grant = auth
        .setup(
            "admin".to_owned(),
            SecretString::from("secret".to_owned()),
            now,
        )
        .await
        .expect("initial setup");

    let runtime = Arc::new(ProviderRuntimeCatalog::new(repository.clone()));
    runtime
        .register_driver(ClaudeOAuthDriver::for_test(
            &upstream_url,
            "http://unused.invalid/token",
        ))
        .expect("register Claude OAuth driver");
    let manager = ProviderManager::new(repository.clone(), runtime.clone());
    let account = manager
        .create_credential_account(
            grant.user.id.as_str(),
            CredentialProviderAccountInput {
                kind: ProviderKind::ClaudeOAuth,
                label: "Claude OAuth".to_owned(),
                group_label: "default".to_owned(),
                priority: 0,
                credential_json: SecretString::from(
                    json!({
                        "type": "claude",
                        "access_token": "access-token",
                        "refresh_token": "refresh-token",
                        "account_uuid": ACCOUNT_UUID,
                        "claude_device_ids": [DEVICE_ID],
                        "expired": "2030-01-01T00:00:00Z",
                        "last_refresh": "2026-08-26T00:00:00Z"
                    })
                    .to_string(),
                ),
                visibility: ProviderVisibility::Private,
            },
            now,
        )
        .await
        .expect("create Claude OAuth account");
    let api_keys = ApiKeyAuthenticator::load(repository.clone())
        .await
        .expect("API key index");
    let created_key = api_keys
        .create(CreateApiKeyInput {
            owner_user_id: &grant.user.id,
            secret: SecretString::from("claude-usage-key"),
            group_label: "default".to_owned(),
            label: "test".to_owned(),
            expires_at: None,
            quota_limit_usd: None,
            now,
        })
        .await
        .expect("create API key");
    let api_key = created_key.key.expose_secret().to_owned();
    let usage = Arc::new(repository.usage_repository());
    let writer = Arc::new(UsageWriter::spawn(usage.clone(), DEFAULT_WRITE_QUEUE));
    let tracking = Arc::new(UsageTracking::new(usage.clone(), writer.clone()));
    let service = ProxyService::with_router(runtime, Arc::new(DefaultProtocolBridge));
    let server_url = spawn(provider_server::router_with_usage(
        service,
        api_keys,
        Some(tracking),
    ))
    .await;
    let identity = json!({
        "device_id": DEVICE_ID,
        "session_id": SESSION_ID
    })
    .to_string();
    let body = json!({
        "model": "claude-opus-5",
        "messages": [{"role": "user", "content": "hello"}],
        "metadata": {"user_id": identity},
        "max_tokens": 64,
        "stream": true
    });
    let response = reqwest::Client::new()
        .post(format!("{server_url}/v1/messages"))
        .bearer_auth(api_key)
        .header("content-type", "application/json")
        .header("user-agent", USER_AGENT)
        .header("x-app", "cli")
        .header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")
        .header("x-claude-code-session-id", SESSION_ID)
        .body(body.to_string())
        .send()
        .await
        .expect("Claude request");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .text()
            .await
            .expect("Claude body")
            .contains("message_stop")
    );
    assert!(writer.drain(Duration::from_secs(10)).await);

    let request_id = usage
        .oldest_request_id()
        .await
        .expect("request lookup")
        .expect("logical request");
    let logical = usage
        .load_logical_request(&request_id)
        .await
        .expect("load logical")
        .expect("logical present");
    assert_eq!(logical.status, LogicalStatus::Succeeded);
    assert_eq!(
        logical.execution,
        Some(ExecutionOutcome::StableSuccessTerminal)
    );
    assert_eq!(logical.delivery, Some(DeliveryOutcome::CleanEof));
    assert_eq!(
        logical.start.client_type,
        provider_core::RequestClient::ClaudeCode
    );
    assert_eq!(logical.start.user_agent.as_deref(), Some(USER_AGENT));

    let attempts = usage
        .load_attempts(&request_id)
        .await
        .expect("load attempts");
    assert_eq!(attempts.len(), 1);
    let attempt = &attempts[0];
    assert_eq!(attempt.provider, ProviderKind::ClaudeOAuth);
    assert_eq!(attempt.account_id, account.account.id.to_string());
    assert_eq!(
        attempt.observation.uncached_input_tokens,
        TokenMetric::ProviderReported { value: 100 }
    );
    assert_eq!(
        attempt.observation.cache_read_input_tokens,
        TokenMetric::ProviderReported { value: 50 }
    );
    assert_eq!(
        attempt.observation.cache_write_input_tokens,
        TokenMetric::ProviderReported { value: 20 }
    );
    assert_eq!(
        attempt.observation.effective_input_tokens,
        TokenMetric::DerivedFromReported {
            value: 170,
            rule_version: 1,
        }
    );
    assert_eq!(
        attempt.observation.output_tokens,
        TokenMetric::ProviderReported { value: 25 }
    );
    assert_eq!(
        attempt.observation.total_tokens,
        TokenMetric::DerivedFromReported {
            value: 195,
            rule_version: 1,
        }
    );
    assert_eq!(attempt.tracking, TrackingState::Complete);
}
