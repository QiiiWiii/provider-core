use provider_core::usage::{
    CacheCapability, CacheEligibility, CacheReportingExpectation, PricingContextBasis, PricingMode,
    TokenInclusionRules, TotalSource, UsageContractSnapshot,
};

pub const CLAUDE_OAUTH_CONTRACT_VERSION: u16 = 1;
pub const CLAUDE_OAUTH_NORMALIZATION_VERSION: u16 = 1;

#[must_use]
pub const fn claude_oauth_usage_contract() -> UsageContractSnapshot {
    UsageContractSnapshot {
        contract_version: CLAUDE_OAUTH_CONTRACT_VERSION,
        normalization_version: CLAUDE_OAUTH_NORMALIZATION_VERSION,
        inclusion: TokenInclusionRules {
            input_includes_cache: false,
            input_categories_mutually_exclusive: true,
            reasoning_included_in_output: true,
            reasoning_applicable: true,
            audio_applicable: false,
            cache_write_applicable: true,
            missing_cache_read_means_zero: true,
            missing_cache_write_means_zero: true,
            total_source: TotalSource::DerivedSum {
                rule_version: CLAUDE_OAUTH_NORMALIZATION_VERSION,
            },
        },
        cache_capability: CacheCapability::Supported,
        cache_eligibility: CacheEligibility::Eligible,
        cache_reporting_expectation: CacheReportingExpectation::Expected,
        pricing_context_basis: PricingContextBasis::EffectiveInput,
        pricing_mode: PricingMode::Default,
    }
}

#[cfg(test)]
mod tests {
    use provider_core::usage::{RawUsageFields, TokenMetric, normalize_usage};

    use super::*;

    #[test]
    fn normalizes_anthropic_independent_cache_and_output_usage() {
        let fields = RawUsageFields::from_claude_usage(&serde_json::json!({
            "input_tokens": 3085,
            "cache_read_input_tokens": 7,
            "cache_creation_input_tokens": 19514,
            "output_tokens": 253,
            "output_tokens_details": { "thinking_tokens": 40 }
        }));
        let observation = normalize_usage(Some(fields), &claude_oauth_usage_contract());

        assert_eq!(
            observation.uncached_input_tokens,
            TokenMetric::ProviderReported { value: 3085 }
        );
        assert_eq!(
            observation.cache_read_input_tokens,
            TokenMetric::ProviderReported { value: 7 }
        );
        assert_eq!(
            observation.cache_write_input_tokens,
            TokenMetric::ProviderReported { value: 19514 }
        );
        assert_eq!(
            observation.effective_input_tokens,
            TokenMetric::DerivedFromReported {
                value: 22606,
                rule_version: CLAUDE_OAUTH_NORMALIZATION_VERSION,
            }
        );
        assert_eq!(
            observation.output_tokens,
            TokenMetric::ProviderReported { value: 253 }
        );
        assert_eq!(
            observation.reasoning_tokens,
            TokenMetric::ProviderReported { value: 40 }
        );
        assert_eq!(
            observation.total_tokens,
            TokenMetric::DerivedFromReported {
                value: 22859,
                rule_version: CLAUDE_OAUTH_NORMALIZATION_VERSION,
            }
        );
        assert!(observation.warnings.is_empty());
    }

    #[test]
    fn omitted_optional_cache_fields_are_zero_by_contract() {
        let fields = RawUsageFields::from_claude_usage(&serde_json::json!({
            "input_tokens": 12,
            "output_tokens": 3
        }));
        let observation = normalize_usage(Some(fields), &claude_oauth_usage_contract());

        assert_eq!(
            observation.cache_read_input_tokens,
            TokenMetric::DerivedFromReported {
                value: 0,
                rule_version: CLAUDE_OAUTH_NORMALIZATION_VERSION,
            }
        );
        assert_eq!(
            observation.cache_write_input_tokens,
            TokenMetric::DerivedFromReported {
                value: 0,
                rule_version: CLAUDE_OAUTH_NORMALIZATION_VERSION,
            }
        );
        assert_eq!(
            observation.effective_input_tokens,
            TokenMetric::DerivedFromReported {
                value: 12,
                rule_version: CLAUDE_OAUTH_NORMALIZATION_VERSION,
            }
        );
        assert_eq!(
            observation.total_tokens,
            TokenMetric::DerivedFromReported {
                value: 15,
                rule_version: CLAUDE_OAUTH_NORMALIZATION_VERSION,
            }
        );
    }
}
