use axum::{
    Json,
    body::Bytes,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use provider_core::{ProviderError, ProviderErrorKind, ProxyRequestError, WireFormat};
use serde_json::{Value, json};

pub(super) struct HttpError {
    status: StatusCode,
    body: Value,
    raw_body: Option<Bytes>,
    retry_after: Option<std::time::Duration>,
}

impl HttpError {
    pub(super) fn authentication(protocol: WireFormat) -> Self {
        Self::new(
            protocol,
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid API key",
        )
    }

    pub(super) fn invalid_request(protocol: WireFormat, message: &'static str) -> Self {
        Self::new(
            protocol,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            message,
        )
    }

    pub(super) fn service_unavailable(protocol: WireFormat, message: &'static str) -> Self {
        Self::new(
            protocol,
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            message,
        )
    }

    pub(super) fn internal(protocol: WireFormat) -> Self {
        Self::new(
            protocol,
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            "internal server error",
        )
    }

    pub(super) fn rate_limited(protocol: WireFormat, message: &str) -> Self {
        Self::new(
            protocol,
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            message,
        )
    }

    pub(super) fn from_proxy_request(protocol: WireFormat, error: ProxyRequestError) -> Self {
        Self::invalid_request(
            protocol,
            match error {
                ProxyRequestError::EmptyModel => "model must be a non-empty string",
            },
        )
    }

    pub(super) fn from_provider(protocol: WireFormat, error: ProviderError) -> Self {
        let (status, error_type) = match error.kind() {
            ProviderErrorKind::InvalidRequest => (StatusCode::BAD_REQUEST, "invalid_request_error"),
            ProviderErrorKind::Authentication if error.upstream_status() == Some(403) => {
                (StatusCode::FORBIDDEN, "permission_error")
            }
            ProviderErrorKind::Authentication => (StatusCode::UNAUTHORIZED, "authentication_error"),
            ProviderErrorKind::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error"),
            ProviderErrorKind::Capacity => (StatusCode::SERVICE_UNAVAILABLE, "api_error"),
            ProviderErrorKind::Upstream => (StatusCode::BAD_GATEWAY, "api_error"),
            ProviderErrorKind::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "api_error"),
        };
        if protocol == WireFormat::ClaudeMessages
            && let Some(upstream_status) = error
                .upstream_status()
                .and_then(|status| StatusCode::from_u16(status).ok())
        {
            if let Some(body) = error.upstream_body().filter(|body| !body.is_empty()) {
                return Self {
                    status: upstream_status,
                    body: Value::Null,
                    raw_body: Some(body.clone()),
                    retry_after: error.retry_after(),
                };
            }
            return Self::new(protocol, upstream_status, error_type, error.message())
                .with_retry_after(error.retry_after());
        }
        Self::new(protocol, status, error_type, error.message())
            .with_retry_after(error.retry_after())
    }

    pub(super) fn new(
        protocol: WireFormat,
        status: StatusCode,
        error_type: &str,
        message: &str,
    ) -> Self {
        let body = match protocol {
            WireFormat::OpenAiResponses | WireFormat::OpenAiChatCompletions => json!({
                "error": { "type": error_type, "message": message }
            }),
            WireFormat::ClaudeMessages => json!({
                "type": "error",
                "error": { "type": error_type, "message": message }
            }),
        };
        Self {
            status,
            body,
            raw_body: None,
            retry_after: None,
        }
    }

    pub(super) fn with_retry_after(mut self, retry_after: Option<std::time::Duration>) -> Self {
        self.retry_after = retry_after;
        self
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let mut response = match self.raw_body {
            Some(body) => (
                self.status,
                [(header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response(),
            None => (self.status, Json(self.body)).into_response(),
        };
        if let Some(retry_after) = self.retry_after {
            let seconds = retry_after
                .as_secs()
                .saturating_add(u64::from(retry_after.subsec_nanos() > 0));
            if let Ok(value) = seconds.to_string().parse() {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}
