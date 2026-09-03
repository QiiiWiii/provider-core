use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{LazyLock, Mutex, PoisonError},
    time::Duration,
};

use provider_core::{
    BoundedBodyError, DiscoveredProviderModel, ProviderError, ProviderErrorKind, ProviderModel,
    ProviderModelInputModality, collect_bounded_body,
};
use secrecy::ExposeSecret;
use serde_json::{Map, Value};

use super::{
    contract::{API_BASE_URL, DAILY_API_BASE_URL},
    credentials::AntigravityCredentials,
    version,
};

const FETCH_AVAILABLE_MODELS_PATH: &str = "/v1internal:fetchAvailableModels";
const MAX_RESPONSE_SIZE: usize = 256 * 1024;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const ALIAS_SUFFIXES: &[&str] = &[
    "-thinking",
    "-agent",
    "-image",
    "-lite",
    "-extra-low",
    "-tiered",
    "-high",
    "-medium",
    "-low",
];

struct ModelDefinition {
    id: &'static str,
    modalities: &'static [ProviderModelInputModality],
}

const TEXT: &[ProviderModelInputModality] = &[ProviderModelInputModality::Text];
const TEXT_IMAGE: &[ProviderModelInputModality] = &[
    ProviderModelInputModality::Text,
    ProviderModelInputModality::Image,
];
const TEXT_IMAGE_AUDIO_VIDEO: &[ProviderModelInputModality] = &[
    ProviderModelInputModality::Text,
    ProviderModelInputModality::Image,
    ProviderModelInputModality::Audio,
    ProviderModelInputModality::Video,
];

