use axum::http::StatusCode;

use provider_core::WireFormat;

use super::{AppState, HttpError};

pub(super) fn ensure_proxy_ready(state: &AppState, protocol: WireFormat) -> Result<(), HttpError> {
    if state.proxy_readiness.get() {
        Ok(())
    } else {
        Err(HttpError::new(
            protocol,
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            "provider runtime recovery is incomplete",
        ))
    }
}
