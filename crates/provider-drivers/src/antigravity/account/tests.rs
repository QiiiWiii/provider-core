use super::*;

#[test]
fn account_exposes_antigravity_usage_profile() {
    let driver = AntigravityDriver::for_test("http://127.0.0.1");
    let account = driver.test_account("access-token");
    let profile = account.usage_profile().expect("usage profile");

    assert_eq!(profile.provider, ProviderKind::Antigravity);
    assert_eq!(profile.contract.contract_version, 1);
}
