use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::stream;
use provider_auth::{ApiKeyId, AuthService, CreateApiKeyInput, UserId};
use provider_core::{
    AccountId, CredentialKind, NewCredential, NewProviderAccount, Provider, ProviderKind,
    ProviderManagementRepository, ProviderModel, ProviderRequest, ProviderStream,
    ProviderVisibility, ProxyService, WireFormat,
};
use provider_protocol::DefaultProtocolBridge;
use provider_storage::SqliteAccountRepository;
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use tokio::net::TcpListener;

use super::*;

struct JsonTestProvider {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
    models: Vec<ProviderModel>,
}

#[async_trait]
impl Provider for JsonTestProvider {
    fn name(&self) -> &'static str {
        "json-test"
    }

    fn native_format(&self) -> WireFormat {
        WireFormat::ClaudeMessages
    }

    fn models(&self) -> &[ProviderModel] {
        &self.models
    }

    async fn execute_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderStream, ProviderError> {
        self.requests
            .lock()
            .expect("request capture lock")
            .push(request);
        Ok(Box::pin(stream::once(async {
            Ok(Bytes::from_static(
                br#"{"id":"msg_helper","type":"message","content":[]}"#,
            ))
        })))
    }

    async fn count_tokens(&self, _request: ProviderRequest) -> Result<u64, ProviderError> {
        Ok(1)
    }
}

#[test]
fn keeps_native_bytes_only_in_restricted_request_metadata() {
    let key = AuthenticatedApiKey {
        key_id: ApiKeyId::new("key-a").expect("API key ID"),
        owner_user_id: UserId::new("user-a").expect("user ID"),
        label: "first".to_owned(),
        group_label: "default".to_owned(),
        quota_limit_atoms: None,
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::USER_AGENT,
        "claude-cli/2.1.220 (external, cli)"
            .parse()
            .expect("user agent"),
    );
    headers.insert("x-app", "cli".parse().expect("x-app"));
    headers.insert(
        "anthropic-beta",
        "claude-code-20250219".parse().expect("beta"),
    );
    let body = Bytes::from_static(
        br#"{"model":"claude-sonnet-4-6","system":[{"type":"text","text":"native-system"}],"messages":[],"metadata":{"user_id":"{\"device_id\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"session_id\":\"11111111-2222-4333-8444-555555555555\"}"}}"#,
    );
    let payload = serde_json::from_slice(&body).expect("request JSON");
    let request = match proxy_request_for_key_from_payload(
        WireFormat::ClaudeMessages,
        &headers,
        body.clone(),
        payload,
        &key,
    ) {
        Ok(request) => request,
        Err(_) => panic!("Claude Code request should be valid"),
    };

    assert_eq!(
        request.metadata.client,
        provider_core::RequestClient::ClaudeCode
    );
    assert_eq!(request.metadata.claude_code_payload.as_ref(), Some(&body));
    let sanitized: Value = serde_json::from_slice(&request.payload).expect("sanitized JSON");
    assert!(sanitized.get("metadata").is_none());
}

#[tokio::test]
async fn measured_minimal_helper_uses_the_non_streaming_messages_path() {
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
            unix_timestamp(),
        )
        .await
        .expect("initial setup");
    repository
        .create_provider_account(
            NewProviderAccount {
                id: AccountId::new("acct-helper-1").expect("account ID"),
                provider: ProviderKind::OpenAiCompatible,
                label: "seed".to_owned(),
                group_label: "default".to_owned(),
                priority: 0,
                config_json: "{}".to_owned(),
                enabled: true,
                credential: NewCredential {
                    kind: CredentialKind::ApiKey,
                    format_version: 1,
                    credential_json: SecretString::from("seed-secret".to_owned()),
                    expires_at: None,
                    last_refreshed_at: None,
                },
            },
            grant.user.id.as_str(),
            ProviderVisibility::Private,
        )
        .await
        .expect("seed provider account");
    let api_keys = ApiKeyAuthenticator::load(repository)
        .await
        .expect("API key index");
    let created_key = api_keys
        .create(CreateApiKeyInput {
            owner_user_id: &grant.user.id,
            secret: SecretString::from("helper-test-api-key"),
            group_label: "default".to_owned(),
            label: "helper-test".to_owned(),
            expires_at: None,
            quota_limit_usd: None,
            now: unix_timestamp(),
        })
        .await
        .expect("create API key");
    let api_key = created_key.key.expose_secret().to_owned();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let service = ProxyService::new(
        Arc::new(JsonTestProvider {
            requests: requests.clone(),
            models: vec![ProviderModel::new("claude-haiku-4-5-20251001", "anthropic")],
        }),
        Arc::new(DefaultProtocolBridge),
        provider_core::ProviderAccountAccess {
            owner_user_id: Some(grant.user.id.as_str().to_owned()),
            visibility: ProviderVisibility::Private,
        },
    );
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind helper server");
    let address = listener.local_addr().expect("helper server address");
    let server = tokio::spawn(axum::serve(listener, router(service, api_keys)).into_future());
    let client = reqwest::Client::new();
    let beta = "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05";
    let user_id = r#"{"device_id":"0000000000000000000000000000000000000000000000000000000000000000","account_uuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","session_id":"11111111-2222-4333-8444-555555555555"}"#;
    let body = format!(
        r#"{{"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":[{{"role":"user","content":"helper probe"}}],"metadata":{{"user_id":{}}}}}"#,
        serde_json::to_string(user_id).expect("user ID")
    );
    let send = |request_id: &'static str| {
        client
            .post(format!("http://{address}/v1/messages"))
            .header("x-api-key", &api_key)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::USER_AGENT, "claude-cli/2.1.220 (external, cli)")
            .header("x-app", "cli")
            .header("anthropic-beta", beta)
            .header("accept", "application/json")
            .header("accept-encoding", "gzip")
            .header("x-stainless-lang", "js")
            .header("x-stainless-runtime", "node")
            .header("x-stainless-retry-count", "0")
            .header("x-stainless-timeout", "600")
            .header("x-stainless-package-version", "0.94.0")
            .header("x-stainless-runtime-version", "v26.3.0")
            .header("x-stainless-os", "Darwin")
            .header("x-stainless-arch", "arm64")
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-dangerous-direct-browser-access", "true")
            .header("x-client-request-id", request_id)
            .header(
                CLAUDE_CODE_SESSION_HEADER,
                "11111111-2222-4333-8444-555555555555",
            )
            .body(body.clone())
    };

    let response = send("66666666-7777-4888-8999-aaaaaaaaaaaa")
        .send()
        .await
        .expect("helper response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    let response: Value =
        serde_json::from_slice(&response.bytes().await.expect("helper response body"))
            .expect("helper response JSON");
    assert_eq!(response["id"], "msg_helper");
    {
        let captured = requests.lock().expect("request capture lock");
        assert_eq!(captured.len(), 1);
        assert_eq!(
            captured[0].metadata.client,
            provider_core::RequestClient::Unknown
        );
        assert!(captured[0].metadata.claude_code_payload.is_none());
        let sanitized: Value =
            serde_json::from_slice(&captured[0].payload).expect("sanitized body");
        assert!(sanitized.get("metadata").is_none());
    }

    let rejected = send("not-a-uuid")
        .send()
        .await
        .expect("near-miss helper response");
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert_eq!(requests.lock().expect("request capture lock").len(), 1);
    server.abort();
}