const MODEL_DEFINITIONS: &[ModelDefinition] = &[
    ModelDefinition {
        id: "claude-opus-4-6-thinking",
        modalities: TEXT_IMAGE,
    },
    ModelDefinition {
        id: "claude-sonnet-4-6",
        modalities: TEXT_IMAGE,
    },
    ModelDefinition {
        id: "gemini-3.6-flash-high",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
    ModelDefinition {
        id: "gemini-3.7-flash-high",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
    ModelDefinition {
        id: "gemini-3-flash",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
    ModelDefinition {
        id: "gemini-3-flash-agent",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
    ModelDefinition {
        id: "gemini-3.1-flash-image",
        modalities: TEXT_IMAGE,
    },
    ModelDefinition {
        id: "gemini-pro-agent",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
    ModelDefinition {
        id: "gemini-3.1-pro-low",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
    ModelDefinition {
        id: "gpt-oss-120b-medium",
        modalities: TEXT,
    },
    ModelDefinition {
        id: "gemini-3.1-flash-lite",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
    ModelDefinition {
        id: "gemini-3.5-flash-low",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
    ModelDefinition {
        id: "gemini-3.5-flash-extra-low",
        modalities: TEXT_IMAGE_AUDIO_VIDEO,
    },
];

static MODELS: LazyLock<Vec<ProviderModel>> = LazyLock::new(|| {
    MODEL_DEFINITIONS
        .iter()
        .map(|definition| {
            ProviderModel::new(definition.id, "antigravity")
                .with_input_modalities(Some(definition.modalities.to_vec()))
        })
        .collect()
});

pub(crate) fn antigravity_models() -> &'static [ProviderModel] {
    &MODELS
}

pub(crate) fn model_pricing_alias(model: &str) -> Option<&'static str> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    compute_pricing_alias(model).map(intern_alias)
}

fn compute_pricing_alias(model: &str) -> Option<String> {
    ALIAS_SUFFIXES.iter().find_map(|suffix| {
        model
            .strip_suffix(suffix)
            .filter(|base| !base.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn intern_alias(value: String) -> &'static str {
    static CACHE: LazyLock<Mutex<HashMap<String, &'static str>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(existing) = cache.get(&value) {
        return existing;
    }
    let leaked: &'static str = Box::leak(value.clone().into_boxed_str());
    cache.insert(value, leaked);
    leaked
}

fn known_modalities(model_id: &str) -> &'static [ProviderModelInputModality] {
    MODEL_DEFINITIONS
        .iter()
        .find(|definition| definition.id == model_id)
        .map(|definition| definition.modalities)
        .unwrap_or_else(|| infer_modalities(model_id))
}

fn infer_modalities(model_id: &str) -> &'static [ProviderModelInputModality] {
    let lower = model_id.to_ascii_lowercase();
    if lower.contains("gpt-oss") || lower.starts_with("gpt-") {
        TEXT
    } else if lower.contains("claude") || lower.contains("image") {
        TEXT_IMAGE
    } else {
        TEXT_IMAGE_AUDIO_VIDEO
    }
}

fn discovered_model(model_id: &str, display_name: Option<&str>) -> DiscoveredProviderModel {
    let modalities = known_modalities(model_id);
    let mut metadata = serde_json::json!({
        "id": model_id,
        "object": "model",
        "owned_by": "antigravity",
        "input_modalities": modalities,
        "visibility": "list",
        "provider": "antigravity",
        "official_client_contract": {
            "endpoint": "cloudcode_stream_generate_content",
            "status": "verified"
        }
    });
    if let Some(display_name) = display_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        metadata["display_name"] = Value::String(display_name.to_owned());
    }
    DiscoveredProviderModel {
        upstream_model: model_id.to_owned(),
        input_modalities: Some(modalities.to_vec()),
        metadata_json: metadata.to_string(),
        routable: true,
        pricing: None,
    }
}

pub(crate) fn parse_discovered_models(payload: &Value) -> Vec<DiscoveredProviderModel> {
    let Some(models) = payload_models(payload) else {
        return Vec::new();
    };
    let excluded = excluded_model_ids(payload);
    let allowlist = allowlisted_model_ids(payload);
    let mut discovered = BTreeMap::new();
    for (model_id, model) in models {
        let model_id = model_id.trim();
        if model_id.is_empty()
            || is_internal_model_id(model_id)
            || excluded.contains(model_id)
            || is_non_conversation_model_id(model_id)
            || allowlist
                .as_ref()
                .is_some_and(|allowlist| !allowlist.contains(model_id))
        {
            continue;
        }
        let display_name = model.as_object().and_then(|object| {
            object_string(object, "displayName").or_else(|| object_string(object, "label"))
        });
        discovered.insert(
            model_id.to_owned(),
            discovered_model(model_id, display_name),
        );
    }
    discovered.into_values().collect()
}

fn payload_models(payload: &Value) -> Option<&Map<String, Value>> {
    payload
        .get("models")
        .and_then(Value::as_object)
        .or_else(|| {
            payload
                .get("response")
                .and_then(Value::as_object)
                .and_then(|response| response.get("models"))
                .and_then(Value::as_object)
        })
}

fn excluded_model_ids(payload: &Value) -> HashSet<String> {
    let mut excluded = HashSet::new();
    collect_string_ids(payload.get("deprecatedModelIds"), &mut excluded);
    collect_string_ids(payload.get("tabModelIds"), &mut excluded);
    if let Some(response) = payload.get("response") {
        collect_string_ids(response.get("deprecatedModelIds"), &mut excluded);
        collect_string_ids(response.get("tabModelIds"), &mut excluded);
    }
    excluded
}

fn collect_string_ids(value: Option<&Value>, ids: &mut HashSet<String>) {
    match value {
        Some(Value::Array(values)) => {
            for value in values {
                if let Some(id) = value.as_str().map(str::trim).filter(|id| !id.is_empty()) {
                    ids.insert(id.to_owned());
                }
            }
        }
        Some(Value::Object(map)) => {
            for key in map.keys() {
                let key = key.trim();
                if !key.is_empty() {
                    ids.insert(key.to_owned());
                }
            }
        }
        _ => {}
    }
}

fn is_internal_model_id(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    lower.starts_with("tab_") || lower.starts_with("chat_")
}

fn is_non_conversation_model_id(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    lower.starts_with("gemini-2.5-") || lower.ends_with("-tiered")
}

fn allowlisted_model_ids(payload: &Value) -> Option<HashSet<String>> {
    let mut ids = HashSet::new();
    collect_allowlist_ids(payload.get("agentModelSorts"), &mut ids);
    collect_allowlist_ids(payload.get("imageGenerationModelIds"), &mut ids);
    if let Some(response) = payload.get("response") {
        collect_allowlist_ids(response.get("agentModelSorts"), &mut ids);
        collect_allowlist_ids(response.get("imageGenerationModelIds"), &mut ids);
    }
    (!ids.is_empty()).then_some(ids)
}

fn collect_allowlist_ids(value: Option<&Value>, ids: &mut HashSet<String>) {
    match value {
        Some(Value::String(id)) => {
            let id = id.trim();
            if !id.is_empty() {
                ids.insert(id.to_owned());
            }
        }
        Some(Value::Array(values)) => {
            for value in values {
                collect_allowlist_ids(Some(value), ids);
            }
        }
        Some(Value::Object(object)) => {
            collect_allowlist_ids(object.get("modelIds"), ids);
            collect_allowlist_ids(object.get("model_ids"), ids);
            collect_allowlist_ids(object.get("groups"), ids);
            for key in ["id", "model", "modelId", "model_id"] {
                if let Some(id) = object_string(object, key)
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                {
                    ids.insert(id.to_owned());
                }
            }
        }
        _ => {}
    }
}

fn object_string<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

fn project_request_body(project_id: &str) -> Value {
    let project_id = project_id.trim();
    if project_id.is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::json!({"project": project_id})
    }
}

#[derive(Clone)]
pub(crate) struct AntigravityModelClient {
    http: reqwest::Client,
    base_urls: Vec<String>,
}

impl AntigravityModelClient {
    pub(crate) fn new() -> Result<Self, reqwest::Error> {
        Self::with_base_urls([DAILY_API_BASE_URL.to_owned(), API_BASE_URL.to_owned()])
    }

    pub(crate) fn with_base_urls<const N: usize>(
        base_urls: [String; N],
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            http: reqwest::Client::builder().http1_only().build()?,
            base_urls: base_urls
                .into_iter()
                .map(|value| value.trim_end_matches('/').to_owned())
                .filter(|value| !value.is_empty())
                .collect(),
        })
    }

    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn with_base_url(base_url: impl Into<String>) -> Result<Self, reqwest::Error> {
        Self::with_base_urls([base_url.into()])
    }

    pub(crate) async fn discover(
        &self,
        credentials: &AntigravityCredentials,
    ) -> Result<Vec<DiscoveredProviderModel>, ProviderError> {
        let mut merged = BTreeMap::new();
        let mut last_error = None;
        for base_url in &self.base_urls {
            match self.fetch_payload(base_url, credentials).await {
                Ok(payload) => {
                    for model in parse_discovered_models(&payload) {
                        merged.entry(model.upstream_model.clone()).or_insert(model);
                    }
                }
                Err(error) => last_error = Some(error),
            }
        }
        if merged.is_empty() {
            return Err(last_error.unwrap_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::Upstream,
                    "Antigravity model discovery returned no routable models",
                )
            }));
        }
        Ok(merged.into_values().collect())
    }

    async fn fetch_payload(
        &self,
        base_url: &str,
        credentials: &AntigravityCredentials,
    ) -> Result<Value, ProviderError> {
        let body =
            serde_json::to_vec(&project_request_body(credentials.project_id())).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Internal,
                    "failed to encode Antigravity model discovery request",
                )
            })?;
        let response = self
            .http
            .post(format!("{base_url}{FETCH_AVAILABLE_MODELS_PATH}"))
            .timeout(DISCOVERY_TIMEOUT)
            .header(reqwest::header::ACCEPT, "*/*")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::USER_AGENT, version::user_agent())
            .bearer_auth(credentials.access_token().expose_secret())
            .body(body)
            .send()
            .await
            .map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::Upstream,
                    "Antigravity model discovery request failed",
                )
            })?;
        let status = response.status();
        if !status.is_success() {
            let kind = match status.as_u16() {
                401 | 403 => ProviderErrorKind::Authentication,
                429 => ProviderErrorKind::RateLimited,
                _ => ProviderErrorKind::Upstream,
            };
            return Err(ProviderError::new(
                kind,
                format!("Antigravity model discovery returned HTTP {status}"),
            )
            .with_upstream_status(status.as_u16()));
        }
        let body = collect_bounded_body(response.bytes_stream(), MAX_RESPONSE_SIZE)
            .await
            .map_err(|error| match error {
                BoundedBodyError::Read(_) => ProviderError::new(
                    ProviderErrorKind::Upstream,
                    "failed to read Antigravity model discovery response",
                ),
                BoundedBodyError::TooLarge => ProviderError::new(
                    ProviderErrorKind::Upstream,
                    "Antigravity model discovery response was too large",
                ),
            })?;
        serde_json::from_slice::<Value>(&body).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Upstream,
                "Antigravity model discovery returned invalid JSON",
            )
        })
    }
}

#[cfg(test)]
#[path = "models_tests.rs"]
mod tests;
