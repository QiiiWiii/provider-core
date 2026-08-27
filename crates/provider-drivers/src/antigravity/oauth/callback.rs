use provider_core::ProviderConfigurationError;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{Instant, timeout_at},
};

use super::super::contract::{CALLBACK_PATH, CALLBACK_PORT};
use super::CALLBACK_READ_TIMEOUT;

pub(super) async fn write_callback_response(stream: &mut TcpStream, accepted: bool) {
    let (status, body) = if accepted {
        (
            "200 OK",
            "Antigravity OAuth complete. You may close this tab.",
        )
    } else {
        ("400 Bad Request", "Antigravity OAuth callback rejected.")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

pub(super) async fn read_callback_request(
    stream: &mut TcpStream,
    deadline: Instant,
) -> Result<Vec<u8>, ProviderConfigurationError> {
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 512];
    let read_deadline = std::cmp::min(deadline, Instant::now() + CALLBACK_READ_TIMEOUT);
    loop {
        let read = timeout_at(read_deadline, stream.read(&mut chunk))
            .await
            .map_err(|_| {
                ProviderConfigurationError::new("Antigravity OAuth authorization expired")
            })?
            .map_err(|_| ProviderConfigurationError::new("Antigravity OAuth callback failed"))?;
        if read == 0 || request.len().saturating_add(read) > 16 * 1024 {
            return Err(ProviderConfigurationError::new(
                "Antigravity OAuth callback is invalid",
            ));
        }
        request.extend_from_slice(&chunk[..read]);
        if request.contains(&b'\n') {
            return Ok(request);
        }
    }
}

pub(super) fn parse_callback_request(
    request: &[u8],
    expected_state: &str,
) -> Result<String, ProviderConfigurationError> {
    let line = std::str::from_utf8(request)
        .ok()
        .and_then(|request| request.lines().next())
        .ok_or_else(|| ProviderConfigurationError::new("Antigravity OAuth callback is invalid"))?;
    let mut parts = line.split_whitespace();
    if parts.next() != Some("GET") {
        return Err(ProviderConfigurationError::new(
            "Antigravity OAuth callback is invalid",
        ));
    }
    let path = parts
        .next()
        .ok_or_else(|| ProviderConfigurationError::new("Antigravity OAuth callback is invalid"))?;
    if parts.next() != Some("HTTP/1.1") || parts.next().is_some() {
        return Err(ProviderConfigurationError::new(
            "Antigravity OAuth callback is invalid",
        ));
    }
    let url = reqwest::Url::parse(&format!("http://localhost{path}"))
        .map_err(|_| ProviderConfigurationError::new("Antigravity OAuth callback is invalid"))?;
    if url.path() != CALLBACK_PATH {
        return Err(ProviderConfigurationError::new(
            "Antigravity OAuth callback is invalid",
        ));
    }
    parse_callback_parameters(&url, expected_state)
}

pub(super) fn parse_callback_url(
    callback_url: &str,
    expected_state: &str,
) -> Result<String, ProviderConfigurationError> {
    let url = reqwest::Url::parse(callback_url.trim()).map_err(|_| {
        ProviderConfigurationError::new("Antigravity OAuth callback URL is invalid")
    })?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
        || url.port_or_known_default() != Some(CALLBACK_PORT)
        || url.path() != CALLBACK_PATH
    {
        return Err(ProviderConfigurationError::new(
            "Antigravity OAuth callback URL is invalid",
        ));
    }
    parse_callback_parameters(&url, expected_state)
}

fn parse_callback_parameters(
    url: &reqwest::Url,
    expected_state: &str,
) -> Result<String, ProviderConfigurationError> {
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (name, value) in url.query_pairs() {
        match name.as_ref() {
            "code" if code.is_none() => code = Some(value.into_owned()),
            "state" if state.is_none() => state = Some(value.into_owned()),
            "error" if error.is_none() => error = Some(value.into_owned()),
            "code" | "state" | "error" => {
                return Err(ProviderConfigurationError::new(
                    "Antigravity OAuth callback has duplicate parameters",
                ));
            }
            _ => {}
        }
    }
    if let Some(error) = error.filter(|value| !value.trim().is_empty()) {
        return Err(ProviderConfigurationError::new(format!(
            "Antigravity OAuth authorization failed: {error}"
        )));
    }
    let state = state
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ProviderConfigurationError::new("Antigravity OAuth callback is missing state")
        })?;
    if state != expected_state {
        return Err(ProviderConfigurationError::new(
            "Antigravity OAuth callback state does not match",
        ));
    }
    code.filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ProviderConfigurationError::new("Antigravity OAuth callback is missing code")
        })
}
