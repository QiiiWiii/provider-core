use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

use reqwest::header::{CACHE_CONTROL, USER_AGENT};

use super::contract::{FALLBACK_ONBOARD_USER_AGENT, FALLBACK_USER_AGENT};

const FALLBACK_VERSION: &str = "2.9.1";
const PLATFORM: &str = "darwin/arm64";
const NODE_API_CLIENT: &str = "google-api-nodejs-client/10.3.0";
const MANIFEST_URL: &str = "https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml";
const CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_MANIFEST_SIZE: usize = 4096;

struct VersionState {
    version: String,
    expires_at: Instant,
}

static VERSION_STATE: OnceLock<RwLock<VersionState>> = OnceLock::new();
static REFRESH_GATE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
static UPDATER_STARTED: OnceLock<()> = OnceLock::new();

fn state() -> &'static RwLock<VersionState> {
    VERSION_STATE.get_or_init(|| {
        RwLock::new(VersionState {
            version: FALLBACK_VERSION.to_owned(),
            expires_at: Instant::now(),
        })
    })
}

fn refresh_gate() -> &'static tokio::sync::Mutex<()> {
    REFRESH_GATE.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub(crate) fn start_background_refresh() {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if UPDATER_STARTED.set(()).is_err() {
        return;
    }
    handle.spawn(async {
        refresh().await;
        let mut interval = tokio::time::interval(CACHE_TTL / 2);
        loop {
            interval.tick().await;
            refresh().await;
        }
    });
}

pub(crate) async fn refresh() {
    if !expired() {
        return;
    }
    let _guard = refresh_gate().lock().await;
    if !expired() {
        return;
    }
    let Ok(client) = reqwest::Client::builder().timeout(FETCH_TIMEOUT).build() else {
        return;
    };
    let Ok(version) = fetch_manifest_version(&client, MANIFEST_URL).await else {
        return;
    };
    let mut cached = state().write().unwrap_or_else(|error| error.into_inner());
    cached.version = version;
    cached.expires_at = Instant::now() + CACHE_TTL;
}

pub(crate) fn user_agent() -> String {
    let version = latest_version();
    format!("antigravity/hub/{version} {PLATFORM}")
}

pub(crate) fn onboard_user_agent() -> String {
    format!("{} {NODE_API_CLIENT}", user_agent())
}

pub(crate) fn latest_version() -> String {
    state()
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .version
        .clone()
}

pub(crate) fn fallback_version() -> &'static str {
    FALLBACK_VERSION
}

pub(crate) fn fallback_user_agent() -> &'static str {
    FALLBACK_USER_AGENT
}

pub(crate) fn fallback_onboard_user_agent() -> &'static str {
    FALLBACK_ONBOARD_USER_AGENT
}

fn expired() -> bool {
    state()
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .expires_at
        <= Instant::now()
}

async fn fetch_manifest_version(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let response = client
        .get(url)
        .header(USER_AGENT, "electron-builder")
        .header(CACHE_CONTROL, "no-cache")
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("manifest returned HTTP {}", response.status()));
    }
    let body = response.bytes().await.map_err(|error| error.to_string())?;
    if body.len() > MAX_MANIFEST_SIZE {
        return Err("manifest is too large".to_owned());
    }
    let body = std::str::from_utf8(&body).map_err(|error| error.to_string())?;
    body.lines()
        .find_map(|line| {
            let value = line.trim().strip_prefix("version:")?.trim();
            let value = value.trim_matches(['"', '\'']);
            is_semver(value).then(|| value.to_owned())
        })
        .ok_or_else(|| "manifest has no valid version".to_owned())
}

fn is_semver(value: &str) -> bool {
    let parts = value.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_user_agents_match_cpa_floor() {
        assert_eq!(fallback_user_agent(), "antigravity/hub/2.9.1 darwin/arm64");
        assert_eq!(
            fallback_onboard_user_agent(),
            "antigravity/hub/2.9.1 darwin/arm64 google-api-nodejs-client/10.3.0"
        );
    }

    #[test]
    fn validates_manifest_versions() {
        assert!(is_semver("2.9.1"));
        assert!(is_semver("10.12.0"));
        assert!(!is_semver("2.9"));
        assert!(!is_semver("2.9.1-beta"));
    }
}
