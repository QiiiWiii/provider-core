pub(super) const CREDENTIAL_FORMAT_VERSION: u32 = 1;
pub(super) const REFRESH_LEAD_SECONDS: i64 = 5 * 60;
pub(super) const PERSISTENCE_RETRY_SECONDS: i64 = 30;
pub(super) const COUNT_TOKENS_RESPONSE_LIMIT: usize = 64 * 1024;
pub(super) const ERROR_RESPONSE_LIMIT: usize = 64 * 1024;
pub(super) const API_ROOT: &str = "https://api.anthropic.com/v1";
