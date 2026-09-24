use super::*;

pub(super) fn log_proxy_provider_error(
    key: &AuthenticatedApiKey,
    logical: Option<&Arc<LogicalTracker>>,
    protocol: WireFormat,
    model: &str,
    stage: &'static str,
    error: &ProviderError,
) {
    error!(
        request_id = logical.map_or("untracked", |tracker| tracker.request_id()),
        api_key_id = %key.key_id,
        model,
        protocol = ?protocol,
        stage,
        error_kind = ?error.kind(),
        upstream_status = ?error.upstream_response_status(),
        "proxy request failed"
    );
}
