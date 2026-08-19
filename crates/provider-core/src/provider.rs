use std::{
    collections::HashSet,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc2822};

use crate::{AccountId, ProviderModel, ProviderRequest, WireFormat};

/// Byte stream crossing the provider and protocol boundaries.
pub type ProviderStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send + 'static>>;

/// Maximum time a proxy route plan may spend waiting for account capacity.
///
/// This is a runtime safety bound, not an upstream wire-contract setting.
pub const DEFAULT_PROVIDER_QUEUE_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_PROVIDER_RETRY_AFTER: Duration = Duration::from_secs(5 * 60);
const MAX_REVIEWED_PROVIDER_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Parse a bounded `Retry-After` delta or HTTP date for local cooldowns.
#[must_use]
pub fn parse_provider_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    let duration = if let Ok(seconds) = value.parse::<u64>() {
        Duration::from_secs(seconds)
    } else {
        let retry_at = OffsetDateTime::parse(value, &Rfc2822).ok()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        let retry_at = u64::try_from(retry_at.unix_timestamp()).ok()?;
        Duration::from_secs(retry_at.checked_sub(now)?)
    };
    (duration <= MAX_PROVIDER_RETRY_AFTER).then_some(duration)
}

#[cfg(test)]
mod retry_after_tests {
    use super::*;

    #[test]
    fn retry_after_accepts_bounded_delta_seconds_and_http_dates() {
        assert_eq!(parse_provider_retry_after("0"), Some(Duration::ZERO));
        assert_eq!(
            parse_provider_retry_after(" 30 "),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_provider_retry_after("300"),
            Some(MAX_PROVIDER_RETRY_AFTER)
        );
        assert_eq!(parse_provider_retry_after("301"), None);
        assert_eq!(parse_provider_retry_after("86400"), None);
        assert_eq!(parse_provider_retry_after("-1"), None);
        let retry_at = OffsetDateTime::from_unix_timestamp(
            i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("current time")
                    .as_secs()
                    + 60,
            )
            .expect("timestamp"),
        )
        .expect("retry timestamp")
        .format(&Rfc2822)
        .expect("HTTP date")
        .replace("+0000", "GMT");
        let parsed = parse_provider_retry_after(&retry_at).expect("bounded HTTP date");
        assert!(parsed <= Duration::from_secs(60));
        assert!(parsed >= Duration::from_secs(58));
        assert_eq!(parse_provider_retry_after("abc"), None);
        assert_eq!(
            ProviderError::new(ProviderErrorKind::RateLimited, "limited")
                .with_retry_after(Duration::from_secs(86_401))
                .retry_after(),
            Some(MAX_PROVIDER_RETRY_AFTER)
        );
        assert_eq!(
            ProviderError::new(ProviderErrorKind::RateLimited, "limited")
                .with_reviewed_retry_after(Duration::from_secs(86_401))
                .retry_after(),
            Some(MAX_REVIEWED_PROVIDER_RETRY_AFTER)
        );
    }
}

/// Stable error categories used by the HTTP layer for protocol-specific mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderErrorKind {
    InvalidRequest,
    Authentication,
    RateLimited,
    Capacity,
    Upstream,
    Internal,
}

/// A reviewed reason why replaying the request on another account is safe.
///
/// Errors are non-replayable by default. Drivers and the account runtime must
/// opt in only when they can prove the request falls into one of these cases.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderFailoverReason {
    AuthenticationExhausted,
    QuotaExhausted,
    RateLimited,
    CapacityExhausted,
    PreconnectFailure,
}

/// A narrowly scoped, provider-reviewed request transformation that may be
/// attempted once after an upstream rejected the original request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderRetryHint {
    StripEncryptedReasoning,
}

/// A safe provider error that may cross crate boundaries.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct ProviderError {
    kind: ProviderErrorKind,
    message: String,
    upstream_status: Option<u16>,
    failover_reason: Option<ProviderFailoverReason>,
    retry_after: Option<Duration>,
    retry_hint: Option<ProviderRetryHint>,
}

impl ProviderError {
    #[must_use]
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            upstream_status: None,
            failover_reason: None,
            retry_after: None,
            retry_hint: None,
        }
    }

    #[must_use]
    pub const fn with_upstream_status(mut self, status: u16) -> Self {
        self.upstream_status = Some(status);
        self
    }

    #[must_use]
    pub const fn with_failover_reason(mut self, reason: ProviderFailoverReason) -> Self {
        self.failover_reason = Some(reason);
        self
    }

    #[must_use]
    pub fn with_retry_after(mut self, retry_after: Duration) -> Self {
        self.retry_after = Some(if retry_after > MAX_PROVIDER_RETRY_AFTER {
            MAX_PROVIDER_RETRY_AFTER
        } else {
            retry_after
        });
        self
    }

    /// Attach a driver-derived cooldown from a reviewed upstream error code.
    /// Unlike an untrusted Retry-After header, this may represent a documented
    /// rolling quota window of up to one day.
    #[must_use]
    pub fn with_reviewed_retry_after(mut self, retry_after: Duration) -> Self {
        self.retry_after = Some(retry_after.min(MAX_REVIEWED_PROVIDER_RETRY_AFTER));
        self
    }

    #[must_use]
    pub const fn with_retry_hint(mut self, retry_hint: ProviderRetryHint) -> Self {
        self.retry_hint = Some(retry_hint);
        self
    }

    #[must_use]
    pub const fn kind(&self) -> ProviderErrorKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub const fn upstream_status(&self) -> Option<u16> {
        self.upstream_status
    }

    #[must_use]
    pub const fn failover_reason(&self) -> Option<ProviderFailoverReason> {
        self.failover_reason
    }

    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    #[must_use]
    pub const fn retry_hint(&self) -> Option<ProviderRetryHint> {
        self.retry_hint
    }
}

