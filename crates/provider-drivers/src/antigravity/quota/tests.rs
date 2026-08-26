use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use provider_core::{QuotaAmount, QuotaGroupScope, QuotaPeriodKind};
use secrecy::SecretString;
use tokio::net::TcpListener;

use super::*;

#[derive(Clone, Default)]
struct CapturedRequest {
    body: Arc<Mutex<Option<Value>>>,
    headers: Arc<Mutex<HeaderMap>>,
}

async fn spawn(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(axum::serve(listener, router).into_future());
    format!("http://{address}")
}

fn credentials() -> AntigravityCredentials {
    AntigravityCredentials::from_json(&SecretString::from(
        serde_json::json!({
            "type": "antigravity",
            "access_token": "quota-token",
            "refresh_token": "refresh-token",
            "project_id": "project-1"
        })
        .to_string(),
    ))
    .expect("credentials")
}

#[tokio::test]
async fn fetches_and_aggregates_model_quotas() {
    let captured = CapturedRequest::default();
    let state = captured.clone();
    let app = Router::new()
            .route(
                FETCH_AVAILABLE_MODELS_PATH,
                post(
                    move |State(state): State<CapturedRequest>,
                          headers: HeaderMap,
                          request: Request| async move {
                        let body = to_bytes(request.into_body(), 1 << 20)
                            .await
                            .expect("body");
                        *state.body.lock().expect("body lock") =
                            Some(serde_json::from_slice(&body).expect("request JSON"));
                        *state.headers.lock().expect("headers lock") = headers;
                        (
                            StatusCode::OK,
                            r#"{"models":{"gemini-3-pro":{"displayName":"Gemini Pro","quotaInfo":{"remainingFraction":0.75}},"gemini-3-flash":{"quotaInfo":{"remainingFraction":0.5}},"claude-sonnet-4":{"quotaInfo":{"remainingFraction":0.9}},"gpt-oss-120b":{"quotaInfo":{"remainingFraction":0.8}}}}"#,
                        )
                            .into_response()
                    },
                ),
            )
            .with_state(state);
    let client = AntigravityQuotaClient::with_base_url(spawn(app).await).expect("quota client");

    let snapshot = client
        .fetch("account-1", &credentials())
        .await
        .expect("quota snapshot");

    assert_eq!(snapshot.groups.len(), 1);
    assert_eq!(snapshot.groups[0].scope, QuotaGroupScope::Aggregate);
    assert_eq!(snapshot.groups[0].metrics.len(), 2);
    assert_eq!(snapshot.groups[0].metrics[0].key, "gemini");
    assert_eq!(
        snapshot.groups[0].metrics[0].remaining,
        Some(QuotaAmount::Decimal(50.0))
    );
    assert_eq!(snapshot.groups[0].metrics[1].key, "claude_gpt");
    assert_eq!(
        snapshot.groups[0].metrics[1].remaining,
        Some(QuotaAmount::Decimal(80.0))
    );
    let body = captured
        .body
        .lock()
        .expect("body lock")
        .clone()
        .expect("body");
    assert_eq!(body, serde_json::json!({"project":"project-1"}));
    let headers = captured.headers.lock().expect("headers lock");
    assert_eq!(
        headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer quota-token")
    );
}

#[test]
fn accepts_proto_remaining_oneof_and_reset_time() {
    let quotas = parse_available_model_quotas(&serde_json::json!({
        "models": {
            "claude-sonnet-4": {
                "quotaInfo": {
                    "remaining": {"case": "remainingFraction", "value": 0.25},
                    "resetTime": "2026-01-01T00:00:00Z"
                }
            }
        }
    }))
    .expect("models");
    let snapshot = normalize_quota("account-1", quotas).expect("snapshot");
    assert_eq!(
        snapshot.groups[0].metrics[0]
            .period
            .as_ref()
            .map(|period| period.kind),
        Some(QuotaPeriodKind::Unknown)
    );
    assert_eq!(
        snapshot.groups[0].metrics[0].remaining,
        Some(QuotaAmount::Decimal(25.0))
    );
}
