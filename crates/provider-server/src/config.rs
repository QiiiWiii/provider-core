use std::{
    fs,
    io::{self, Write as _},
    net::IpAddr,
    path::Path,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use provider_runtime::ProviderRuntimeConfig;

/// Default bind address for local (non-container) runs.
pub(crate) const DEFAULT_LISTEN_ADDRESS: &str = "127.0.0.1:8317";

/// Override with host:port, e.g. `0.0.0.0:8317` inside Docker.
pub(crate) const LISTEN_ADDRESS_ENV: &str = "LISTEN_ADDRESS";

pub(crate) const DATABASE_PATH: &str = "data/provider-core.db";

/// Set to `0`, `false` or `off` to stop fetching the models.dev model catalog.
pub(crate) const CATALOG_SYNC_ENV: &str = "CATALOG_SYNC";

/// Exact reverse-proxy peer allowed to supply the client IP header.
pub(crate) const TRUSTED_PROXY_IP_ENV: &str = "TRUSTED_PROXY_IP";

/// Base64-encoded 32-byte key used for the single supported credential ciphertext format.
///
/// Optional: deployments that manage secrets centrally set it, everything else
/// falls back to [`CREDENTIAL_KEY_PATH`].
pub(crate) const PROVIDER_CREDENTIAL_KEY_ENV: &str = "PROVIDER_CREDENTIAL_KEY";

/// Maximum number of requests executing concurrently for one provider account.
pub(crate) const PROVIDER_INFERENCE_CONCURRENCY_ENV: &str = "PROVIDER_INFERENCE_CONCURRENCY";

/// Maximum number of additional requests waiting for one provider account.
pub(crate) const PROVIDER_INFERENCE_QUEUE_CAPACITY_ENV: &str = "PROVIDER_INFERENCE_QUEUE_CAPACITY";

/// Maximum time, in seconds, a request may wait for provider account capacity.
pub(crate) const PROVIDER_INFERENCE_QUEUE_TIMEOUT_SECONDS_ENV: &str =
    "PROVIDER_INFERENCE_QUEUE_TIMEOUT_SECONDS";

/// Key generated on first start when the env var is unset, so a fresh install
/// runs unconfigured. Kept beside the database: the credentials it decrypts are
/// worthless without it, so a backup that takes one must take the other.
pub(crate) const CREDENTIAL_KEY_PATH: &str = "data/credential.key";

/// Set to `json` for one JSON object per line, which is what a log shipper
/// wants. Anything else keeps the human-readable format.
pub(crate) const LOG_FORMAT_ENV: &str = "LOG_FORMAT";

/// Level directives, e.g. `info,provider_server=debug`. Read by `tracing`'s
/// `EnvFilter`, so it keeps the name the rest of the Rust world already uses.
pub(crate) const LOG_FILTER_ENV: &str = "RUST_LOG";

/// Quiet enough to leave on in production: request lines, startup, and warnings.
pub(crate) const DEFAULT_LOG_FILTER: &str = "info";

/// Resolved listen address. Docker sets `LISTEN_ADDRESS=0.0.0.0:8317`.
pub(crate) fn listen_address() -> String {
    std::env::var(LISTEN_ADDRESS_ENV).unwrap_or_else(|_| DEFAULT_LISTEN_ADDRESS.to_owned())
}

/// Whether to keep the model catalog up to date over the network.
///
/// On by default: without a catalog every cost is `unavailable`, which reads like
/// a bug rather than a choice. The switch exists so an operator who does not want
/// the outbound request can turn it off and still get token counts.
pub(crate) fn catalog_sync_enabled() -> bool {
    match std::env::var(CATALOG_SYNC_ENV) {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    }
}

/// Whether to emit JSON rather than the human-readable format. An unrecognized
/// value reads as the default instead of failing: a typo in a log setting must
/// not stop the server from starting.
pub(crate) fn json_log_format() -> bool {
    std::env::var(LOG_FORMAT_ENV).is_ok_and(|value| value.trim().eq_ignore_ascii_case("json"))
}

pub(crate) fn provider_runtime_config() -> Result<ProviderRuntimeConfig, io::Error> {
    let defaults = ProviderRuntimeConfig::default();
    let inference_concurrency = parse_usize_env(
        PROVIDER_INFERENCE_CONCURRENCY_ENV,
        defaults.inference_concurrency,
        1,
    )?;
    let inference_queue_capacity = parse_usize_env(
        PROVIDER_INFERENCE_QUEUE_CAPACITY_ENV,
        defaults.inference_queue_capacity,
        0,
    )?;
    if inference_concurrency
        .checked_add(inference_queue_capacity)
        .is_none()
    {
        return Err(invalid_config(
            "provider inference concurrency plus queue capacity is too large",
        ));
    }
    let queue_timeout_seconds = parse_u64_env(
        PROVIDER_INFERENCE_QUEUE_TIMEOUT_SECONDS_ENV,
        defaults.queue_timeout.as_secs(),
        1,
    )?;

    Ok(ProviderRuntimeConfig {
        inference_concurrency,
        inference_queue_capacity,
        queue_timeout: Duration::from_secs(queue_timeout_seconds),
    })
}

fn parse_usize_env(name: &str, default: usize, minimum: usize) -> Result<usize, io::Error> {
    match std::env::var(name) {
        Ok(value) => parse_usize(name, &value, minimum),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(invalid_config(format!("{name} is invalid: {error}"))),
    }
}

fn parse_u64_env(name: &str, default: u64, minimum: u64) -> Result<u64, io::Error> {
    match std::env::var(name) {
        Ok(value) => parse_u64(name, &value, minimum),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(invalid_config(format!("{name} is invalid: {error}"))),
    }
}

fn parse_usize(name: &str, value: &str, minimum: usize) -> Result<usize, io::Error> {
    let parsed = value
        .trim()
        .parse::<usize>()
        .map_err(|error| invalid_config(format!("{name} must be an integer: {error}")))?;
    if parsed < minimum {
        return Err(invalid_config(format!("{name} must be at least {minimum}")));
    }
    Ok(parsed)
}

fn parse_u64(name: &str, value: &str, minimum: u64) -> Result<u64, io::Error> {
    let parsed = value
        .trim()
        .parse::<u64>()
        .map_err(|error| invalid_config(format!("{name} must be an integer: {error}")))?;
    if parsed < minimum {
        return Err(invalid_config(format!("{name} must be at least {minimum}")));
    }
    Ok(parsed)
}

fn invalid_config(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

pub(crate) fn trusted_proxy_ip() -> Result<Option<IpAddr>, io::Error> {
    match std::env::var(TRUSTED_PROXY_IP_ENV) {
        Ok(value) => value.trim().parse().map(Some).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{TRUSTED_PROXY_IP_ENV} must be one IP address: {error}"),
            )
        }),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{TRUSTED_PROXY_IP_ENV} is invalid: {error}"),
        )),
    }
}

