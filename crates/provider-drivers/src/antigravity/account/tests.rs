use super::*;
use provider_core::ProviderErrorKind;

#[test]
fn account_exposes_antigravity_usage_profile() {
    let driver = AntigravityDriver::for_test("http://127.0.0.1");
    let account = driver.test_account("access-token");
    let profile = account.usage_profile().expect("usage profile");

    assert_eq!(profile.provider, ProviderKind::Antigravity);
    assert_eq!(profile.contract.contract_version, 1);
}

#[tokio::test]
async fn discover_models_fails_closed_when_upstream_is_unavailable() {
    let driver = AntigravityDriver::for_test("http://127.0.0.1:1");
    let account = driver.test_account("access-token");
    let error = account
        .discover_models()
        .await
        .expect_err("unavailable discovery");
    assert_eq!(error.kind(), ProviderErrorKind::Upstream);
}
