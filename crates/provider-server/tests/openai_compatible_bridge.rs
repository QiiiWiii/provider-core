use std::sync::{Arc, Mutex};

use axum::Router;
use provider_auth::{ApiKeyAuthenticator, AuthService, CreateApiKeyInput};
use provider_core::{ProviderKind, ProviderVisibility, ProxyService};
use provider_drivers::openai_compatible::OpenAiCompatibleDriver;
use provider_management::{DirectProviderAccountInput, ProviderManager};
use provider_protocol::DefaultProtocolBridge;
use provider_runtime::ProviderRuntimeCatalog;
use provider_storage::SqliteAccountRepository;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{ServerConfig, pki_types::PrivateKeyDer},
};

#[derive(Clone, Default)]
struct UpstreamState {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

#[derive(Clone)]
struct CapturedRequest {
    path: String,
    session: Option<String>,
    body: Value,
}

async fn spawn(router: Router) -> (String, JoinHandle<std::io::Result<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let address = listener.local_addr().expect("server address");
    let handle = tokio::spawn(axum::serve(listener, router).into_future());
    (format!("http://{address}"), handle)
}

async fn spawn_tls_upstream(state: UpstreamState) -> (std::net::SocketAddr, JoinHandle<()>) {
    let certified = rcgen::generate_simple_self_signed(vec!["opencode.ai".to_owned()])
        .expect("test certificate");
    let certificate = certified.cert.der().clone();
    let private_key = PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into());
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], private_key)
        .expect("TLS config");
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind TLS upstream");
    let address = listener.local_addr().expect("TLS address");
    let server = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            let state = state.clone();
            tokio::spawn(async move {
                let Ok(mut stream) = acceptor.accept(stream).await else {
                    return;
                };
                let Some((path, headers, body)) = read_http_request(&mut stream).await else {
                    return;
                };
                if path == "/zen/go/v1/models" {
                    let body = r#"{"object":"list","data":[{"id":"deepseek-v4.1-flash","object":"model","owned_by":"opencode"}]}"#;
                    write_http_response(&mut stream, "application/json", body).await;
                    return;
                }
                if path != "/zen/go/v1/chat/completions" {
                    return;
                }
                let session = headers.iter().find_map(|(name, value)| {
                    name.eq_ignore_ascii_case("x-opencode-session")
                        .then(|| value.to_owned())
                });
                let Ok(body) = serde_json::from_slice(&body) else {
                    return;
                };
                state
                    .requests
                    .lock()
                    .expect("request capture")
                    .push(CapturedRequest {
                        path,
                        session,
                        body,
                    });
                let response = r#"data: {"id":"chatcmpl_1","created":123,"model":"deepseek-v4.1-flash","choices":[{"index":0,"delta":{"reasoning_content":"think"},"finish_reason":null}]}

data: {"id":"chatcmpl_1","created":123,"model":"deepseek-v4.1-flash","choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":"stop"}]}

data: {"id":"chatcmpl_1","created":123,"model":"deepseek-v4.1-flash","choices":[],"usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}}

data: [DONE]

"#;
                write_http_response(&mut stream, "text/event-stream", response).await;
            });
        }
    });
    (address, server)
}

async fn read_http_request<S>(stream: &mut S) -> Option<(String, Vec<(String, String)>, Vec<u8>)>
where
    S: AsyncReadExt + Unpin,
{
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 || request.len() > 1024 * 1024 {
            return None;
        }
        request.extend_from_slice(&chunk[..read]);
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = std::str::from_utf8(&request[..header_end]).ok()?;
    let mut lines = head.split("\r\n");
    let path = lines.next()?.split_whitespace().nth(1)?.to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect::<Vec<_>>();
    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    while request.len() - header_end < content_length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 || request.len() > 1024 * 1024 {
            return None;
        }
        request.extend_from_slice(&chunk[..read]);
    }
    Some((
        path,
        headers,
        request[header_end..header_end + content_length].to_vec(),
    ))
}

