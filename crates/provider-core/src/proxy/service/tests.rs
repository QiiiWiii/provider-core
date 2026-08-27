use async_trait::async_trait;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::{StreamExt, stream};

use super::*;
use crate::{PreparedProviderRequest, ProviderRoute, RequestMetadata, RoutableProviderModel};

#[derive(Clone, Copy)]
enum RouteResult {
    HeaderError(Option<crate::ProviderFailoverReason>),
    StreamError,
    Success,
}

struct TestRoute {
    calls: Arc<Mutex<Vec<String>>>,
    account: String,
    result: RouteResult,
    claude_code_only: bool,
}

#[async_trait]
impl ProviderRoute for TestRoute {
    fn provider_name(&self) -> &'static str {
        "test"
    }

    fn native_format(&self) -> WireFormat {
        WireFormat::OpenAiResponses
    }

    fn accepts_request(&self, metadata: &RequestMetadata) -> bool {
        !self.claude_code_only || metadata.client == crate::RequestClient::ClaudeCode
    }

    fn requires_claude_code(&self) -> bool {
        self.claude_code_only
    }

    async fn execute_stream(
        &self,
        _request: ProviderRequest,
        _pricing: Option<&ProviderModelPricingRecord>,
        _tracking: Option<&Arc<dyn crate::usage::RequestTracking>>,
    ) -> Result<ProviderStream, ProviderError> {
        self.calls
            .lock()
            .expect("calls lock")
            .push(self.account.clone());
        match self.result {
            RouteResult::HeaderError(reason) => {
                let error = ProviderError::new(ProviderErrorKind::Upstream, "failed")
                    .with_upstream_status(500);
                Err(match reason {
                    Some(reason) => error.with_failover_reason(reason),
                    None => error,
                })
            }
            RouteResult::StreamError => Ok(Box::pin(stream::once(async {
                Err(
                    ProviderError::new(ProviderErrorKind::Upstream, "stream failed")
                        .with_failover_reason(crate::ProviderFailoverReason::RateLimited),
                )
            }))),
            RouteResult::Success => Ok(Box::pin(stream::once(async {
                Ok(Bytes::from_static(b"ok"))
            }))),
        }
    }

    async fn count_tokens(&self, _request: ProviderRequest) -> Result<u64, ProviderError> {
        self.calls
            .lock()
            .expect("calls lock")
            .push(self.account.clone());
        match self.result {
            RouteResult::HeaderError(reason) => {
                let error = ProviderError::new(ProviderErrorKind::RateLimited, "limited")
                    .with_upstream_status(429);
                Err(match reason {
                    Some(reason) => error.with_failover_reason(reason),
                    None => error,
                })
            }
            RouteResult::StreamError => Err(ProviderError::new(
                ProviderErrorKind::Upstream,
                "token count failed",
            )),
            RouteResult::Success => Ok(34),
        }
    }
}

struct TestRouter {
    routes: Vec<ProviderRouteCandidate>,
    committed: Arc<Mutex<Vec<String>>>,
}

impl ProviderRouter for TestRouter {
    fn models(
        &self,
        _user_id: &str,
        _account_ids: Option<&HashSet<AccountId>>,
    ) -> Vec<RoutableProviderModel> {
        Vec::new()
    }

    fn routes(&self, _query: &crate::ProviderRouteQuery<'_>) -> Vec<ProviderRouteCandidate> {
        self.routes.clone()
    }

    fn commit_session_affinity(
        &self,
        _routing_scope: &str,
        _model: &str,
        _session_id: Option<&str>,
        account_id: &AccountId,
    ) {
        self.committed
            .lock()
            .expect("commit lock")
            .push(account_id.to_string());
    }
}

struct TestProtocol {
    prepares: Arc<Mutex<u32>>,
}

impl ProtocolBridge for TestProtocol {
    fn supports(&self, _source: WireFormat, _target: WireFormat) -> bool {
        true
    }

    fn prepare(
        &self,
        request: ProxyRequest,
        target: WireFormat,
        _input_modalities: Option<&[crate::ProviderModelInputModality]>,
    ) -> Result<PreparedProviderRequest, ProviderError> {
        *self.prepares.lock().expect("prepare lock") += 1;
        Ok(PreparedProviderRequest::new(
            ProviderRequest::from_proxy(request, target),
            Box::new(IdentityTranslator),
        ))
    }
}

