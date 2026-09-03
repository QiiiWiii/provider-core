use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use provider_core::ProviderErrorKind;
use secrecy::SecretString;
use serde_json::Value;
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
            "access_token": "model-token",
            "refresh_token": "refresh-token",
            "project_id": "project-1"
        })
        .to_string(),
    ))
    .expect("credentials")
}

fn ids(models: &[DiscoveredProviderModel]) -> Vec<&str> {
    models
        .iter()
        .map(|model| model.upstream_model.as_str())
        .collect()
}

#[test]
fn maps_known_and_new_variants_to_a_base_price_model() {
    assert_eq!(
        model_pricing_alias("claude-opus-4-6-thinking"),
        Some("claude-opus-4-6")
    );
    assert_eq!(
        model_pricing_alias("gemini-3.6-flash-high"),
        Some("gemini-3.6-flash")
    );
    assert_eq!(
        model_pricing_alias("gemini-3.7-flash-high"),
        Some("gemini-3.7-flash")
    );
    assert_eq!(
        model_pricing_alias("gemini-3-flash-agent"),
        Some("gemini-3-flash")
    );
    assert_eq!(
        model_pricing_alias("gemini-3.1-flash-image"),
        Some("gemini-3.1-flash")
    );
    assert_eq!(model_pricing_alias("gemini-pro-agent"), Some("gemini-pro"));
    assert_eq!(
        model_pricing_alias("gemini-3.1-pro-low"),
        Some("gemini-3.1-pro")
    );
    assert_eq!(
        model_pricing_alias("gpt-oss-120b-medium"),
        Some("gpt-oss-120b")
    );
    assert_eq!(
        model_pricing_alias("gemini-3.1-flash-lite"),
        Some("gemini-3.1-flash")
    );
    assert_eq!(
        model_pricing_alias("gemini-3.5-flash-low"),
        Some("gemini-3.5-flash")
    );
    assert_eq!(
        model_pricing_alias("gemini-3.5-flash-extra-low"),
        Some("gemini-3.5-flash")
    );
    assert_eq!(
        model_pricing_alias("gemini-3.8-flash-high"),
        Some("gemini-3.8-flash")
    );
    assert_eq!(
        model_pricing_alias("gemini-3.8-flash-tiered"),
        Some("gemini-3.8-flash")
    );
    assert_eq!(model_pricing_alias("gemini-3.7-flash"), None);
    assert_eq!(model_pricing_alias("gemini-3-flash"), None);
}

#[test]
fn parse_keeps_conversation_models_and_drops_internal_ids() {
    let payload = serde_json::json!({
        "deprecatedModelIds": ["gemini-2.5-pro"],
        "tabModelIds": ["special-tab-model"],
        "models": {
            "gemini-3.8-flash-high": {"displayName": "Gemini 3.8 Flash (High)"},
            "gemini-3-flash": {},
            "tab_flash_lite_preview": {},
            "chat_20706": {},
            "special-tab-model": {},
            "gemini-2.5-pro": {},
            "gemini-3.8-flash-tiered": {},
            "  ": {}
        }
    });
    let models = parse_discovered_models(&payload);
    assert_eq!(ids(&models), ["gemini-3-flash", "gemini-3.8-flash-high"]);
    assert!(models[1].metadata_json.contains("Gemini 3.8 Flash (High)"));
    assert_eq!(
        models[1].input_modalities.as_deref(),
        Some(TEXT_IMAGE_AUDIO_VIDEO)
    );
}

#[test]
fn parse_reads_wrapped_response_models() {
    let payload = serde_json::json!({
        "response": {
            "deprecatedModelIds": ["old"],
            "models": {
                "gemini-3.8-flash-high": {},
                "old": {}
            }
        }
    });
    assert_eq!(
        ids(&parse_discovered_models(&payload)),
        ["gemini-3.8-flash-high"]
    );
}

#[test]
fn parse_drops_legacy_and_tiered_ids() {
    let payload = serde_json::json!({
        "models": {
            "gemini-3.8-flash-high": {},
            "gemini-2.5-flash": {},
            "gemini-3.8-flash-tiered": {}
        }
    });
    assert_eq!(
        ids(&parse_discovered_models(&payload)),
        ["gemini-3.8-flash-high"]
    );
}

#[test]
fn parse_intersects_agent_allowlist_when_present() {
    let payload = serde_json::json!({
        "agentModelSorts": ["gemini-3.8-flash-high"],
        "imageGenerationModelIds": ["gemini-3.1-flash-image"],
        "models": {
            "gemini-3.8-flash-high": {},
            "gemini-3.1-flash-image": {},
            "gemini-3.7-flash-high": {}
        }
    });
    assert_eq!(
        ids(&parse_discovered_models(&payload)),
        ["gemini-3.1-flash-image", "gemini-3.8-flash-high"]
    );
}

