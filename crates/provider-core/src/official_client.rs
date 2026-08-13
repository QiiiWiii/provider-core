use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OfficialClientContractStatus {
    Verified,
    Unsupported,
    Blocked,
    NeedsReview,
}

impl OfficialClientContractStatus {
    #[must_use]
    pub const fn allows_production_routing(self) -> bool {
        matches!(self, Self::Verified)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OfficialClientBaseline {
    pub profile_id: &'static str,
    pub official_client: &'static str,
    pub reference_repository: &'static str,
    pub reference_commit: &'static str,
    pub simulated_client_version: &'static str,
    pub endpoint_contracts: &'static [OfficialClientEndpointContract],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OfficialClientEndpointContract {
    pub id: &'static str,
    pub status: OfficialClientContractStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_verified_contracts_allow_production_routing() {
        assert!(OfficialClientContractStatus::Verified.allows_production_routing());
        for status in [
            OfficialClientContractStatus::Unsupported,
            OfficialClientContractStatus::Blocked,
            OfficialClientContractStatus::NeedsReview,
        ] {
            assert!(!status.allows_production_routing());
        }
    }
}
