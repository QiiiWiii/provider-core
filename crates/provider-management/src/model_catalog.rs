use std::sync::Arc;

use provider_core::{
    DiscoveredProviderModel, ProviderAccount, ProviderError, ProviderModelPricingCatalog,
    ProviderModelPricingRecord, ProviderModelPricingSource, StoredProviderModel,
};
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct ModelCatalogSnapshot {
    pub models: Vec<StoredProviderModel>,
}

#[derive(Clone)]
pub struct ModelCatalogService {
    pricing: Option<Arc<dyn ProviderModelPricingCatalog>>,
}

impl Default for ModelCatalogService {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelCatalogService {
    #[must_use]
    pub fn new() -> Self {
        Self { pricing: None }
    }

    #[must_use]
    pub fn with_pricing(pricing: Arc<dyn ProviderModelPricingCatalog>) -> Self {
        Self {
            pricing: Some(pricing),
        }
    }

    pub async fn discover(
        &self,
        account: &dyn ProviderAccount,
    ) -> Result<Vec<DiscoveredProviderModel>, ModelCatalogError> {
        let mut models = account.discover_models().await?;
        self.attach_catalog(&mut models, |model| account.model_pricing_alias(model));
        Ok(models)
    }

    fn attach_catalog<F>(&self, models: &mut [DiscoveredProviderModel], pricing_alias: F)
    where
        F: Fn(&str) -> Option<&'static str>,
    {
        let Some(catalog) = self.pricing.as_ref() else {
            return;
        };
        for model in models {
            if model.input_modalities.is_none() {
                model.input_modalities = catalog.exact_input_modalities(&model.upstream_model);
            }
            if model.pricing.is_none() {
                let pricing = catalog.exact_pricing(&model.upstream_model).or_else(|| {
                    pricing_alias(&model.upstream_model)
                        .and_then(|alias| catalog.exact_pricing(alias))
                });
                model.pricing = pricing.map(|pricing| ProviderModelPricingRecord {
                    source: ProviderModelPricingSource::Catalog,
                    pricing,
                });
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum ModelCatalogError {
    #[error("provider model discovery failed: {0}")]
    Discovery(#[from] ProviderError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use provider_core::{
        ProviderModelInputModality, ProviderModelPricing, ProviderModelPricingCatalog,
    };

    struct Catalog;

    impl ProviderModelPricingCatalog for Catalog {
        fn exact_pricing(&self, upstream_model: &str) -> Option<ProviderModelPricing> {
            (upstream_model == "gemini-3.7-flash").then(|| ProviderModelPricing {
                input: Some("1".to_owned()),
                output: Some("2".to_owned()),
                cache_read: None,
                cache_write: None,
                reasoning: None,
                input_audio: None,
                output_audio: None,
                tiers: Vec::new(),
            })
        }

        fn exact_input_modalities(
            &self,
            upstream_model: &str,
        ) -> Option<Vec<ProviderModelInputModality>> {
            matches!(upstream_model, "catalog-model" | "upstream-model").then_some(vec![
                ProviderModelInputModality::Audio,
                ProviderModelInputModality::Video,
            ])
        }
    }

    #[test]
    fn catalog_fills_missing_modalities_without_overwriting_discovery() {
        let service = ModelCatalogService::with_pricing(Arc::new(Catalog));
        let mut models = vec![
            DiscoveredProviderModel {
                upstream_model: "catalog-model".to_owned(),
                input_modalities: None,
                metadata_json: "{}".to_owned(),
                routable: true,
                pricing: None,
            },
            DiscoveredProviderModel {
                upstream_model: "upstream-model".to_owned(),
                input_modalities: Some(vec![
                    ProviderModelInputModality::Text,
                    ProviderModelInputModality::Image,
                ]),
                metadata_json: "{}".to_owned(),
                routable: true,
                pricing: None,
            },
            DiscoveredProviderModel {
                upstream_model: "missing-model".to_owned(),
                input_modalities: Some(vec![ProviderModelInputModality::Text]),
                metadata_json: "{}".to_owned(),
                routable: true,
                pricing: None,
            },
        ];

        service.attach_catalog(&mut models, |_| None);

        assert_eq!(
            models[0].input_modalities,
            Some(vec![
                ProviderModelInputModality::Audio,
                ProviderModelInputModality::Video,
            ])
        );
        assert_eq!(
            models[1].input_modalities,
            Some(vec![
                ProviderModelInputModality::Text,
                ProviderModelInputModality::Image,
            ])
        );
        assert_eq!(
            models[2].input_modalities,
            Some(vec![ProviderModelInputModality::Text])
        );
    }

    #[test]
    fn pricing_alias_fills_a_missing_variant_price_from_the_base_model() {
        let service = ModelCatalogService::with_pricing(Arc::new(Catalog));
        let mut models = vec![DiscoveredProviderModel {
            upstream_model: "gemini-3.7-flash-high".to_owned(),
            input_modalities: None,
            metadata_json: "{}".to_owned(),
            routable: true,
            pricing: None,
        }];

        service.attach_catalog(&mut models, |model| {
            (model == "gemini-3.7-flash-high").then_some("gemini-3.7-flash")
        });

        let pricing = models[0].pricing.as_ref().expect("aliased pricing");
        assert_eq!(pricing.source, ProviderModelPricingSource::Catalog);
        assert_eq!(pricing.pricing.input.as_deref(), Some("1"));
        assert_eq!(pricing.pricing.output.as_deref(), Some("2"));
    }
}