#[test]
fn discovered_conversation_models_keep_verified_cloud_code_contract() {
    let payload = serde_json::json!({
        "models": { "gemini-3.8-flash-high": {} }
    });
    let models = parse_discovered_models(&payload);
    assert!(models[0].metadata_json.contains("\"verified\""));
}

#[test]
fn parse_reads_live_agent_sorts_and_object_deprecations() {
    let payload = serde_json::json!({
        "agentModelSorts": [{
            "displayName": "Recommended",
            "groups": [{
                "modelIds": [
                    "gemini-3.8-flash-high",
                    "gemini-3.7-flash-high",
                    "gemini-3-flash"
                ]
            }]
        }],
        "imageGenerationModelIds": ["gemini-3.1-flash-image"],
        "deprecatedModelIds": {"gemini-3.1-pro-high": {"replacedBy": "gemini-pro-agent"}},
        "models": {
            "gemini-3.8-flash-high": {},
            "gemini-3.7-flash-high": {},
            "gemini-3-flash": {},
            "gemini-3.1-flash-image": {},
            "gemini-3.1-pro-high": {},
            "gemini-3.8-flash-tiered": {},
            "tab_flash_lite_preview": {}
        }
    });
    assert_eq!(
        ids(&parse_discovered_models(&payload)),
        [
            "gemini-3-flash",
            "gemini-3.1-flash-image",
            "gemini-3.7-flash-high",
            "gemini-3.8-flash-high"
        ]
    );
}

#[test]
fn infers_modalities_for_unknown_families() {
    assert_eq!(infer_modalities("gpt-oss-120b-high"), TEXT);
    assert_eq!(infer_modalities("claude-opus-4-7"), TEXT_IMAGE);
    assert_eq!(
        infer_modalities("gemini-3.9-flash-high"),
        TEXT_IMAGE_AUDIO_VIDEO
    );
}

#[tokio::test]
async fn discovers_remote_models_and_captures_project_request() {
    let captured = CapturedRequest::default();
    let state = captured.clone();
    let app = Router::new()
        .route(
            FETCH_AVAILABLE_MODELS_PATH,
            post(
                move |State(state): State<CapturedRequest>, headers: HeaderMap, request: Request| async move {
                    let body = to_bytes(request.into_body(), 1 << 20)
                        .await
                        .expect("body");
                    *state.body.lock().expect("body lock") =
                        Some(serde_json::from_slice(&body).expect("request JSON"));
                    *state.headers.lock().expect("headers lock") = headers;
                    (
                        StatusCode::OK,
                        r#"{"deprecatedModelIds":["gemini-2.5-pro"],"models":{"gemini-3.8-flash-high":{"displayName":"Gemini 3.8 Flash (High)"},"tab_flash_lite_preview":{},"gemini-2.5-pro":{}}}"#,
                    )
                        .into_response()
                },
            ),
        )
        .with_state(state);
    let client = AntigravityModelClient::with_base_url(spawn(app).await).expect("client");

    let models = client.discover(&credentials()).await.expect("models");

    assert_eq!(ids(&models), ["gemini-3.8-flash-high"]);
    assert_eq!(
        captured.body.lock().expect("body lock").clone(),
        Some(serde_json::json!({"project": "project-1"}))
    );
    assert_eq!(
        captured
            .headers
            .lock()
            .expect("headers lock")
            .get(reqwest::header::AUTHORIZATION)
            .map(|value| value.to_str().expect("authorization")),
        Some("Bearer model-token")
    );
}

#[tokio::test]
async fn merges_daily_then_production_models() {
    let daily = Router::new().route(
        FETCH_AVAILABLE_MODELS_PATH,
        post(|| async {
            (
                StatusCode::OK,
                r#"{"models":{"gemini-3.8-flash-high":{},"gemini-3-flash":{}}}"#,
            )
        }),
    );
    let production = Router::new().route(
        FETCH_AVAILABLE_MODELS_PATH,
        post(|| async {
            (
                StatusCode::OK,
                r#"{"models":{"gemini-3.7-flash-high":{},"gemini-3-flash":{}}}"#,
            )
        }),
    );
    let client =
        AntigravityModelClient::with_base_urls([spawn(daily).await, spawn(production).await])
            .expect("client");

    let models = client.discover(&credentials()).await.expect("models");
    assert_eq!(
        ids(&models),
        [
            "gemini-3-flash",
            "gemini-3.7-flash-high",
            "gemini-3.8-flash-high"
        ]
    );
}

#[tokio::test]
async fn classifies_rate_limited_model_discovery() {
    let app = Router::new().route(
        FETCH_AVAILABLE_MODELS_PATH,
        post(|| async { StatusCode::TOO_MANY_REQUESTS }),
    );
    let client = AntigravityModelClient::with_base_url(spawn(app).await).expect("client");
    let error = client.discover(&credentials()).await.expect_err("429");
    assert_eq!(error.kind(), ProviderErrorKind::RateLimited);
    assert_eq!(error.upstream_status(), Some(429));
}
