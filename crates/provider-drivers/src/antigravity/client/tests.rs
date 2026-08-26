use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{Response, StatusCode},
    response::IntoResponse,
    routing::post,
};
use futures_util::StreamExt;
use provider_core::{
    ProviderErrorKind, ProviderFailoverReason, ProviderRequest, RequestMetadata, WireFormat,
};
use secrecy::SecretString;
use serde_json::Value;
use tokio::net::TcpListener;

use super::*;

#[derive(Clone, Default)]
struct Calls(Arc<Mutex<Vec<String>>>);

#[derive(Clone, Default)]
struct FallbackCalls {
    daily: Arc<Mutex<usize>>,
    production: Arc<Mutex<Vec<Value>>>,
}

async fn cloud_code(State(calls): State<Calls>, request: Request) -> Response<Body> {
    calls
        .0
        .lock()
        .expect("calls lock")
        .push(request.uri().path().to_owned());
    if request.uri().path().ends_with(":countTokens") {
        return Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(r#"{"totalTokens":42}"#))
            .expect("count response");
    }
    (
        StatusCode::OK,
        [("content-type", "text/event-stream")],
        "data: {\"response\":{\"responseId\":\"native-1\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}}\n\n",
    )
        .into_response()
}

async fn spawn(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(axum::serve(listener, router).into_future());
    format!("http://{address}")
}

#[tokio::test]
async fn falls_back_from_daily_inference_to_production_and_preserves_cpa_body() {
    let calls = FallbackCalls::default();
    let daily_calls = calls.daily.clone();
    let production_calls = calls.production.clone();
    let app = Router::new().route(
        "/{*path}",
        post(move |request: Request| {
            let daily_calls = daily_calls.clone();
            let production_calls = production_calls.clone();
            async move {
                let path = request.uri().path().to_owned();
                let body = to_bytes(request.into_body(), 1 << 20)
                    .await
                    .expect("body");
                if path.starts_with("/daily-") {
                    *daily_calls.lock().expect("daily lock") += 1;
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [("content-type", "application/json")],
                        r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"quota"}}"#,
                    )
                        .into_response()
                } else {
                    production_calls
                        .lock()
                        .expect("production lock")
                        .push(serde_json::from_slice(&body).expect("request JSON"));
                    (
                        StatusCode::OK,
                        [("content-type", "text/event-stream")],
                        "data: {\"response\":{\"responseId\":\"native-1\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}}\n\n",
                    )
                        .into_response()
                }
            }
        }),
    );
    let base = spawn(app).await;
    let client = AntigravityClient::with_base_urls([
        format!("{base}/daily-"),
        format!("{base}/production-"),
    ])
    .expect("client");
    let credentials = AntigravityCredentials::from_json(&SecretString::from(
        serde_json::json!({
            "type": "antigravity",
            "access_token": "access",
            "refresh_token": "refresh",
            "project_id": "project-1"
        })
        .to_string(),
    ))
    .expect("credentials");
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3-flash".to_owned(),
        payload: bytes::Bytes::from_static(br#"{"input":"hello"}"#),
        metadata: RequestMetadata::default(),
    };
    let mut stream = client
        .execute_stream(&credentials, &request)
        .await
        .expect("fallback stream");
    while stream.next().await.is_some() {}
    assert_eq!(*calls.daily.lock().expect("daily lock"), 1);
    let production = calls.production.lock().expect("production lock");
    assert_eq!(production.len(), 1);
    assert_eq!(production[0]["project"], "project-1");
    assert_eq!(production[0]["requestType"], "agent");
    assert!(
        production[0]["requestId"]
            .as_str()
            .is_some_and(|value| value.starts_with("agent-"))
    );
}