/// Resolved credential key: the env var, else the stored key, else a new one.
pub(crate) fn provider_credential_key() -> Result<[u8; 32], io::Error> {
    match std::env::var(PROVIDER_CREDENTIAL_KEY_ENV) {
        Ok(encoded) if !encoded.trim().is_empty() => {
            return decode_credential_key(&encoded, PROVIDER_CREDENTIAL_KEY_ENV);
        }
        // An empty value is what `KEY: ${KEY}` in a compose file yields when the
        // host variable is unset. That is an unconfigured deployment, not a
        // broken key, so it falls through instead of refusing to start.
        Ok(_) | Err(std::env::VarError::NotPresent) => {}
        Err(error) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{PROVIDER_CREDENTIAL_KEY_ENV} is invalid: {error}"),
            ));
        }
    }

    match fs::read_to_string(CREDENTIAL_KEY_PATH) {
        Ok(encoded) => decode_credential_key(&encoded, CREDENTIAL_KEY_PATH),
        Err(error) if error.kind() == io::ErrorKind::NotFound => store_new_credential_key(),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("failed to read {CREDENTIAL_KEY_PATH}: {error}"),
        )),
    }
}

fn decode_credential_key(encoded: &str, source: &str) -> Result<[u8; 32], io::Error> {
    let decoded = STANDARD.decode(encoded.trim()).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{source} must be valid base64: {error}"),
        )
    })?;
    decoded.try_into().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{source} must decode to exactly 32 bytes"),
        )
    })
}

/// Generates a key and persists it before returning. A key that only lived for
/// this run would leave every credential written under it unreadable on restart.
fn store_new_credential_key() -> Result<[u8; 32], io::Error> {
    let mut key = [0_u8; 32];
    getrandom::fill(&mut key).map_err(|error| {
        io::Error::other(format!(
            "failed to generate a provider credential key: {error}"
        ))
    })?;

    if let Some(parent) = Path::new(CREDENTIAL_KEY_PATH)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }

    let mut file = options.open(CREDENTIAL_KEY_PATH).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to write {CREDENTIAL_KEY_PATH}: {error}"),
        )
    })?;
    file.write_all(STANDARD.encode(key).as_bytes())?;
    // The credentials sealed under this key reach the database through its own
    // fsync. A key still sitting in the page cache when the process dies would
    // leave them permanently unreadable, so this one cannot be lazy either.
    file.sync_all()?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_config_defaults_match_production_baseline() {
        let config = ProviderRuntimeConfig::default();
        assert_eq!(config.inference_concurrency, 10);
        assert_eq!(config.inference_queue_capacity, 20);
        assert_eq!(config.queue_timeout, Duration::from_secs(30));
    }

    #[test]
    fn runtime_config_values_must_be_valid() {
        assert_eq!(parse_usize("TEST", "10", 1).expect("valid value"), 10);
        assert_eq!(parse_u64("TEST", "30", 1).expect("valid value"), 30);
        assert!(parse_usize("TEST", "0", 1).is_err());
        assert!(parse_u64("TEST", "0", 1).is_err());
        assert!(parse_usize("TEST", "invalid", 1).is_err());
        assert!(parse_u64("TEST", "invalid", 1).is_err());
    }
}
