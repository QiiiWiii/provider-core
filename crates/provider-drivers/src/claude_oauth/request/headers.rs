use provider_core::ProviderError;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use super::internal;

pub(super) fn insert(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &str,
) -> Result<(), ProviderError> {
    let value =
        HeaderValue::from_str(value).map_err(|_| internal("invalid Claude OAuth header"))?;
    headers.insert(HeaderName::from_static(name), value);
    Ok(())
}

pub(super) fn insert_default(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &str,
) -> Result<(), ProviderError> {
    if !headers.contains_key(name) {
        insert(headers, name, value)?;
    }
    Ok(())
}
