use provider_core::{
    PreparedProviderRequest, ProtocolBridge, ProviderError, ProviderErrorKind,
    ProviderModelInputModality, ProviderRequest, ProviderStream, ProxyRequest, ResponseTranslator,
    WireFormat,
};

use crate::{claude, openai_chat};

#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultProtocolBridge;

impl ProtocolBridge for DefaultProtocolBridge {
    fn supports(&self, source: WireFormat, target: WireFormat) -> bool {
        source == target
            || matches!(
                (source, target),
                (WireFormat::ClaudeMessages, WireFormat::OpenAiResponses)
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
    fn does_not_convert_between_openai_protocols() {
        let bridge = DefaultProtocolBridge;
        assert!(bridge.supports(
            WireFormat::OpenAiChatCompletions,
            WireFormat::OpenAiChatCompletions
        ));
        assert!(bridge.supports(WireFormat::OpenAiResponses, WireFormat::OpenAiResponses));
        assert!(!bridge.supports(
            WireFormat::OpenAiResponses,
            WireFormat::OpenAiChatCompletions
        ));
        assert!(!bridge.supports(
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
}
