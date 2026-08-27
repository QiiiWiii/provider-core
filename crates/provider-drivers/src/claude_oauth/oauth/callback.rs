use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use provider_core::{ProviderConfigurationError, ProviderOAuthCallback};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
    time::{Instant, timeout_at},
};

pub(super) struct ClaudeCallbackSubmitter {
    pub(super) callback_tx: mpsc::Sender<ClaudeCallbackResult>,
    pub(super) state: String,
}

pub(super) enum ClaudeCallbackResult {
    Success {
        code: String,
        state: String,
    },
    Error {
        code: String,
        description: Option<String>,
    },
}

impl ProviderOAuthCallback for ClaudeCallbackSubmitter {
    fn submit(&self, callback_url: &str) -> Result<(), ProviderConfigurationError> {
        let callback = parse_callback_url(callback_url, &self.state)?;
        self.callback_tx
            .try_send(callback)
            .map_err(|_| ProviderConfigurationError::new("Claude OAuth callback is unavailable"))
    }
}

pub(super) async fn write_callback_response(stream: &mut TcpStream, accepted: bool) {
    let (status, response_body) = if accepted {
        (
            "200 OK",
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Claude OAuth complete</title></head><body><main><h1>Claude OAuth authorization complete</h1><p>You may close this window.</p></main><script>window.close()</script></body></html>",
        )
    } else {
        (
            "400 Bad Request",
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Claude OAuth failed</title></head><body><main><h1>Claude OAuth authorization failed</h1><p>You may close this window and return to the provider setup.</p></main></body></html>",
        )
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{response_body}",
        response_body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

pub(super) async fn read_callback_request(
    stream: &mut TcpStream,
    deadline: Instant,
) -> Result<Vec<u8>, ProviderConfigurationError> {
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 512];
    loop {
        let read = timeout_at(deadline, stream.read(&mut chunk))
            .await
            .map_err(|_| ProviderConfigurationError::new("Claude OAuth authorization expired"))?
            .map_err(|_| ProviderConfigurationError::new("Claude OAuth callback failed"))?;
        if read == 0 || request.len().saturating_add(read) > 16 * 1024 {
            return Err(ProviderConfigurationError::new(
                "Claude OAuth callback is invalid",
            ));
        }
        request.extend_from_slice(&chunk[..read]);
        if request.contains(&b'\n') {
            return Ok(request);
        }
    }
}

pub(super) fn parse_callback(
    request: &[u8],
    expected_state: &str,
) -> Result<ClaudeCallbackResult, ProviderConfigurationError> {
    let line = std::str::from_utf8(request)
        .ok()
        .and_then(|request| request.lines().next())
        .ok_or_else(|| ProviderConfigurationError::new("Claude OAuth callback is invalid"))?;
    let mut request_line = line.split_whitespace();
    if request_line.next() != Some("GET") {
        return Err(ProviderConfigurationError::new(
            "Claude OAuth callback is invalid",
        ));
    }
    let path = request_line
        .next()
        .ok_or_else(|| ProviderConfigurationError::new("Claude OAuth callback is invalid"))?;
    if request_line.next() != Some("HTTP/1.1") || request_line.next().is_some() {
        return Err(ProviderConfigurationError::new(
            "Claude OAuth callback is invalid",
        ));
    }
    let url = reqwest::Url::parse(&format!("http://localhost{path}"))
        .map_err(|_| ProviderConfigurationError::new("Claude OAuth callback is invalid"))?;
    if url.path() != "/callback" {
        return Err(ProviderConfigurationError::new(
            "Claude OAuth callback is invalid",
        ));
    }
    parse_callback_parameters(&url, expected_state)
}

pub(super) fn parse_callback_url(
    callback_url: &str,
    expected_state: &str,
) -> Result<ClaudeCallbackResult, ProviderConfigurationError> {
    let url = reqwest::Url::parse(callback_url.trim())
        .map_err(|_| ProviderConfigurationError::new("Claude OAuth callback URL is invalid"))?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
        || url.port_or_known_default() != Some(54545)
        || url.path() != "/callback"
    {
        return Err(ProviderConfigurationError::new(
            "Claude OAuth callback URL is invalid",
        ));
    }
    parse_callback_parameters(&url, expected_state)
}

fn parse_callback_parameters(
    url: &reqwest::Url,
    expected_state: &str,
) -> Result<ClaudeCallbackResult, ProviderConfigurationError> {
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut error_description = None;
    for (name, value) in url.query_pairs() {
        match name.as_ref() {
            "code" if code.is_none() => code = Some(value.into_owned()),
            "state" if state.is_none() => state = Some(value.into_owned()),
            "error" if error.is_none() => error = Some(value.into_owned()),
            "error_description" if error_description.is_none() => {
                error_description = Some(value.into_owned())
            }
            "code" | "state" | "error" | "error_description" => {
                return Err(ProviderConfigurationError::new(
                    "Claude OAuth callback has duplicate parameters",
                ));
            }
            _ => {}
        }
    }
    if state.as_deref() != Some(expected_state) {
        return Err(ProviderConfigurationError::new(
            "Claude OAuth callback state does not match",
        ));
    }
    if let Some(error) = error {
        let error = error.trim().to_owned();
        if error.is_empty() {
            return Err(ProviderConfigurationError::new(
                "Claude OAuth callback has an empty error",
            ));
        }
        return Ok(ClaudeCallbackResult::Error {
            code: error,
            description: error_description.and_then(super::normalized),
        });
    }
    let code = code
        .and_then(|value| {
            let mut parts = value.splitn(3, '#');
            let code = parts.next().unwrap_or_default().trim().to_owned();
            let state_fragment = parts.next().unwrap_or_default().to_owned();
            (!code.is_empty()).then_some((code, state_fragment))
        })
        .ok_or_else(|| ProviderConfigurationError::new("Claude OAuth callback is missing code"))?;
    Ok(ClaudeCallbackResult::Success {
        code: code.0,
        state: if code.1.is_empty() {
            expected_state.to_owned()
        } else {
            code.1
        },
    })
}

pub(super) fn random_base64(length: usize) -> Result<String, ProviderConfigurationError> {
    let mut bytes = vec![0_u8; length];
    getrandom::fill(&mut bytes).map_err(|_| {
        ProviderConfigurationError::new("failed to generate Claude OAuth PKCE state")
    })?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub(super) fn random_hex(length: usize) -> Result<String, ProviderConfigurationError> {
    let mut bytes = vec![0_u8; length];
    getrandom::fill(&mut bytes)
        .map_err(|_| ProviderConfigurationError::new("failed to generate Claude OAuth state"))?;
    let mut encoded = String::with_capacity(length.saturating_mul(2));
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(encoded, "{byte:02x}");
    }
    Ok(encoded)
}