struct IdentityTranslator;

impl ResponseTranslator for IdentityTranslator {
    fn translate_stream(self: Box<Self>, stream: ProviderStream) -> ProviderStream {
        stream
    }
}

fn candidate(
    account: &str,
    result: RouteResult,
    calls: &Arc<Mutex<Vec<String>>>,
) -> ProviderRouteCandidate {
    ProviderRouteCandidate {
        account_id: Some(AccountId::new(account).expect("account ID")),
        priority: 0,
        upstream_model: account.to_owned(),
        input_modalities: None,
        responses_lite: false,
        pricing: None,
        route: Arc::new(TestRoute {
            calls: calls.clone(),
            account: account.to_owned(),
            result,
            claude_code_only: false,
        }),
    }
}

fn restricted_candidate(
    account: &str,
    result: RouteResult,
    calls: &Arc<Mutex<Vec<String>>>,
) -> ProviderRouteCandidate {
    ProviderRouteCandidate {
        account_id: Some(AccountId::new(account).expect("account ID")),
        priority: 0,
        upstream_model: account.to_owned(),
        input_modalities: None,
        responses_lite: false,
        pricing: None,
        route: Arc::new(TestRoute {
            calls: calls.clone(),
            account: account.to_owned(),
            result,
            claude_code_only: true,
        }),
    }
}

