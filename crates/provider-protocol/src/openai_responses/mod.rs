mod history;
mod request;
mod response;
mod tools;

use std::collections::HashMap;

use provider_core::{ProviderError, ProviderRequest, ProxyRequest};

pub(crate) use response::ResponsesResponseTranslator;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ToolTarget {
    Function,
    Custom,
    LocalShell,
    ApplyPatch,
    ToolSearch,
    Namespace {
        namespace: String,
        name: String,
        custom: bool,
    },
}

pub(crate) type ToolTargets = HashMap<String, ToolTarget>;

pub(crate) fn prepare_chat_request(
    request: ProxyRequest,
) -> Result<(ProviderRequest, ResponsesResponseTranslator), ProviderError> {
    request::prepare_chat_request(request)
}
