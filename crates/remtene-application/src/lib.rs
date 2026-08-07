//! Application use cases and stable ports. This crate never depends on concrete adapters.

pub mod ports;

mod asr_health_controller;
mod asr_route;
mod history_controller;
mod history_settings_controller;
mod llm_configuration_controller;
mod llm_contract;
mod recording_settings_controller;
mod session_coordinator;
mod system_settings_controller;
mod text_route;
mod trace;
mod transcription_orchestrator;

pub use asr_health_controller::{
    AsrHealthCheckError, AsrHealthCheckOutcome, AsrHealthController, AsrModelSwitchError,
};
pub use asr_route::{AsrRouteError, ResolvedAsrRoute, resolve_asr_route};
pub use history_controller::{HistoryController, HistoryError};
pub use history_settings_controller::{HistorySettingsController, HistorySettingsError};
pub use llm_configuration_controller::{
    LlmApiKeyStatus, LlmConfigurationController, LlmConfigurationError, LlmConnectionFailure,
    LlmConnectionTestOutcome,
};
pub use llm_contract::{
    LLM_CONTRACT_VERSION, LLM_OUTPUT_SCHEMA_JSON, LLM_SYSTEM_PROMPT_VERSION, LlmPrompt,
    PromptContractError, compose_llm_prompt,
};
pub use ports::{LlmRouteCandidate, LlmRouteResolution, ResolvedLlmRoute};
pub use recording_settings_controller::{
    RECORDING_DURATION_OPTIONS_SECONDS, RecordingSettingsController, RecordingSettingsError,
};
pub use session_coordinator::{
    CoordinatorError, SessionCoordinator, SessionMutation, StartSessionOutcome,
};
pub use system_settings_controller::{SystemSettingsController, SystemSettingsError};
pub use text_route::{DirectDeliveryReason, TextProcessingRoute, text_processing_route};
pub use transcription_orchestrator::{
    AudioCleanupOutcome, CancelOutcome, Completion, DeliveryKind, FinalizationWarning,
    FinishOutcome, OrchestratorError, OrchestratorPorts, QuitOutcome, StartOutcome,
    TranscriptionOrchestrator,
};

/// 将应用层交付 trace 连接到 Composition Root 选定的统一 Sink。
pub fn configure_diagnostics_trace(sink: &std::sync::Arc<dyn ports::DiagnosticsSink>) {
    trace::configure(sink);
}
