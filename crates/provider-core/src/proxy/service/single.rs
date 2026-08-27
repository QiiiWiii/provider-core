use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    AccountId, Provider, ProviderAccountAccess, ProviderError, ProviderRequest, ProviderRoute,
    ProviderRouteCandidate, ProviderRouter, ProviderStream, RoutableProviderModel, WireFormat,
};

pub(super) struct SingleProviderRouter {
    provider: Arc<dyn Provider>,
    route: Arc<dyn ProviderRoute>,
    access: ProviderAccountAccess,
}

impl SingleProviderRouter {
    pub(super) fn new(provider: Arc<dyn Provider>, access: ProviderAccountAccess) -> Self {
        let route: Arc<dyn ProviderRoute> = Arc::new(SingleProviderRoute {
            provider: provider.clone(),
        });
        Self {
            provider,
            route,
            access,
        }
    }
}

impl ProviderRouter for SingleProviderRouter {
    fn models(
        &self,
        user_id: &str,
        _account_ids: Option<&HashSet<AccountId>>,
    ) -> Vec<RoutableProviderModel> {
        if self.access.allows(user_id) {
            self.provider
                .models()
                .iter()
                .cloned()
                .map(|model| RoutableProviderModel {
                    model,
                    native_formats: vec![self.provider.native_format()],
                })
                .collect()
        } else {
            Vec::new()
        }
    }

    fn routes(&self, query: &crate::ProviderRouteQuery<'_>) -> Vec<ProviderRouteCandidate> {
        if !self.access.allows(query.user_id)
            || !query
                .native_formats
                .contains(&self.provider.native_format())
        {
            return Vec::new();
        }
        vec![ProviderRouteCandidate {
            account_id: None,
            priority: 0,
            upstream_model: query.model.to_owned(),
            input_modalities: self
                .provider
                .models()
                .iter()
                .find(|candidate| candidate.id == query.model)
                .and_then(|candidate| candidate.input_modalities.clone()),
            responses_lite: false,
            pricing: None,
            route: self.route.clone(),
        }]
    }
}

struct SingleProviderRoute {
    provider: Arc<dyn Provider>,
}

#[async_trait]
impl ProviderRoute for SingleProviderRoute {
    fn provider_name(&self) -> &'static str {
        self.provider.name()
    }

    fn native_format(&self) -> WireFormat {
        self.provider.native_format()
    }

    async fn execute_stream(
        &self,
        request: ProviderRequest,
        _pricing: Option<&crate::ProviderModelPricingRecord>,
        // A bare `Provider` has no account or established usage contract, so
        // there is nothing to attribute an attempt to.
        _tracking: Option<&Arc<dyn crate::usage::RequestTracking>>,
    ) -> Result<ProviderStream, ProviderError> {
        self.provider.execute_stream(request).await
    }

    async fn count_tokens(&self, request: ProviderRequest) -> Result<u64, ProviderError> {
        self.provider.count_tokens(request).await
    }
}
