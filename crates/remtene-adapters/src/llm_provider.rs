//! LLM provider implementation for text processing.

use remtene_application::LLM_CONTRACT_VERSION;
use remtene_application::ports::{
    LlmProvider, LlmRouteCandidate, LlmRouteResolution, PortError, PortFuture, ResolvedLlmRoute,
    TextProcessingRequest, TextProcessingResult,
};
use remtene_domain::IntentDecision;
use serde::Deserialize;

use crate::llm::resolve_openai_compatible_route;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum CanonicalIntent {
    Dictation,
    TextCommand,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalLlmResponse {
    schema_version: u16,
    intent: CanonicalIntent,
    final_text: String,
}

/// Parses the model-generated content after a concrete Provider Adapter has
/// removed its own HTTP response envelope.
///
/// The parser deliberately accepts one JSON object only. It never guesses a
/// JSON boundary inside Markdown or natural language and never includes raw
/// Provider content in returned errors.
pub fn parse_canonical_response(
    session_id: remtene_domain::SessionId,
    request_id: remtene_domain::RequestId,
    content: &str,
) -> Result<TextProcessingResult, PortError> {
    let response: CanonicalLlmResponse =
        serde_json::from_str(content).map_err(|_| invalid_response_error())?;
    if response.schema_version != LLM_CONTRACT_VERSION || response.final_text.trim().is_empty() {
        return Err(invalid_response_error());
    }

    Ok(TextProcessingResult {
        session_id,
        request_id,
        intent: match response.intent {
            CanonicalIntent::Dictation => IntentDecision::Dictation,
            CanonicalIntent::TextCommand => IntentDecision::TextCommand,
        },
        final_text: response.final_text,
    })
}

fn invalid_response_error() -> PortError {
    PortError {
        code: "llm.invalid_response".to_owned(),
        safe_message_key: "llm.invalid_response".to_owned(),
        retryable: false,
    }
}

/// Fail-closed LLM provider used when the concrete Provider cannot initialize.
///
/// It deliberately never returns the ASR transcript as a successful LLM
/// result. That distinction lets the Orchestrator preserve the raw transcript
/// through its explicit fallback path instead of falsely reporting AI success.
pub struct UnavailableLlmProvider {
    error: PortError,
}

impl UnavailableLlmProvider {
    #[must_use]
    pub fn new() -> Self {
        Self {
            error: PortError {
                code: "llm.provider_unavailable".to_owned(),
                safe_message_key: "llm.provider_unavailable".to_owned(),
                retryable: true,
            },
        }
    }

    #[must_use]
    pub const fn from_error(error: PortError) -> Self {
        Self { error }
    }
}

impl Default for UnavailableLlmProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl LlmProvider for UnavailableLlmProvider {
    fn resolve_route(
        &self,
        candidate: Option<LlmRouteCandidate>,
    ) -> PortFuture<'_, LlmRouteResolution> {
        let error = self.error.clone();
        Box::pin(async move {
            let Some(candidate) = candidate else {
                return LlmRouteResolution::NoConfiguration;
            };
            match resolve_openai_compatible_route(&candidate) {
                Ok(route) => LlmRouteResolution::Unavailable {
                    route: Some(route),
                    error,
                },
                Err(error) => LlmRouteResolution::Unavailable { route: None, error },
            }
        })
    }

    fn process(
        &self,
        _route: ResolvedLlmRoute,
        _request: TextProcessingRequest,
    ) -> PortFuture<'_, Result<TextProcessingResult, PortError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }

    fn cancel(
        &self,
        _request_id: remtene_domain::RequestId,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remtene_domain::{ProcessingMode, RequestId, SessionId};

    fn route() -> ResolvedLlmRoute {
        ResolvedLlmRoute::new(
            "primary",
            "https://provider.invalid/v1/chat/completions",
            "test-model",
            "llm.openai_compatible.fingerprint",
        )
    }

    #[test]
    fn unavailable_provider_never_passes_raw_text_through_as_success() {
        let provider = UnavailableLlmProvider::new();
        let request = TextProcessingRequest {
            session_id: SessionId::new(),
            request_id: RequestId::new(),
            processing_mode: ProcessingMode::Faithful,
            raw_transcript: "hello world".to_owned(),
            selected_text: None,
        };

        let error = futures::executor::block_on(provider.process(route(), request))
            .expect_err("unavailable provider must fail closed");
        assert_eq!(error.code, "llm.provider_unavailable");
        assert!(error.retryable);
    }

    #[test]
    fn unavailable_provider_reports_the_same_safe_state_during_resolution() {
        let provider = UnavailableLlmProvider::new();
        let request_id = RequestId::new();

        let resolution = futures::executor::block_on(provider.resolve_route(Some(
            LlmRouteCandidate::new("https://provider.invalid/v1", "test-model"),
        )));
        assert!(matches!(
            resolution,
            LlmRouteResolution::Unavailable {
                route: Some(_),
                error,
            }
                if error.code == "llm.provider_unavailable" && error.retryable
        ));
        futures::executor::block_on(provider.cancel(request_id))
            .expect("cancel remains idempotent");
    }

    #[test]
    fn strict_parser_accepts_only_the_canonical_closed_object() {
        let session_id = SessionId::new();
        let request_id = RequestId::new();
        let result = parse_canonical_response(
            session_id,
            request_id,
            r#"{"schema_version":1,"intent":"text_command","final_text":"Hello"}"#,
        )
        .expect("canonical response");

        assert_eq!(result.session_id, session_id);
        assert_eq!(result.request_id, request_id);
        assert_eq!(result.intent, IntentDecision::TextCommand);
        assert_eq!(result.final_text, "Hello");
    }

    #[test]
    fn strict_parser_rejects_extra_fields_and_edit_actions() {
        for content in [
            r#"{"schema_version":1,"intent":"dictation","final_text":"ok","action":"replace"}"#,
            r#"{"schema_version":1,"intent":"dictation","final_text":"ok","target":"selection"}"#,
            r#"{"schema_version":1,"intent":"dictation","final_text":"ok","tool_calls":[]}"#,
        ] {
            let error =
                parse_canonical_response(SessionId::new(), RequestId::new(), content).unwrap_err();
            assert_eq!(error.code, "llm.invalid_response");
            assert!(!error.retryable);
        }
    }

    #[test]
    fn strict_parser_rejects_fences_commentary_and_trailing_objects() {
        for content in [
            "```json\n{\"schema_version\":1,\"intent\":\"dictation\",\"final_text\":\"ok\"}\n```",
            "Result: {\"schema_version\":1,\"intent\":\"dictation\",\"final_text\":\"ok\"}",
            "{\"schema_version\":1,\"intent\":\"dictation\",\"final_text\":\"ok\"} trailing",
            "{\"schema_version\":1,\"intent\":\"dictation\",\"final_text\":\"ok\"}\n{}",
        ] {
            assert!(parse_canonical_response(SessionId::new(), RequestId::new(), content).is_err());
        }
    }

    #[test]
    fn strict_parser_rejects_wrong_version_intent_and_empty_text() {
        for content in [
            r#"{"schema_version":2,"intent":"dictation","final_text":"ok"}"#,
            r#"{"schema_version":1,"intent":"replace","final_text":"ok"}"#,
            r#"{"schema_version":1,"intent":"dictation","final_text":"  \n "}"#,
            r#"{"intent":"dictation","final_text":"ok"}"#,
        ] {
            assert!(parse_canonical_response(SessionId::new(), RequestId::new(), content).is_err());
        }
    }

    #[test]
    fn strict_parser_never_copies_provider_content_into_the_error() {
        let secret_marker = "PROVIDER_RAW_SECRET_MARKER";
        let error = parse_canonical_response(SessionId::new(), RequestId::new(), secret_marker)
            .unwrap_err();

        assert!(!error.code.contains(secret_marker));
        assert!(!error.safe_message_key.contains(secret_marker));
    }
}
