pub(crate) const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub(crate) const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
pub(crate) const USERINFO_ENDPOINT: &str = "https://www.googleapis.com/oauth2/v2/userinfo?alt=json";
pub(crate) const API_BASE_URL: &str = "https://cloudcode-pa.googleapis.com";
pub(crate) const DAILY_API_BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";
pub(crate) const API_VERSION: &str = "v1internal";
pub(crate) const CALLBACK_PORT: u16 = 51121;
pub(crate) const CALLBACK_ADDRESS: &str = "127.0.0.1:51121";
pub(crate) const CALLBACK_PATH: &str = "/oauth-callback";
pub(crate) const REDIRECT_URI: &str = "http://localhost:51121/oauth-callback";
pub(crate) const SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs";
pub(crate) const FALLBACK_USER_AGENT: &str = "antigravity/hub/2.9.1 darwin/arm64";
pub(crate) const FALLBACK_ONBOARD_USER_AGENT: &str =
    "antigravity/hub/2.9.1 darwin/arm64 google-api-nodejs-client/10.3.0";
pub(crate) const GOOG_API_CLIENT: &str = "gl-node/22.21.1";
pub(crate) const CREDENTIAL_FORMAT_VERSION: u32 = 1;
pub(crate) const REFRESH_LEAD_SECONDS: i64 = 5 * 60;
pub(crate) const PERSISTENCE_RETRY_SECONDS: i64 = 30;
