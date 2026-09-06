use super::{
    ManagementState,
    quota_estimate::{estimate_json, estimates_for_accounts, primary_estimate},
    shared::{ApiError, data, parse_account_id, require_super_admin, unix_timestamp},
};
use axum::{
    Json,
    extract::{Extension, Path, State},
};
use provider_auth::AuthenticatedSession;
use serde_json::Value;

pub(super) async fn get_quota(
    State(state): State<ManagementState>,
    Extension(session): Extension<AuthenticatedSession>,
    Path(account_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let account_id = parse_account_id(&account_id)?;
    let quota = state
        .manager
        .quota(session.user.id.as_str(), &account_id, unix_timestamp())
        .await?;
    let estimate = estimate_for_account(&state, &session, account_id.as_str(), &quota).await;
    Ok(data(quota_json(&quota, estimate.as_ref())?))
}

pub(super) async fn refresh_quota(
    State(state): State<ManagementState>,
    Extension(session): Extension<AuthenticatedSession>,
    Path(account_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_super_admin(&session)?;
    let account_id = parse_account_id(&account_id)?;
    let quota = state
        .manager
        .refresh_quota(session.user.id.as_str(), &account_id, unix_timestamp())
        .await?;
    let estimate = estimate_for_account(&state, &session, account_id.as_str(), &quota).await;
    Ok(data(quota_json(&quota, estimate.as_ref())?))
}

async fn estimate_for_account(
    state: &ManagementState,
    session: &AuthenticatedSession,
    account_id: &str,
    quota: &provider_core::ProviderQuotaView,
) -> Option<provider_usage::QuotaLimitEstimatePoint> {
    if require_super_admin(session).is_err() {
        return None;
    }
    let estimates = estimates_for_accounts(state, &[account_id.to_owned()]).await;
    primary_estimate(quota, estimates.get(account_id)?).cloned()
}

fn quota_json(
    quota: &provider_core::ProviderQuotaView,
    estimate: Option<&provider_usage::QuotaLimitEstimatePoint>,
) -> Result<Value, ApiError> {
    let mut value = serde_json::to_value(quota).map_err(|_| ApiError::internal())?;
    value
        .as_object_mut()
        .ok_or_else(ApiError::internal)?
        .insert(
            "estimate".to_owned(),
            estimate.map_or(Value::Null, estimate_json),
        );
    Ok(value)
}
