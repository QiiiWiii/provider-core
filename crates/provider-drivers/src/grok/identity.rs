use reqwest::RequestBuilder;

use super::contract::BASELINE;

pub(crate) const DEFAULT_PROXY_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
pub(crate) const CLIENT_MODE: &str = "headless";
pub(crate) const CLIENT_IDENTIFIER: &str = "grok-shell";
const TOKEN_AUTH_HEADER: &str = "X-XAI-Token-Auth";
const TOKEN_AUTH_VALUE: &str = "xai-grok-cli";

pub(crate) fn user_agent() -> String {
    format!(
        "grok-shell/{} ({}; {})",
        BASELINE.simulated_client_version,
        std::env::consts::OS,
        normalized_architecture()
    )
}

pub(crate) fn session_headers(request: RequestBuilder) -> RequestBuilder {
    client_headers(request).header(TOKEN_AUTH_HEADER, TOKEN_AUTH_VALUE)
}

pub(crate) fn inference_headers(request: RequestBuilder) -> RequestBuilder {
    model_headers(request).header("x-authenticateresponse", "authenticate-response")
}

pub(crate) fn model_headers(request: RequestBuilder) -> RequestBuilder {
    session_headers(request).header("x-grok-client-identifier", CLIENT_IDENTIFIER)
}

fn client_headers(request: RequestBuilder) -> RequestBuilder {
    request
        .header("x-grok-client-version", BASELINE.simulated_client_version)
        .header("x-grok-client-mode", CLIENT_MODE)
        .header(reqwest::header::USER_AGENT, user_agent())
}

fn normalized_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "arm64" => "aarch64",
        architecture => architecture,
    }
}
