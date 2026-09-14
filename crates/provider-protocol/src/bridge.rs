use provider_core::{
    PreparedProviderRequest, ProtocolBridge, ProviderError, ProviderErrorKind,
    ProviderModelInputModality, ProviderRequest, ProviderStream, ProxyRequest, ResponseTranslator,
    WireFormat,
};

use crate::{claude, openai_chat, openai_responses};

#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultProtocolBridge;

impl ProtocolBridge for DefaultProtocolBridge {
    fn supports(&self, source: WireFormat, target: WireFormat) -> bool {
        source == target
            || matches!(
                (source, target),
                (
                    WireFormat::ClaudeMessages | WireFormat::OpenAiChatCompletions,
                    WireFormat::OpenAiResponses
                ) | (
                    WireFormat::OpenAiResponses,
                    WireFormat::OpenAiChatCompletions
                )
            )
    }

    fn prepare(
        &self,
        request: ProxyRequest,
        target: WireFormat,
        input_modalities: Option<&[ProviderModelInputModality]>,
    ) -> Result<PreparedProviderRequest, ProviderError> {
        let explicitly_without_image = explicitly_without_image(input_modalities);
        if request.format == target {
            let mut request = ProviderRequest::from_proxy(request, target);
            if target == WireFormat::OpenAiChatCompletions && explicitly_without_image {
                openai_chat::omit_tool_images(&mut request)?;
            }
            return Ok(PreparedProviderRequest::new(
                request,
                Box::new(IdentityResponseTranslator),
            ));
        }

        match (request.format, target) {
            (WireFormat::ClaudeMessages, WireFormat::OpenAiResponses) => {
                let (request, response) = claude::prepare_responses_request(request)?;
                Ok(PreparedProviderRequest::new(request, Box::new(response)))
            }
            (WireFormat::OpenAiChatCompletions, WireFormat::OpenAiResponses) => {
                let (request, response) = openai_chat::prepare_responses_request(request)?;
                Ok(PreparedProviderRequest::new(request, Box::new(response)))
            }
            (WireFormat::OpenAiResponses, WireFormat::OpenAiChatCompletions) => {
                let (mut request, response) = openai_responses::prepare_chat_request(request)?;
                if explicitly_without_image {
                    openai_chat::omit_tool_images(&mut request)?;
                }
                Ok(PreparedProviderRequest::new(request, Box::new(response)))
            }
            _ => Err(unsupported_conversion()),
        }
    }
}

fn explicitly_without_image(input_modalities: Option<&[ProviderModelInputModality]>) -> bool {
    input_modalities
        .is_some_and(|modalities| !modalities.contains(&ProviderModelInputModality::Image))
}

fn unsupported_conversion() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::InvalidRequest,
        "the selected provider does not support this protocol conversion",
    )
}

struct IdentityResponseTranslator;

impl ResponseTranslator for IdentityResponseTranslator {
    fn translate_stream(self: Box<Self>, stream: ProviderStream) -> ProviderStream {
        stream
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use serde_json::Value;

    use super::*;

    #[test]
    fn any_explicit_modality_set_without_image_omits_tool_images() {
        assert!(explicitly_without_image(Some(&[
            ProviderModelInputModality::Audio,
            ProviderModelInputModality::Pdf,
        ])));
        assert!(!explicitly_without_image(Some(&[
            ProviderModelInputModality::Video,
            ProviderModelInputModality::Image,
        ])));
        assert!(!explicitly_without_image(None));
    }

    #[test]
    fn supports_both_openai_conversion_directions() {
        let bridge = DefaultProtocolBridge;
        assert!(bridge.supports(
            WireFormat::OpenAiChatCompletions,
            WireFormat::OpenAiChatCompletions
        ));
        assert!(bridge.supports(WireFormat::OpenAiResponses, WireFormat::OpenAiResponses));
        assert!(bridge.supports(
            WireFormat::OpenAiResponses,
            WireFormat::OpenAiChatCompletions
        ));
        assert!(bridge.supports(
            WireFormat::OpenAiChatCompletions,
            WireFormat::OpenAiResponses
        ));
        assert!(!bridge.supports(WireFormat::OpenAiResponses, WireFormat::ClaudeMessages));
        assert!(bridge.supports(WireFormat::ClaudeMessages, WireFormat::OpenAiResponses));
        assert!(!bridge.supports(
            WireFormat::ClaudeMessages,
            WireFormat::OpenAiChatCompletions
        ));
    }

    #[test]
    fn responses_tool_images_are_omitted_for_text_only_chat_models() {
        let request = ProxyRequest::new(
            WireFormat::OpenAiResponses,
            "model",
            Bytes::from_static(
                br#"{"model":"model","input":[{"type":"function_call","call_id":"call_1","name":"inspect","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":[{"type":"input_text","text":"result"},{"type":"input_image","image_url":"data:image/png;base64,a"}]}],"tools":[{"type":"function","name":"inspect"}]}"#,
            ),
        )
        .expect("Responses request");
        let prepared = DefaultProtocolBridge
            .prepare(
                request,
                WireFormat::OpenAiChatCompletions,
                Some(&[ProviderModelInputModality::Text]),
            )
            .expect("converted request");
        let (request, _) = prepared.into_parts();
        let body: Value = serde_json::from_slice(&request.payload).expect("Chat request JSON");

        assert_eq!(
            body["messages"][1]["content"],
            "result\n\n[image omitted: unsupported by upstream]"
        );
    }
}
