use super::*;

impl ProviderModelRouter {
    pub(super) fn describe_unavailable_routes(&self, query: &ProviderRouteQuery<'_>) -> String {
        let now = Instant::now();
        let cooldowns = self.cooldowns();
        let mut reasons = BTreeMap::<String, usize>::new();
        for (id, account) in self.account_snapshot().iter() {
            if !account.access.allows(query.user_id)
                || query.account_ids.is_some_and(|ids| !ids.contains(id))
            {
                continue;
            }
            let reason = if !account.account.runtime_state().available_for_requests() {
                "account unavailable".to_owned()
            } else if !query
                .native_formats
                .contains(&account.route.native_format())
            {
                "protocol incompatible".to_owned()
            } else if let Some(cooldown) = cooldowns
                .get(&CooldownKey {
                    account_id: id.clone(),
                    model: query.model.to_owned(),
                })
                .filter(|cooldown| cooldown.until > now)
            {
                format!(
                    "model temporarily cooling down after {:?}; retry in {}s",
                    cooldown.reason,
                    cooldown.until.duration_since(now).as_secs() + 1
                )
            } else {
                let models = account
                    .models
                    .iter()
                    .filter(|m| m.effective_model() == query.model)
                    .collect::<Vec<_>>();
                if models.is_empty() {
                    "model not configured under this name".to_owned()
                } else if models.iter().all(|m| !m.enabled) {
                    "model disabled".to_owned()
                } else if models.iter().all(|m| !m.available) {
                    "model unavailable".to_owned()
                } else if models.iter().any(|m| {
                    m.enabled && m.available && m.routable && model_contract_is_routable(m)
                }) {
                    "route state changed or continuation binding unavailable; retry with complete input history".to_owned()
                } else {
                    "model routing contract unavailable".to_owned()
                }
            };
            *reasons
                .entry(format!("{}: {reason}", account.account.provider_name()))
                .or_default() += 1;
        }
        if reasons.is_empty() {
            return "no provider accounts are accessible to this API key; check its provider groups".to_owned();
        }
        reasons
            .into_iter()
            .map(|(reason, count)| format!("{reason} ({count} account(s))"))
            .collect::<Vec<_>>()
            .join("; ")
    }
}
