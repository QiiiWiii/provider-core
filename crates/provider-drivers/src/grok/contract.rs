use provider_core::{
    OfficialClientBaseline, OfficialClientContractStatus, OfficialClientEndpointContract,
};

const DEVICE_OAUTH: OfficialClientEndpointContract =
    endpoint("device_oauth", OfficialClientContractStatus::NeedsReview);
const TOKEN_REFRESH: OfficialClientEndpointContract =
    endpoint("token_refresh", OfficialClientContractStatus::Blocked);
pub(crate) const RESPONSES: OfficialClientEndpointContract =
    endpoint("responses_http", OfficialClientContractStatus::Verified);
const USER: OfficialClientEndpointContract =
    endpoint("user", OfficialClientContractStatus::Verified);
const BILLING: OfficialClientEndpointContract =
    endpoint("billing", OfficialClientContractStatus::Verified);
const MODELS: OfficialClientEndpointContract =
    endpoint("models", OfficialClientContractStatus::Verified);
pub(crate) const MEDIA: OfficialClientEndpointContract =
    endpoint("media", OfficialClientContractStatus::Unsupported);

const ENDPOINTS: &[OfficialClientEndpointContract] = &[
    DEVICE_OAUTH,
    TOKEN_REFRESH,
    RESPONSES,
    USER,
    BILLING,
    MODELS,
    MEDIA,
];

pub(crate) const BASELINE: OfficialClientBaseline = OfficialClientBaseline {
    profile_id: "grok-build-v0.2.105",
    official_client: "Grok Build",
    reference_repository: "../../agent/grok-build",
    reference_commit: "7cfcb20d2b50b0d18801a6c0af2e401c0e060894",
    simulated_client_version: "0.2.105",
    endpoint_contracts: ENDPOINTS,
};

const fn endpoint(
    id: &'static str,
    status: OfficialClientContractStatus,
) -> OfficialClientEndpointContract {
    OfficialClientEndpointContract { id, status }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn baseline_and_endpoint_inventory_are_explicit() {
        assert_eq!(BASELINE.profile_id, "grok-build-v0.2.105");
        assert_eq!(BASELINE.official_client, "Grok Build");
        assert_eq!(BASELINE.reference_repository, "../../agent/grok-build");
        assert_eq!(
            BASELINE.reference_commit,
            "7cfcb20d2b50b0d18801a6c0af2e401c0e060894"
        );
        assert_eq!(BASELINE.simulated_client_version, "0.2.105");

        let ids = BASELINE
            .endpoint_contracts
            .iter()
            .map(|endpoint| endpoint.id)
            .collect::<HashSet<_>>();
        assert_eq!(ids.len(), BASELINE.endpoint_contracts.len());
        assert!(RESPONSES.status.allows_production_routing());
        assert!(!MEDIA.status.allows_production_routing());
        assert!(!TOKEN_REFRESH.status.allows_production_routing());
    }
}
