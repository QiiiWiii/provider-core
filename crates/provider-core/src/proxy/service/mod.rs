mod response_linkage;
#[cfg(test)]
mod response_linkage_tests;
mod single;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    AccountId, ProtocolBridge, Provider, ProviderAccountAccess, ProviderError, ProviderErrorKind,
    ProviderModel, ProviderModelPricingRecord, ProviderRequest, ProviderRouteCandidate,
    ProviderRouter, ProviderStream, ProxyRequest, ResponseTranslator, WireFormat,
    usage::ProviderUsageProfile,
};

use response_linkage::observe_response_id;
#[cfg(test)]
use response_linkage::take_response_linkage_events;
use single::SingleProviderRouter;

/// Application service that delegates proxy operations to the active provider.
#[derive(Clone)]
pub struct ProxyService {
    router: Arc<dyn ProviderRouter>,
    protocol: Arc<dyn ProtocolBridge>,
    queue_timeout: Duration,
}

impl ProxyService {
    #[must_use]
    pub fn new(
        provider: Arc<dyn Provider>,
        protocol: Arc<dyn ProtocolBridge>,
        access: ProviderAccountAccess,
    ) -> Self {
        Self::with_router(
            Arc::new(SingleProviderRouter::new(provider, access)),
            protocol,
        )
    }

    #[must_use]
    pub fn with_router(router: Arc<dyn ProviderRouter>, protocol: Arc<dyn ProtocolBridge>) -> Self {
        Self::with_router_and_queue_timeout(router, protocol, crate::DEFAULT_PROVIDER_QUEUE_TIMEOUT)
    }

    #[must_use]
    pub fn with_router_and_queue_timeout(
        router: Arc<dyn ProviderRouter>,
        protocol: Arc<dyn ProtocolBridge>,
        queue_timeout: Duration,
    ) -> Self {
        Self {
            router,
            protocol,
            queue_timeout,
        }
    }

    #[must_use]
    pub fn models(
        &self,
        user_id: &str,
        source_format: WireFormat,
        account_ids: Option<&HashSet<AccountId>>,
    ) -> Vec<ProviderModel> {
        self.models_for_request(
            user_id,
            source_format,
            account_ids,
            &crate::RequestMetadata::default(),
        )
    }

    pub fn models_for_request(
        &self,
        user_id: &str,
        source_format: WireFormat,
        account_ids: Option<&HashSet<AccountId>>,
        metadata: &crate::RequestMetadata,
    ) -> Vec<ProviderModel> {
        self.router
            .models_for_request(user_id, account_ids, metadata)
            .into_iter()
            .filter(|model| {
                model
                    .native_formats
                    .iter()
                    .any(|target| self.protocol.supports(source_format, *target))
            })
            .map(|model| model.model)
            .collect()
    }

    pub async fn execute_stream(
        &self,
        user_id: &str,
        request: ProxyRequest,
        account_ids: Option<&HashSet<AccountId>>,
    ) -> Result<ProviderStream, ProviderError> {
        self.execute_tracked_stream(user_id, request, None, account_ids)
            .await
    }

    /// Execute a request, reporting usage facts through `tracking`.
    ///
    /// Tracking is passed straight down to the route: the attempt boundary is
    /// decided where upstream calls are actually made, not here.
    pub async fn execute_tracked_stream(
        &self,
        user_id: &str,
        request: ProxyRequest,
        tracking: Option<&Arc<dyn crate::usage::RequestTracking>>,
        account_ids: Option<&HashSet<AccountId>>,
    ) -> Result<ProviderStream, ProviderError> {
        self.prepare_stream(user_id, request, account_ids)?
            .execute_stream(tracking)
            .await
    }

    pub async fn count_tokens(
        &self,
        user_id: &str,
        request: ProxyRequest,
        account_ids: Option<&HashSet<AccountId>>,
    ) -> Result<u64, ProviderError> {
        let mut prepared = self.prepare_stream(user_id, request, account_ids)?;
        prepared.count_input_tokens().await
    }

    pub fn prepare_stream(
        &self,
        user_id: &str,
        mut request: ProxyRequest,
        account_ids: Option<&HashSet<AccountId>>,
    ) -> Result<PreparedProxyExecution, ProviderError> {
        request
            .metadata
            .routing_scope
            .get_or_insert_with(|| user_id.to_owned());
        let routes = self.resolve_routes(user_id, &request, account_ids)?;
        Ok(PreparedProxyExecution {
            router: self.router.clone(),
            protocol: self.protocol.clone(),
            request,
            routes,
            queue_timeout: self.queue_timeout,
        })
    }

    fn resolve_routes(
        &self,
        user_id: &str,
        request: &ProxyRequest,
        account_ids: Option<&HashSet<AccountId>>,
    ) -> Result<Vec<ProviderRouteCandidate>, ProviderError> {
        let native_formats = [
            WireFormat::OpenAiResponses,
            WireFormat::OpenAiChatCompletions,
            WireFormat::ClaudeMessages,
        ]
        .into_iter()
        .filter(|target| self.protocol.supports(request.format, *target))
        .collect::<Vec<_>>();
        let routing_scope = request.metadata.routing_scope.as_deref().unwrap_or(user_id);
        let routes = self.router.routes(&crate::ProviderRouteQuery {
            user_id,
            routing_scope,
            model: &request.model,
            native_formats: &native_formats,
            session_id: request
                .metadata
                .routing_session_id
                .as_deref()
                .or(request.metadata.session_id.as_deref()),
            previous_response_id: request.metadata.previous_response_id.as_deref(),
            account_ids,
        });
        let restricted_route = routes
            .iter()
            .any(|route| route.route.requires_claude_code());
        let routes = routes
            .into_iter()
            .filter(|route| route.route.accepts_request(&request.metadata))
            .collect::<Vec<_>>();
        if routes.is_empty() {
            if restricted_route && request.metadata.client != crate::RequestClient::ClaudeCode {
                return Err(ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "Claude OAuth provider requires a Claude Code client",
                )
                .with_upstream_status(403));
            }
            if request.metadata.previous_response_id.is_some() {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "continuation state is unavailable for this API key; resend complete input history",
                ));
            }
            Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "no available provider supports the requested model and protocol",
            ))
        } else {
            Ok(routes)
        }
    }
}

