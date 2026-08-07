use std::sync::Mutex;

use remtene_domain::{
    SessionEvent, SessionId, SettingsSnapshot, TerminalOutcome, TimestampMs, TranscriptionSession,
    TransitionEffect, TransitionError,
};
use thiserror::Error;

use crate::ResolvedAsrRoute;

#[derive(Clone, Debug)]
struct ActiveSession {
    session: TranscriptionSession,
    asr_route: ResolvedAsrRoute,
}

#[derive(Debug, Default)]
pub struct SessionCoordinator {
    active: Mutex<Option<ActiveSession>>,
}

impl SessionCoordinator {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            active: Mutex::new(None),
        }
    }

    pub fn try_start(
        &self,
        session_id: SessionId,
        created_at: TimestampMs,
        settings: SettingsSnapshot,
        asr_route: ResolvedAsrRoute,
    ) -> Result<StartSessionOutcome, CoordinatorError> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| CoordinatorError::StateLockPoisoned)?;

        if let Some(session) = active.as_ref() {
            return Ok(StartSessionOutcome::Busy {
                active_session_id: session.session.id(),
            });
        }

        *active = Some(ActiveSession {
            session: TranscriptionSession::new(
                session_id,
                created_at,
                settings,
                asr_route.engine(),
            ),
            asr_route,
        });
        Ok(StartSessionOutcome::Accepted { session_id })
    }

    pub fn active_snapshot(&self) -> Result<Option<TranscriptionSession>, CoordinatorError> {
        self.active
            .lock()
            .map(|active| active.as_ref().map(|active| active.session.clone()))
            .map_err(|_| CoordinatorError::StateLockPoisoned)
    }

    pub fn active_asr_route(&self) -> Result<Option<ResolvedAsrRoute>, CoordinatorError> {
        self.active
            .lock()
            .map(|active| active.as_ref().map(|active| active.asr_route))
            .map_err(|_| CoordinatorError::StateLockPoisoned)
    }

    pub fn apply(
        &self,
        session_id: SessionId,
        event: SessionEvent,
    ) -> Result<SessionMutation, CoordinatorError> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| CoordinatorError::StateLockPoisoned)?;
        let active_session = active.as_mut().ok_or(CoordinatorError::SessionNotFound)?;
        let session = &mut active_session.session;
        if session.id() != session_id {
            return Err(CoordinatorError::SessionNotFound);
        }

        let effect = session.apply(event)?;
        let terminal_outcome = session.terminal_outcome();
        if terminal_outcome.is_some() {
            active.take();
        }

        Ok(SessionMutation {
            effect,
            terminal_outcome,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartSessionOutcome {
    Accepted { session_id: SessionId },
    Busy { active_session_id: SessionId },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionMutation {
    pub effect: TransitionEffect,
    pub terminal_outcome: Option<TerminalOutcome>,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CoordinatorError {
    #[error("active session state lock is poisoned")]
    StateLockPoisoned,
    #[error("session was not found")]
    SessionNotFound,
    #[error(transparent)]
    InvalidTransition(#[from] TransitionError),
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Barrier},
        thread,
        time::Duration,
    };

    use remtene_domain::{
        AsrPreference, HistoryPolicy, ProcessingMode, RecordingMode, SettingsSnapshotInput,
    };

    use super::*;

    fn settings() -> SettingsSnapshot {
        SettingsSnapshot::new(SettingsSnapshotInput {
            version: 1,
            recording_mode: RecordingMode::Toggle,
            max_recording_duration: Duration::from_secs(600),
            recording_shortcut: None,
            processing_mode: ProcessingMode::Faithful,
            asr_preference: AsrPreference::Qwen,
            llm: None,
            read_selected_text: false,
            clipboard_bridge_allowed: false,
            auto_copy_result: false,
            local_diagnostics_enabled: true,
            history_policy: HistoryPolicy::default(),
        })
        .expect("test settings must be valid")
    }

    #[test]
    fn concurrent_starts_accept_exactly_one_session() {
        const CONTENDERS: usize = 8;
        let coordinator = Arc::new(SessionCoordinator::new());
        let barrier = Arc::new(Barrier::new(CONTENDERS));
        let handles = (0..CONTENDERS)
            .map(|_| {
                let coordinator = Arc::clone(&coordinator);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    coordinator
                        .try_start(
                            SessionId::new(),
                            TimestampMs::new(1),
                            settings(),
                            ResolvedAsrRoute::new(remtene_domain::AsrEngine::Qwen),
                        )
                        .expect("coordinator lock must remain healthy")
                })
            })
            .collect::<Vec<_>>();

        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().expect("contender must not panic"))
            .collect::<Vec<_>>();
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, StartSessionOutcome::Accepted { .. }))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, StartSessionOutcome::Busy { .. }))
                .count(),
            CONTENDERS - 1
        );
    }

    #[test]
    fn terminal_session_releases_single_flight_slot() {
        let coordinator = SessionCoordinator::new();
        let first_id = SessionId::new();
        coordinator
            .try_start(
                first_id,
                TimestampMs::new(1),
                settings(),
                ResolvedAsrRoute::new(remtene_domain::AsrEngine::Qwen),
            )
            .expect("start must succeed");
        coordinator
            .apply(first_id, SessionEvent::PreflightPassed)
            .expect("preflight must advance");
        coordinator
            .apply(first_id, SessionEvent::CancelRecording)
            .expect("cancel must terminate");

        let second_id = SessionId::new();
        assert_eq!(
            coordinator.try_start(
                second_id,
                TimestampMs::new(2),
                settings(),
                ResolvedAsrRoute::new(remtene_domain::AsrEngine::Whisper),
            ),
            Ok(StartSessionOutcome::Accepted {
                session_id: second_id
            })
        );
    }

    #[test]
    fn events_for_another_session_cannot_mutate_active_session() {
        let coordinator = SessionCoordinator::new();
        let active_id = SessionId::new();
        coordinator
            .try_start(
                active_id,
                TimestampMs::new(1),
                settings(),
                ResolvedAsrRoute::new(remtene_domain::AsrEngine::Qwen),
            )
            .expect("start must succeed");

        assert_eq!(
            coordinator.apply(SessionId::new(), SessionEvent::PreflightPassed),
            Err(CoordinatorError::SessionNotFound)
        );
        assert_eq!(
            coordinator
                .active_snapshot()
                .expect("snapshot lock must remain healthy")
                .expect("session should remain active")
                .phase(),
            remtene_domain::SessionPhase::Preparing
        );
    }

    #[test]
    fn resolved_asr_route_is_frozen_for_the_active_session() {
        let coordinator = SessionCoordinator::new();
        let session_id = SessionId::new();
        let route = ResolvedAsrRoute::new(remtene_domain::AsrEngine::Qwen);

        coordinator
            .try_start(session_id, TimestampMs::new(1), settings(), route)
            .expect("start must succeed");

        assert_eq!(coordinator.active_asr_route(), Ok(Some(route)));
        assert_eq!(
            coordinator
                .active_snapshot()
                .expect("snapshot lock must remain healthy")
                .expect("session should remain active")
                .selected_asr_engine(),
            remtene_domain::AsrEngine::Qwen
        );
    }
}
