use std::sync::{Arc, Mutex};

use crate::{AccountId, ProviderRouteCandidate, ProviderRouter, RoutableProviderModel};

pub(super) struct BindingRouter {
    pub(super) bindings: Arc<Mutex<Vec<(String, String, AccountId)>>>,
}

impl ProviderRouter for BindingRouter {
    fn models(
        &self,
        _user_id: &str,
        _account_ids: Option<&std::collections::HashSet<AccountId>>,
    ) -> Vec<RoutableProviderModel> {
        Vec::new()
    }

    fn routes(&self, _query: &crate::ProviderRouteQuery<'_>) -> Vec<ProviderRouteCandidate> {
        Vec::new()
    }

    fn bind_response_id(&self, routing_scope: &str, response_id: &str, account_id: &AccountId) {
        self.bindings.lock().expect("bindings lock").push((
            routing_scope.to_owned(),
            response_id.to_owned(),
            account_id.clone(),
        ));
    }
}
