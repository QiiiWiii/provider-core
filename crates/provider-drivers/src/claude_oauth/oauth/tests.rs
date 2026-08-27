use super::*;
use axum::{Json, Router, routing::get};
use serde_json::json;

#[test]
fn callback_requires_matching_state_and_extracts_code_fragment() {
    let request = b"GET /callback?code=auth-code%23embedded-state&state=expected HTTP/1.1\r\nHost: localhost\r\n\r\n";
    assert!(matches!(
        parse_callback(request, "expected").expect("callback"),
        ClaudeCallbackResult::Success { code, state }
            if code == "auth-code" && state == "embedded-state"
    ));
    assert!(parse_callback(request, "different").is_err());
    assert!(matches!(
        parse_callback_url(
            "http://localhost:54545/callback?code=auth-code&state=expected",
            "expected"
        )
        .expect("submitted callback"),
        ClaudeCallbackResult::Success { code, state }
            if code == "auth-code" && state == "expected"
    ));
    assert!(
        parse_callback_url(
            "https://attacker.example/callback?code=auth-code&state=expected",
            "expected"
        )
        .is_err()
    );
    assert!(
        parse_callback_url(
            "http://localhost:54545/callback?code=auth-code&state=expected&state=expected",
            "expected"
        )
        .is_err()
    );
}

#[test]
fn callback_error_finishes_authorization_without_waiting_for_code() {
    assert!(matches!(
        parse_callback_url(
            "http://localhost:54545/callback?error=access_denied&error_description=User%20denied&state=expected",
            "expected"
        )
        .expect("error callback"),
        ClaudeCallbackResult::Error { code, description }
            if code == "access_denied" && description.as_deref() == Some("User denied")
    ));
    assert!(
        parse_callback_url(
            "http://localhost:54545/callback?error=access_denied",
            "expected"
        )
        .is_err()
    );
    assert!(
        parse_callback_url(
            "http://localhost:54545/callback?error=access_denied&state=wrong",
            "expected"
        )
        .is_err()
    );
}

#[test]
fn token_exchange_body_matches_cpa_field_order() {
    let body = serde_json::to_string(&AuthorizationCodeRequest {
        grant_type: "authorization_code",
        code: "auth-code",
        redirect_uri: REDIRECT_URI,
        client_id: CLIENT_ID,
        code_verifier: "verifier",
        state: "state",
    })
    .expect("token body");
    assert_eq!(
        body,
        format!(
            r#"{{"grant_type":"authorization_code","code":"auth-code","redirect_uri":"{REDIRECT_URI}","client_id":"{CLIENT_ID}","code_verifier":"verifier","state":"state"}}"#
        )
    );
}

#[tokio::test]
async fn control_plane_headers_match_cpa_axios_profile() {
    let app = Router::new().route(
        "/profile",
        get(|headers: axum::http::HeaderMap| async move {
            assert_eq!(headers["accept"], "application/json, text/plain, */*");
            assert_eq!(headers["content-type"], "application/json");
            assert_eq!(headers["authorization"], "Bearer access-token");
            assert_eq!(headers["cache-control"], "no-cache");
            assert_eq!(headers["user-agent"], "axios/1.15.2");
            assert_eq!(headers["accept-encoding"], "gzip, compress, deflate, br");
            Json(json!({"ok": true}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("profile listener");
    let address = listener.local_addr().expect("profile address");
    let server = tokio::spawn(axum::serve(listener, app).into_future());
    let result: serde_json::Value = control_plane_json(
        &reqwest::Client::new(),
        &format!("http://{address}/profile"),
        "access-token",
        "profile",
    )
    .await
    .expect("profile response");
    assert_eq!(result, json!({"ok": true}));
    server.abort();
}
