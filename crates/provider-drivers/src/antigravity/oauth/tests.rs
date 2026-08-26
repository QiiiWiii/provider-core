use axum::{Router, body::to_bytes, extract::Request, http::StatusCode, routing::post};
use tokio::net::TcpListener;

use super::*;

async fn spawn(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(axum::serve(listener, router).into_future());
    format!("http://{address}")
}

#[test]
fn accepts_local_callback_and_validates_state() {
    let code = parse_callback_url(
        "http://localhost:51121/oauth-callback?code=auth-code&state=state-1",
        "state-1",
    )
    .expect("callback");
    assert_eq!(code, "auth-code");
}

#[test]
fn rejects_callback_with_wrong_origin_or_duplicate_state() {
    assert!(
        parse_callback_url(
            "https://localhost:51121/oauth-callback?code=auth-code&state=state-1",
            "state-1",
        )
        .is_err()
    );
    assert!(
        parse_callback_url(
            "http://localhost:51121/oauth-callback?code=auth-code&state=state-1&state=state-1",
            "state-1",
        )
        .is_err()
    );
}

#[tokio::test]
async fn discovers_project_and_polls_onboarding_with_cpa_headers() {
    let app = Router::new()
        .route(
            "/api/v1internal:loadCodeAssist",
            post(|request: Request| async move {
                assert_eq!(
                    request
                        .headers()
                        .get("authorization")
                        .and_then(|value| value.to_str().ok()),
                    Some("Bearer access")
                );
                assert_eq!(
                    request
                        .headers()
                        .get("user-agent")
                        .and_then(|value| value.to_str().ok()),
                    Some(version::fallback_user_agent())
                );
                (
                    StatusCode::OK,
                    r#"{"allowedTiers":[{"id":"free-tier","isDefault":true}]}"#,
                )
            }),
        )
        .route(
            "/daily/v1internal:onboardUser",
            post(|request: Request| async move {
                assert_eq!(
                    request
                        .headers()
                        .get("x-goog-api-client")
                        .and_then(|value| value.to_str().ok()),
                    Some(GOOG_API_CLIENT)
                );
                assert_eq!(
                    request
                        .headers()
                        .get("user-agent")
                        .and_then(|value| value.to_str().ok()),
                    Some(version::fallback_onboard_user_agent())
                );
                let body = to_bytes(request.into_body(), 4096).await.expect("body");
                let body: Value = serde_json::from_slice(&body).expect("body JSON");
                assert_eq!(body["metadata"]["ide_version"], version::fallback_version());
                (
                    StatusCode::OK,
                    r#"{"done":true,"response":{"cloudaicompanionProject":{"id":"project-a"}}}"#,
                )
            }),
        );
    let base = spawn(app).await;
    let client = AntigravityOAuthClient::for_test(base);
    assert_eq!(
        client.fetch_project_id("access").await.expect("project ID"),
        "project-a"
    );
}
