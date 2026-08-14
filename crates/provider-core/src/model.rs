use crate::AccountId;
use serde::{Deserialize, Serialize};

/// Model metadata exposed by a provider.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderModel {
    pub id: String,
    pub object: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<u64>,
    pub owned_by: String,
    pub input_modalities: Option<Vec<ProviderModelInputModality>>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub supports_image_detail_original: bool,
}

impl ProviderModel {
    #[must_use]
    pub fn new(id: impl Into<String>, owned_by: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            object: "model".to_owned(),
            created: None,
            owned_by: owned_by.into(),
            input_modalities: None,
            supports_image_detail_original: false,
        }
    }

    #[must_use]
    pub const fn with_created(mut self, created: u64) -> Self {
        self.created = Some(created);
        self
    }

    #[must_use]
    pub fn with_input_modalities(
        mut self,
        input_modalities: Option<Vec<ProviderModelInputModality>>,
    ) -> Self {
        self.supports_image_detail_original = input_modalities
            .as_deref()
            .is_some_and(|modalities| modalities.contains(&ProviderModelInputModality::Image));
        self.input_modalities = input_modalities;
        self
    }
}

const fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderModelInputModality {
    Text,
    Image,
    Pdf,
    Audio,
    Video,
}

pub fn validate_input_modalities(
    input_modalities: Option<&[ProviderModelInputModality]>,
) -> Result<(), &'static str> {
    let Some(input_modalities) = input_modalities else {
        return Ok(());
    };
    if input_modalities.is_empty()
        || input_modalities
            .iter()
            .enumerate()
            .any(|(index, modality)| input_modalities[index + 1..].contains(modality))
    {
        Err("input_modalities must be null or a non-empty array of unique supported modalities")
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderModelPricingSource {
    Catalog,
    Manual,
}

impl ProviderModelPricingSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Catalog => "catalog",
            Self::Manual => "manual",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelPricing {
    pub input: Option<String>,
    pub output: Option<String>,
    pub cache_read: Option<String>,
    pub cache_write: Option<String>,
    pub reasoning: Option<String>,
    pub input_audio: Option<String>,
    pub output_audio: Option<String>,
    pub tiers: Vec<ProviderModelPricingTier>,
}

impl ProviderModelPricing {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.input.is_none()
            && self.output.is_none()
            && self.cache_read.is_none()
            && self.cache_write.is_none()
            && self.reasoning.is_none()
            && self.input_audio.is_none()
            && self.output_audio.is_none()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelPricingTier {
    pub threshold_tokens: u64,
    pub input: Option<String>,
    pub output: Option<String>,
    pub cache_read: Option<String>,
    pub cache_write: Option<String>,
    pub reasoning: Option<String>,
    pub input_audio: Option<String>,
    pub output_audio: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderModelPricingRecord {
    pub source: ProviderModelPricingSource,
    pub pricing: ProviderModelPricing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredProviderModel {
    pub upstream_model: String,
    pub input_modalities: Option<Vec<ProviderModelInputModality>>,
    pub metadata_json: String,
    pub routable: bool,
    pub pricing: Option<ProviderModelPricingRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredProviderModel {
    pub account_id: AccountId,
    pub upstream_model: String,
    pub alias: Option<String>,
    pub enabled: bool,
    pub available: bool,
    pub routable: bool,
    pub input_modalities: Option<Vec<ProviderModelInputModality>>,
    pub metadata_json: String,
    pub pricing: Option<ProviderModelPricingRecord>,
    pub last_seen_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl StoredProviderModel {
    #[must_use]
    pub fn effective_model(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.upstream_model)
    }

    #[must_use]
    pub fn is_publicly_listed(&self) -> bool {
        serde_json::from_str::<serde_json::Value>(&self.metadata_json)
            .expect("stored provider model metadata must be valid JSON")
            .get("visibility")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|visibility| visibility == "list")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderModelOverride {
    pub alias: Option<String>,
    pub enabled: bool,
    pub input_modalities: Option<Vec<ProviderModelInputModality>>,
    pub pricing: Option<Option<ProviderModelPricing>>,
    pub updated_at: i64,
}

pub trait ProviderModelPricingCatalog: Send + Sync {
    fn exact_pricing(&self, upstream_model: &str) -> Option<ProviderModelPricing>;

    fn exact_input_modalities(
        &self,
        upstream_model: &str,
    ) -> Option<Vec<ProviderModelInputModality>>;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn stored_model(metadata_json: &str) -> StoredProviderModel {
        StoredProviderModel {
            account_id: AccountId::new("account-1").expect("account ID"),
            upstream_model: "model-1".to_owned(),
            alias: None,
            enabled: true,
            available: true,
            routable: true,
            input_modalities: None,
            metadata_json: metadata_json.to_owned(),
            pricing: None,
            last_seen_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn public_listing_follows_provider_visibility_metadata() {
        assert!(stored_model(r#"{"visibility":"list"}"#).is_publicly_listed());
        assert!(!stored_model(r#"{"visibility":"hide"}"#).is_publicly_listed());
        assert!(!stored_model(r#"{"visibility":"none"}"#).is_publicly_listed());
        assert!(stored_model(r#"{"id":"compatible"}"#).is_publicly_listed());
    }

    #[test]
    fn model_capability_metadata_is_explicit_and_derives_original_image_detail() {
        assert_eq!(
            serde_json::to_value(ProviderModel::new("unknown", "owner")).expect("serialize"),
            json!({
                "id": "unknown",
                "object": "model",
                "owned_by": "owner",
                "input_modalities": null
            })
        );
        assert_eq!(
            serde_json::to_value(ProviderModel::new("vision", "owner").with_input_modalities(
                Some(vec![
                    ProviderModelInputModality::Text,
                    ProviderModelInputModality::Image,
                ])
            ))
            .expect("serialize"),
            json!({
                "id": "vision",
                "object": "model",
                "owned_by": "owner",
                "input_modalities": ["text", "image"],
                "supports_image_detail_original": true
            })
        );
    }

    #[test]
    fn modality_contract_accepts_ordered_unique_supported_modalities() {
        assert!(validate_input_modalities(None).is_ok());
        assert!(validate_input_modalities(Some(&[ProviderModelInputModality::Text])).is_ok());
        assert!(
            validate_input_modalities(Some(&[
                ProviderModelInputModality::Video,
                ProviderModelInputModality::Image,
                ProviderModelInputModality::Pdf,
            ]))
            .is_ok()
        );
        assert!(validate_input_modalities(Some(&[])).is_err());
        assert!(
            validate_input_modalities(Some(&[
                ProviderModelInputModality::Audio,
                ProviderModelInputModality::Audio,
            ]))
            .is_err()
        );
    }
}
