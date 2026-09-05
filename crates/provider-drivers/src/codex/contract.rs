use provider_core::{
    OfficialClientBaseline, OfficialClientContractStatus, OfficialClientEndpointContract,
};

const DEVICE_OAUTH: OfficialClientEndpointContract =
    endpoint("device_oauth", OfficialClientContractStatus::Verified);
const TOKEN_REFRESH: OfficialClientEndpointContract =
    endpoint("token_refresh", OfficialClientContractStatus::Verified);
pub(crate) const RESPONSES: OfficialClientEndpointContract =
    endpoint("responses_http", OfficialClientContractStatus::Verified);
pub(crate) const RESPONSES_LITE: OfficialClientEndpointContract =
    endpoint("responses_lite", OfficialClientContractStatus::Verified);
const MODELS: OfficialClientEndpointContract =
    endpoint("models", OfficialClientContractStatus::Verified);
const QUOTA: OfficialClientEndpointContract =
    endpoint("quota", OfficialClientContractStatus::Verified);
const RESPONSES_WEBSOCKET: OfficialClientEndpointContract = endpoint(
    "responses_websocket",
    OfficialClientContractStatus::Unsupported,
);

const ENDPOINTS: &[OfficialClientEndpointContract] = &[
    DEVICE_OAUTH,
    TOKEN_REFRESH,
    RESPONSES,
    RESPONSES_LITE,
    MODELS,
    QUOTA,
    RESPONSES_WEBSOCKET,
];

pub(crate) const BASELINE: OfficialClientBaseline = OfficialClientBaseline {
    profile_id: "codex-rust-v0.153.4",
    official_client: "Codex CLI",
    reference_repository: "../../agent/codex",
    reference_commit: "3d2ee51ca2d5db578f328aa75e20aa22c0197c9a",
    simulated_client_version: "0.153.4",
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
        assert_eq!(BASELINE.profile_id, "codex-rust-v0.153.4");
        assert_eq!(BASELINE.official_client, "Codex CLI");
        assert_eq!(BASELINE.reference_repository, "../../agent/codex");
        assert_eq!(
            BASELINE.reference_commit,
            "3d2ee51ca2d5db578f328aa75e20aa22c0197c9a"
        );
        assert_eq!(BASELINE.simulated_client_version, "0.153.4");

        let ids = BASELINE
            .endpoint_contracts
            .iter()
            .map(|endpoint| endpoint.id)
            .collect::<HashSet<_>>();
        assert_eq!(ids.len(), BASELINE.endpoint_contracts.len());
        assert!(RESPONSES.status.allows_production_routing());
        assert!(RESPONSES_LITE.status.allows_production_routing());
    }
}
