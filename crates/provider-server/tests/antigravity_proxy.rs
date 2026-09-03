use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::Request,
    http::Response,
    routing::post,
};
use provider_auth::{ApiKeyAuthenticator, AuthService, CreateApiKeyInput};
use provider_core::{
    AccountId, CredentialKind, NewCredential, NewProviderAccount, ProviderAccountAccess,
    ProviderKind, ProviderManagementRepository, ProviderVisibility, ProxyService,
    StoredProviderModel, usage::TokenMetric,
};
use provider_drivers::antigravity::AntigravityDriver;
use provider_protocol::DefaultProtocolBridge;
use provider_runtime::{ProviderModelRouter, ProviderRuntime};
use provider_storage::{SqliteAccountRepository, SqliteUsageRepository};
use provider_usage::{
    DEFAULT_WRITE_QUEUE, LogicalStatus, UsageRepository, UsageTracking, UsageWriter,
};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};

#[derive(Clone, Default)]
struct Captured {
    requests: Arc<Mutex<Vec<Value>>>,
}

struct Deployment {
    server_url: String,
    api_key: String,
    server: JoinHandle<Result<(), std::io::Error>>,
    usage: Arc<SqliteUsageRepository>,
    writer: Arc<UsageWriter>,
}

async fn cloud_code(
    axum::extract::State(captured): axum::extract::State<Captured>,
    request: Request,
) -> Response<Body> {
    let path = request.uri().path().to_owned();
    if path.ends_with(":fetchAvailableModels") {
        return Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"models":{"gemini-3-flash":{"displayName":"Gemini 3 Flash"}}}"#,
            ))
            .expect("Cloud Code models response");
    }
    let body = to_bytes(request.into_body(), 1 << 20)
        .await
        .expect("Cloud Code body");
    captured
        .requests
        .lock()
        .expect("capture lock")
        .push(serde_json::from_slice(&body).expect("Cloud Code JSON"));
    if path.ends_with(":countTokens") {
        return Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(r#"{"totalTokens":7}"#))
            .expect("Cloud Code count response");
    }
    let response_number = captured.requests.lock().expect("capture lock").len();
    let response = Body::from(format!(
        "data: {}\n\n",
        json!({
            "response": {
                "responseId": format!("native-response-{response_number}"),
                "candidates": [{
                    "content": {"parts": [{"text": "hello"}]},
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": 2,
                    "candidatesTokenCount": 1,
                    "totalTokenCount": 3
                }
            }
        })
    ));
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(response)
        .expect("Cloud Code response")
}

async fn spawn(router: Router) -> (String, JoinHandle<Result<(), std::io::Error>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let address = listener.local_addr().expect("test server address");
    let handle = tokio::spawn(axum::serve(listener, router).into_future());
    (format!("http://{address}"), handle)
}

async fn deployment(upstream_url: &str) -> Deployment {
    let driver = AntigravityDriver::for_test(upstream_url.to_owned());
    let runtime = ProviderRuntime::new(driver.clone());
    let runtime_account = driver.test_account("mock-token");
    runtime
        .register(runtime_account.clone())
        .await
        .expect("register Antigravity account");
    let discovered_models = runtime_account
        .discover_models()
        .await
        .expect("discover Antigravity models");

    let repository = Arc::new(
        SqliteAccountRepository::in_memory()
            .await
            .expect("repository"),
    );
    let auth = AuthService::new(repository.clone());
    let grant = auth
        .setup(
            "admin".to_owned(),
            SecretString::from("secret".to_owned()),
            1_700_000_000,
        )
        .await
        .expect("initial setup");
    repository
        .create_provider_account(
            NewProviderAccount {
                id: AccountId::new("test-antigravity").expect("account ID"),
                provider: ProviderKind::Antigravity,
                label: "seed".to_owned(),
                group_label: "default".to_owned(),
                priority: 0,
                config_json: "{}".to_owned(),
                enabled: true,
                credential: NewCredential {
                    kind: CredentialKind::Oauth,
                    format_version: 1,
                    credential_json: SecretString::from(
                        json!({
                            "type": "antigravity",
                            "access_token": "access",
                            "refresh_token": "refresh",
                            "project_id": "test-project"
                        })
                        .to_string(),
                    ),
                    expires_at: None,
                    last_refreshed_at: None,
                },
            },
            grant.user.id.as_str(),
            ProviderVisibility::Private,
        )
        .await
        .expect("seed provider account");
    let api_keys = ApiKeyAuthenticator::load(repository.clone())
        .await
        .expect("API key index");
    let created_key = api_keys
        .create(CreateApiKeyInput {
            owner_user_id: &grant.user.id,
            secret: SecretString::from("test-api-key"),
            group_labels: vec!["default".to_owned()],
            label: "test".to_owned(),
            expires_at: None,
            quota_limit_usd: None,
            now: 1_700_000_000,
        })
        .await
        .expect("create API key");
    let api_key = created_key.key.expose_secret().to_owned();
    let usage = Arc::new(repository.usage_repository());
    let writer = Arc::new(UsageWriter::spawn(usage.clone(), DEFAULT_WRITE_QUEUE));
    let tracking = Arc::new(UsageTracking::new(usage.clone(), writer.clone()));
    let router = ProviderModelRouter::new();
    let models = discovered_models
        .into_iter()
        .map(|model| StoredProviderModel {
            account_id: runtime_account.account_id().clone(),
            upstream_model: model.upstream_model,
            alias: None,
            enabled: true,
            available: true,
            routable: model.routable,
            input_modalities: model.input_modalities,
            metadata_json: model.metadata_json,
            pricing: model.pricing,
            last_seen_at: None,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        })
        .collect();
    router
        .replace_account_models(
            runtime,
            runtime_account,
            models,
            ProviderAccountAccess {
                owner_user_id: Some(grant.user.id.to_string()),
                visibility: ProviderVisibility::Private,
            },
            0,
        )
        .expect("install Antigravity model routes");
    let service = ProxyService::with_router(Arc::new(router), Arc::new(DefaultProtocolBridge));
    let (server_url, server) = spawn(provider_server::router_with_usage(
        service,
        api_keys,
        Some(tracking),
    ))
    .await;
    Deployment {
        server_url,
        api_key,
        server,
        usage,
        writer,
    }
}

#[tokio::test]
async fn serves_responses_chat_and_claude_clients_without_provider_allowlist() {
    let captured = Captured::default();
    let upstream = Router::new()
        .route("/v1internal:streamGenerateContent", post(cloud_code))
        .route("/v1internal:countTokens", post(cloud_code))
        .route("/v1internal:fetchAvailableModels", post(cloud_code))
        .with_state(captured.clone());
    let (upstream_url, upstream_server) = spawn(upstream).await;
    let deployment = deployment(&upstream_url).await;
    let client = reqwest::Client::new();

    let responses = client
        .post(format!("{}/v1/responses", deployment.server_url))
        .bearer_auth(&deployment.api_key)
        .header("content-type", "application/json")
        .body(
            json!({
                "model": "gemini-3-flash",
                "stream": true,
                "input": "hello"
            })
            .to_string(),
        )
        .send()
        .await
        .expect("Responses request")
        .text()
        .await
        .expect("Responses SSE");
    assert!(responses.contains("response.output_text.delta"));
    assert!(responses.contains("response.completed"));

    let continuation = client
        .post(format!("{}/v1/responses", deployment.server_url))
        .bearer_auth(&deployment.api_key)
        .header("content-type", "application/json")
        .body(
            json!({
                "model": "gemini-3-flash",
                "stream": true,
                "previous_response_id": "resp_native-response-1",
                "input": [
                    {"type":"message","role":"user","content":"hello"},
                    {"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]},
                    {"type":"message","role":"user","content":"continue"}
                ]
            })
            .to_string(),
        )
        .send()
        .await
        .expect("Responses continuation request")
        .text()
        .await
        .expect("Responses continuation SSE");
    assert!(continuation.contains("response.completed"));

    let third = client
        .post(format!("{}/v1/responses", deployment.server_url))
        .bearer_auth(&deployment.api_key)
        .header("content-type", "application/json")
        .body(
            json!({
                "model": "gemini-3-flash",
                "stream": true,
                "previous_response_id": "resp_native-response-2",
                "input": [
                    {"type":"message","role":"user","content":"hello"},
                    {"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]},
                    {"type":"message","role":"user","content":"continue"},
                    {"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]},
                    {"type":"message","role":"user","content":"third"}
                ]
            })
            .to_string(),
        )
        .send()
        .await
        .expect("Responses third request")
        .text()
        .await
        .expect("Responses third SSE");
    assert!(third.contains("response.completed"));

    let (second_contents, third_contents) = {
        let requests = captured.requests.lock().expect("capture lock");
        (
            requests[1]["request"]["contents"]
                .as_array()
                .expect("second contents")
                .clone(),
            requests[2]["request"]["contents"]
                .as_array()
                .expect("third contents")
                .clone(),
        )
    };
    assert_eq!(second_contents.len(), 3);
    assert_eq!(second_contents[0]["role"], "user");
    assert_eq!(second_contents[0]["parts"][0]["text"], "hello");
    assert_eq!(second_contents[1]["role"], "model");
    assert_eq!(second_contents[1]["parts"][0]["text"], "hello");
    assert_eq!(second_contents[2]["role"], "user");
    assert_eq!(second_contents[2]["parts"][0]["text"], "continue");

    assert_eq!(third_contents.len(), 5);
    assert_eq!(third_contents[0]["parts"][0]["text"], "hello");
    assert_eq!(third_contents[2]["parts"][0]["text"], "continue");
    assert_eq!(third_contents[4]["parts"][0]["text"], "third");
    let chat = client
        .post(format!("{}/v1/chat/completions", deployment.server_url))
        .bearer_auth(&deployment.api_key)
        .header("content-type", "application/json")
        .body(
            json!({
                "model": "gemini-3-flash",
                "stream": true,
                "messages": [{"role": "user", "content": "hello"}]
            })
            .to_string(),
        )
        .send()
        .await
        .expect("Chat request")
        .text()
        .await
        .expect("Chat SSE");
    assert!(chat.contains("\"object\":\"chat.completion.chunk\""));
    assert!(chat.contains("\"content\":\"hello\""));

    let claude = client
        .post(format!("{}/v1/messages", deployment.server_url))
        .bearer_auth(&deployment.api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(
            json!({
                "model": "gemini-3-flash",
                "stream": true,
                "max_tokens": 128,
                "messages": [{"role": "user", "content": "hello"}]
            })
            .to_string(),
        )
        .send()
        .await
        .expect("Claude request")
        .text()
        .await
        .expect("Claude SSE");
    assert!(claude.contains("event: message_start"));
    assert!(claude.contains("event: content_block_delta"));
    assert!(claude.contains("\"text_delta\""));

    let count = client
        .post(format!(
            "{}/v1/messages/count_tokens",
            deployment.server_url
        ))
        .header("x-api-key", &deployment.api_key)
        .header("content-type", "application/json")
        .body(
            json!({
                "model": "gemini-3-flash",
                "messages": [{"role": "user", "content": "hello"}]
            })
            .to_string(),
        )
        .send()
        .await
        .expect("count tokens request")
        .text()
        .await
        .map(|body| serde_json::from_str::<Value>(&body).expect("count tokens JSON"))
        .expect("count tokens response");
    assert_eq!(count["input_tokens"], 7, "count response={count}");

    {
        let requests = captured.requests.lock().expect("capture lock");
        assert_eq!(requests.len(), 6);
        for request in requests.iter().take(5) {
            assert_eq!(request["project"], "test-project");
            assert_eq!(request["requestType"], "agent");
            assert_eq!(request["userAgent"], "antigravity");
            assert!(
                request["requestId"]
                    .as_str()
                    .is_some_and(|value| value.starts_with("agent-"))
            );
        }
        assert!(requests[5].get("project").is_none());
        assert!(requests[5].get("model").is_none());
        assert!(requests[5].get("requestType").is_none());
        let continuation_contents = requests[1]["request"]["contents"]
            .as_array()
            .expect("continuation contents");
        assert!(continuation_contents.iter().any(|content| {
            content["parts"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part["text"] == "hello"))
        }));
    }

    assert!(deployment.writer.drain(Duration::from_secs(10)).await);
    let request_ids = deployment
        .usage
        .request_ids()
        .await
        .expect("usage request lookup");
    assert_eq!(request_ids.len(), 5);
    for request_id in request_ids {
        let logical = deployment
            .usage
            .load_logical_request(&request_id)
            .await
            .expect("load logical request")
            .expect("logical request present");
        assert_eq!(logical.status, LogicalStatus::Succeeded);
        let attempts = deployment
            .usage
            .load_attempts(&request_id)
            .await
            .expect("load Antigravity attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].provider, ProviderKind::Antigravity);
        assert_eq!(
            attempts[0].observation.effective_input_tokens,
            TokenMetric::ProviderReported { value: 2 }
        );
        assert_eq!(
            attempts[0].observation.output_tokens,
            TokenMetric::ProviderReported { value: 1 }
        );
        assert_eq!(
            attempts[0].observation.total_tokens,
            TokenMetric::ProviderReported { value: 3 }
        );
        assert_eq!(
            logical.final_attempt_id.as_deref(),
            Some(attempts[0].attempt_id.as_str())
        );
    }

    deployment.server.abort();
    upstream_server.abort();
}
