//! Product rules that must not depend on Tauri, operating systems, providers, or model runtimes.

mod ids;
mod session;
mod settings;

pub use ids::{DeliveryId, RequestId, SessionId};
pub use session::{
    FailureCategory, RejectReason, SessionEvent, SessionPhase, TerminalOutcome,
    TranscriptionSession, TransitionEffect, TransitionError,
};
pub use settings::{
    AsrEngine, AsrPreference, HistoryPolicy, IntentDecision, LlmNonSecretSettings, ProcessingMode,
    RecordingMode, RecordingShortcut, SettingsSnapshot, SettingsSnapshotInput,
    SettingsValidationError, TimestampMs,
};
