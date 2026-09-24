use provider_core::{BoundedBodyError, ProviderError, collect_bounded_body};
use std::time::Duration;

pub(crate) async fn read_detail(response: reqwest::Response) -> String {
    read_detail_with_timeout(response, Duration::from_secs(10)).await
}

async fn read_detail_with_timeout(response: reqwest::Response, timeout: Duration) -> String {
    match tokio::time::timeout(
        timeout,
        collect_bounded_body(response.bytes_stream(), 64 * 1024),
    )
    .await
    {
        Ok(Ok(body)) => render_detail(&body),
        Ok(Err(BoundedBodyError::Read(_))) => "[upstream error body could not be read]".to_owned(),
        Ok(Err(BoundedBodyError::TooLarge)) => {
            "[upstream error body exceeds 65536 bytes]".to_owned()
        }
        Err(_) => "[upstream error body read timed out]".to_owned(),
    }
}

pub(crate) fn render_detail(body: &[u8]) -> String {
    if body.is_empty() {
        return "[empty upstream error body]".to_owned();
    }
    if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(body) {
        redact_fields(&mut value);
        return value.to_string();
    }
    String::from_utf8_lossy(body)
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

fn redact_fields(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                let key = key.to_ascii_lowercase().replace('-', "_");
                if matches!(
                    key.as_str(),
                    "authorization"
                        | "proxy_authorization"
                        | "cookie"
                        | "set_cookie"
                        | "api_key"
                        | "x_api_key"
                        | "access_token"
                        | "refresh_token"
                        | "id_token"
                        | "client_secret"
                        | "password"
                ) {
                    *value = serde_json::Value::String("[REDACTED]".to_owned());
                } else {
                    redact_fields(value);
                }
            }
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(redact_fields),
        _ => {}
    }
}

pub(crate) fn redact_credentials(error: ProviderError, secrets: &[&str]) -> ProviderError {
    let mut message = error.message().to_owned();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        let escaped = serde_json::to_string(secret).expect("string serialization");
        message = message.replace(&escaped[1..escaped.len() - 1], "[REDACTED]");
        message = message.replace(secret, "[REDACTED]");
    }
    error.with_message(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stalled_error_body_has_a_total_deadline() {
        let router = axum::Router::new().route(
            "/",
            axum::routing::get(|| async {
                let stream =
                    futures_util::stream::pending::<Result<bytes::Bytes, std::io::Error>>();
                axum::http::Response::builder()
                    .status(429)
                    .body(axum::body::Body::from_stream(stream))
                    .expect("response")
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let response = reqwest::get(format!("http://{address}/"))
            .await
            .expect("headers");
        assert_eq!(response.status().as_u16(), 429);
        let detail = read_detail_with_timeout(response, Duration::from_millis(30)).await;
        server.abort();
        assert!(detail.contains("timed out"));
    }

    #[test]
    fn preserves_error_fields_and_long_messages() {
        let value = serde_json::json!({"error": {
            "message": "detail".repeat(300), "code": "model_not_found",
            "type": "invalid_request_error", "param": "model"
        }});
        assert_eq!(
            render_detail(value.to_string().as_bytes()),
            value.to_string()
        );
        assert_eq!(render_detail(b"gateway unavailable"), "gateway unavailable");
        assert_eq!(render_detail(b""), "[empty upstream error body]");
    }

    #[test]
    fn redacts_credentials_in_json_text_and_html_without_losing_reason() {
        let escaped_secret = "secret\"with\\escaping";
        let value = serde_json::json!({"message": format!("invalid {escaped_secret}")});
        let error = ProviderError::new(
            provider_core::ProviderErrorKind::Authentication,
            render_detail(value.to_string().as_bytes()),
        );
        let error = redact_credentials(error, &[escaped_secret]);
        assert!(error.message().contains("[REDACTED]"));
        assert!(!error.message().contains("escaping"));
        for body in [
            r#"{"error":{"message":"invalid key fake-secret"},"debug":{"authorization":"Bearer other-secret"}}"#,
            "invalid key fake-secret",
            "<html>invalid key fake-secret</html>",
        ] {
            let error = ProviderError::new(
                provider_core::ProviderErrorKind::Authentication,
                render_detail(body.as_bytes()),
            )
            .with_upstream_status(401);
            let error = redact_credentials(error, &["fake-secret"]);
            assert!(!error.message().contains("fake-secret"));
            assert!(!error.message().contains("other-secret"));
            assert!(error.message().contains("invalid key"));
            assert_eq!(error.upstream_status(), Some(401));
        }
        assert!(render_detail(b"model unavailable: \xff").contains("model unavailable"));
    }
}
