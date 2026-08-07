//! Versioned DTOs for Renderer IPC and the local ASR Worker protocol.

mod error;
mod ui;
mod worker;

pub use error::{AppError, ErrorCategory, ErrorSeverity};
pub use ui::{
    AppSnapshot, AsrReadiness, AutostartStatusView, CommandAccepted, ControlPanelNavigationEvent,
    ControlPanelNavigationTarget, DeleteLlmApiKeyCommand, HistoryClearAllCommand,
    HistoryClearAllResult, HistoryCopyCommand, HistoryCopyResult, HistoryPage, HistoryPolicyView,
    HistoryQuery, HistoryRecordView, LifecycleState, LlmApiKeyMutationResult, LlmApiKeyState,
    LlmApiKeyStatusView, LlmConnectionTestErrorCode, LlmConnectionTestResult,
    LlmConnectionTestStatus, LlmSettingsView, LlmTestConnectionCommand, LlmUpstreamErrorView,
    LocalAsrModel, MicrophonePermission, ModelSummary, ProcessingModeView, RecordingModeView,
    ResetUnrecoverableLlmSecretsCommand, RevealLlmApiKeyCommand, RevealLlmApiKeyResult,
    SecretStorageKind, SessionAccepted, SessionCommand, SessionPhaseView, SessionPublicSnapshot,
    SessionTerminalEvent, SessionTerminalOutcomeView, SessionUserState, SetAutoCopyResultCommand,
    SetAutoCopyResultResult, SetAutostartCommand, SetAutostartResult, SetHistoryEnabledCommand,
    SetHistoryEnabledResult, SetHistoryLimitCommand, SetHistoryLimitResult,
    SetHistoryRetentionCommand, SetHistoryRetentionResult, SetLlmApiKeyCommand,
    SetLlmSettingsCommand, SetLlmSettingsResult, SetLocalDiagnosticsCommand,
    SetLocalDiagnosticsResult, SetRecordingPreferencesCommand, SetRecordingPreferencesResult,
    SetRecordingShortcutCommand, SetRecordingShortcutResult, SetTextProcessingSettingsCommand,
    SetTextProcessingSettingsResult, SettingsView, StartRecordingCommand, SystemPermission,
    UserNotification, UserNotificationCode,
};
pub use worker::{
    AudioArtifactId, AudioArtifactIdError, AudioFormatDto, CancelRequest, CancelledResult,
    CoreHello, CoreToWorkerEnvelope, CoreToWorkerMessage, HealthCheckRequest, HealthResult,
    HealthStatus, ProtocolDirection, ShutdownComplete, ShutdownRequest, TranscribeRequest,
    TranscriptResult, WorkerCapability, WorkerEngineId, WorkerError, WorkerErrorCode,
    WorkerMessageKind, WorkerProtocolError, WorkerProtocolPhase, WorkerProtocolState, WorkerReady,
    WorkerToCoreEnvelope, WorkerToCoreMessage,
};

pub const CONTRACT_VERSION: u16 = 1;