#[tokio::test]
async fn counts_tokens_through_cpa_endpoint_and_falls_back_to_production() {
    let calls = FallbackCalls::default();
    let daily_calls = calls.daily.clone();
    let production_calls = calls.production.clone();
    let app = Router::new().route(
        "/{*path}",
        post(move |request: Request| {
            let daily_calls = daily_calls.clone();
            let production_calls = production_calls.clone();
            async move {
                let path = request.uri().path().to_owned();
                let body = to_bytes(request.into_body(), 1 << 20).await.expect("body");
                if path.starts_with("/daily-") {
                    *daily_calls.lock().expect("daily lock") += 1;
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [("content-type", "application/json")],
                        r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"quota"}}"#,
                    )
                        .into_response()
                } else {
                    production_calls
                        .lock()
                        .expect("production lock")
                        .push(serde_json::from_slice(&body).expect("request JSON"));
                    (
                        StatusCode::OK,
                        [("content-type", "application/json")],
                        r#"{"totalTokens":42}"#,
                    )
                        .into_response()
                }
            }
        }),
    );
    let base = spawn(app).await;
    let client = AntigravityClient::with_base_urls([
        format!("{base}/daily-"),
        format!("{base}/production-"),
    ])
    .expect("client");
    let credentials = AntigravityCredentials::from_json(&SecretString::from(
        serde_json::json!({
            "type": "antigravity",
            "access_token": "access",
            "refresh_token": "refresh",
            "project_id": "project-1"
        })
        .to_string(),
    ))
    .expect("credentials");
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3-flash".to_owned(),
        payload: bytes::Bytes::from_static(br#"{"input":"hello"}"#),
        metadata: RequestMetadata::default(),
    };
    let count = client
        .count_tokens(&credentials, &request)
        .await
        .expect("count tokens");
    assert_eq!(count, 42);
    assert_eq!(*calls.daily.lock().expect("daily lock"), 1);
    let production = calls.production.lock().expect("production lock");
    assert_eq!(production.len(), 1);
    assert!(production[0].get("project").is_none());
    assert!(production[0].get("model").is_none());
    assert!(production[0].get("requestType").is_none());
    assert!(production[0]["request"].get("sessionId").is_none());
    assert_eq!(production[0]["request"]["contents"][0]["role"], "user");
}

#[tokio::test]
async fn uses_one_daily_endpoint_for_inference_and_count_tokens() {
    let calls = Calls::default();
    let app = Router::new()
        .route("/{*path}", post(cloud_code))
        .with_state(calls.clone());
    let base = spawn(app).await;
    let client = AntigravityClient::with_base_url(base).expect("client");
    let credentials = AntigravityCredentials::from_json(&SecretString::from(
        serde_json::json!({
            "type": "antigravity",
            "access_token": "access",
            "refresh_token": "refresh",
            "project_id": "project-1"
        })
        .to_string(),
    ))
    .expect("credentials");
    let request = ProviderRequest {
        format: WireFormat::OpenAiResponses,
        model: "gemini-3-flash".to_owned(),
        payload: bytes::Bytes::from_static(br#"{"input":"hello"}"#),
        metadata: RequestMetadata::default(),
    };
    let mut stream = client
        .execute_stream(&credentials, &request)
        .await
        .expect("stream");
    while stream.next().await.is_some() {}
    assert_eq!(
        client
            .count_tokens(&credentials, &request)
            .await
            .expect("count tokens"),
        42
    );
    let paths = calls.0.lock().expect("calls lock").clone();
    assert_eq!(
        paths,
        vec![
            "/v1internal:streamGenerateContent",
            "/v1internal:countTokens"
        ]
    );
}

#[tokio::test]
async fn classifies_cpa_http_errors_for_routing() {
    let app = Router::new()
        .route(
            "/quota",
            post(|| async {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    r#"{"error":{"status":"RESOURCE_EXHAUSTED","message":"quota"}}"#,
                )
            }),
        )
        .route(
            "/auth",
            post(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    r#"{"error":{"status":"UNAUTHENTICATED","message":"expired"}}"#,
                )
            }),
        );
    let base = spawn(app).await;
    let quota_response = reqwest::Client::new()
        .post(format!("{base}/quota"))
        .send()
        .await
        .expect("quota response");
    let quota = status_error(quota_response, StatusCode::TOO_MANY_REQUESTS).await;
    assert_eq!(quota.kind(), ProviderErrorKind::Capacity);
    assert_eq!(
        quota.failover_reason(),
        Some(ProviderFailoverReason::QuotaExhausted)
    );
    let auth_response = reqwest::Client::new()
        .post(format!("{base}/auth"))
        .send()
        .await
        .expect("auth response");
    let auth = status_error(auth_response, StatusCode::UNAUTHORIZED).await;
    assert_eq!(auth.kind(), ProviderErrorKind::Authentication);
    assert_eq!(
        auth.failover_reason(),
        Some(ProviderFailoverReason::AuthenticationExhausted)
    );
}
