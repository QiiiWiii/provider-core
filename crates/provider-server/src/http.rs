use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
#[cfg(test)]
use axum::{
    body::{Body, Bytes},
    http::HeaderMap,
};
use provider_auth::{ApiKeyAuthenticator, AuthService};
#[cfg(test)]
use provider_auth::{ApiKeyPatch, AuthenticatedApiKey, CreateApiKeyInput};
use provider_core::ProxyService;
#[cfg(test)]
use provider_core::{ProviderError, ProviderErrorKind, WireFormat};
use provider_management::ProviderManager;
use provider_usage::UsageTracking;
use serde_json::{Value, json};

mod claude_code;
mod error;
mod models;
mod proxy;
mod readiness;
mod request;
mod static_ui;
mod tracking;

use error::HttpError;
use models::models;
#[cfg(test)]
use models::models_protocol;
#[cfg(test)]
use proxy::require_stream_true;
use proxy::{chat_completions, count_tokens, messages, responses};
#[cfg(test)]
use request::proxy_request_for_key_from_payload;
#[cfg(test)]
use request::{
    CLAUDE_CODE_SESSION_HEADER, claude_code_cache_key, claude_code_session_id, proxy_request,
    responses_cache_key, unix_timestamp,
};
use static_ui::ui_service;

const PUBLIC_DIR: &str = "/app/public";
pub(crate) const MAX_PROXY_BODY_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_MANAGEMENT_BODY_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
struct AppState {
    service: ProxyService,
    api_keys: ApiKeyAuthenticator,
    /// `None` disables usage tracking entirely, which is how every path stays
    /// working when there is no database to record into.
    usage: Option<Arc<UsageTracking>>,
    proxy_readiness: ProxyReadiness,
}
#[derive(Clone)]
pub struct ProxyReadiness(Arc<AtomicBool>);

pub(crate) struct ManagementRouterConfig {
    pub(crate) usage: Option<crate::usage_http::UsageServices>,
    pub(crate) trusted_proxy_ip: Option<std::net::IpAddr>,
    pub(crate) proxy_readiness: ProxyReadiness,
}

impl ProxyReadiness {
    pub fn new(ready: bool) -> Self {
        Self(Arc::new(AtomicBool::new(ready)))
    }

    pub(crate) fn signal(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }

    fn get(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub fn router(service: ProxyService, api_keys: ApiKeyAuthenticator) -> Router {
    router_with_usage(service, api_keys, None)
}

pub fn router_with_usage(
    service: ProxyService,
    api_keys: ApiKeyAuthenticator,
    usage: Option<Arc<UsageTracking>>,
) -> Router {
    router_with_usage_and_readiness(service, api_keys, usage, ProxyReadiness::new(true))
}

fn router_with_usage_and_readiness(
    service: ProxyService,
    api_keys: ApiKeyAuthenticator,
    usage: Option<Arc<UsageTracking>>,
    proxy_readiness: ProxyReadiness,
) -> Router {
    Router::new()
        .route("/healthz", get(liveness))
        .route("/livez", get(liveness))
        .route("/readyz", get(readiness))
        .route("/v1/models", get(models))
        .route("/v1/responses", post(responses))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .layer(DefaultBodyLimit::max(MAX_PROXY_BODY_BYTES))
        .layer(middleware::from_fn(reject_compressed_request))
        .with_state(AppState {
            service,
            api_keys,
            usage,
            proxy_readiness,
        })
        .fallback_service(ui_service(PUBLIC_DIR))
}

pub fn router_with_management(
    service: ProxyService,
    manager: ProviderManager,
    auth: AuthService,
    api_keys: ApiKeyAuthenticator,
) -> Router {
    router_with_management_and_usage(service, manager, auth, api_keys, None, None)
}

pub fn router_with_management_and_usage(
    service: ProxyService,
    manager: ProviderManager,
    auth: AuthService,
    api_keys: ApiKeyAuthenticator,
    usage: Option<crate::usage_http::UsageServices>,
    trusted_proxy_ip: Option<std::net::IpAddr>,
) -> Router {
    router_with_management_usage_and_readiness(
        service,
        manager,
        auth,
        api_keys,
        ManagementRouterConfig {
            usage,
            trusted_proxy_ip,
            proxy_readiness: ProxyReadiness::new(true),
        },
    )
}

pub(crate) fn router_with_management_usage_and_readiness(
    service: ProxyService,
    manager: ProviderManager,
    auth: AuthService,
    api_keys: ApiKeyAuthenticator,
    config: ManagementRouterConfig,
) -> Router {
    let ManagementRouterConfig {
        usage,
        trusted_proxy_ip,
        proxy_readiness,
    } = config;
    let auth_state = crate::auth_http::AuthHttpState::new(
        auth.clone(),
        api_keys.clone(),
        manager.clone(),
        trusted_proxy_ip,
    );
    let mut management = crate::management_http::router(manager, usage.clone());
    if let Some(usage) = &usage {
        // Behind the same session guard as the rest of management: usage is read
        // by a logged-in person, never with a proxy API key.
        management = management.merge(crate::usage_http::router(usage.clone()));
    }
    let management = crate::auth_http::protect(management, auth)
        .layer(DefaultBodyLimit::max(MAX_MANAGEMENT_BODY_BYTES))
        .layer(middleware::from_fn(reject_compressed_request));
    router_with_usage_and_readiness(
        service,
        api_keys,
        usage.map(|usage| usage.tracking),
        proxy_readiness,
    )
    .merge(crate::auth_http::router(auth_state))
    .merge(management)
}

pub(crate) async fn reject_compressed_request(request: Request, next: Next) -> Response {
    let compressed = request
        .headers()
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|value| {
            value.to_str().map_or(true, |value| {
                value
                    .split(',')
                    .map(str::trim)
                    .any(|encoding| !encoding.eq_ignore_ascii_case("identity"))
            })
        });
    if compressed {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(json!({
                "error": {
                    "type": "invalid_request_error",
                    "message": "compressed request bodies are not supported"
                }
            })),
        )
            .into_response();
    }
    next.run(request).await
}

async fn liveness() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn readiness(State(state): State<AppState>) -> Response {
    let database_ready = state.api_keys.quota_ledger_ready().await.is_ok();
    let writer_ready = state
        .usage
        .as_ref()
        .is_none_or(|usage| usage.quota_ledger_ready());
    let providers_ready = state.proxy_readiness.get();
    let ready = database_ready && writer_ready && providers_ready;
    (
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(json!({
            "status": if ready { "ready" } else { "not_ready" },
            "database": database_ready,
            "quota_ledger": writer_ready,
            "providers": providers_ready
        })),
    )
        .into_response()
}
#[cfg(test)]
#[path = "http/claude_code_flow_tests.rs"]
mod claude_code_flow_tests;
#[cfg(test)]
#[path = "http/tests.rs"]
mod tests;
