use super::{
    ModelResponse, OpenAiCompatibleAccount, OpenAiCompatibleConfig, OpenAiCompatibleDriver,
    OpenAiUpstreamProtocol, normalize_models, opencode_session_header,
    require_event_stream_content_type, sanitize_error_detail, truncate_error_detail,
};
use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::post,
};
use bytes::Bytes;
use provider_core::{
    AccountAuthState, AccountId, ProviderAccount, ProviderErrorKind, ProviderModelInputModality,
    ProviderRequest, RequestMetadata, WireFormat,
};
use secrecy::SecretString;
use serde_json::json;
use tokio::net::TcpListener;

use crate::compatibility::CompatibleCredentials;

#[test]
fn requires_explicit_upstream_protocol() {
    assert!(
        OpenAiCompatibleConfig::parse(r#"{"base_url":"https://api.example.com/v1"}"#,).is_err()
    );
}

#[test]
fn maps_protocol_to_wire_format_and_endpoint() {
    let chat = OpenAiCompatibleConfig::parse(
        r#"{"base_url":"https://api.example.com/v1","upstream_protocol":"chat_completions"}"#,
    )
    .expect("chat config");
    assert_eq!(
        chat.upstream_protocol.wire_format(),
        WireFormat::OpenAiChatCompletions
    );
    assert_eq!(chat.upstream_protocol.endpoint(), "chat/completions");

    let responses = OpenAiCompatibleConfig::parse(
        r#"{"base_url":"https://api.example.com/v1","upstream_protocol":"responses"}"#,
    )
    .expect("responses config");
    assert_eq!(
        responses.upstream_protocol.wire_format(),
        WireFormat::OpenAiResponses
    );
    assert_eq!(responses.upstream_protocol.endpoint(), "responses");
}

#[test]
fn maps_the_opencode_go_endpoint_to_its_pricing_catalog() {
    let config = OpenAiCompatibleConfig::parse(
        r#"{"base_url":"https://opencode.ai/zen/go/v1/","upstream_protocol":"responses"}"#,
    )
    .expect("OpenCode config");
    assert_eq!(config.model_pricing_catalog_provider(), Some("opencode-go"));

    let other = OpenAiCompatibleConfig::parse(
        r#"{"base_url":"https://api.example.com/v1","upstream_protocol":"responses"}"#,
    )
    .expect("generic config");
    assert_eq!(other.model_pricing_catalog_provider(), None);
}

#[test]
fn opencode_go_derives_a_stable_isolated_session_header() {
    let config = OpenAiCompatibleConfig::parse(
        r#"{"base_url":"https://opencode.ai/zen/go/v1","upstream_protocol":"responses"}"#,
    )
    .expect("OpenCode config");
    let account_id = AccountId::new("opencode-account").expect("account ID");
    let mut metadata = RequestMetadata::default();
    metadata.routing_session_id = Some("rs_tenant_session".to_owned());

    let first = opencode_session_header(&config, &account_id, "deepseek-v4.1-flash", &metadata)
        .expect("OpenCode session header");
    let repeated = opencode_session_header(&config, &account_id, "deepseek-v4.1-flash", &metadata)
        .expect("OpenCode session header");
    assert_eq!(first, repeated);
    assert!(first.starts_with("oc_") && !first.contains("rs_tenant_session"));

    metadata.routing_session_id = Some("rs_other_session".to_owned());
    let other = opencode_session_header(&config, &account_id, "deepseek-v4.1-flash", &metadata)
        .expect("OpenCode session header");
    assert_ne!(first, other);

    metadata.opencode_session_id = Some("caller_session".to_owned());
    assert_eq!(
        opencode_session_header(&config, &account_id, "deepseek-v4.1-flash", &metadata).as_deref(),
        Some("caller_session")
    );
}

#[test]
fn opencode_go_always_has_a_session_header() {
    let config = OpenAiCompatibleConfig::parse(
        r#"{"base_url":"https://opencode.ai/zen/go/v1","upstream_protocol":"responses"}"#,
    )
    .expect("OpenCode config");
    let account_id = AccountId::new("opencode-account").expect("account ID");
    let session = opencode_session_header(
        &config,
        &account_id,
        "deepseek-v4.1-flash",
        &RequestMetadata::default(),
    )
    .expect("OpenCode session header");
    assert!(uuid::Uuid::parse_str(&session).is_ok());
}

#[test]
fn generic_accounts_do_not_generate_an_opencode_session_header() {
    let config = OpenAiCompatibleConfig::parse(
        r#"{"base_url":"https://api.example.com/v1","upstream_protocol":"responses"}"#,
    )
    .expect("generic config");
    let account_id = AccountId::new("generic-account").expect("account ID");
    assert_eq!(
        opencode_session_header(
            &config,
            &account_id,
            "test-model",
            &RequestMetadata::default(),
        ),
        None
    );
}

#[test]
fn streaming_response_requires_event_stream_content_type() {
    let mut headers = reqwest::header::HeaderMap::new();
    assert!(require_event_stream_content_type(&headers, 200).is_err());

    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    assert!(require_event_stream_content_type(&headers, 200).is_err());

    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    assert!(require_event_stream_content_type(&headers, 200).is_ok());
}

#[tokio::test]
async fn execute_stream_rejects_successful_json_response() {
    async fn json_response() -> Response<Body> {
        Response::builder()
            .status(StatusCode::OK)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"id":"not-an-sse-stream"}"#))
            .expect("JSON response")
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let address = listener.local_addr().expect("mock upstream address");
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new().route("/v1/responses", post(json_response)),
        )
        .into_future(),
    );
    let driver = OpenAiCompatibleDriver::for_test(reqwest::Client::new());
    let account = OpenAiCompatibleAccount {
        driver,
        account_id: AccountId::new("compatible-test").expect("account ID"),
        credential_revision: 1,
        credential_identity_revision: 0,
        config: OpenAiCompatibleConfig {
            base_url: format!("http://{address}/v1"),
            upstream_protocol: OpenAiUpstreamProtocol::Responses,
        },
        credentials: CompatibleCredentials {
            api_key: SecretString::from("test-key".to_owned()),
        },
        auth_state: AccountAuthState::Active,
        http: tokio::sync::OnceCell::new(),
    };
    let result = account
        .execute_stream(ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "test-model".to_owned(),
            payload: Bytes::from_static(br#"{"model":"test-model","stream":true,"input":"hello"}"#),
            metadata: RequestMetadata::default(),
        })
        .await;
    let error = match result {
        Ok(_) => panic!("JSON success response must not be treated as SSE"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ProviderErrorKind::Upstream);
    assert_eq!(error.upstream_status(), Some(200));
    assert_eq!(
        error.message(),
        "OpenAI-compatible upstream did not return text/event-stream"
    );
    server.abort();
}