pub struct PreparedProxyExecution {
    router: Arc<dyn ProviderRouter>,
    protocol: Arc<dyn ProtocolBridge>,
    request: ProxyRequest,
    routes: Vec<ProviderRouteCandidate>,
    queue_timeout: Duration,
}

impl PreparedProxyExecution {
    #[must_use]
    pub fn pricing(&self) -> Option<&ProviderModelPricingRecord> {
        self.routes.first().and_then(|route| route.pricing.as_ref())
    }

    #[must_use]
    pub fn usage_profile(&self) -> Option<ProviderUsageProfile> {
        self.routes
            .first()
            .and_then(|route| route.route.usage_profile())
    }

    #[must_use]
    pub fn maximum_attempts(&self) -> u32 {
        self.routes
            .iter()
            .map(|route| route.route.maximum_attempts())
            .sum()
    }

    pub async fn count_input_tokens(&mut self) -> Result<u64, ProviderError> {
        let model = self.request.model.clone();
        let session_id = self
            .request
            .metadata
            .routing_session_id
            .clone()
            .or_else(|| self.request.metadata.session_id.clone());
        let routing_scope = self
            .request
            .metadata
            .routing_scope
            .clone()
            .unwrap_or_default();
        let mut last_error = None;
        for index in 0..self.routes.len() {
            let (route, request, _) = self.prepare_candidate(index)?;
            match route.route.count_tokens(request).await {
                Ok(count) => {
                    if let Some(account_id) = route.account_id.as_ref() {
                        self.router.record_route_success(account_id, &model);
                        self.router.commit_session_affinity(
                            &routing_scope,
                            &model,
                            session_id.as_deref(),
                            account_id,
                        );
                    }
                    return Ok(count);
                }
                Err(error) => {
                    let Some(reason) = error.failover_reason() else {
                        return Err(error);
                    };
                    if let Some(account_id) = route.account_id.as_ref() {
                        self.router.record_route_failure_with_retry_after(
                            account_id,
                            &model,
                            reason,
                            error.retry_after(),
                        );
                    }
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.expect("a non-empty token count route plan must succeed or fail"))
    }

    pub async fn execute_stream(
        self,
        tracking: Option<&Arc<dyn crate::usage::RequestTracking>>,
    ) -> Result<ProviderStream, ProviderError> {
        let queue_deadline = Instant::now() + self.queue_timeout;
        let model = self.request.model.clone();
        let session_id = self
            .request
            .metadata
            .routing_session_id
            .clone()
            .or_else(|| self.request.metadata.session_id.clone());
        let routing_scope = self
            .request
            .metadata
            .routing_scope
            .clone()
            .unwrap_or_default();
        let mut last_error = None;
        for index in 0..self.routes.len() {
            let (route, request, response) = self.prepare_candidate(index)?;
            match route
                .route
                .execute_stream_with_deadline(
                    request,
                    route.pricing.as_ref(),
                    tracking,
                    queue_deadline,
                )
                .await
            {
                Ok(stream) => {
                    if let Some(account_id) = route.account_id.as_ref() {
                        self.router.record_route_success(account_id, &model);
                        self.router.commit_session_affinity(
                            &routing_scope,
                            &model,
                            session_id.as_deref(),
                            account_id,
                        );
                    }
                    let stream = response.translate_stream(stream);
                    return Ok(match route.account_id.clone() {
                        Some(account_id) if route.route.tracks_response_id() => {
                            let bind_response_id_at_created =
                                route.route.supports_previous_response_id();
                            observe_response_id(
                                stream,
                                self.router.clone(),
                                routing_scope.clone(),
                                account_id,
                                bind_response_id_at_created,
                            )
                        }
                        _ => stream,
                    });
                }
                Err(error) => {
                    let Some(reason) = error.failover_reason() else {
                        return Err(error);
                    };
                    if self.request.metadata.previous_response_id.is_some() {
                        return Err(error);
                    }
                    if let Some(account_id) = route.account_id.as_ref() {
                        self.router.record_route_failure_with_retry_after(
                            account_id,
                            &model,
                            reason,
                            error.retry_after(),
                        );
                    }
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.expect("a non-empty route plan must either succeed or fail"))
    }

    fn prepare_candidate(
        &self,
        index: usize,
    ) -> Result<
        (
            &ProviderRouteCandidate,
            ProviderRequest,
            Box<dyn ResponseTranslator>,
        ),
        ProviderError,
    > {
        let route = &self.routes[index];
        let mut request = self.request.clone();
        request.model = route.upstream_model.clone();
        request.metadata.responses_lite = route.responses_lite;
        if !route.route.requires_claude_code() {
            request.metadata.client = crate::RequestClient::Unknown;
            request.metadata.user_agent = None;
            request.metadata.claude_code_beta = None;
            request.metadata.claude_code_user_id = None;
            request.metadata.claude_code_session_id = None;
            request.metadata.claude_code_headers.clear();
            request.metadata.claude_code_helper_profile = false;
            request.metadata.claude_code_payload = None;
        }
        let prepared = self.protocol.prepare(
            request,
            route.route.native_format(),
            route.input_modalities.as_deref(),
        )?;
        let (request, response) = prepared.into_parts();
        Ok((route, request, response))
    }
}
