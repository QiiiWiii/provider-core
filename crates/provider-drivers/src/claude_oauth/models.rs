use provider_core::{DiscoveredProviderModel, ProviderError, ProviderErrorKind, ProviderModel};

pub(super) const CLAUDE_OAUTH_MODELS: &[&str] = &[
    "claude-haiku-4-5-20251001",
    "claude-sonnet-4-5-20250929",
    "claude-sonnet-4-6",
    "claude-opus-4-6",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-fable-5",
    "claude-opus-4-5-20251101",
    "claude-opus-4-1-20250805",
    "claude-opus-4-20250514",
    "claude-sonnet-4-20250514",
    "claude-3-7-sonnet-20250219",
    "claude-3-5-haiku-20241022",
];

pub(super) fn discover_models() -> Result<Vec<DiscoveredProviderModel>, ProviderError> {
    CLAUDE_OAUTH_MODELS
        .iter()
        .map(|id| {
            let metadata_json = serde_json::to_string(&ProviderModel::new(*id, "anthropic"))
                .map_err(|_| {
                    ProviderError::new(
                        ProviderErrorKind::Internal,
                        "failed to normalize Claude OAuth model",
                    )
                })?;
            Ok(DiscoveredProviderModel {
                upstream_model: (*id).to_owned(),
                input_modalities: None,
                metadata_json,
                routable: true,
                pricing: None,
            })
        })
        .collect()
}
