use std::sync::LazyLock;

use provider_core::{DiscoveredProviderModel, ProviderModel, ProviderModelInputModality};

struct ModelDefinition {
    id: &'static str,
    modalities: &'static [ProviderModelInputModality],
}

const TEXT: &[ProviderModelInputModality] = &[ProviderModelInputModality::Text];
const TEXT_IMAGE: &[ProviderModelInputModality] = &[
    ProviderModelInputModality::Text,
    ProviderModelInputModality::Image,
];
const TEXT_IMAGE_AUDIO_VIDEO: &[ProviderModelInputModality] = &[
    ProviderModelInputModality::Text,
    ProviderModelInputModality::Image,
    ProviderModelInputModality::Audio,
    ProviderModelInputModality::Video,
];

const MODEL_DEFINITIONS: &[ModelDefinition] = &[
    ModelDefinition {
        id: "claude-opus-4-6-thinking",
        modalities: TEXT_IMAGE,
    },
    ModelDefinition {
        id: "claude-sonnet-4-6",
        modalities: TEXT_IMAGE,
    },
    ModelDefinition {
        id: "gemini-3.6-flash-high",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
    ModelDefinition {
        id: "gemini-3.7-flash-high",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
    ModelDefinition {
        id: "gemini-3-flash",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
    ModelDefinition {
        id: "gemini-3-flash-agent",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
    ModelDefinition {
        id: "gemini-3.1-flash-image",
        modalities: TEXT_IMAGE,
    },
    ModelDefinition {
        id: "gemini-pro-agent",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
    ModelDefinition {
        id: "gemini-3.1-pro-low",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
    ModelDefinition {
        id: "gpt-oss-120b-medium",
        modalities: TEXT,
    },
    ModelDefinition {
        id: "gemini-3.1-flash-lite",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
    ModelDefinition {
        id: "gemini-3.5-flash-low",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
    ModelDefinition {
        id: "gemini-3.5-flash-extra-low",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
];

static MODELS: LazyLock<Vec<ProviderModel>> = LazyLock::new(|| {
    MODEL_DEFINITIONS
        .iter()
        .map(|definition| {
            ProviderModel::new(definition.id, "antigravity")
                .with_input_modalities(Some(definition.modalities.to_vec()))
        })
        .collect()
});

pub(crate) fn antigravity_models() -> &'static [ProviderModel] {
    &MODELS
}

const PRICING_ALIASES: &[(&str, &str)] = &[
    ("claude-opus-4-6-thinking", "claude-opus-4-6"),
    ("gemini-3.6-flash-high", "gemini-3.6-flash"),
    ("gemini-3.7-flash-high", "gemini-3.7-flash"),
    ("gemini-3-flash-agent", "gemini-3-flash"),
    ("gemini-3.1-flash-image", "gemini-3.1-flash"),
    ("gemini-pro-agent", "gemini-pro"),
    ("gemini-3.1-pro-low", "gemini-3.1-pro"),
    ("gpt-oss-120b-medium", "gpt-oss-120b"),
    ("gemini-3.1-flash-lite", "gemini-3.1-flash"),
    ("gemini-3.5-flash-low", "gemini-3.5-flash"),
    ("gemini-3.5-flash-extra-low", "gemini-3.5-flash"),
];

pub(crate) fn model_pricing_alias(model: &str) -> Option<&'static str> {
    PRICING_ALIASES
        .iter()
        .find(|(variant, _)| *variant == model)
        .map(|(_, base)| *base)
}

pub(crate) fn discovered_models() -> Vec<DiscoveredProviderModel> {
    MODEL_DEFINITIONS
        .iter()
        .map(|definition| DiscoveredProviderModel {
            upstream_model: definition.id.to_owned(),
            input_modalities: Some(definition.modalities.to_vec()),
            metadata_json: serde_json::json!({
                "id": definition.id,
                "object": "model",
                "owned_by": "antigravity",
                "input_modalities": definition.modalities,
                "visibility": "list",
                "provider": "antigravity",
                "official_client_contract": {
                    "endpoint": "cloudcode_stream_generate_content",
                    "status": "verified"
                }
            })
            .to_string(),
            routable: true,
            pricing: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{PRICING_ALIASES, model_pricing_alias};

    #[test]
    fn maps_every_known_variant_to_a_base_price_model() {
        for (variant, base) in PRICING_ALIASES {
            assert_eq!(model_pricing_alias(variant), Some(*base));
        }
        assert_eq!(model_pricing_alias("gemini-3.7-flash"), None);
    }
}