#[tokio::test]
async fn execute_stream_forwards_the_opencode_session_header() {
    async fn stream_response(
        State(captured): State<Arc<Mutex<Option<String>>>>,
        headers: HeaderMap,
    ) -> Response<Body> {
        *captured.lock().expect("capture lock") = headers
            .get("x-opencode-session")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        Response::builder()
            .status(StatusCode::OK)
            .header(reqwest::header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from("data: [DONE]\n\n"))
            .expect("stream response")
    }

    let captured = Arc::new(Mutex::new(None));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let address = listener.local_addr().expect("mock upstream address");
    let server = tokio::spawn(
        axum::serve(
            listener,
            Router::new()
                .route("/v1/responses", post(stream_response))
                .with_state(Arc::clone(&captured)),
        )
        .into_future(),
    );
    let account = OpenAiCompatibleAccount {
        driver: OpenAiCompatibleDriver::for_test(reqwest::Client::new()),
        account_id: AccountId::new("compatible-test").expect("account ID"),
        credential_revision: 1,
        credential_identity_revision: 0,
        config: OpenAiCompatibleConfig {
            base_url: format!("http://{address}/v1"),
            upstream_protocol: OpenAiUpstreamProtocol::Responses,
        },
        credentials: CompatibleCredentials {
            api_key: SecretString::from("test-key".to_owned()),
        },
        auth_state: AccountAuthState::Active,
        http: tokio::sync::OnceCell::new(),
    };
    let mut metadata = RequestMetadata::default();
    metadata.opencode_session_id = Some("session_123".to_owned());

    let _stream = account
        .execute_stream(ProviderRequest {
            format: WireFormat::OpenAiResponses,
            model: "test-model".to_owned(),
            payload: Bytes::from_static(br#"{"model":"test-model","stream":true}"#),
            metadata,
        })
        .await
        .expect("stream response");

    assert_eq!(
        captured.lock().expect("capture lock").as_deref(),
        Some("session_123")
    );
    server.abort();
}

#[test]
fn openai_error_objects_surface_their_message() {
    let body = serde_json::to_vec(&json!({
        "error": { "message": "model not found", "type": "invalid_request_error" }
    }))
    .expect("json");
    assert_eq!(
        sanitize_error_detail(&body).as_deref(),
        Some(r#"{"error":{"message":"model not found","type":"invalid_request_error"}}"#)
    );
}

#[test]
fn nested_and_flat_error_shapes_are_accepted() {
    for value in [
        json!({"message":"flat failure"}),
        json!({"error":"string failure"}),
    ] {
        assert_eq!(
            sanitize_error_detail(value.to_string().as_bytes()),
            Some(value.to_string())
        );
    }
    assert!(
        sanitize_error_detail(b"model unavailable: \xff")
            .expect("detail")
            .contains("model unavailable")
    );
}

#[test]
fn error_detail_is_trimmed_and_length_limited() {
    let long = "x".repeat(super::MAX_ERROR_DETAIL_CHARS + 1);
    let truncated = truncate_error_detail(&long);
    assert!(truncated.ends_with("..."));
    assert_eq!(truncated.chars().count(), super::MAX_ERROR_DETAIL_CHARS + 3);
    assert_eq!(
        sanitize_error_detail(b"  hello\nworld  ").as_deref(),
        Some("hello world")
    );
}

#[test]
fn discovered_input_modalities_are_explicit_and_validated() {
    let models = normalize_models(
        vec![ModelResponse {
            id: "vision".to_owned(),
            created: None,
            owned_by: None,
            input_modalities: Some(vec![
                ProviderModelInputModality::Audio,
                ProviderModelInputModality::Pdf,
            ]),
        }],
        "openai_compatible",
    )
    .expect("valid modalities");
    assert_eq!(
        models[0].input_modalities,
        Some(vec![
            ProviderModelInputModality::Audio,
            ProviderModelInputModality::Pdf,
        ])
    );

    assert!(
        normalize_models(
            vec![ModelResponse {
                id: "invalid".to_owned(),
                created: None,
                owned_by: None,
                input_modalities: Some(vec![
                    ProviderModelInputModality::Video,
                    ProviderModelInputModality::Video,
                ]),
            }],
            "openai_compatible",
        )
        .is_err()
    );
}
