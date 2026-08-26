use provider_core::usage::{
    CacheCapability, CacheEligibility, CacheReportingExpectation, PricingContextBasis, PricingMode,
    TokenInclusionRules, TotalSource, UsageContractSnapshot,
};

pub const ANTIGRAVITY_CONTRACT_VERSION: u16 = 1;
pub const ANTIGRAVITY_NORMALIZATION_VERSION: u16 = 1;

#[must_use]
pub const fn antigravity_usage_contract(
    cache_eligibility: CacheEligibility,
    pricing_mode: PricingMode,
) -> UsageContractSnapshot {
    UsageContractSnapshot {
        contract_version: ANTIGRAVITY_CONTRACT_VERSION,
        normalization_version: ANTIGRAVITY_NORMALIZATION_VERSION,
        inclusion: TokenInclusionRules {
            input_includes_cache: true,
            input_categories_mutually_exclusive: false,
            reasoning_included_in_output: true,
            reasoning_applicable: true,
            audio_applicable: false,
            cache_write_applicable: false,
            missing_cache_read_means_zero: true,
            missing_cache_write_means_zero: false,
            total_source: TotalSource::Reported,
        },
        cache_capability: CacheCapability::Supported,
        cache_eligibility,
        cache_reporting_expectation: CacheReportingExpectation::Expected,
        pricing_context_basis: PricingContextBasis::EffectiveInput,
        pricing_mode,
    }
}

#[cfg(test)]
mod tests {
    use provider_core::usage::{CacheEligibility, PricingMode, TokenMetric};
    use provider_core::{RawUsageFields, normalize_usage};

    use super::antigravity_usage_contract;

    #[test]
    fn cloud_code_usage_maps_to_responses_token_semantics() {
        let fields = RawUsageFields::from_responses_usage(&serde_json::json!({
            "input_tokens": 120,
            "input_tokens_details": {"cached_tokens": 100},
            "output_tokens": 12,
            "output_tokens_details": {"reasoning_tokens": 4},
            "total_tokens": 132,
        }));
        let observation = normalize_usage(
            Some(fields),
            &antigravity_usage_contract(CacheEligibility::Eligible, PricingMode::Default),
        );

        assert_eq!(
            observation.uncached_input_tokens,
            TokenMetric::DerivedFromReported {
                value: 20,
                rule_version: 1,
            }
        );
        assert_eq!(
            observation.effective_input_tokens,
            TokenMetric::ProviderReported { value: 120 }
        );
        assert_eq!(
            observation.output_tokens,
            TokenMetric::ProviderReported { value: 12 }
        );
        assert_eq!(
            observation.reasoning_tokens,
            TokenMetric::ProviderReported { value: 4 }
        );
        assert_eq!(
            observation.total_tokens,
            TokenMetric::ProviderReported { value: 132 }
        );
        assert!(observation.warnings.is_empty());
    }
}