/// Runtime provider boundary used by the proxy service.
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;

    fn native_format(&self) -> WireFormat;

    fn models(&self) -> &[ProviderModel];

    async fn execute_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderStream, ProviderError>;

    async fn count_tokens(&self, request: ProviderRequest) -> Result<u64, ProviderError>;
}

/// One concrete provider account selected for a request.
#[async_trait]
pub trait ProviderRoute: Send + Sync {
    fn provider_name(&self) -> &'static str;

    fn native_format(&self) -> WireFormat;

    /// Whether response IDs from this account may be reused by a later request.
    fn supports_previous_response_id(&self) -> bool {
        false
    }

    /// Whether response IDs should be bound to this account for later routing.
    /// Stateless providers can track the ID for account affinity even when the
    /// field itself is removed before the upstream request.
    fn tracks_response_id(&self) -> bool {
        self.supports_previous_response_id()
    }

    fn usage_profile(&self) -> Option<crate::usage::ProviderUsageProfile> {
        None
    }

    /// Maximum number of real upstream attempts one execution can make.
    fn maximum_attempts(&self) -> u32 {
        1
    }

    /// Execute the request, opening one tracked attempt per real upstream call.
    ///
    /// `tracking` is threaded down here rather than handled by the caller because
    /// a refresh-and-retry happens inside this call: only the code that decides
    /// to make a second upstream call can report it as a second attempt.
    async fn execute_stream(
        &self,
        request: ProviderRequest,
        pricing: Option<&crate::ProviderModelPricingRecord>,
        tracking: Option<&Arc<dyn crate::usage::RequestTracking>>,
    ) -> Result<ProviderStream, ProviderError>;

    /// Execute the request with a shared route-plan capacity deadline.
    ///
    /// Providers that do not implement account-level capacity can use the
    /// default behavior. Account-backed runtime routes override this method so
    /// retries across candidates share one queue deadline.
    async fn execute_stream_with_deadline(
        &self,
        request: ProviderRequest,
        pricing: Option<&crate::ProviderModelPricingRecord>,
        tracking: Option<&Arc<dyn crate::usage::RequestTracking>>,
        _queue_deadline: Instant,
    ) -> Result<ProviderStream, ProviderError> {
        self.execute_stream(request, pricing, tracking).await
    }

    async fn count_tokens(&self, request: ProviderRequest) -> Result<u64, ProviderError>;
}

#[derive(Clone)]
pub struct ProviderRouteCandidate {
    pub account_id: Option<AccountId>,
    pub priority: u32,
    pub upstream_model: String,
    pub input_modalities: Option<Vec<crate::ProviderModelInputModality>>,
    pub responses_lite: bool,
    pub pricing: Option<crate::ProviderModelPricingRecord>,
    pub route: Arc<dyn ProviderRoute>,
}

#[derive(Clone, Debug)]
pub struct RoutableProviderModel {
    pub model: ProviderModel,
    pub native_formats: Vec<WireFormat>,
}

pub struct ProviderRouteQuery<'a> {
    pub user_id: &'a str,
    pub routing_scope: &'a str,
    pub model: &'a str,
    pub native_formats: &'a [WireFormat],
    pub session_id: Option<&'a str>,
    pub previous_response_id: Option<&'a str>,
    pub account_ids: Option<&'a HashSet<AccountId>>,
}

/// In-memory model index used before protocol conversion and provider execution.
pub trait ProviderRouter: Send + Sync {
    fn models(
        &self,
        user_id: &str,
        account_ids: Option<&HashSet<AccountId>>,
    ) -> Vec<RoutableProviderModel>;

    fn routes(&self, query: &ProviderRouteQuery<'_>) -> Vec<ProviderRouteCandidate>;

    fn commit_session_affinity(
        &self,
        _routing_scope: &str,
        _model: &str,
        _session_id: Option<&str>,
        _account_id: &AccountId,
    ) {
    }

    fn record_route_failure(
        &self,
        _account_id: &AccountId,
        _model: &str,
        _reason: ProviderFailoverReason,
    ) {
    }

    fn record_route_failure_with_retry_after(
        &self,
        account_id: &AccountId,
        model: &str,
        reason: ProviderFailoverReason,
        _retry_after: Option<Duration>,
    ) {
        self.record_route_failure(account_id, model, reason);
    }

    fn record_route_success(&self, _account_id: &AccountId, _model: &str) {}

    fn bind_response_id(&self, _routing_scope: &str, _response_id: &str, _account_id: &AccountId) {}
}

/// Shared metadata and native protocol implemented by one upstream driver.
pub trait ProviderDriver: Send + Sync {
    fn name(&self) -> &'static str;

    fn native_format(&self) -> WireFormat;

    fn models(&self) -> &[ProviderModel];
}
