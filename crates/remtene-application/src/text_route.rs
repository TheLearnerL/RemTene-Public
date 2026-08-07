use remtene_domain::ProcessingMode;

use crate::ports::{LlmRouteResolution, ResolvedLlmRoute};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectDeliveryReason {
    RawMode,
    LlmNotConfigured,
    LlmUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TextProcessingRoute {
    DirectAsr(DirectDeliveryReason),
    Llm(ResolvedLlmRoute),
}

#[must_use]
pub fn text_processing_route(
    processing_mode: ProcessingMode,
    llm_resolution: LlmRouteResolution,
) -> TextProcessingRoute {
    match processing_mode {
        ProcessingMode::Raw => TextProcessingRoute::DirectAsr(DirectDeliveryReason::RawMode),
        ProcessingMode::Faithful | ProcessingMode::Structured => match llm_resolution {
            LlmRouteResolution::Ready(route) => TextProcessingRoute::Llm(route),
            LlmRouteResolution::NoConfiguration | LlmRouteResolution::MissingSecret(_) => {
                TextProcessingRoute::DirectAsr(DirectDeliveryReason::LlmNotConfigured)
            }
            LlmRouteResolution::Unavailable { .. } => {
                TextProcessingRoute::DirectAsr(DirectDeliveryReason::LlmUnavailable)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::ports::{PortError, ResolvedLlmRoute};

    use super::*;

    fn route() -> ResolvedLlmRoute {
        ResolvedLlmRoute::new(
            "primary",
            "https://provider.invalid/v1/chat/completions",
            "user-model",
            "llm.openai_compatible.fingerprint",
        )
    }

    #[test]
    fn raw_mode_always_short_circuits_even_a_ready_llm_route() {
        assert_eq!(
            text_processing_route(ProcessingMode::Raw, LlmRouteResolution::Ready(route())),
            TextProcessingRoute::DirectAsr(DirectDeliveryReason::RawMode)
        );
    }

    #[test]
    fn ai_modes_distinguish_missing_unavailable_and_ready_routes() {
        assert_eq!(
            text_processing_route(
                ProcessingMode::Structured,
                LlmRouteResolution::NoConfiguration
            ),
            TextProcessingRoute::DirectAsr(DirectDeliveryReason::LlmNotConfigured)
        );
        assert_eq!(
            text_processing_route(
                ProcessingMode::Structured,
                LlmRouteResolution::MissingSecret(route())
            ),
            TextProcessingRoute::DirectAsr(DirectDeliveryReason::LlmNotConfigured)
        );
        assert_eq!(
            text_processing_route(
                ProcessingMode::Faithful,
                LlmRouteResolution::Unavailable {
                    route: Some(route()),
                    error: PortError {
                        code: "llm.secret_unavailable".to_owned(),
                        safe_message_key: "llm.secret_unavailable".to_owned(),
                        retryable: true,
                    },
                }
            ),
            TextProcessingRoute::DirectAsr(DirectDeliveryReason::LlmUnavailable)
        );
        assert_eq!(
            text_processing_route(ProcessingMode::Faithful, LlmRouteResolution::Ready(route())),
            TextProcessingRoute::Llm(route())
        );
    }
}
