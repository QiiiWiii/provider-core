pub(super) fn claude_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .build()
        .expect("Claude OAuth HTTP client configuration must be valid")
}