async fn write_http_response<S>(stream: &mut S, content_type: &str, body: &str)
where
    S: AsyncWriteExt + Unpin,
{
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[tokio::test]
async fn responses_client_reaches_chat_upstream_and_receives_responses_sse() {
    let upstream_state = UpstreamState::default();
    let (upstream_address, upstream_server) = spawn_tls_upstream(upstream_state.clone()).await;
    let compatible_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs("opencode.ai", &[upstream_address])
        .build()
        .expect("compatible client");

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
    let runtime = Arc::new(ProviderRuntimeCatalog::new(repository.clone()));
    runtime
        .register_driver(OpenAiCompatibleDriver::for_test(compatible_client))
        .expect("register compatible driver");
    let manager = ProviderManager::new(repository.clone(), runtime.clone());
    manager
        .create_direct_account(
            grant.user.id.as_str(),
            DirectProviderAccountInput {
                kind: ProviderKind::OpenAiCompatible,
                label: "OpenCode".to_owned(),
                group_label: "default".to_owned(),
                priority: 0,
                config_json: json!({
                    "base_url":"https://opencode.ai/zen/go/v1",
                    "upstream_protocol":"chat_completions"
                })
                .to_string(),
                api_key: SecretString::from("upstream-key".to_owned()),
                visibility: ProviderVisibility::Private,
            },
            unix_timestamp(),
        )
        .await
        .expect("create compatible account");
    let api_keys = ApiKeyAuthenticator::load(repository)
        .await
        .expect("API key index");
    let created_key = api_keys
        .create(CreateApiKeyInput {
            owner_user_id: &grant.user.id,
            secret: SecretString::from("test-api-key".to_owned()),
            group_labels: vec!["default".to_owned()],
            label: "test".to_owned(),
            expires_at: None,
            quota_limit_usd: None,
            now: unix_timestamp(),
        })
        .await
        .expect("create API key");
    let service = ProxyService::with_router(runtime, Arc::new(DefaultProtocolBridge));
    let (server_url, server) = spawn(provider_server::router(service, api_keys)).await;

    let request_body = json!({
        "model":"deepseek-v4.1-flash",
        "stream":true,
        "stream_options":{"include_usage":true,"include_obfuscation":false},
        "prompt_cache_key":"codex-session",
        "client_metadata":{"turn_id":42},
        "instructions":"be concise",
        "input":"hello",
        "reasoning":{"effort":"high","summary":"auto"},
        "tools":[{"type":"custom","name":"exec","format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}}]
    })
    .to_string();
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{server_url}/v1/responses"))
        .bearer_auth(created_key.key.expose_secret())
        .header("content-type", "application/json")
        .body(request_body.clone())
        .send()
        .await
        .expect("proxy response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let response = response.text().await.expect("Responses SSE");
    assert!(response.contains("response.reasoning_summary_text.delta"));
    assert!(response.contains("response.output_text.delta"));
    assert!(response.contains("response.completed"));
    assert!(response.contains(r#""input_tokens":4"#));

    let repeated = client
        .post(format!("{server_url}/v1/responses"))
        .bearer_auth(created_key.key.expose_secret())
        .header("content-type", "application/json")
        .body(request_body)
        .send()
        .await
        .expect("repeated proxy response");
    assert_eq!(repeated.status(), reqwest::StatusCode::OK);
    assert!(
        repeated
            .text()
            .await
            .expect("repeated Responses SSE")
            .contains("response.completed")
    );

    let captured = upstream_state.requests.lock().expect("captured requests");
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].path, "/zen/go/v1/chat/completions");
    assert_eq!(captured[0].session, captured[1].session);
    assert!(
        captured[0]
            .session
            .as_deref()
            .is_some_and(|session| session.starts_with("oc_") && !session.contains("codex-session"))
    );
    assert_eq!(captured[0].body["messages"][0]["role"], "developer");
    assert_eq!(captured[0].body["messages"][1]["content"], "hello");
    assert_eq!(captured[0].body["reasoning_effort"], "high");
    assert_eq!(captured[0].body["stream_options"]["include_usage"], true);
    assert_eq!(captured[0].body["tools"][0]["type"], "function");
    assert_eq!(captured[0].body["tools"][0]["function"]["name"], "exec");
    assert_eq!(
        captured[0].body["tools"][0]["function"]["parameters"]["properties"]["input"]["type"],
        "string"
    );
    assert!(
        captured[0].body["tools"][0]["function"]
            .get("format")
            .is_none()
    );
    assert!(captured[0].body.get("prompt_cache_key").is_none());
    assert!(captured[0].body.get("client_metadata").is_none());

    server.abort();
    upstream_server.abort();
}

fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_secs() as i64
}
