use thiserror::Error;

use crate::{AsrEngine, SessionId, SettingsSnapshot, TimestampMs};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPhase {
    Preparing,
    Recording,
    Recognizing,
    Processing,
    Delivering,
    Finalizing,
    Terminated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RejectReason {
    SecureInput,
    SelectionTooLong,
    PermissionUnavailable,
    AsrUnavailable,
    RecordingHudUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureCategory {
    Audio,
    Asr,
    Llm,
    Delivery,
    Storage,
    Lifecycle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalOutcome {
    Completed,
    Cancelled,
    Rejected(RejectReason),
    Failed(FailureCategory),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    PreflightPassed,
    Reject(RejectReason),
    FinishRecording,
    CancelRecording,
    NoSpeech,
    BeginProcessing,
    BeginDelivery,
    BeginFinalizing,
    Complete,
    Fail(FailureCategory),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionEffect {
    Applied,
    IgnoredDuplicate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptionSession {
    id: SessionId,
    phase: SessionPhase,
    created_at: TimestampMs,
    settings: SettingsSnapshot,
    selected_asr_engine: AsrEngine,
    terminal_outcome: Option<TerminalOutcome>,
}

impl TranscriptionSession {
    #[must_use]
    pub const fn new(
        id: SessionId,
        created_at: TimestampMs,
        settings: SettingsSnapshot,
        selected_asr_engine: AsrEngine,
    ) -> Self {
        Self {
            id,
            phase: SessionPhase::Preparing,
            created_at,
            settings,
            selected_asr_engine,
            terminal_outcome: None,
        }
    }

    #[must_use]
    pub const fn id(&self) -> SessionId {
        self.id
    }

    #[must_use]
    pub const fn phase(&self) -> SessionPhase {
        self.phase
    }

    #[must_use]
    pub const fn created_at(&self) -> TimestampMs {
        self.created_at
    }

    #[must_use]
    pub const fn settings(&self) -> &SettingsSnapshot {
        &self.settings
    }

    #[must_use]
    pub const fn selected_asr_engine(&self) -> AsrEngine {
        self.selected_asr_engine
    }

    #[must_use]
    pub const fn terminal_outcome(&self) -> Option<TerminalOutcome> {
        self.terminal_outcome
    }

    #[must_use]
    pub const fn is_terminated(&self) -> bool {
        matches!(self.phase, SessionPhase::Terminated)
    }

    pub fn apply(&mut self, event: SessionEvent) -> Result<TransitionEffect, TransitionError> {
        if self.is_terminated() {
            return Ok(TransitionEffect::IgnoredDuplicate);
        }

        let next = match (self.phase, event) {
            (SessionPhase::Preparing, SessionEvent::PreflightPassed) => SessionPhase::Recording,
            (SessionPhase::Preparing, SessionEvent::Reject(reason)) => {
                return Ok(self.terminate(TerminalOutcome::Rejected(reason)));
            }
            (SessionPhase::Recording, SessionEvent::FinishRecording) => SessionPhase::Recognizing,
            (SessionPhase::Recording, SessionEvent::CancelRecording) => {
                return Ok(self.terminate(TerminalOutcome::Cancelled));
            }
            (SessionPhase::Recognizing, SessionEvent::NoSpeech) => {
                return Ok(self.terminate(TerminalOutcome::Cancelled));
            }
            (SessionPhase::Recognizing, SessionEvent::BeginProcessing) => SessionPhase::Processing,
            (SessionPhase::Recognizing | SessionPhase::Processing, SessionEvent::BeginDelivery) => {
                SessionPhase::Delivering
            }
            (SessionPhase::Delivering, SessionEvent::BeginFinalizing) => SessionPhase::Finalizing,
            (SessionPhase::Finalizing, SessionEvent::Complete) => {
                return Ok(self.terminate(TerminalOutcome::Completed));
            }
            (_, SessionEvent::Fail(category)) => {
                return Ok(self.terminate(TerminalOutcome::Failed(category)));
            }
            (SessionPhase::Recognizing, SessionEvent::FinishRecording) => {
                return Ok(TransitionEffect::IgnoredDuplicate);
            }
            (phase, invalid_event) => {
                return Err(TransitionError {
                    phase,
                    event: invalid_event,
                });
            }
        };

        self.phase = next;
        Ok(TransitionEffect::Applied)
    }

    fn terminate(&mut self, outcome: TerminalOutcome) -> TransitionEffect {
        self.phase = SessionPhase::Terminated;
        self.terminal_outcome = Some(outcome);
        TransitionEffect::Applied
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("event {event:?} is invalid while session is in phase {phase:?}")]
pub struct TransitionError {
    pub phase: SessionPhase,
    pub event: SessionEvent,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{
        AsrPreference, HistoryPolicy, LlmNonSecretSettings, ProcessingMode, RecordingMode,
        SettingsSnapshotInput,
    };

    fn settings(mode: ProcessingMode) -> SettingsSnapshot {
        SettingsSnapshot::new(SettingsSnapshotInput {
            version: 1,
            recording_mode: RecordingMode::Toggle,
            max_recording_duration: Duration::from_secs(600),
            recording_shortcut: None,
            processing_mode: mode,
            asr_preference: AsrPreference::Qwen,
            llm: Some(
                LlmNonSecretSettings::new("https://provider.example/v1", "user-model")
                    .expect("test LLM settings must be valid"),
            ),
            read_selected_text: true,
            clipboard_bridge_allowed: false,
            auto_copy_result: false,
            local_diagnostics_enabled: true,
            history_policy: HistoryPolicy::default(),
        })
        .expect("test settings must be valid")
    }

    fn session(mode: ProcessingMode) -> TranscriptionSession {
        TranscriptionSession::new(
            SessionId::new(),
            TimestampMs::new(1),
            settings(mode),
            AsrEngine::Qwen,
        )
    }

    #[test]
    fn happy_path_reaches_completed_terminal_outcome() {
        let mut session = session(ProcessingMode::Faithful);

        for event in [
            SessionEvent::PreflightPassed,
            SessionEvent::FinishRecording,
            SessionEvent::BeginProcessing,
            SessionEvent::BeginDelivery,
            SessionEvent::BeginFinalizing,
            SessionEvent::Complete,
        ] {
            assert_eq!(session.apply(event), Ok(TransitionEffect::Applied));
        }

        assert_eq!(session.phase(), SessionPhase::Terminated);
        assert_eq!(session.terminal_outcome(), Some(TerminalOutcome::Completed));
    }

    #[test]
    fn invalid_event_does_not_change_phase() {
        let mut session = session(ProcessingMode::Faithful);

        let result = session.apply(SessionEvent::BeginDelivery);

        assert!(matches!(result, Err(TransitionError { .. })));
        assert_eq!(session.phase(), SessionPhase::Preparing);
    }

    #[test]
    fn finish_and_terminal_events_are_idempotent() {
        let mut session = session(ProcessingMode::Raw);
        session
            .apply(SessionEvent::PreflightPassed)
            .expect("preflight should pass");
        session
            .apply(SessionEvent::FinishRecording)
            .expect("first finish should apply");

        assert_eq!(
            session.apply(SessionEvent::FinishRecording),
            Ok(TransitionEffect::IgnoredDuplicate)
        );
        session
            .apply(SessionEvent::BeginDelivery)
            .expect("raw mode may deliver directly");
        session
            .apply(SessionEvent::BeginFinalizing)
            .expect("delivery should finalize");
        session
            .apply(SessionEvent::Complete)
            .expect("finalization should complete");
        assert_eq!(
            session.apply(SessionEvent::Fail(FailureCategory::Lifecycle)),
            Ok(TransitionEffect::IgnoredDuplicate)
        );
        assert_eq!(session.terminal_outcome(), Some(TerminalOutcome::Completed));
    }

    #[test]
    fn cancel_is_only_valid_during_recording() {
        let mut session = session(ProcessingMode::Faithful);
        assert!(session.apply(SessionEvent::CancelRecording).is_err());
        session
            .apply(SessionEvent::PreflightPassed)
            .expect("preflight should pass");
        assert_eq!(
            session.apply(SessionEvent::CancelRecording),
            Ok(TransitionEffect::Applied)
        );
        assert_eq!(session.terminal_outcome(), Some(TerminalOutcome::Cancelled));
    }

    #[test]
    fn no_speech_is_a_benign_terminal_result_only_after_recording_finishes() {
        let mut session = session(ProcessingMode::Faithful);
        assert!(session.apply(SessionEvent::NoSpeech).is_err());
        session
            .apply(SessionEvent::PreflightPassed)
            .expect("preflight should pass");
        assert!(session.apply(SessionEvent::NoSpeech).is_err());
        session
            .apply(SessionEvent::FinishRecording)
            .expect("recording should finish");

        assert_eq!(
            session.apply(SessionEvent::NoSpeech),
            Ok(TransitionEffect::Applied)
        );
        assert_eq!(session.phase(), SessionPhase::Terminated);
        assert_eq!(session.terminal_outcome(), Some(TerminalOutcome::Cancelled));
    }
}
