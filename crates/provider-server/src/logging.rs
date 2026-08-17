//! Log output setup.

use tracing_subscriber::{EnvFilter, fmt};

use crate::config::{DEFAULT_LOG_FILTER, LOG_FILTER_ENV, json_log_format};

/// Installs the subscriber. Call once, before anything worth logging happens.
///
/// Until this runs, every event is dropped on the floor — including the ones
/// `axum`, `hyper`, `sqlx` and `reqwest` already emit on their own. Installing a
/// subscriber is what makes those visible; nothing else in the tree changes.
pub fn init_logging() {
    let filter = EnvFilter::try_from_env(LOG_FILTER_ENV)
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
    let builder = fmt().with_env_filter(filter);
    if json_log_format() {
        builder.json().init();
    } else {
        builder.compact().init();
    }
}