fn request() -> ProxyRequest {
    ProxyRequest::new(
        WireFormat::OpenAiResponses,
        "shared",
        Bytes::from_static(br#"{"model":"shared"}"#),
    )
    .expect("request")
    .with_metadata(RequestMetadata {
        session_id: Some("session".to_owned()),
        ..RequestMetadata::default()
    })
}

type TestService = (
    ProxyService,
    Arc<Mutex<Vec<String>>>,
    Arc<Mutex<u32>>,
    Arc<Mutex<Vec<String>>>,
);

fn service(results: &[(&str, RouteResult)]) -> TestService {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let prepares = Arc::new(Mutex::new(0));
    let committed = Arc::new(Mutex::new(Vec::new()));
    let routes = results
        .iter()
        .map(|(account, result)| candidate(account, *result, &calls))
        .collect();
    (
        ProxyService::with_router(
            Arc::new(TestRouter {
                routes,
                committed: committed.clone(),
            }),
            Arc::new(TestProtocol {
                prepares: prepares.clone(),
            }),
        ),
        calls,
        prepares,
        committed,
    )
}

#[tokio::test]
async fn explicit_failover_prepares_each_candidate_and_commits_the_success() {
    let (service, calls, prepares, committed) = service(&[
        (
            "account-a",
            RouteResult::HeaderError(Some(crate::ProviderFailoverReason::RateLimited)),
        ),
        ("account-b", RouteResult::Success),
    ]);
    let mut stream = service
        .execute_stream("owner", request(), None)
        .await
        .expect("fallback stream");
    assert_eq!(stream.next().await.expect("item").expect("chunk"), "ok");
    assert_eq!(*calls.lock().expect("calls"), ["account-a", "account-b"]);
    assert_eq!(*prepares.lock().expect("prepares"), 2);
    assert_eq!(*committed.lock().expect("committed"), ["account-b"]);
}

#[tokio::test]
async fn restricted_route_rejects_non_claude_code_requests_before_execution() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let service = ProxyService::with_router(
        Arc::new(TestRouter {
            routes: vec![restricted_candidate(
                "claude-code",
                RouteResult::Success,
                &calls,
            )],
            committed: Arc::new(Mutex::new(Vec::new())),
        }),
        Arc::new(TestProtocol {
            prepares: Arc::new(Mutex::new(0)),
        }),
    );

    let error = match service.execute_stream("owner", request(), None).await {
        Ok(_) => panic!("non-Claude Code request must be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ProviderErrorKind::Authentication);
    assert_eq!(error.upstream_status(), Some(403));
    assert!(calls.lock().expect("calls").is_empty());

    let request = request().with_metadata(RequestMetadata {
        client: crate::RequestClient::ClaudeCode,
        ..RequestMetadata::default()
    });
    let mut stream = service
        .execute_stream("owner", request, None)
        .await
        .expect("Claude Code request");
    assert_eq!(stream.next().await.expect("item").expect("chunk"), "ok");
    assert_eq!(*calls.lock().expect("calls"), ["claude-code"]);
}

#[tokio::test]
async fn token_count_fails_over_and_commits_the_successful_account() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let committed = Arc::new(Mutex::new(Vec::new()));
    let service = ProxyService::with_router(
        Arc::new(TestRouter {
            routes: vec![
                candidate(
                    "account-a",
                    RouteResult::HeaderError(Some(crate::ProviderFailoverReason::RateLimited)),
                    &calls,
                ),
                candidate("account-b", RouteResult::Success, &calls),
            ],
            committed: committed.clone(),
        }),
        Arc::new(TestProtocol {
            prepares: Arc::new(Mutex::new(0)),
        }),
    );

    let count = service
        .count_tokens("owner", request(), None)
        .await
        .expect("fallback token count");
    assert_eq!(count, 34);
    assert_eq!(
        calls.lock().expect("calls lock").as_slice(),
        ["account-a", "account-b"]
    );
    assert_eq!(
        committed.lock().expect("commit lock").as_slice(),
        ["account-b"]
    );
}

#[tokio::test]
async fn unknown_header_error_never_replays_another_provider() {
    let (service, calls, _, _) = service(&[
        ("account-a", RouteResult::HeaderError(None)),
        ("account-b", RouteResult::Success),
    ]);
    assert!(
        service
            .execute_stream("owner", request(), None)
            .await
            .is_err()
    );
    assert_eq!(*calls.lock().expect("calls"), ["account-a"]);
}

#[tokio::test]
async fn missing_continuation_binding_returns_an_actionable_error() {
    let (service, _, _, _) = service(&[]);
    let mut request = request();
    request.metadata.previous_response_id = Some("resp_old".to_owned());

    let error = match service.execute_stream("owner", request, None).await {
        Ok(_) => panic!("missing continuation binding must fail"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ProviderErrorKind::InvalidRequest);
    assert!(error.message().contains("resend complete input history"));
}

#[tokio::test]
async fn stream_error_never_reenters_failover() {
    let (service, calls, _, _) = service(&[
        ("account-a", RouteResult::StreamError),
        ("account-b", RouteResult::Success),
    ]);
    let mut stream = service
        .execute_stream("owner", request(), None)
        .await
        .expect("opened stream");
    assert!(stream.next().await.expect("item").is_err());
    assert_eq!(*calls.lock().expect("calls"), ["account-a"]);
}

#[tokio::test]
async fn route_plan_tries_every_candidate_in_the_group() {
    let failure = RouteResult::HeaderError(Some(crate::ProviderFailoverReason::PreconnectFailure));
    let (service, calls, _, _) = service(&[
        ("account-a", failure),
        ("account-b", failure),
        ("account-c", failure),
        ("account-d", RouteResult::Success),
    ]);
    let mut stream = service
        .execute_stream("owner", request(), None)
        .await
        .expect("fourth candidate succeeds");
    assert_eq!(stream.next().await.expect("item").expect("chunk"), "ok");
    assert_eq!(
        *calls.lock().expect("calls"),
        ["account-a", "account-b", "account-c", "account-d"]
    );
}

#[tokio::test]
async fn capacity_failure_fails_over_without_marking_cooldown() {
    let (service, calls, _, committed) = service(&[
        (
            "account-a",
            RouteResult::HeaderError(Some(crate::ProviderFailoverReason::CapacityExhausted)),
        ),
        ("account-b", RouteResult::Success),
    ]);
    let mut stream = service
        .execute_stream("owner", request(), None)
        .await
        .expect("fallback stream");
    assert_eq!(stream.next().await.expect("item").expect("chunk"), "ok");
    assert_eq!(*calls.lock().expect("calls"), ["account-a", "account-b"]);
    assert_eq!(*committed.lock().expect("committed"), ["account-b"]);
}

#[tokio::test]
async fn previous_response_id_does_not_fail_over_capacity() {
    let (service, calls, _, _) = service(&[
        (
            "account-a",
            RouteResult::HeaderError(Some(crate::ProviderFailoverReason::CapacityExhausted)),
        ),
        ("account-b", RouteResult::Success),
    ]);
    let request = request().with_metadata(RequestMetadata {
        previous_response_id: Some("resp-1".to_owned()),
        ..RequestMetadata::default()
    });
    assert!(
        service
            .execute_stream("owner", request, None)
            .await
            .is_err()
    );
    assert_eq!(*calls.lock().expect("calls"), ["account-a"]);
}
