use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll, Waker},
    time::Instant,
};

use futures::lock::Mutex as AsyncMutex;
use remtene_domain::{
    AsrEngine, DeliveryId, FailureCategory, RejectReason, RequestId, SessionEvent, SessionId,
    SettingsSnapshot, TerminalOutcome,
};
use thiserror::Error;

use crate::ports::TargetDisplayHint;
use crate::ports::{
    ASR_NO_SPEECH_CODE, AUDIO_EMPTY_CAPTURE_CODE, AsrEnginePort, AsrRequest, AsrResult,
    AudioCapture, AudioCaptureRef, AudioRef, ClipboardBridge, ClipboardTextWriter, Clock,
    DiagnosticEvent, DiagnosticsSink, EngineHealth, HistoryRecord, HistoryStore, IdGenerator,
    InsertOutcome, LifecycleFence, LlmProvider, MicrophoneAccess, MicrophonePermissionPort,
    OutputAdapter, PortError, RecordingCue, RecordingCuePort, RecordingDeadlineGuard,
    RecordingDeadlinePort, RecordingHudPort, RecordingHudState, SettingsStore, TargetContextPort,
    TargetRevalidation, TargetSecurity, TargetSnapshotRef, TemporaryTextOutput,
    TemporaryTextStatus, TextProcessingRequest, TextProcessingResult, UserDirectedPasteOutcome,
    UserNotificationKind, UserNotificationPort,
};
use crate::{
    CoordinatorError, DirectDeliveryReason, LlmRouteCandidate, LlmRouteResolution,
    ResolvedAsrRoute, ResolvedLlmRoute, SessionCoordinator, StartSessionOutcome,
    TextProcessingRoute, resolve_asr_route, text_processing_route,
};

const UNBOUNDED_M0_DEADLINE_MS: u64 = u64::MAX;

#[derive(Clone)]
pub struct OrchestratorPorts {
    pub settings: Arc<dyn SettingsStore>,
    pub targets: Arc<dyn TargetContextPort>,
    pub microphone_permission: Arc<dyn MicrophonePermissionPort>,
    pub audio: Arc<dyn AudioCapture>,
    pub recording_cue: Arc<dyn RecordingCuePort>,
    pub recording_deadline: Arc<dyn RecordingDeadlinePort>,
    pub recording_hud: Arc<dyn RecordingHudPort>,
    pub asr: Arc<dyn AsrEnginePort>,
    pub llm: Arc<dyn LlmProvider>,
    pub output: Arc<dyn OutputAdapter>,
    pub clipboard: Arc<dyn ClipboardBridge>,
    pub clipboard_text_writer: Arc<dyn ClipboardTextWriter>,
    pub temporary_text: Arc<dyn TemporaryTextOutput>,
    pub user_notifications: Arc<dyn UserNotificationPort>,
    pub history: Arc<dyn HistoryStore>,
    pub diagnostics: Arc<dyn DiagnosticsSink>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGenerator>,
}

pub struct TranscriptionOrchestrator {
    ports: OrchestratorPorts,
    sessions: SessionCoordinator,
    runtime: Mutex<RuntimeState>,
    configuration_gate: Arc<AsyncMutex<()>>,
    operations: OperationBarrier,
    exit: ExitCompletion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApplicationActivity {
    Idle,
    Busy,
    Quitting,
}

impl TranscriptionOrchestrator {
    #[must_use]
    pub fn new(ports: OrchestratorPorts) -> Self {
        Self {
            ports,
            sessions: SessionCoordinator::new(),
            runtime: Mutex::new(RuntimeState::default()),
            configuration_gate: Arc::new(AsyncMutex::new(())),
            operations: OperationBarrier::new(),
            exit: ExitCompletion::new(),
        }
    }

    pub async fn start(self: &Arc<Self>) -> Result<StartOutcome, OrchestratorError> {
        let Some(_operation_guard) = self.operations.enter() else {
            return Ok(StartOutcome::Quitting);
        };
        let session_id = self.ports.ids.session_id();
        crate::trace::session_mark("开始录音");

        // Session start and LLM configuration mutations share one ordering
        // boundary. Acquire it before publishing `starting`: a mutation that
        // already owns the gate completes first, while a mutation arriving
        // later observes this Session as active and fails closed.
        let configuration_guard = self.configuration_gate.lock().await;
        {
            let mut runtime = self.runtime()?;
            if runtime.quitting {
                return Ok(StartOutcome::Quitting);
            }
            if let Some(active_session_id) = runtime.active_session_id() {
                return Ok(StartOutcome::Busy { active_session_id });
            }
            runtime.starting = Some(session_id);
        }

        let settings = match self.ports.settings.load().await {
            Ok(settings) => settings,
            Err(error) => {
                self.record_port_error(Some(session_id), "preparing", &error);
                self.release_start_reservation(session_id)?;
                return Ok(StartOutcome::Failed(FailureCategory::Storage));
            }
        };

        if !self.start_reservation_is_current(session_id)? {
            return Ok(StartOutcome::Quitting);
        }

        let processing_route = self
            .resolve_text_processing_route(session_id, &settings)
            .await;
        if !self.start_reservation_is_current(session_id)? {
            return Ok(StartOutcome::Quitting);
        }
        drop(configuration_guard);

        let target_result = self.ports.targets.capture().await;
        if !self.start_reservation_is_current(session_id)? {
            return Ok(StartOutcome::Quitting);
        }
        let target = match target_result {
            Ok(target) => target,
            Err(error) => {
                self.record_port_error(Some(session_id), "target_capture", &error);
                self.release_start_reservation(session_id)?;
                crate::trace::decision(
                    "capture",
                    "拒绝开始",
                    &format!("捕获端口报错 {}", error.code),
                );
                return Ok(StartOutcome::Rejected(RejectReason::PermissionUnavailable));
            }
        };

        // Safe targets keep the exact AX path. Targets without an accessible
        // control use the explicitly enabled user-directed paste path instead:
        // the user owns the keyboard focus at the eventual dispatch boundary.
        crate::trace::decision(
            "capture",
            match target.security {
                TargetSecurity::Safe => "目标安全（可尝试插入）",
                TargetSecurity::Unknown => "目标未知（将尝试用户导向粘贴）",
                TargetSecurity::SecureInput => "安全输入状态（输入位置由用户决定）",
            },
            &format!("有选区={}", target.has_selection),
        );

        let target_display_hint = target.display_hint;

        let selected_text = if target.security == TargetSecurity::Safe
            && target.has_selection
            && settings.read_selected_text()
            && matches!(&processing_route, TextProcessingRoute::Llm(_))
        {
            let selection_result = self
                .ports
                .targets
                .read_selected_text(&target.target_ref)
                .await;
            if !self.start_reservation_is_current(session_id)? {
                return Ok(StartOutcome::Quitting);
            }
            let selection_result = match selection_result {
                Err(error) if settings.clipboard_bridge_allowed() => {
                    self.record_port_error(Some(session_id), "selection_read_native", &error);
                    let result = self
                        .ports
                        .clipboard
                        .read_selected_text(&target.target_ref)
                        .await;
                    if !self.start_reservation_is_current(session_id)? {
                        return Ok(StartOutcome::Quitting);
                    }
                    result
                }
                result => result,
            };
            match selection_result {
                Ok(selection) if selection.exceeded_limit => {
                    self.release_start_reservation(session_id)?;
                    return Ok(StartOutcome::Rejected(RejectReason::SelectionTooLong));
                }
                Ok(selection) => selection.text,
                Err(error) => {
                    self.record_port_error(Some(session_id), "selection_read", &error);
                    self.release_start_reservation(session_id)?;
                    return Ok(StartOutcome::Rejected(RejectReason::PermissionUnavailable));
                }
            }
        } else {
            None
        };

        let microphone_access = self
            .ports
            .microphone_permission
            .request_recording_access()
            .await;
        if !self.start_reservation_is_current(session_id)? {
            return Ok(StartOutcome::Quitting);
        }
        match microphone_access {
            Ok(MicrophoneAccess::Granted) => {}
            Ok(
                MicrophoneAccess::Denied
                | MicrophoneAccess::Restricted
                | MicrophoneAccess::Unavailable,
            ) => {
                self.release_start_reservation(session_id)?;
                self.raise_user_notification_best_effort(
                    session_id,
                    UserNotificationKind::MicrophonePermission,
                )
                .await;
                return Ok(StartOutcome::Rejected(RejectReason::PermissionUnavailable));
            }
            Err(error) => {
                self.record_port_error(Some(session_id), "microphone_permission", &error);
                self.release_start_reservation(session_id)?;
                self.raise_user_notification_best_effort(
                    session_id,
                    UserNotificationKind::MicrophonePermission,
                )
                .await;
                return Ok(StartOutcome::Rejected(RejectReason::PermissionUnavailable));
            }
        }

        {
            let mut runtime = self.runtime()?;
            if runtime.quitting || runtime.starting != Some(session_id) {
                return Ok(StartOutcome::Quitting);
            }
            runtime.starting = None;
            runtime.preparing = Some(PreparingWorkflow {
                session_id,
                settings: settings.clone(),
                processing_route,
                target_ref: target.target_ref,
                target_security: target.security,
                selected_text,
                lifecycle: LifecycleFence::new(),
                recording_deadline: None,
                recording_limit_elapsed: false,
                stage: PreparingStage::StartingAudio,
            });
        }

        let (recording, asr_route) = join_futures(
            self.prepare_recording(
                session_id,
                target_display_hint,
                settings.max_recording_duration(),
            ),
            self.resolve_route(session_id, &settings),
        )
        .await;
        let capture = match recording? {
            RecordingPreparation::Ready(capture) => capture,
            RecordingPreparation::FailedAudio => {
                return Ok(StartOutcome::Failed(FailureCategory::Audio));
            }
            RecordingPreparation::FailedLifecycle => {
                return Ok(StartOutcome::Failed(FailureCategory::Lifecycle));
            }
            RecordingPreparation::RejectedHud => {
                return Ok(StartOutcome::Rejected(
                    RejectReason::RecordingHudUnavailable,
                ));
            }
            RecordingPreparation::Aborted => return Ok(StartOutcome::Quitting),
        };
        let Some(asr_route) = asr_route? else {
            if !self.claim_preparing_capture(session_id, &capture)? {
                return Ok(StartOutcome::Quitting);
            }
            self.cancel_claimed_preparing_capture(session_id, capture)
                .await?;
            self.raise_user_notification_best_effort(session_id, UserNotificationKind::Asr)
                .await;
            return Ok(StartOutcome::Rejected(RejectReason::AsrUnavailable));
        };
        if !self.is_current_preparing_capture(session_id, &capture)? {
            return Ok(StartOutcome::Quitting);
        }

        match self
            .sessions
            .try_start(session_id, self.ports.clock.now(), settings, asr_route)?
        {
            StartSessionOutcome::Accepted { .. } => {}
            StartSessionOutcome::Busy { active_session_id } => {
                if self.claim_preparing_capture(session_id, &capture)? {
                    self.cancel_claimed_preparing_capture(session_id, capture)
                        .await?;
                }
                return Ok(StartOutcome::Busy { active_session_id });
            }
        }

        let hud_result = self
            .ports
            .recording_hud
            .update(session_id, RecordingHudState::Recording)
            .await;
        if let Err(error) = &hud_result {
            self.record_port_error(Some(session_id), "recording_hud_update_ready", error);
        }
        if hud_result.is_err() {
            if !self.claim_preparing_capture(session_id, &capture)? {
                let _ = self
                    .sessions
                    .apply(session_id, SessionEvent::Fail(FailureCategory::Lifecycle));
                return Ok(StartOutcome::Quitting);
            }
            self.cancel_claimed_preparing_capture(session_id, capture)
                .await?;
            let _ = self.sessions.apply(
                session_id,
                SessionEvent::Reject(RejectReason::RecordingHudUnavailable),
            );
            return Ok(StartOutcome::Rejected(
                RejectReason::RecordingHudUnavailable,
            ));
        }

        let (activated, recording_limit_elapsed) = {
            let mut runtime = self.runtime()?;
            let is_ready = !runtime.quitting
                && runtime.preparing.as_ref().is_some_and(|preparing| {
                    preparing.session_id == session_id
                        && matches!(
                            &preparing.stage,
                            PreparingStage::Capturing {
                                capture: active_capture
                            } if active_capture == &capture
                        )
                });
            if !is_ready {
                (false, false)
            } else {
                let preparing = runtime
                    .preparing
                    .take()
                    .expect("ready preparation must still exist");
                let recording_limit_elapsed = preparing.recording_limit_elapsed;
                runtime.active = Some(ActiveWorkflow {
                    session_id,
                    settings: preparing.settings,
                    processing_route: preparing.processing_route,
                    asr_route,
                    target_ref: preparing.target_ref,
                    target_security: preparing.target_security,
                    selected_text: preparing.selected_text,
                    lifecycle: preparing.lifecycle,
                    recording_deadline: preparing.recording_deadline,
                    stage: RuntimeStage::Recording { capture },
                });
                (true, recording_limit_elapsed)
            }
        };
        if !activated {
            let _ = self
                .sessions
                .apply(session_id, SessionEvent::Fail(FailureCategory::Lifecycle));
            return Ok(StartOutcome::Quitting);
        }
        self.sessions
            .apply(session_id, SessionEvent::PreflightPassed)?;
        if recording_limit_elapsed {
            let _ = self.finish_recording(session_id).await?;
        }

        Ok(StartOutcome::Started { session_id })
    }

    async fn prepare_recording(
        self: &Arc<Self>,
        session_id: SessionId,
        target_display_hint: Option<TargetDisplayHint>,
        recording_limit: std::time::Duration,
    ) -> Result<RecordingPreparation, OrchestratorError> {
        self.play_recording_cue_best_effort(session_id, RecordingCue::Start)
            .await;
        if !self.is_current_preparing_start(session_id)? {
            return Ok(RecordingPreparation::Aborted);
        }
        let capture = match self.ports.audio.start(session_id).await {
            Ok(capture) => capture,
            Err(error) => {
                self.record_port_error(Some(session_id), "audio_start", &error);
                return if self.remove_preparing_if_current(session_id)? {
                    Ok(RecordingPreparation::FailedAudio)
                } else {
                    Ok(RecordingPreparation::Aborted)
                };
            }
        };

        if !self.set_preparing_capture_if_current(session_id, capture.clone())? {
            self.cancel_capture(session_id, capture).await?;
            self.hide_recording_hud_best_effort(session_id).await;
            return Ok(RecordingPreparation::Aborted);
        }

        let orchestrator = Arc::clone(self);
        let deadline = match self.ports.recording_deadline.schedule(
            recording_limit,
            Box::pin(async move {
                orchestrator.recording_limit_elapsed(session_id).await;
            }),
        ) {
            Ok(deadline) => deadline,
            Err(error) => {
                self.record_port_error(Some(session_id), "recording_deadline", &error);
                if !self.claim_preparing_capture(session_id, &capture)? {
                    self.hide_recording_hud_best_effort(session_id).await;
                    return Ok(RecordingPreparation::Aborted);
                }
                self.cancel_claimed_preparing_capture(session_id, capture)
                    .await?;
                return Ok(RecordingPreparation::FailedLifecycle);
            }
        };

        if !self.set_preparing_deadline_if_current(session_id, &capture, deadline)? {
            self.hide_recording_hud_best_effort(session_id).await;
            return Ok(RecordingPreparation::Aborted);
        }

        let hud_result = self
            .ports
            .recording_hud
            .show(
                session_id,
                RecordingHudState::Preparing,
                target_display_hint,
                recording_limit,
            )
            .await;
        if let Err(error) = &hud_result {
            self.record_port_error(Some(session_id), "recording_hud_show", error);
        }
        if hud_result.is_err() {
            if !self.claim_preparing_capture(session_id, &capture)? {
                self.hide_recording_hud_best_effort(session_id).await;
                return Ok(RecordingPreparation::Aborted);
            }
            self.cancel_claimed_preparing_capture(session_id, capture)
                .await?;
            return Ok(RecordingPreparation::RejectedHud);
        }

        if !self.is_current_preparing_capture(session_id, &capture)? {
            self.hide_recording_hud_best_effort(session_id).await;
            return Ok(RecordingPreparation::Aborted);
        }
        Ok(RecordingPreparation::Ready(capture))
    }

    pub async fn cancel_recording(
        &self,
        session_id: SessionId,
    ) -> Result<CancelOutcome, OrchestratorError> {
        let Some(_operation_guard) = self.operations.enter() else {
            return Ok(CancelOutcome::NotFound);
        };
        let selection = {
            let mut runtime = self.runtime()?;
            if let Some(preparing) = runtime
                .preparing
                .as_mut()
                .filter(|preparing| preparing.session_id == session_id)
            {
                match &preparing.stage {
                    PreparingStage::StartingAudio => {
                        CaptureCancellationSelection::Immediate(CancelOutcome::NotRecording)
                    }
                    PreparingStage::Capturing { capture } => {
                        let capture = capture.clone();
                        preparing.recording_deadline.take();
                        preparing.stage = PreparingStage::Cancelling {
                            capture: capture.clone(),
                            in_flight: true,
                        };
                        CaptureCancellationSelection::Capture {
                            capture,
                            target: CaptureCancellationTarget::Preparing,
                        }
                    }
                    PreparingStage::Cancelling {
                        capture,
                        in_flight: false,
                    } => {
                        let capture = capture.clone();
                        preparing.stage = PreparingStage::Cancelling {
                            capture: capture.clone(),
                            in_flight: true,
                        };
                        CaptureCancellationSelection::Capture {
                            capture,
                            target: CaptureCancellationTarget::Preparing,
                        }
                    }
                    PreparingStage::Cancelling {
                        in_flight: true, ..
                    } => CaptureCancellationSelection::Immediate(CancelOutcome::CleanupInProgress),
                }
            } else if let Some(active) = runtime
                .active
                .as_mut()
                .filter(|active| active.session_id == session_id)
            {
                match &active.stage {
                    RuntimeStage::Recording { capture } => {
                        let capture = capture.clone();
                        let completion = CaptureCleanupCompletion::Cancelled;
                        active.recording_deadline.take();
                        active.stage = RuntimeStage::Cancelling {
                            capture: capture.clone(),
                            completion,
                            in_flight: true,
                        };
                        CaptureCancellationSelection::Capture {
                            capture,
                            target: CaptureCancellationTarget::Active(completion),
                        }
                    }
                    RuntimeStage::Cancelling {
                        capture,
                        completion,
                        in_flight: false,
                    } => {
                        let capture = capture.clone();
                        let completion = *completion;
                        active.stage = RuntimeStage::Cancelling {
                            capture: capture.clone(),
                            completion,
                            in_flight: true,
                        };
                        CaptureCancellationSelection::Capture {
                            capture,
                            target: CaptureCancellationTarget::Active(completion),
                        }
                    }
                    RuntimeStage::Cancelling {
                        in_flight: true, ..
                    } => CaptureCancellationSelection::Immediate(CancelOutcome::CleanupInProgress),
                    _ => CaptureCancellationSelection::Immediate(CancelOutcome::NotRecording),
                }
            } else {
                CaptureCancellationSelection::Immediate(CancelOutcome::NotFound)
            }
        };

        match selection {
            CaptureCancellationSelection::Immediate(outcome) => Ok(outcome),
            CaptureCancellationSelection::Capture { capture, target } => {
                self.finish_capture_cancellation(session_id, capture, target)
                    .await
            }
        }
    }

    async fn finish_capture_cancellation(
        &self,
        session_id: SessionId,
        capture: AudioCaptureRef,
        target: CaptureCancellationTarget,
    ) -> Result<CancelOutcome, OrchestratorError> {
        if let Err(error) = self.cancel_capture(session_id, capture).await {
            self.set_audio_cleanup_in_flight_if_current(session_id, false)?;
            return Err(error);
        }
        self.play_recording_cue_best_effort(session_id, RecordingCue::Cancel)
            .await;
        if target == CaptureCancellationTarget::Preparing {
            self.remove_preparing_if_current(session_id)?;
            self.hide_recording_hud_best_effort(session_id).await;
            return Ok(CancelOutcome::Cancelled);
        }
        if !self.is_current_stage(session_id, |stage| {
            matches!(stage, RuntimeStage::Cancelling { .. })
        })? {
            return Ok(CancelOutcome::Cancelled);
        }
        let CaptureCancellationTarget::Active(completion) = target else {
            unreachable!("preparing cancellation returned above")
        };
        match completion {
            CaptureCleanupCompletion::Cancelled => {
                if !self.apply_if_current(session_id, SessionEvent::CancelRecording)? {
                    return Ok(CancelOutcome::Cancelled);
                }
                self.publish_recording_hud_terminal_best_effort(
                    session_id,
                    TerminalOutcome::Cancelled,
                )
                .await;
                self.clear_active_if_current(session_id).await?;
                Ok(CancelOutcome::Cancelled)
            }
            CaptureCleanupCompletion::FailAudio => {
                if self
                    .fail_current(session_id, FailureCategory::Audio)
                    .await?
                {
                    Ok(CancelOutcome::CleanupRecovered)
                } else {
                    Ok(CancelOutcome::Cancelled)
                }
            }
        }
    }

    pub async fn finish_recording(
        &self,
        session_id: SessionId,
    ) -> Result<FinishOutcome, OrchestratorError> {
        let Some(_operation_guard) = self.operations.enter() else {
            return Ok(FinishOutcome::Discarded);
        };
        let (capture, deadline) = {
            let mut runtime = self.runtime()?;
            let Some(active) = runtime.active.as_mut() else {
                return Ok(FinishOutcome::NotRecording);
            };
            if active.session_id != session_id {
                return Ok(FinishOutcome::NotRecording);
            }
            let RuntimeStage::Recording { capture } = &active.stage else {
                return Ok(FinishOutcome::NotRecording);
            };
            let capture = capture.clone();
            let deadline = active.recording_deadline.take();
            active.stage = RuntimeStage::FinishingAudio {
                capture: capture.clone(),
            };
            (capture, deadline)
        };
        drop(deadline);

        let audio = match self.ports.audio.finish(capture.clone()).await {
            Ok(audio) => audio,
            Err(error) if error.code == AUDIO_EMPTY_CAPTURE_CODE => {
                self.record_no_speech(session_id, "audio_finish");
                if !self.is_current_stage(session_id, |stage| {
                    matches!(stage, RuntimeStage::FinishingAudio { .. })
                })? {
                    return Ok(FinishOutcome::Discarded);
                }
                let completion = CaptureCleanupCompletion::Cancelled;
                if !self.set_stage_if_current(
                    session_id,
                    RuntimeStage::Cancelling {
                        capture: capture.clone(),
                        completion,
                        in_flight: true,
                    },
                )? {
                    return Ok(FinishOutcome::Discarded);
                }
                // Even a known-empty finish error does not prove that the capture
                // handle is closed. Reuse the normal cancellation cleanup before
                // exposing the benign no-output result.
                self.finish_capture_cancellation(
                    session_id,
                    capture,
                    CaptureCancellationTarget::Active(completion),
                )
                .await?;
                return Ok(FinishOutcome::NoSpeech);
            }
            Err(error) => {
                self.record_port_error(Some(session_id), "audio_finish", &error);
                if !self.is_current_stage(session_id, |stage| {
                    matches!(stage, RuntimeStage::FinishingAudio { .. })
                })? {
                    return Ok(FinishOutcome::Discarded);
                }
                if !self.set_stage_if_current(
                    session_id,
                    RuntimeStage::Cancelling {
                        capture: capture.clone(),
                        completion: CaptureCleanupCompletion::FailAudio,
                        in_flight: true,
                    },
                )? {
                    return Ok(FinishOutcome::Discarded);
                }
                // A failed finish does not prove the microphone stream is closed.
                // Retain the capture handle until explicit cancellation completes.
                if let Err(error) = self.cancel_capture(session_id, capture).await {
                    self.set_audio_cleanup_in_flight_if_current(session_id, false)?;
                    return Err(error);
                }
                return if self
                    .fail_current(session_id, FailureCategory::Audio)
                    .await?
                {
                    Ok(FinishOutcome::Failed(FailureCategory::Audio))
                } else {
                    Ok(FinishOutcome::Discarded)
                };
            }
        };

        self.play_recording_cue_best_effort(session_id, RecordingCue::Finish)
            .await;

        if !self.is_current_stage(session_id, |stage| {
            matches!(stage, RuntimeStage::FinishingAudio { .. })
        })? {
            self.cleanup_audio(session_id, audio.audio_ref).await?;
            return Ok(FinishOutcome::Discarded);
        }

        let Some(context) = self.active_context(session_id)? else {
            self.cleanup_audio(session_id, audio.audio_ref).await?;
            return Ok(FinishOutcome::Discarded);
        };
        // AudioCapture::finish success guarantees capture is closed. Only now may
        // the domain enter Recognizing.
        if !self.apply_if_current(session_id, SessionEvent::FinishRecording)? {
            self.cleanup_audio(session_id, audio.audio_ref).await?;
            return Ok(FinishOutcome::Discarded);
        }
        let request_id = self.ports.ids.request_id();
        if !self.set_stage_if_current(
            session_id,
            RuntimeStage::Recognizing {
                request_id,
                audio_ref: Some(audio.audio_ref.clone()),
            },
        )? {
            self.cleanup_audio(session_id, audio.audio_ref).await?;
            return Ok(FinishOutcome::Discarded);
        }

        self.update_recording_hud_best_effort(session_id, RecordingHudState::Recognizing)
            .await;

        let asr_result = self
            .ports
            .asr
            .transcribe(AsrRequest {
                session_id,
                request_id,
                engine: context.asr_route.engine(),
                audio,
                language_hint: None,
                deadline_ms: UNBOUNDED_M0_DEADLINE_MS,
            })
            .await;

        if let Some(audio_ref) = self.begin_recognizing_audio_cleanup(session_id, request_id)? {
            if let Err(error) = self.cleanup_audio(session_id, audio_ref.clone()).await {
                self.set_audio_cleanup_in_flight_if_current(session_id, false)?;
                return Err(error);
            }
            if !self.complete_recognizing_audio_cleanup(session_id, request_id, &audio_ref)? {
                return Ok(FinishOutcome::Discarded);
            }
        }

        if !self.is_expected_request(session_id, ExpectedRequest::Asr(request_id))? {
            self.record_late_response(session_id, "asr_late");
            return Ok(FinishOutcome::Discarded);
        }

        let asr_result = match asr_result {
            Ok(result)
                if self.valid_asr_result(&result, session_id, request_id, context.asr_route) =>
            {
                result
            }
            Ok(_) => {
                self.record_late_response(session_id, "asr_correlation_mismatch");
                return if self.fail_current(session_id, FailureCategory::Asr).await? {
                    Ok(FinishOutcome::Failed(FailureCategory::Asr))
                } else {
                    Ok(FinishOutcome::Discarded)
                };
            }
            Err(error) if error.code == ASR_NO_SPEECH_CODE => {
                self.record_no_speech(session_id, "asr");
                return if self.complete_no_speech_current(session_id).await? {
                    Ok(FinishOutcome::NoSpeech)
                } else {
                    Ok(FinishOutcome::Discarded)
                };
            }
            Err(error) => {
                self.record_port_error(Some(session_id), "asr", &error);
                return if self.fail_current(session_id, FailureCategory::Asr).await? {
                    Ok(FinishOutcome::Failed(FailureCategory::Asr))
                } else {
                    Ok(FinishOutcome::Discarded)
                };
            }
        };

        if asr_result.final_text.trim().is_empty() {
            self.record_no_speech(session_id, "asr");
            return if self.complete_no_speech_current(session_id).await? {
                Ok(FinishOutcome::NoSpeech)
            } else {
                Ok(FinishOutcome::Discarded)
            };
        }

        match context.processing_route.clone() {
            TextProcessingRoute::DirectAsr(reason) => {
                let forced_temporary_status = (reason == DirectDeliveryReason::LlmUnavailable)
                    .then_some(TemporaryTextStatus::LlmFallback);
                self.deliver(
                    session_id,
                    asr_result.final_text,
                    Some(reason),
                    forced_temporary_status,
                )
                .await
            }
            TextProcessingRoute::Llm(route) => {
                self.process_with_llm(session_id, route, asr_result.final_text)
                    .await
            }
        }
    }

    /// Retries a previously failed audio cancellation or finalized-artifact deletion.
    ///
    /// A cleanup failure deliberately keeps the Single-flight slot occupied. The future real
    /// adapter/supervisor may call this operation after a transient OS error; success then closes
    /// the failed Session without attempting ASR, LLM, delivery, or history again.
    pub async fn retry_audio_cleanup(
        &self,
        session_id: SessionId,
    ) -> Result<AudioCleanupOutcome, OrchestratorError> {
        let Some(_operation_guard) = self.operations.enter() else {
            return Ok(AudioCleanupOutcome::Quitting);
        };
        let debt = {
            let mut runtime = self.runtime()?;
            let debt = runtime
                .audio_cleanup_debts
                .iter()
                .find(|debt| debt.session_id == session_id)
                .cloned();
            if debt.is_some() {
                let in_flight = if let Some(preparing) = runtime
                    .preparing
                    .as_mut()
                    .filter(|preparing| preparing.session_id == session_id)
                {
                    match &mut preparing.stage {
                        PreparingStage::Cancelling { in_flight, .. } => in_flight,
                        _ => return Ok(AudioCleanupOutcome::NotPending),
                    }
                } else if let Some(active) = runtime
                    .active
                    .as_mut()
                    .filter(|active| active.session_id == session_id)
                {
                    match &mut active.stage {
                        RuntimeStage::Cancelling { in_flight, .. }
                        | RuntimeStage::CleaningAudio { in_flight, .. } => in_flight,
                        _ => return Ok(AudioCleanupOutcome::NotPending),
                    }
                } else {
                    return Ok(AudioCleanupOutcome::NotPending);
                };
                if *in_flight {
                    return Ok(AudioCleanupOutcome::InProgress);
                }
                *in_flight = true;
            }
            debt
        };
        let Some(debt) = debt else {
            return Ok(AudioCleanupOutcome::NotPending);
        };

        let cleanup_result = match debt.resource.clone() {
            AudioCleanupResource::Capture(capture) => {
                self.cancel_capture(session_id, capture).await
            }
            AudioCleanupResource::Finalized(audio_ref) => {
                self.cleanup_audio(session_id, audio_ref).await
            }
        };
        if let Err(error) = cleanup_result {
            self.set_audio_cleanup_in_flight_if_current(session_id, false)?;
            return Err(error);
        }

        let completion = {
            let runtime = self.runtime()?;
            if runtime.preparing.as_ref().is_some_and(|preparing| {
                preparing.session_id == session_id
                    && matches!(preparing.stage, PreparingStage::Cancelling { .. })
            }) {
                Some(AudioCleanupCompletion::Preparing)
            } else {
                runtime
                    .active
                    .as_ref()
                    .filter(|active| active.session_id == session_id)
                    .and_then(|active| match &active.stage {
                        RuntimeStage::Cancelling { completion, .. } => {
                            Some(AudioCleanupCompletion::Capture(*completion))
                        }
                        RuntimeStage::CleaningAudio { .. } => {
                            Some(AudioCleanupCompletion::FailAudio)
                        }
                        _ => None,
                    })
            }
        };

        match completion {
            Some(AudioCleanupCompletion::Preparing) => {
                self.remove_preparing_if_current(session_id)?;
                self.hide_recording_hud_best_effort(session_id).await;
            }
            Some(AudioCleanupCompletion::Capture(CaptureCleanupCompletion::Cancelled)) => {
                if self.apply_if_current(session_id, SessionEvent::CancelRecording)? {
                    self.publish_recording_hud_terminal_best_effort(
                        session_id,
                        TerminalOutcome::Cancelled,
                    )
                    .await;
                    self.clear_active_if_current(session_id).await?;
                }
            }
            Some(
                AudioCleanupCompletion::Capture(CaptureCleanupCompletion::FailAudio)
                | AudioCleanupCompletion::FailAudio,
            ) => {
                let _ = self
                    .fail_current(session_id, FailureCategory::Audio)
                    .await?;
            }
            None => {}
        }

        Ok(AudioCleanupOutcome::Recovered)
    }

    async fn recording_limit_elapsed(self: Arc<Self>, session_id: SessionId) {
        let should_finish = {
            let Ok(mut runtime) = self.runtime() else {
                return;
            };
            if let Some(preparing) = runtime
                .preparing
                .as_mut()
                .filter(|preparing| preparing.session_id == session_id)
            {
                preparing.recording_limit_elapsed = true;
                false
            } else {
                runtime.active.as_ref().is_some_and(|active| {
                    active.session_id == session_id
                        && matches!(active.stage, RuntimeStage::Recording { .. })
                })
            }
        };

        if should_finish {
            match self.finish_recording(session_id).await {
                Ok(
                    FinishOutcome::Completed(_)
                    | FinishOutcome::Failed(_)
                    | FinishOutcome::NoSpeech
                    | FinishOutcome::Discarded
                    | FinishOutcome::NotRecording,
                ) => {}
                Err(_) => {
                    self.record_port_error(
                        Some(session_id),
                        "recording_deadline_finish",
                        &PortError {
                            code: "recording.deadline_finish_failed".to_owned(),
                            safe_message_key: "errors.recording.deadline_finish_failed".to_owned(),
                            retryable: false,
                        },
                    );
                }
            }
        }
    }

    pub async fn quit(&self) -> Result<QuitOutcome, OrchestratorError> {
        match self.exit.begin() {
            ExitRole::Leader(attempt) => {
                let result = self.run_leader_quit().await;
                self.exit.complete(&attempt, result.clone());
                result
            }
            ExitRole::Follower(completion) => completion.await,
            ExitRole::Completed(outcome) => Ok(outcome),
        }
    }

    async fn run_leader_quit(&self) -> Result<QuitOutcome, OrchestratorError> {
        // Close the public workflow entry before observing runtime state. Every
        // operation that entered earlier holds a guard until all of its awaits and
        // cleanup work have completed.
        self.operations.begin_quiescing();
        let (starting, mut preparing, mut active) = {
            let mut runtime = self.runtime()?;
            runtime.quitting = true;
            let starting = runtime.starting.take();
            let preparing = runtime.preparing.take();
            let active = runtime.active.take();
            (starting, preparing, active)
        };
        if let Some(preparing) = preparing.as_mut() {
            preparing.recording_deadline.take();
        }
        if let Some(active) = active.as_mut() {
            active.recording_deadline.take();
        }

        let mut lifecycle = None;
        let outcome = if let Some(active) = active.as_ref() {
            let session_id = active.session_id;
            active.lifecycle.invalidate();
            lifecycle = Some(active.lifecycle.clone());
            let _ = self
                .sessions
                .apply(session_id, SessionEvent::Fail(FailureCategory::Lifecycle));

            match &active.stage {
                RuntimeStage::Recording { capture } | RuntimeStage::FinishingAudio { capture } => {
                    let _ = self.cancel_capture(session_id, capture.clone()).await;
                }
                RuntimeStage::Recognizing {
                    request_id,
                    audio_ref,
                } => {
                    if let Err(error) = self.ports.asr.cancel(*request_id).await {
                        self.record_port_error(Some(session_id), "asr_cancel", &error);
                    }
                    if let Some(audio_ref) = audio_ref {
                        let _ = self.cleanup_audio(session_id, audio_ref.clone()).await;
                    }
                }
                RuntimeStage::Processing { request_id } => {
                    if let Err(error) = self.ports.llm.cancel(*request_id).await {
                        self.record_port_error(Some(session_id), "llm_cancel", &error);
                    }
                }
                // The in-flight cancellation operation owns cleanup and is covered
                // by the operation barrier. Any resulting debt is retried only
                // after that operation has drained, so shutdown never overlaps it.
                RuntimeStage::Cancelling { .. }
                | RuntimeStage::CleaningAudio { .. }
                | RuntimeStage::Delivering { .. }
                | RuntimeStage::Finalizing { .. } => {}
            }
            self.hide_recording_hud_best_effort(session_id).await;
            QuitOutcome::Terminated(session_id)
        } else if let Some(preparing) = preparing.as_ref() {
            let session_id = preparing.session_id;
            preparing.lifecycle.invalidate();
            lifecycle = Some(preparing.lifecycle.clone());
            match &preparing.stage {
                PreparingStage::Capturing { capture }
                | PreparingStage::Cancelling {
                    capture,
                    in_flight: false,
                } => {
                    let _ = self.cancel_capture(session_id, capture.clone()).await;
                }
                PreparingStage::StartingAudio
                | PreparingStage::Cancelling {
                    in_flight: true, ..
                } => {}
            }
            self.hide_recording_hud_best_effort(session_id).await;
            QuitOutcome::Terminated(session_id)
        } else {
            starting.map_or(QuitOutcome::Idle, QuitOutcome::Terminated)
        };

        // No executor thread is blocked here: both barriers are Waker-driven
        // futures. After they resolve, no entered workflow and no already-started
        // irreversible commit can outlive this return boundary.
        self.operations.wait_quiescent().await;
        if let Some(lifecycle) = lifecycle {
            lifecycle.wait_quiescent().await;
        }

        // A workflow that was already in cancellation/cleanup may have published
        // a retryable debt while shutdown waited. Retry only after every entered
        // operation is quiescent, then fail the exit if ownership is still not
        // released. A failed exit restores the active workflow so a later exit
        // attempt can retry without admitting a new Session.
        self.retry_all_audio_cleanup_debts().await?;
        if let Some(debt) = self.first_audio_cleanup_debt()? {
            if let Some(preparing) = preparing {
                let mut runtime = self.runtime()?;
                if runtime.preparing.is_none() {
                    runtime.preparing = Some(preparing);
                }
            }
            if let Some(active) = active {
                let mut runtime = self.runtime()?;
                if runtime.active.is_none() {
                    runtime.active = Some(active);
                }
            }
            return Err(debt.orchestrator_error());
        }

        Ok(outcome)
    }

    async fn resolve_route(
        &self,
        session_id: SessionId,
        settings: &SettingsSnapshot,
    ) -> Result<Option<ResolvedAsrRoute>, OrchestratorError> {
        let (qwen_health, whisper_health) = match settings.asr_preference() {
            remtene_domain::AsrPreference::Qwen => (
                self.engine_health(session_id, AsrEngine::Qwen).await,
                EngineHealth::Missing,
            ),
            remtene_domain::AsrPreference::Whisper => (
                EngineHealth::Missing,
                self.engine_health(session_id, AsrEngine::Whisper).await,
            ),
        };

        if !self.start_reservation_is_current(session_id)? {
            return Ok(None);
        }

        Ok(resolve_asr_route(settings.asr_preference(), qwen_health, whisper_health).ok())
    }

    async fn resolve_text_processing_route(
        &self,
        session_id: SessionId,
        settings: &SettingsSnapshot,
    ) -> TextProcessingRoute {
        if settings.processing_mode() == remtene_domain::ProcessingMode::Raw {
            return TextProcessingRoute::DirectAsr(DirectDeliveryReason::RawMode);
        }

        let candidate = settings
            .llm()
            .map(|llm| LlmRouteCandidate::new(llm.base_url().to_owned(), llm.model().to_owned()));
        let resolution = self.ports.llm.resolve_route(candidate).await;
        if let LlmRouteResolution::Unavailable { error, .. } = &resolution {
            self.record_port_error(Some(session_id), "llm_route", error);
        }
        text_processing_route(settings.processing_mode(), resolution)
    }

    pub(crate) fn configuration_gate(&self) -> Arc<AsyncMutex<()>> {
        Arc::clone(&self.configuration_gate)
    }

    /// Enters the application-wide quiescence barrier for an irreversible
    /// operation owned by another Application controller.
    ///
    /// History copy and clear are not Session workflows, but they still must
    /// be rejected once shutdown starts and must finish before shutdown
    /// returns. The returned guard provides that ordering without exposing the
    /// barrier implementation outside Application.
    pub(crate) fn enter_external_operation(&self) -> Option<crate::ports::CommitGuard> {
        self.operations.enter()
    }

    pub(crate) fn has_active_work(&self) -> Result<bool, OrchestratorError> {
        let runtime = self.runtime()?;
        Ok(runtime.starting.is_some() || runtime.preparing.is_some() || runtime.active.is_some())
    }

    pub(crate) fn application_activity(&self) -> Result<ApplicationActivity, OrchestratorError> {
        let runtime = self.runtime()?;
        if runtime.quitting {
            return Ok(ApplicationActivity::Quitting);
        }
        if runtime.starting.is_some() || runtime.preparing.is_some() || runtime.active.is_some() {
            return Ok(ApplicationActivity::Busy);
        }
        Ok(ApplicationActivity::Idle)
    }

    async fn engine_health(&self, session_id: SessionId, engine: AsrEngine) -> EngineHealth {
        match self.ports.asr.health(engine).await {
            Ok(health) => health,
            Err(error) => {
                self.record_port_error(Some(session_id), "asr_health", &error);
                EngineHealth::Unhealthy
            }
        }
    }

    async fn process_with_llm(
        &self,
        session_id: SessionId,
        route: ResolvedLlmRoute,
        raw_transcript: String,
    ) -> Result<FinishOutcome, OrchestratorError> {
        let Some(context) = self.active_context(session_id)? else {
            return Ok(FinishOutcome::Discarded);
        };
        if !self.apply_if_current(session_id, SessionEvent::BeginProcessing)? {
            return Ok(FinishOutcome::Discarded);
        }

        let request_id = self.ports.ids.request_id();
        if !self.set_stage_if_current(session_id, RuntimeStage::Processing { request_id })? {
            return Ok(FinishOutcome::Discarded);
        }
        self.update_recording_hud_best_effort(session_id, RecordingHudState::Processing)
            .await;

        let llm_started = Instant::now();
        self.ports.diagnostics.record(DiagnosticEvent {
            session_id: Some(session_id),
            phase: Some("llm.request".to_owned()),
            state: Some("sent".to_owned()),
            duration_ms: None,
            error_code: None,
            detail: Some(format!("mode={:?}", context.settings.processing_mode())),
        });
        let result = self
            .ports
            .llm
            .process(
                route,
                TextProcessingRequest {
                    session_id,
                    request_id,
                    processing_mode: context.settings.processing_mode(),
                    raw_transcript: raw_transcript.clone(),
                    selected_text: context.selected_text,
                },
            )
            .await;
        self.ports.diagnostics.record(DiagnosticEvent {
            session_id: Some(session_id),
            phase: Some("llm.response".to_owned()),
            state: Some(if result.is_ok() { "received" } else { "failed" }.to_owned()),
            duration_ms: Some(u64::try_from(llm_started.elapsed().as_millis()).unwrap_or(u64::MAX)),
            error_code: result.as_ref().err().map(|error| error.code.clone()),
            detail: None,
        });

        if !self.is_expected_request(session_id, ExpectedRequest::Llm(request_id))? {
            self.record_late_response(session_id, "llm_late");
            return Ok(FinishOutcome::Discarded);
        }

        match result {
            Ok(result) if !self.valid_llm_result(&result, session_id, request_id) => {
                self.record_late_response(session_id, "llm_correlation_mismatch");
                // The returned text is untrusted and must be discarded, but
                // the locally produced ASR transcript still belongs to this
                // current Session. Treat every LLM protocol failure uniformly
                // and preserve that known-good local result in the safe
                // temporary-output path.
                self.deliver(
                    session_id,
                    raw_transcript,
                    None,
                    Some(TemporaryTextStatus::LlmFallback),
                )
                .await
            }
            Ok(result) if result.final_text.trim().is_empty() => {
                self.record_late_response(session_id, "llm_empty_response");
                self.deliver(
                    session_id,
                    raw_transcript,
                    None,
                    Some(TemporaryTextStatus::LlmFallback),
                )
                .await
            }
            Ok(result) => {
                self.deliver(session_id, result.final_text, None, None)
                    .await
            }
            Err(error) => {
                self.record_port_error(Some(session_id), "llm", &error);
                self.deliver(
                    session_id,
                    raw_transcript,
                    None,
                    Some(TemporaryTextStatus::LlmFallback),
                )
                .await
            }
        }
    }

    async fn deliver(
        &self,
        session_id: SessionId,
        final_text: String,
        direct_delivery_reason: Option<DirectDeliveryReason>,
        forced_temporary_status: Option<TemporaryTextStatus>,
    ) -> Result<FinishOutcome, OrchestratorError> {
        let Some(context) = self.active_context(session_id)? else {
            return Ok(FinishOutcome::Discarded);
        };
        if !self.apply_if_current(session_id, SessionEvent::BeginDelivery)? {
            return Ok(FinishOutcome::Discarded);
        }

        let delivery_id = self.ports.ids.delivery_id();
        if !self.set_stage_if_current(session_id, RuntimeStage::Delivering { delivery_id })? {
            return Ok(FinishOutcome::Discarded);
        }
        self.update_recording_hud_best_effort(session_id, RecordingHudState::Delivering)
            .await;

        let delivery_progress = if let Some(status) = forced_temporary_status {
            // A caller-forced temporary result (for example an LLM fallback)
            // remains a visible fallback and never triggers another output path.
            crate::trace::decision("deliver", "临时文本框", "调用方强制（未尝试插入）");
            self.show_temporary_text(session_id, delivery_id, final_text.clone(), status)
                .await?
        } else if context.target_security != TargetSecurity::Safe {
            crate::trace::decision(
                "deliver",
                "用户导向粘贴",
                "捕获阶段没有可验证文本控件，输入位置以当前键盘焦点为准",
            );
            self.try_user_directed_or_temporary(
                session_id,
                delivery_id,
                final_text.clone(),
                TemporaryTextStatus::NotInserted,
                &context,
            )
            .await?
        } else {
            match self.ports.targets.revalidate(&context.target_ref).await {
                Ok(TargetRevalidation::Valid(target)) => {
                    if !self.is_expected_delivery(session_id, delivery_id)? {
                        return Ok(FinishOutcome::Discarded);
                    }
                    match self
                        .ports
                        .output
                        .insert(
                            target,
                            final_text.clone(),
                            delivery_id,
                            context.lifecycle.clone(),
                        )
                        .await
                    {
                        Ok(InsertOutcome::Inserted) => {
                            crate::trace::decision("deliver", "已插入光标处", "");
                            DeliveryProgress::Delivered(DeliveryKind::Inserted, None)
                        }
                        Ok(InsertOutcome::NotInserted) => {
                            if !self.is_expected_delivery(session_id, delivery_id)? {
                                return Ok(FinishOutcome::Discarded);
                            }
                            crate::trace::decision(
                                "deliver",
                                "回退剪贴板",
                                "AX 插入未发生（具体原因见上方 dispatch 记录）",
                            );
                            self.try_clipboard_or_temporary(
                                session_id,
                                delivery_id,
                                final_text.clone(),
                                TemporaryTextStatus::NotInserted,
                                &context,
                            )
                            .await?
                        }
                        Ok(InsertOutcome::Indeterminate) => {
                            if !self.is_expected_delivery(session_id, delivery_id)? {
                                return Ok(FinishOutcome::Discarded);
                            }
                            // 不确定时禁止再走剪贴板：文本可能已经写进去了，
                            // 二次投递会造成重复插入。
                            crate::trace::decision(
                                "deliver",
                                "临时文本框",
                                "AX 结果不确定，禁止二次投递",
                            );
                            self.show_temporary_text(
                                session_id,
                                delivery_id,
                                final_text.clone(),
                                TemporaryTextStatus::Indeterminate,
                            )
                            .await?
                        }
                        Err(error) => {
                            self.record_port_error(Some(session_id), "output_insert", &error);
                            if !self.is_expected_delivery(session_id, delivery_id)? {
                                return Ok(FinishOutcome::Discarded);
                            }
                            crate::trace::decision(
                                "deliver",
                                "回退剪贴板",
                                &format!("插入端口报错 {}", error.code),
                            );
                            self.try_clipboard_or_temporary(
                                session_id,
                                delivery_id,
                                final_text.clone(),
                                TemporaryTextStatus::NotInserted,
                                &context,
                            )
                            .await?
                        }
                    }
                }
                Ok(TargetRevalidation::Invalid | TargetRevalidation::Indeterminate) => {
                    if !self.is_expected_delivery(session_id, delivery_id)? {
                        return Ok(FinishOutcome::Discarded);
                    }
                    crate::trace::decision(
                        "deliver",
                        "用户导向粘贴",
                        "精确目标复核未通过且尚未写入，改投用户当前焦点",
                    );
                    self.try_user_directed_or_temporary(
                        session_id,
                        delivery_id,
                        final_text.clone(),
                        TemporaryTextStatus::NotInserted,
                        &context,
                    )
                    .await?
                }
                Err(error) => {
                    self.record_port_error(Some(session_id), "target_revalidate", &error);
                    if !self.is_expected_delivery(session_id, delivery_id)? {
                        return Ok(FinishOutcome::Discarded);
                    }
                    crate::trace::decision(
                        "deliver",
                        "用户导向粘贴",
                        &format!("精确复核报错 {} 且尚未写入", error.code),
                    );
                    self.try_user_directed_or_temporary(
                        session_id,
                        delivery_id,
                        final_text.clone(),
                        TemporaryTextStatus::NotInserted,
                        &context,
                    )
                    .await?
                }
            }
        };

        let (delivery, notification_after_completion) = match delivery_progress {
            DeliveryProgress::Delivered(delivery, notification) => (delivery, notification),
            DeliveryProgress::Discarded => return Ok(FinishOutcome::Discarded),
            DeliveryProgress::Failed => {
                return Ok(FinishOutcome::Failed(FailureCategory::Delivery));
            }
        };

        if !self.is_expected_delivery(session_id, delivery_id)? {
            return Ok(FinishOutcome::Discarded);
        }
        if !self.apply_if_current(session_id, SessionEvent::BeginFinalizing)? {
            return Ok(FinishOutcome::Discarded);
        }
        if !self.set_stage_if_current(session_id, RuntimeStage::Finalizing { delivery_id })? {
            return Ok(FinishOutcome::Discarded);
        }
        self.update_recording_hud_best_effort(session_id, RecordingHudState::Finalizing)
            .await;

        let mut warnings = Vec::new();
        if context.settings.auto_copy_result() {
            if let Err(error) = self
                .ports
                .clipboard_text_writer
                .write_text(final_text.clone())
                .await
            {
                self.record_port_error(Some(session_id), "finalize_auto_copy", &error);
                warnings.push(FinalizationWarning::AutoCopyFailed);
            }
            if !self.is_expected_finalization(session_id, delivery_id)? {
                return Ok(FinishOutcome::Discarded);
            }
        }
        if context.settings.history_policy().enabled {
            let record = HistoryRecord {
                delivery_id,
                final_text: final_text.clone(),
                created_at: self.ports.clock.now(),
            };
            match self
                .ports
                .history
                .save_with_policy(record, &context.settings, context.lifecycle.clone())
                .await
            {
                Ok(()) => {}
                Err(error) => {
                    self.record_port_error(Some(session_id), "history_save", &error);
                    warnings.push(FinalizationWarning::HistorySaveFailed);
                }
            }
            if !self.is_expected_finalization(session_id, delivery_id)? {
                return Ok(FinishOutcome::Discarded);
            }
        }

        if !self.apply_if_current(session_id, SessionEvent::Complete)? {
            return Ok(FinishOutcome::Discarded);
        }
        self.update_recording_hud_best_effort(session_id, RecordingHudState::Completed)
            .await;
        self.publish_recording_hud_terminal_best_effort(session_id, TerminalOutcome::Completed)
            .await;
        self.clear_active_if_current(session_id).await?;
        if let Some(notification) = notification_after_completion {
            self.raise_user_notification_best_effort(session_id, notification)
                .await;
        }

        Ok(FinishOutcome::Completed(Completion {
            final_text,
            delivery,
            direct_delivery_reason,
            warnings,
        }))
    }

    async fn show_temporary_text(
        &self,
        session_id: SessionId,
        delivery_id: DeliveryId,
        final_text: String,
        status: TemporaryTextStatus,
    ) -> Result<DeliveryProgress, OrchestratorError> {
        let Some(context) = self.active_context(session_id)? else {
            return Ok(DeliveryProgress::Discarded);
        };
        match self
            .ports
            .temporary_text
            .show(
                session_id,
                delivery_id,
                final_text,
                status,
                context.lifecycle,
            )
            .await
        {
            Ok(()) if self.is_expected_delivery(session_id, delivery_id)? => {
                let notification = match status {
                    TemporaryTextStatus::LlmFallback => Some(UserNotificationKind::Llm),
                    // The indeterminate temporary-text surface already contains
                    // the warning and the complete recoverable text. Opening a
                    // second Delivery feedback window only duplicates that
                    // state, obscures the text, and steals focus.
                    TemporaryTextStatus::Indeterminate | TemporaryTextStatus::NotInserted => None,
                };
                Ok(DeliveryProgress::Delivered(
                    DeliveryKind::TemporaryText,
                    notification,
                ))
            }
            Ok(()) => Ok(DeliveryProgress::Discarded),
            Err(error) => {
                self.record_port_error(Some(session_id), "temporary_text", &error);
                if self
                    .fail_current(session_id, FailureCategory::Delivery)
                    .await?
                {
                    Ok(DeliveryProgress::Failed)
                } else {
                    Ok(DeliveryProgress::Discarded)
                }
            }
        }
    }

    async fn try_clipboard_or_temporary(
        &self,
        session_id: SessionId,
        delivery_id: DeliveryId,
        final_text: String,
        fallback_status: TemporaryTextStatus,
        context: &ActiveContext,
    ) -> Result<DeliveryProgress, OrchestratorError> {
        if !context.settings.clipboard_bridge_allowed() {
            crate::trace::decision("clipboard", "临时文本框", "设置中未允许剪贴板桥接");
            return self
                .show_temporary_text(session_id, delivery_id, final_text, fallback_status)
                .await;
        }

        let target = match self.ports.targets.revalidate(&context.target_ref).await {
            Ok(TargetRevalidation::Valid(target)) => target,
            Ok(TargetRevalidation::Invalid | TargetRevalidation::Indeterminate) => {
                crate::trace::decision(
                    "clipboard",
                    "用户导向粘贴",
                    "目标绑定粘贴复核未通过且尚未派发",
                );
                return self
                    .try_user_directed_or_temporary(
                        session_id,
                        delivery_id,
                        final_text,
                        fallback_status,
                        context,
                    )
                    .await;
            }
            Err(error) => {
                self.record_port_error(Some(session_id), "clipboard_target_revalidate", &error);
                crate::trace::decision(
                    "clipboard",
                    "用户导向粘贴",
                    &format!("目标绑定粘贴复核报错 {} 且尚未派发", error.code),
                );
                return self
                    .try_user_directed_or_temporary(
                        session_id,
                        delivery_id,
                        final_text,
                        fallback_status,
                        context,
                    )
                    .await;
            }
        };
        if !self.is_expected_delivery(session_id, delivery_id)? {
            return Ok(DeliveryProgress::Discarded);
        }

        match self
            .ports
            .clipboard
            .insert_and_restore(
                target,
                final_text.clone(),
                delivery_id,
                context.lifecycle.clone(),
            )
            .await
        {
            Ok(InsertOutcome::Inserted) => {
                crate::trace::decision("clipboard", "已经由剪贴板插入", "");
                Ok(DeliveryProgress::Delivered(
                    DeliveryKind::ClipboardBridge,
                    None,
                ))
            }
            Ok(InsertOutcome::NotInserted) => {
                if !self.is_expected_delivery(session_id, delivery_id)? {
                    return Ok(DeliveryProgress::Discarded);
                }
                crate::trace::decision("clipboard", "临时文本框", "⌘V 模拟未生效");
                self.show_temporary_text(session_id, delivery_id, final_text, fallback_status)
                    .await
            }
            Ok(InsertOutcome::Indeterminate) => {
                if !self.is_expected_delivery(session_id, delivery_id)? {
                    return Ok(DeliveryProgress::Discarded);
                }
                crate::trace::decision("clipboard", "临时文本框", "剪贴板结果不确定");
                self.show_temporary_text(
                    session_id,
                    delivery_id,
                    final_text,
                    TemporaryTextStatus::Indeterminate,
                )
                .await
            }
            Err(error) => {
                self.record_port_error(Some(session_id), "clipboard_insert", &error);
                if !self.is_expected_delivery(session_id, delivery_id)? {
                    return Ok(DeliveryProgress::Discarded);
                }
                crate::trace::decision(
                    "clipboard",
                    "临时文本框",
                    &format!("剪贴板端口报错 {}", error.code),
                );
                self.show_temporary_text(session_id, delivery_id, final_text, fallback_status)
                    .await
            }
        }
    }

    async fn try_user_directed_or_temporary(
        &self,
        session_id: SessionId,
        delivery_id: DeliveryId,
        final_text: String,
        fallback_status: TemporaryTextStatus,
        context: &ActiveContext,
    ) -> Result<DeliveryProgress, OrchestratorError> {
        if !context.settings.clipboard_bridge_allowed() {
            crate::trace::decision("userpaste", "临时文本框", "设置中未允许剪贴板兼容投递");
            return self
                .show_temporary_text(session_id, delivery_id, final_text, fallback_status)
                .await;
        }
        if !self.is_expected_delivery(session_id, delivery_id)? {
            return Ok(DeliveryProgress::Discarded);
        }

        match self
            .ports
            .clipboard
            .insert_at_current_focus_and_restore(
                final_text.clone(),
                delivery_id,
                context.lifecycle.clone(),
            )
            .await
        {
            Ok(UserDirectedPasteOutcome::Dispatched) => {
                crate::trace::decision(
                    "userpaste",
                    "已发送到当前输入位置",
                    "按键事件已派发；目标不提供内容回读能力",
                );
                Ok(DeliveryProgress::Delivered(
                    DeliveryKind::ClipboardBridge,
                    None,
                ))
            }
            Ok(UserDirectedPasteOutcome::NotDispatched) => {
                if !self.is_expected_delivery(session_id, delivery_id)? {
                    return Ok(DeliveryProgress::Discarded);
                }
                crate::trace::decision("userpaste", "临时文本框", "按键事件未派发");
                self.show_temporary_text(session_id, delivery_id, final_text, fallback_status)
                    .await
            }
            Ok(UserDirectedPasteOutcome::Indeterminate) => {
                if !self.is_expected_delivery(session_id, delivery_id)? {
                    return Ok(DeliveryProgress::Discarded);
                }
                crate::trace::decision("userpaste", "临时文本框", "按键派发结果不确定");
                self.show_temporary_text(
                    session_id,
                    delivery_id,
                    final_text,
                    TemporaryTextStatus::Indeterminate,
                )
                .await
            }
            Err(error) => {
                self.record_port_error(Some(session_id), "user_directed_paste", &error);
                if !self.is_expected_delivery(session_id, delivery_id)? {
                    return Ok(DeliveryProgress::Discarded);
                }
                crate::trace::decision(
                    "userpaste",
                    "临时文本框",
                    &format!("通用粘贴端口报错 {}", error.code),
                );
                self.show_temporary_text(session_id, delivery_id, final_text, fallback_status)
                    .await
            }
        }
    }

    fn runtime(&self) -> Result<MutexGuard<'_, RuntimeState>, OrchestratorError> {
        self.runtime
            .lock()
            .map_err(|_| OrchestratorError::RuntimeLockPoisoned)
    }

    fn start_reservation_is_current(
        &self,
        session_id: SessionId,
    ) -> Result<bool, OrchestratorError> {
        let runtime = self.runtime()?;
        Ok(!runtime.quitting
            && (runtime.starting == Some(session_id)
                || runtime
                    .preparing
                    .as_ref()
                    .is_some_and(|preparing| preparing.session_id == session_id)))
    }

    fn release_start_reservation(&self, session_id: SessionId) -> Result<(), OrchestratorError> {
        let mut runtime = self.runtime()?;
        if runtime.starting == Some(session_id) {
            runtime.starting = None;
        }
        Ok(())
    }

    fn active_context(
        &self,
        session_id: SessionId,
    ) -> Result<Option<ActiveContext>, OrchestratorError> {
        let runtime = self.runtime()?;
        Ok(runtime
            .active
            .as_ref()
            .filter(|active| active.session_id == session_id)
            .map(ActiveContext::from))
    }

    fn is_current_preparing_start(&self, session_id: SessionId) -> Result<bool, OrchestratorError> {
        let runtime = self.runtime()?;
        Ok(!runtime.quitting
            && runtime.preparing.as_ref().is_some_and(|preparing| {
                preparing.session_id == session_id
                    && matches!(preparing.stage, PreparingStage::StartingAudio)
            }))
    }

    fn set_preparing_capture_if_current(
        &self,
        session_id: SessionId,
        capture: AudioCaptureRef,
    ) -> Result<bool, OrchestratorError> {
        let mut runtime = self.runtime()?;
        if runtime.quitting {
            return Ok(false);
        }
        let Some(preparing) = runtime
            .preparing
            .as_mut()
            .filter(|preparing| preparing.session_id == session_id)
        else {
            return Ok(false);
        };
        if !matches!(preparing.stage, PreparingStage::StartingAudio) {
            return Ok(false);
        }
        preparing.stage = PreparingStage::Capturing { capture };
        Ok(true)
    }

    fn set_preparing_deadline_if_current(
        &self,
        session_id: SessionId,
        capture: &AudioCaptureRef,
        deadline: RecordingDeadlineGuard,
    ) -> Result<bool, OrchestratorError> {
        let mut runtime = self.runtime()?;
        if runtime.quitting {
            return Ok(false);
        }
        let Some(preparing) = runtime
            .preparing
            .as_mut()
            .filter(|preparing| preparing.session_id == session_id)
        else {
            return Ok(false);
        };
        if !matches!(
            &preparing.stage,
            PreparingStage::Capturing {
                capture: active_capture
            } if active_capture == capture
        ) {
            return Ok(false);
        }
        preparing.recording_deadline = Some(deadline);
        Ok(true)
    }

    fn is_current_preparing_capture(
        &self,
        session_id: SessionId,
        capture: &AudioCaptureRef,
    ) -> Result<bool, OrchestratorError> {
        let runtime = self.runtime()?;
        Ok(!runtime.quitting
            && runtime.preparing.as_ref().is_some_and(|preparing| {
                preparing.session_id == session_id
                    && matches!(
                        &preparing.stage,
                        PreparingStage::Capturing {
                            capture: active_capture
                        } if active_capture == capture
                    )
            }))
    }

    fn claim_preparing_capture(
        &self,
        session_id: SessionId,
        capture: &AudioCaptureRef,
    ) -> Result<bool, OrchestratorError> {
        let mut runtime = self.runtime()?;
        if runtime.quitting {
            return Ok(false);
        }
        let Some(preparing) = runtime
            .preparing
            .as_mut()
            .filter(|preparing| preparing.session_id == session_id)
        else {
            return Ok(false);
        };
        let matches_capture = matches!(
            &preparing.stage,
            PreparingStage::Capturing {
                capture: active_capture
            } if active_capture == capture
        );
        if !matches_capture {
            return Ok(false);
        }
        preparing.stage = PreparingStage::Cancelling {
            capture: capture.clone(),
            in_flight: true,
        };
        Ok(true)
    }

    fn remove_preparing_if_current(
        &self,
        session_id: SessionId,
    ) -> Result<bool, OrchestratorError> {
        let mut runtime = self.runtime()?;
        let is_current = runtime
            .preparing
            .as_ref()
            .is_some_and(|preparing| preparing.session_id == session_id);
        if !is_current {
            return Ok(false);
        }
        if let Some(preparing) = runtime.preparing.take() {
            preparing.lifecycle.invalidate();
        }
        Ok(true)
    }

    async fn cancel_claimed_preparing_capture(
        &self,
        session_id: SessionId,
        capture: AudioCaptureRef,
    ) -> Result<(), OrchestratorError> {
        if let Err(error) = self.cancel_capture(session_id, capture).await {
            self.set_audio_cleanup_in_flight_if_current(session_id, false)?;
            return Err(error);
        }
        self.remove_preparing_if_current(session_id)?;
        self.hide_recording_hud_best_effort(session_id).await;
        Ok(())
    }

    fn set_stage_if_current(
        &self,
        session_id: SessionId,
        stage: RuntimeStage,
    ) -> Result<bool, OrchestratorError> {
        let mut runtime = self.runtime()?;
        if runtime.quitting {
            return Ok(false);
        }
        let Some(active) = runtime.active.as_mut() else {
            return Ok(false);
        };
        if active.session_id != session_id {
            return Ok(false);
        }
        active.stage = stage;
        Ok(true)
    }

    fn is_current_stage(
        &self,
        session_id: SessionId,
        predicate: impl FnOnce(&RuntimeStage) -> bool,
    ) -> Result<bool, OrchestratorError> {
        let runtime = self.runtime()?;
        Ok(!runtime.quitting
            && runtime
                .active
                .as_ref()
                .is_some_and(|active| active.session_id == session_id && predicate(&active.stage)))
    }

    fn apply_if_current(
        &self,
        session_id: SessionId,
        event: SessionEvent,
    ) -> Result<bool, OrchestratorError> {
        if !self.is_current_stage(session_id, |_| true)? {
            return Ok(false);
        }
        match self.sessions.apply(session_id, event) {
            Ok(_) => Ok(true),
            Err(CoordinatorError::SessionNotFound) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    async fn fail_current(
        &self,
        session_id: SessionId,
        category: FailureCategory,
    ) -> Result<bool, OrchestratorError> {
        let is_current = {
            let runtime = self.runtime()?;
            runtime
                .active
                .as_ref()
                .is_some_and(|active| active.session_id == session_id)
        };
        if !is_current {
            return Ok(false);
        }

        let terminal_outcome = match self
            .sessions
            .apply(session_id, SessionEvent::Fail(category))
        {
            Ok(mutation) => mutation.terminal_outcome,
            Err(CoordinatorError::SessionNotFound) => None,
            Err(error) => return Err(error.into()),
        };
        if let Some(outcome) = terminal_outcome {
            self.publish_recording_hud_terminal_best_effort(session_id, outcome)
                .await;
        }
        self.clear_active_if_current(session_id).await?;
        let notification = match category {
            FailureCategory::Asr => Some(UserNotificationKind::Asr),
            FailureCategory::Audio
            | FailureCategory::Llm
            | FailureCategory::Delivery
            | FailureCategory::Storage
            | FailureCategory::Lifecycle => None,
        };
        if let Some(notification) = notification {
            self.raise_user_notification_best_effort(session_id, notification)
                .await;
        }
        Ok(true)
    }

    async fn complete_no_speech_current(
        &self,
        session_id: SessionId,
    ) -> Result<bool, OrchestratorError> {
        if !self.apply_if_current(session_id, SessionEvent::NoSpeech)? {
            return Ok(false);
        }
        self.publish_recording_hud_terminal_best_effort(session_id, TerminalOutcome::Cancelled)
            .await;
        self.clear_active_if_current(session_id).await?;
        Ok(true)
    }

    async fn clear_active_if_current(
        &self,
        session_id: SessionId,
    ) -> Result<(), OrchestratorError> {
        let cleared = {
            let mut runtime = self.runtime()?;
            if runtime
                .active
                .as_ref()
                .is_some_and(|active| active.session_id == session_id)
                && let Some(active) = runtime.active.take()
            {
                active.lifecycle.invalidate();
                true
            } else {
                false
            }
        };
        if cleared {
            self.hide_recording_hud_best_effort(session_id).await;
        }
        Ok(())
    }

    async fn raise_user_notification_best_effort(
        &self,
        session_id: SessionId,
        kind: UserNotificationKind,
    ) {
        if let Err(error) = self.ports.user_notifications.raise(session_id, kind).await {
            self.record_port_error(Some(session_id), "user_notification", &error);
        }
    }

    async fn update_recording_hud_best_effort(
        &self,
        session_id: SessionId,
        state: RecordingHudState,
    ) {
        if let Err(error) = self.ports.recording_hud.update(session_id, state).await {
            self.record_port_error(Some(session_id), "recording_hud_update", &error);
        }
    }

    async fn publish_recording_hud_terminal_best_effort(
        &self,
        session_id: SessionId,
        outcome: TerminalOutcome,
    ) {
        if let Err(error) = self
            .ports
            .recording_hud
            .publish_terminal(session_id, outcome)
            .await
        {
            self.record_port_error(Some(session_id), "recording_hud_publish_terminal", &error);
        }
    }

    async fn hide_recording_hud_best_effort(&self, session_id: SessionId) {
        if let Err(error) = self.ports.recording_hud.hide(session_id).await {
            self.record_port_error(Some(session_id), "recording_hud_hide", &error);
        }
    }

    async fn play_recording_cue_best_effort(&self, session_id: SessionId, cue: RecordingCue) {
        if let Err(error) = self.ports.recording_cue.play(cue).await {
            self.record_port_error(Some(session_id), "recording_cue", &error);
        }
    }

    fn begin_recognizing_audio_cleanup(
        &self,
        session_id: SessionId,
        request_id: RequestId,
    ) -> Result<Option<AudioRef>, OrchestratorError> {
        let mut runtime = self.runtime()?;
        if runtime.quitting {
            return Ok(None);
        }
        let Some(active) = runtime.active.as_mut() else {
            return Ok(None);
        };
        if active.session_id != session_id {
            return Ok(None);
        }
        let audio_ref = match &active.stage {
            RuntimeStage::Recognizing {
                request_id: active_request_id,
                audio_ref,
            } if *active_request_id == request_id => audio_ref.clone(),
            _ => None,
        };
        if let Some(audio_ref) = audio_ref {
            active.stage = RuntimeStage::CleaningAudio {
                request_id,
                audio_ref: audio_ref.clone(),
                in_flight: true,
            };
            Ok(Some(audio_ref))
        } else {
            Ok(None)
        }
    }

    fn complete_recognizing_audio_cleanup(
        &self,
        session_id: SessionId,
        request_id: RequestId,
        cleaned_audio_ref: &AudioRef,
    ) -> Result<bool, OrchestratorError> {
        let mut runtime = self.runtime()?;
        if runtime.quitting {
            return Ok(false);
        }
        let Some(active) = runtime.active.as_mut() else {
            return Ok(false);
        };
        if active.session_id != session_id {
            return Ok(false);
        }
        let matches_cleanup = matches!(
            &active.stage,
            RuntimeStage::CleaningAudio {
                request_id: active_request_id,
                audio_ref,
                in_flight: true,
            } if *active_request_id == request_id && audio_ref == cleaned_audio_ref
        );
        if !matches_cleanup {
            return Ok(false);
        }
        active.stage = RuntimeStage::Recognizing {
            request_id,
            audio_ref: None,
        };
        Ok(true)
    }

    fn set_audio_cleanup_in_flight_if_current(
        &self,
        session_id: SessionId,
        value: bool,
    ) -> Result<(), OrchestratorError> {
        let mut runtime = self.runtime()?;
        let Some(active) = runtime
            .active
            .as_mut()
            .filter(|active| active.session_id == session_id)
        else {
            if let Some(preparing) = runtime
                .preparing
                .as_mut()
                .filter(|preparing| preparing.session_id == session_id)
                && let PreparingStage::Cancelling { in_flight, .. } = &mut preparing.stage
            {
                *in_flight = value;
            }
            return Ok(());
        };
        match &mut active.stage {
            RuntimeStage::Cancelling { in_flight, .. }
            | RuntimeStage::CleaningAudio { in_flight, .. } => *in_flight = value,
            _ => {}
        }
        Ok(())
    }

    fn is_expected_request(
        &self,
        session_id: SessionId,
        expected: ExpectedRequest,
    ) -> Result<bool, OrchestratorError> {
        self.is_current_stage(session_id, |stage| match (stage, expected) {
            (
                RuntimeStage::Recognizing { request_id, .. },
                ExpectedRequest::Asr(expected_request_id),
            ) => *request_id == expected_request_id,
            (
                RuntimeStage::Processing { request_id },
                ExpectedRequest::Llm(expected_request_id),
            ) => *request_id == expected_request_id,
            _ => false,
        })
    }

    fn is_expected_delivery(
        &self,
        session_id: SessionId,
        delivery_id: DeliveryId,
    ) -> Result<bool, OrchestratorError> {
        self.is_current_stage(session_id, |stage| {
            matches!(
                stage,
                RuntimeStage::Delivering {
                    delivery_id: active_delivery_id
                } if *active_delivery_id == delivery_id
            )
        })
    }

    fn is_expected_finalization(
        &self,
        session_id: SessionId,
        delivery_id: DeliveryId,
    ) -> Result<bool, OrchestratorError> {
        self.is_current_stage(session_id, |stage| {
            matches!(
                stage,
                RuntimeStage::Finalizing {
                    delivery_id: active_delivery_id
                } if *active_delivery_id == delivery_id
            )
        })
    }

    fn valid_asr_result(
        &self,
        result: &AsrResult,
        session_id: SessionId,
        request_id: RequestId,
        route: ResolvedAsrRoute,
    ) -> bool {
        result.session_id == session_id
            && result.request_id == request_id
            && result.engine == route.engine()
    }

    fn valid_llm_result(
        &self,
        result: &TextProcessingResult,
        session_id: SessionId,
        request_id: RequestId,
    ) -> bool {
        result.session_id == session_id && result.request_id == request_id
    }

    async fn cancel_capture(
        &self,
        session_id: SessionId,
        capture: AudioCaptureRef,
    ) -> Result<(), OrchestratorError> {
        match self.ports.audio.cancel(capture.clone()).await {
            Ok(()) => {
                self.clear_audio_cleanup_debt(session_id, &AudioCleanupResource::Capture(capture))
            }
            Err(error) => {
                self.record_port_error(Some(session_id), "audio_cancel", &error);
                let debt = AudioCleanupDebt {
                    session_id,
                    phase: "audio_cancel",
                    resource: AudioCleanupResource::Capture(capture),
                    error,
                };
                let orchestrator_error = debt.orchestrator_error();
                self.record_audio_cleanup_debt(debt)?;
                Err(orchestrator_error)
            }
        }
    }

    async fn cleanup_audio(
        &self,
        session_id: SessionId,
        audio_ref: AudioRef,
    ) -> Result<(), OrchestratorError> {
        match self.ports.audio.cleanup(audio_ref.clone()).await {
            Ok(()) => self
                .clear_audio_cleanup_debt(session_id, &AudioCleanupResource::Finalized(audio_ref)),
            Err(error) => {
                self.record_port_error(Some(session_id), "audio_cleanup", &error);
                let debt = AudioCleanupDebt {
                    session_id,
                    phase: "audio_cleanup",
                    resource: AudioCleanupResource::Finalized(audio_ref),
                    error,
                };
                let orchestrator_error = debt.orchestrator_error();
                self.record_audio_cleanup_debt(debt)?;
                Err(orchestrator_error)
            }
        }
    }

    fn record_audio_cleanup_debt(&self, debt: AudioCleanupDebt) -> Result<(), OrchestratorError> {
        let mut runtime = self.runtime()?;
        if let Some(existing) = runtime.audio_cleanup_debts.iter_mut().find(|existing| {
            existing.session_id == debt.session_id && existing.resource == debt.resource
        }) {
            *existing = debt;
        } else {
            runtime.audio_cleanup_debts.push(debt);
        }
        Ok(())
    }

    fn clear_audio_cleanup_debt(
        &self,
        session_id: SessionId,
        resource: &AudioCleanupResource,
    ) -> Result<(), OrchestratorError> {
        let mut runtime = self.runtime()?;
        runtime
            .audio_cleanup_debts
            .retain(|debt| debt.session_id != session_id || &debt.resource != resource);
        Ok(())
    }

    fn first_audio_cleanup_debt(&self) -> Result<Option<AudioCleanupDebt>, OrchestratorError> {
        Ok(self.runtime()?.audio_cleanup_debts.first().cloned())
    }

    async fn retry_all_audio_cleanup_debts(&self) -> Result<(), OrchestratorError> {
        let debts = self.runtime()?.audio_cleanup_debts.clone();
        for debt in debts {
            let _ = match debt.resource {
                AudioCleanupResource::Capture(capture) => {
                    self.cancel_capture(debt.session_id, capture).await
                }
                AudioCleanupResource::Finalized(audio_ref) => {
                    self.cleanup_audio(debt.session_id, audio_ref).await
                }
            };
        }
        Ok(())
    }

    fn record_port_error(&self, session_id: Option<SessionId>, phase: &str, error: &PortError) {
        self.ports.diagnostics.record(DiagnosticEvent {
            session_id,
            phase: Some(phase.to_owned()),
            state: Some("failed".to_owned()),
            duration_ms: None,
            error_code: Some(error.code.clone()),
            detail: None,
        });
    }

    fn record_no_speech(&self, session_id: SessionId, phase: &str) {
        self.ports.diagnostics.record(DiagnosticEvent {
            session_id: Some(session_id),
            phase: Some(phase.to_owned()),
            state: Some("no_speech".to_owned()),
            duration_ms: None,
            error_code: None,
            detail: None,
        });
    }

    fn record_late_response(&self, session_id: SessionId, phase: &str) {
        self.ports.diagnostics.record(DiagnosticEvent {
            session_id: Some(session_id),
            phase: Some(phase.to_owned()),
            state: Some("discarded".to_owned()),
            duration_ms: None,
            error_code: Some("late_or_mismatched_response".to_owned()),
            detail: None,
        });
    }
}

async fn join_futures<A, B>(first: A, second: B) -> (A::Output, B::Output)
where
    A: Future,
    B: Future,
{
    let mut first = Box::pin(first);
    let mut second = Box::pin(second);
    let mut first_output = None;
    let mut second_output = None;

    std::future::poll_fn(move |context| {
        if first_output.is_none()
            && let Poll::Ready(output) = first.as_mut().poll(context)
        {
            first_output = Some(output);
        }
        if second_output.is_none()
            && let Poll::Ready(output) = second.as_mut().poll(context)
        {
            second_output = Some(output);
        }
        match (first_output.take(), second_output.take()) {
            (Some(first), Some(second)) => Poll::Ready((first, second)),
            (first, second) => {
                first_output = first;
                second_output = second;
                Poll::Pending
            }
        }
    })
    .await
}

#[derive(Default)]
struct RuntimeState {
    quitting: bool,
    starting: Option<SessionId>,
    preparing: Option<PreparingWorkflow>,
    active: Option<ActiveWorkflow>,
    audio_cleanup_debts: Vec<AudioCleanupDebt>,
}

impl RuntimeState {
    fn active_session_id(&self) -> Option<SessionId> {
        self.active
            .as_ref()
            .map(|active| active.session_id)
            .or_else(|| {
                self.preparing
                    .as_ref()
                    .map(|preparing| preparing.session_id)
            })
            .or(self.starting)
            .or_else(|| self.audio_cleanup_debts.first().map(|debt| debt.session_id))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AudioCleanupDebt {
    session_id: SessionId,
    phase: &'static str,
    resource: AudioCleanupResource,
    error: PortError,
}

impl AudioCleanupDebt {
    fn orchestrator_error(&self) -> OrchestratorError {
        OrchestratorError::AudioCleanupFailed {
            session_id: self.session_id,
            phase: self.phase,
            source: self.error.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AudioCleanupResource {
    Capture(AudioCaptureRef),
    Finalized(AudioRef),
}

struct PreparingWorkflow {
    session_id: SessionId,
    settings: SettingsSnapshot,
    processing_route: TextProcessingRoute,
    target_ref: TargetSnapshotRef,
    target_security: TargetSecurity,
    selected_text: Option<String>,
    lifecycle: LifecycleFence,
    recording_deadline: Option<RecordingDeadlineGuard>,
    recording_limit_elapsed: bool,
    stage: PreparingStage,
}

enum PreparingStage {
    StartingAudio,
    Capturing {
        capture: AudioCaptureRef,
    },
    Cancelling {
        capture: AudioCaptureRef,
        in_flight: bool,
    },
}

struct ActiveWorkflow {
    session_id: SessionId,
    settings: SettingsSnapshot,
    processing_route: TextProcessingRoute,
    asr_route: ResolvedAsrRoute,
    target_ref: TargetSnapshotRef,
    target_security: TargetSecurity,
    selected_text: Option<String>,
    lifecycle: LifecycleFence,
    recording_deadline: Option<RecordingDeadlineGuard>,
    stage: RuntimeStage,
}

#[derive(Clone)]
struct ActiveContext {
    settings: SettingsSnapshot,
    processing_route: TextProcessingRoute,
    asr_route: ResolvedAsrRoute,
    target_ref: TargetSnapshotRef,
    target_security: TargetSecurity,
    selected_text: Option<String>,
    lifecycle: LifecycleFence,
}

impl From<&ActiveWorkflow> for ActiveContext {
    fn from(active: &ActiveWorkflow) -> Self {
        Self {
            settings: active.settings.clone(),
            processing_route: active.processing_route.clone(),
            asr_route: active.asr_route,
            target_ref: active.target_ref.clone(),
            target_security: active.target_security,
            selected_text: active.selected_text.clone(),
            lifecycle: active.lifecycle.clone(),
        }
    }
}

enum RuntimeStage {
    Recording {
        capture: AudioCaptureRef,
    },
    Cancelling {
        capture: AudioCaptureRef,
        completion: CaptureCleanupCompletion,
        in_flight: bool,
    },
    FinishingAudio {
        capture: AudioCaptureRef,
    },
    Recognizing {
        request_id: RequestId,
        audio_ref: Option<AudioRef>,
    },
    CleaningAudio {
        request_id: RequestId,
        audio_ref: AudioRef,
        in_flight: bool,
    },
    Processing {
        request_id: RequestId,
    },
    Delivering {
        delivery_id: DeliveryId,
    },
    Finalizing {
        delivery_id: DeliveryId,
    },
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CaptureCleanupCompletion {
    Cancelled,
    FailAudio,
}

#[derive(Clone, Copy)]
enum AudioCleanupCompletion {
    Preparing,
    Capture(CaptureCleanupCompletion),
    FailAudio,
}

enum RecordingPreparation {
    Ready(AudioCaptureRef),
    FailedAudio,
    FailedLifecycle,
    RejectedHud,
    Aborted,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CaptureCancellationTarget {
    Preparing,
    Active(CaptureCleanupCompletion),
}

enum CaptureCancellationSelection {
    Immediate(CancelOutcome),
    Capture {
        capture: AudioCaptureRef,
        target: CaptureCancellationTarget,
    },
}

/// Application-internal async quiescence barrier. It reuses the same proven
/// Waker-driven primitive as the adapter commit fence, but guards whole public
/// workflow futures so shutdown can also cover StartingAudio and cancellable
/// remote work without depending on a particular async runtime.
struct OperationBarrier {
    fence: LifecycleFence,
}

impl OperationBarrier {
    fn new() -> Self {
        Self {
            fence: LifecycleFence::new(),
        }
    }

    fn enter(&self) -> Option<crate::ports::CommitGuard> {
        self.fence.begin_commit()
    }

    fn begin_quiescing(&self) {
        self.fence.invalidate();
    }

    fn wait_quiescent(&self) -> crate::ports::LifecycleQuiescence {
        self.fence.wait_quiescent()
    }
}

type QuitResult = Result<QuitOutcome, OrchestratorError>;

/// Coordinates all shutdown callers around exactly one leader. Followers never
/// repeat cancellation or observe partially quiesced state; they await the same
/// cached result through a runtime-independent Waker-driven future.
struct ExitCompletion {
    inner: Arc<Mutex<ExitState>>,
}

enum ExitState {
    Open,
    Running(ExitAttempt),
    Complete(QuitOutcome),
}

enum ExitRole {
    Leader(ExitAttempt),
    Follower(ExitWaiter),
    Completed(QuitOutcome),
}

impl ExitCompletion {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ExitState::Open)),
        }
    }

    fn begin(&self) -> ExitRole {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*state {
            ExitState::Open => {
                let attempt = ExitAttempt::new();
                *state = ExitState::Running(attempt.clone());
                ExitRole::Leader(attempt)
            }
            ExitState::Running(attempt) => ExitRole::Follower(ExitWaiter {
                attempt: attempt.clone(),
            }),
            ExitState::Complete(outcome) => ExitRole::Completed(*outcome),
        }
    }

    fn complete(&self, attempt: &ExitAttempt, result: QuitResult) {
        attempt.complete(result.clone());
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(&*state, ExitState::Running(current) if current.same_attempt(attempt)) {
            *state = match result {
                Ok(outcome) => ExitState::Complete(outcome),
                Err(_) => ExitState::Open,
            };
        }
    }
}

#[derive(Clone)]
struct ExitAttempt {
    inner: Arc<Mutex<ExitAttemptState>>,
}

struct ExitAttemptState {
    result: Option<QuitResult>,
    waiters: Vec<Waker>,
}

impl ExitAttempt {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ExitAttemptState {
                result: None,
                waiters: Vec::new(),
            })),
        }
    }

    fn same_attempt(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    fn complete(&self, result: QuitResult) {
        let waiters = {
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.result = Some(result);
            std::mem::take(&mut state.waiters)
        };
        for waiter in waiters {
            waiter.wake();
        }
    }
}

struct ExitWaiter {
    attempt: ExitAttempt,
}

impl Future for ExitWaiter {
    type Output = QuitResult;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self
            .attempt
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(result) = &state.result {
            Poll::Ready(result.clone())
        } else {
            if !state
                .waiters
                .iter()
                .any(|waiter| waiter.will_wake(context.waker()))
            {
                state.waiters.push(context.waker().clone());
            }
            Poll::Pending
        }
    }
}

#[derive(Clone, Copy)]
enum ExpectedRequest {
    Asr(RequestId),
    Llm(RequestId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartOutcome {
    Started { session_id: SessionId },
    Busy { active_session_id: SessionId },
    Rejected(RejectReason),
    Failed(FailureCategory),
    Quitting,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelOutcome {
    Cancelled,
    CleanupRecovered,
    CleanupInProgress,
    NotRecording,
    NotFound,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioCleanupOutcome {
    Recovered,
    InProgress,
    NotPending,
    Quitting,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FinishOutcome {
    Completed(Completion),
    Failed(FailureCategory),
    NoSpeech,
    Discarded,
    NotRecording,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completion {
    pub final_text: String,
    pub delivery: DeliveryKind,
    pub direct_delivery_reason: Option<DirectDeliveryReason>,
    pub warnings: Vec<FinalizationWarning>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryKind {
    Inserted,
    ClipboardBridge,
    TemporaryText,
}

enum DeliveryProgress {
    Delivered(DeliveryKind, Option<UserNotificationKind>),
    Discarded,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinalizationWarning {
    AutoCopyFailed,
    HistorySaveFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuitOutcome {
    Idle,
    Terminated(SessionId),
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum OrchestratorError {
    #[error("orchestrator runtime state lock is poisoned")]
    RuntimeLockPoisoned,
    #[error("audio cleanup failed during {phase} for session {session_id}: {source}")]
    AudioCleanupFailed {
        session_id: SessionId,
        phase: &'static str,
        #[source]
        source: PortError,
    },
    #[error(transparent)]
    Coordinator(#[from] CoordinatorError),
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::{Mutex, mpsc};
    use std::task::{Context, Poll, Wake, Waker};
    use std::thread;
    use std::time::{Duration, Instant};

    use remtene_domain::{
        AsrPreference, HistoryPolicy, IntentDecision, ProcessingMode, RecordingMode,
        SettingsSnapshotInput, TimestampMs,
    };

    use super::*;
    use crate::ports::{
        AudioFormat, CapturedTarget, FinalizedAudio, PortFuture, SelectionSnapshot,
        ValidatedTargetRef,
    };

    #[test]
    fn public_async_workflows_are_send_and_do_not_capture_mutex_guards() {
        fn assert_send<T: Send>(_: T) {}

        let (orchestrator, _) = harness(MockConfig::faithful());
        assert_send(orchestrator.start());
        assert_send(orchestrator.finish_recording(SessionId::new()));
        assert_send(orchestrator.cancel_recording(SessionId::new()));
        assert_send(orchestrator.quit());
    }

    #[test]
    fn faithful_flow_runs_all_stages_and_saves_only_final_text() {
        let mut config = MockConfig::faithful();
        config.target = Ok(CapturedTarget {
            display_hint: Some(TargetDisplayHint { x: -900, y: 420 }),
            ..target(TargetSecurity::Safe, true)
        });
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("faithful flow should not violate orchestration invariants");

        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected completion, got {outcome:?}");
        };
        assert_eq!(completion.final_text, "clean text");
        assert_eq!(completion.delivery, DeliveryKind::Inserted);
        assert_eq!(completion.direct_delivery_reason, None);
        assert!(completion.warnings.is_empty());

        let calls = mock.calls();
        assert_eq!(calls.selection_reads, 1);
        assert_eq!(calls.asr_requests.len(), 1);
        assert_eq!(calls.llm_requests.len(), 1);
        assert_eq!(
            calls.llm_requests[0].selected_text.as_deref(),
            Some("context")
        );
        assert_eq!(calls.audio_cleanups, 1);
        assert_eq!(calls.inserted_texts, vec!["clean text"]);
        assert!(calls.temporary_texts.is_empty());
        assert_eq!(calls.history_records.len(), 1);
        assert_eq!(calls.history_records[0].final_text, "clean text");
        assert_eq!(calls.history_policy_enforcements, 1);
        assert_eq!(
            calls.recording_hud_shows,
            vec![(session_id, RecordingHudState::Preparing)]
        );
        assert_eq!(
            calls.recording_hud_show_hints,
            vec![Some(TargetDisplayHint { x: -900, y: 420 })]
        );
        assert_eq!(
            calls.recording_hud_updates,
            vec![
                (session_id, RecordingHudState::Recording),
                (session_id, RecordingHudState::Recognizing),
                (session_id, RecordingHudState::Processing),
                (session_id, RecordingHudState::Delivering),
                (session_id, RecordingHudState::Finalizing),
                (session_id, RecordingHudState::Completed),
            ]
        );
        assert_eq!(
            calls.recording_hud_terminals,
            vec![(session_id, TerminalOutcome::Completed)]
        );
        assert_eq!(calls.recording_hud_hides, vec![session_id]);
    }

    #[test]
    fn recording_limit_uses_the_normal_finish_path_and_submits_only_once() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let session_id = start_session(&orchestrator);

        block_on(Arc::clone(&orchestrator).recording_limit_elapsed(session_id));
        assert_eq!(
            block_on(orchestrator.finish_recording(session_id))
                .expect("a simultaneous manual finish remains typed"),
            FinishOutcome::NotRecording
        );

        let calls = mock.calls();
        assert_eq!(calls.audio_finishes, 1);
        assert_eq!(calls.asr_requests.len(), 1);
        assert_eq!(calls.llm_requests.len(), 1);
        assert_eq!(calls.inserted_texts, vec!["clean text"]);
        assert_eq!(calls.history_records.len(), 1);
    }

    #[test]
    fn recording_cues_wrap_microphone_ownership_in_the_expected_order() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("finish should remain valid");
        assert!(matches!(outcome, FinishOutcome::Completed(_)));

        let calls = mock.calls();
        assert_eq!(
            calls.recording_cues,
            vec![RecordingCue::Start, RecordingCue::Finish]
        );
        assert_eq!(
            calls.recording_lifecycle_events,
            vec!["cue_start", "audio_start", "audio_finish", "cue_finish"]
        );
    }

    #[test]
    fn user_cancellation_plays_cancel_cue_only_after_capture_closes() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let session_id = start_session(&orchestrator);

        assert_eq!(
            block_on(orchestrator.cancel_recording(session_id)).expect("cancel"),
            CancelOutcome::Cancelled
        );
        let calls = mock.calls();
        assert_eq!(
            calls.recording_cues,
            vec![RecordingCue::Start, RecordingCue::Cancel]
        );
        assert_eq!(
            calls.recording_lifecycle_events,
            vec!["cue_start", "audio_start", "audio_cancel", "cue_cancel"]
        );
    }

    #[test]
    fn cue_failure_is_diagnostic_only_and_never_blocks_the_workflow() {
        let mut config = MockConfig::faithful();
        config.recording_cue = Err(port_error("recording_cue.play_failed"));
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("cue failure is best effort");
        assert!(matches!(outcome, FinishOutcome::Completed(_)));
        assert_eq!(mock.calls().recording_cues.len(), 2);
        assert_eq!(
            mock.calls()
                .diagnostics
                .iter()
                .filter(|event| event.error_code.as_deref() == Some("recording_cue.play_failed"))
                .count(),
            2
        );
    }

    #[test]
    fn recording_limit_during_health_finishes_immediately_after_preflight() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let (health_started, release_health) = mock.install_health_gate();
        let start_orchestrator = Arc::clone(&orchestrator);
        let start_worker = thread::spawn(move || {
            block_on(start_orchestrator.start()).expect("start should remain typed")
        });
        health_started
            .recv_timeout(Duration::from_secs(2))
            .expect("health check should still be pending");
        let session_id = mock.calls().recording_hud_shows[0].0;

        block_on(Arc::clone(&orchestrator).recording_limit_elapsed(session_id));
        release_health
            .send(())
            .expect("health check should still be pending");
        assert_eq!(
            start_worker.join().expect("start thread should not panic"),
            StartOutcome::Started { session_id }
        );
        assert_eq!(
            block_on(orchestrator.finish_recording(session_id))
                .expect("automatic finish should consume the recording"),
            FinishOutcome::NotRecording
        );

        let calls = mock.calls();
        assert_eq!(calls.audio_finishes, 1);
        assert_eq!(calls.asr_requests.len(), 1);
        assert_eq!(calls.llm_requests.len(), 1);
        assert_eq!(calls.history_records.len(), 1);
    }

    #[test]
    fn a_completed_session_does_not_block_the_next_one() {
        // Single-flight bounds concurrency, not lifetime: once a dictation completes, the
        // next trigger must run the whole chain again.
        let mut config = MockConfig::faithful();
        config.settings = settings(ProcessingMode::Raw, false, true);
        let (orchestrator, mock) = harness(config);

        let first = start_session(&orchestrator);
        let first_outcome = block_on(orchestrator.finish_recording(first))
            .expect("first dictation should not violate orchestration invariants");
        assert!(
            matches!(first_outcome, FinishOutcome::Completed(_)),
            "expected the first dictation to complete, got {first_outcome:?}"
        );

        let second = match block_on(orchestrator.start())
            .expect("second start should not violate invariants")
        {
            StartOutcome::Started { session_id } => session_id,
            outcome => panic!("a completed session must not keep the slot: {outcome:?}"),
        };
        assert_ne!(first, second, "each dictation gets a fresh session");

        let second_outcome = block_on(orchestrator.finish_recording(second))
            .expect("second dictation should not violate orchestration invariants");
        let FinishOutcome::Completed(completion) = second_outcome else {
            panic!("expected the second dictation to complete, got {second_outcome:?}");
        };
        assert_eq!(completion.final_text, "raw text");

        let calls = mock.calls();
        assert_eq!(calls.asr_requests.len(), 2, "both dictations reach ASR");
        assert_eq!(
            calls.inserted_texts,
            vec!["raw text", "raw text"],
            "both dictations deliver their text"
        );
        assert_eq!(calls.audio_cleanups, 2, "neither dictation leaks its audio");
    }

    #[test]
    fn raw_mode_bypasses_selection_and_llm_and_can_disable_history() {
        let mut config = MockConfig::faithful();
        config.settings = settings(ProcessingMode::Raw, true, false);
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome =
            block_on(orchestrator.finish_recording(session_id)).expect("raw flow should be valid");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected completion, got {outcome:?}");
        };

        assert_eq!(
            completion.direct_delivery_reason,
            Some(DirectDeliveryReason::RawMode)
        );
        let calls = mock.calls();
        assert_eq!(calls.selection_reads, 0);
        assert_eq!(
            calls.llm_route_resolutions, 0,
            "raw mode must not inspect Provider configuration or SecretStore"
        );
        assert!(calls.llm_requests.is_empty());
        assert!(calls.history_records.is_empty());
        assert_eq!(calls.history_policy_enforcements, 0);
        assert_eq!(
            calls.recording_hud_updates,
            vec![
                (session_id, RecordingHudState::Recording),
                (session_id, RecordingHudState::Recognizing),
                (session_id, RecordingHudState::Delivering),
                (session_id, RecordingHudState::Finalizing),
                (session_id, RecordingHudState::Completed),
            ]
        );
    }

    #[test]
    fn missing_api_delivers_asr_without_reading_selection_or_calling_llm() {
        let mut config = MockConfig::faithful();
        config.settings = settings(ProcessingMode::Faithful, false, true);
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("missing API is a supported direct-delivery route");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected completion, got {outcome:?}");
        };

        assert_eq!(completion.final_text, "raw text");
        assert_eq!(
            completion.direct_delivery_reason,
            Some(DirectDeliveryReason::LlmNotConfigured)
        );
        let calls = mock.calls();
        assert_eq!(calls.selection_reads, 0);
        assert!(calls.llm_requests.is_empty());
        assert_eq!(calls.inserted_texts, vec!["raw text"]);
        assert_eq!(calls.clipboard_selection_reads, 0);
        assert_eq!(calls.clipboard_insert_attempts, 0);
    }

    #[test]
    fn unavailable_llm_route_preserves_raw_text_in_temporary_output() {
        let mut config = MockConfig::faithful();
        config.llm_route_error = Some(retryable_port_error("llm.secret_unavailable"));
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("unavailable LLM route should preserve the local transcript");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected fallback completion, got {outcome:?}");
        };

        assert_eq!(completion.final_text, "raw text");
        assert_eq!(completion.delivery, DeliveryKind::TemporaryText);
        assert_eq!(
            completion.direct_delivery_reason,
            Some(DirectDeliveryReason::LlmUnavailable)
        );
        let calls = mock.calls();
        assert_eq!(calls.llm_route_resolutions, 1);
        assert_eq!(calls.selection_reads, 0);
        assert!(calls.llm_requests.is_empty());
        assert!(calls.inserted_texts.is_empty());
        assert_eq!(
            calls.temporary_text_statuses,
            vec![TemporaryTextStatus::LlmFallback]
        );
    }

    #[test]
    fn resolved_llm_route_is_frozen_for_the_active_session() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let session_id = start_session(&orchestrator);

        let current = mock.config.lock().expect("config lock").settings.clone();
        let mut changed = current.into_input();
        changed.llm = Some(
            remtene_domain::LlmNonSecretSettings::new(
                "https://changed.invalid/v1",
                "changed-model",
            )
            .expect("changed LLM settings"),
        );
        mock.config.lock().expect("config lock").settings =
            SettingsSnapshot::new(changed).expect("changed settings");

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("a settings change must not reroute an active session");
        assert!(matches!(outcome, FinishOutcome::Completed(_)));

        let calls = mock.calls();
        assert_eq!(calls.llm_routes.len(), 1);
        assert_eq!(
            calls.llm_routes[0].endpoint(),
            "https://provider.invalid/v1/chat/completions"
        );
        assert_eq!(calls.llm_routes[0].model(), "model");
    }

    #[test]
    fn secure_input_uses_user_directed_paste_without_reading_selection() {
        let mut config = MockConfig::faithful();
        config.settings = settings_with_clipboard(ProcessingMode::Faithful, true, true, true);
        config.target = Ok(target(TargetSecurity::SecureInput, true));
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("user owns the secure input focus under compatibility mode");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected user-directed completion, got {outcome:?}");
        };

        assert_eq!(completion.delivery, DeliveryKind::ClipboardBridge);
        let calls = mock.calls();
        assert_eq!(calls.audio_starts, 1);
        assert_eq!(calls.microphone_permission_requests, 1);
        assert_eq!(calls.selection_reads, 0);
        assert_eq!(calls.user_directed_paste_attempts, 1);
        assert_eq!(calls.user_directed_pasted_texts, vec!["clean text"]);
        assert!(calls.inserted_texts.is_empty());
        assert!(calls.temporary_texts.is_empty());
        assert_eq!(calls.history_records.len(), 1);
    }

    #[test]
    fn oversized_selection_is_rejected_without_recording() {
        let mut config = MockConfig::faithful();
        config.selection = Ok(SelectionSnapshot {
            text: Some("too much context".to_owned()),
            anchor_normalized_to_end: true,
            exceeded_limit: true,
        });
        let (orchestrator, mock) = harness(config);

        assert_eq!(
            block_on(orchestrator.start()).expect("selection rejection should be deterministic"),
            StartOutcome::Rejected(RejectReason::SelectionTooLong)
        );
        let calls = mock.calls();
        assert_eq!(calls.selection_reads, 1);
        assert_eq!(calls.audio_starts, 0);
        assert_eq!(calls.health_checks, 0);
        assert!(calls.user_notifications.is_empty());
    }

    #[test]
    fn selection_permission_failure_does_not_claim_microphone_is_disabled() {
        let mut config = MockConfig::faithful();
        config.selection = Err(port_error("selection_permission_unavailable"));
        let (orchestrator, mock) = harness(config);

        assert_eq!(
            block_on(orchestrator.start()).expect("selection rejection should remain typed"),
            StartOutcome::Rejected(RejectReason::PermissionUnavailable)
        );
        let calls = mock.calls();
        assert_eq!(calls.selection_reads, 1);
        assert_eq!(calls.microphone_permission_requests, 0);
        assert!(calls.user_notifications.is_empty());
    }

    #[test]
    fn unavailable_asr_cancels_parallel_recording_without_downstream_effects() {
        let mut config = MockConfig::faithful();
        config.qwen_health = EngineHealth::Unhealthy;
        config.whisper_health = EngineHealth::Missing;
        let (orchestrator, mock) = harness(config);

        assert_eq!(
            block_on(orchestrator.start()).expect("ASR rejection should be deterministic"),
            StartOutcome::Rejected(RejectReason::AsrUnavailable)
        );
        let calls = mock.calls();
        assert_eq!(calls.health_checks, 1);
        assert_eq!(calls.microphone_permission_requests, 1);
        assert_eq!(calls.audio_starts, 1);
        assert_eq!(calls.audio_cancels, 1);
        assert_eq!(calls.audio_finishes, 0);
        assert_eq!(calls.recording_hud_shows.len(), 1);
        assert_eq!(calls.recording_hud_shows[0].1, RecordingHudState::Preparing);
        assert!(calls.recording_hud_updates.is_empty());
        assert_eq!(calls.recording_hud_hides.len(), 1);
        assert!(calls.asr_requests.is_empty());
        assert!(calls.llm_requests.is_empty());
        assert!(calls.inserted_texts.is_empty());
        assert!(calls.history_records.is_empty());
        assert_eq!(
            calls.user_notifications,
            vec![(calls.recording_hud_shows[0].0, UserNotificationKind::Asr)]
        );
    }

    #[test]
    fn recording_and_preparing_hud_are_active_while_health_is_pending() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let (health_started, release_health) = mock.install_health_gate();

        let start_orchestrator = Arc::clone(&orchestrator);
        let start_worker = thread::spawn(move || {
            block_on(start_orchestrator.start()).expect("parallel start must remain valid")
        });
        health_started
            .recv_timeout(Duration::from_secs(2))
            .expect("health check should reach the controlled pending point");

        let pending_calls = mock.calls();
        assert_eq!(pending_calls.audio_starts, 1);
        assert_eq!(pending_calls.recording_hud_shows.len(), 1);
        assert_eq!(
            pending_calls.recording_hud_shows[0].1,
            RecordingHudState::Preparing
        );
        assert!(pending_calls.recording_hud_updates.is_empty());

        release_health
            .send(())
            .expect("pending health check should still be waiting");
        let StartOutcome::Started { session_id } =
            start_worker.join().expect("start thread should not panic")
        else {
            panic!("healthy route should activate the recording Session");
        };
        assert_eq!(
            mock.calls().recording_hud_updates,
            vec![(session_id, RecordingHudState::Recording)]
        );
        assert_eq!(
            block_on(orchestrator.cancel_recording(session_id))
                .expect("test recording should cancel cleanly"),
            CancelOutcome::Cancelled
        );
    }

    #[test]
    fn cancel_during_pending_health_discards_preparation_without_session_revival() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let (health_started, release_health) = mock.install_health_gate();

        let start_orchestrator = Arc::clone(&orchestrator);
        let start_worker = thread::spawn(move || {
            block_on(start_orchestrator.start()).expect("parallel start must remain valid")
        });
        health_started
            .recv_timeout(Duration::from_secs(2))
            .expect("health check should reach the controlled pending point");
        let session_id = mock.calls().recording_hud_shows[0].0;

        assert_eq!(
            block_on(orchestrator.finish_recording(session_id))
                .expect("preparing HUD must not finish before route freeze"),
            FinishOutcome::NotRecording
        );
        assert_eq!(
            block_on(orchestrator.cancel_recording(session_id))
                .expect("preparing recording cancellation should be valid"),
            CancelOutcome::Cancelled
        );
        release_health
            .send(())
            .expect("pending health check should still be waiting");
        assert_eq!(
            start_worker.join().expect("start thread should not panic"),
            StartOutcome::Quitting
        );

        let calls = mock.calls();
        assert_eq!(calls.audio_cancels, 1);
        assert_eq!(calls.recording_hud_hides, vec![session_id]);
        assert!(calls.recording_hud_updates.is_empty());
        assert!(calls.asr_requests.is_empty());
        assert!(calls.llm_requests.is_empty());
        assert!(calls.history_records.is_empty());
    }

    #[test]
    fn authorized_clipboard_selection_fallback_is_used_only_after_native_read_failure() {
        let mut config = MockConfig::faithful();
        config.settings = settings_with_clipboard(ProcessingMode::Faithful, true, true, true);
        config.selection = Err(port_error("native_selection_unavailable"));
        config.clipboard_selection = Ok(SelectionSnapshot {
            text: Some("clipboard context".to_owned()),
            anchor_normalized_to_end: true,
            exceeded_limit: false,
        });
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        block_on(orchestrator.cancel_recording(session_id))
            .expect("fallback selection must not affect cancellation");
        let calls = mock.calls();
        assert_eq!(calls.selection_reads, 1);
        assert_eq!(calls.clipboard_selection_reads, 1);
        assert_eq!(calls.audio_starts, 1);
    }

    #[test]
    fn denied_microphone_permission_rejects_before_opening_audio_or_hud() {
        let mut config = MockConfig::faithful();
        config.microphone_access = Ok(MicrophoneAccess::Denied);
        let (orchestrator, mock) = harness(config);

        assert_eq!(
            block_on(orchestrator.start()).expect("permission rejection should be deterministic"),
            StartOutcome::Rejected(RejectReason::PermissionUnavailable)
        );

        let calls = mock.calls();
        assert_eq!(calls.microphone_permission_requests, 1);
        assert_eq!(calls.audio_starts, 0);
        assert!(calls.recording_hud_shows.is_empty());
        assert!(calls.asr_requests.is_empty());
        assert!(calls.llm_requests.is_empty());
        assert!(calls.inserted_texts.is_empty());
        assert!(calls.history_records.is_empty());
        assert_eq!(calls.user_notifications.len(), 1);
        assert_eq!(
            calls.user_notifications[0].1,
            UserNotificationKind::MicrophonePermission
        );
    }

    #[test]
    fn recording_deadline_failure_cancels_the_registered_capture_and_allows_recovery() {
        let mut config = MockConfig::faithful();
        config.recording_deadline = Err(port_error("recording_deadline_unavailable"));
        let (orchestrator, mock) = harness(config);

        assert_eq!(
            block_on(orchestrator.start()).expect("deadline rejection should remain typed"),
            StartOutcome::Failed(FailureCategory::Lifecycle)
        );
        let calls = mock.calls();
        assert_eq!(calls.audio_starts, 1);
        assert_eq!(calls.audio_cancels, 1);
        assert!(calls.recording_hud_shows.is_empty());
        assert!(calls.diagnostics.iter().any(|event| {
            event.phase.as_deref() == Some("recording_deadline")
                && event.error_code.as_deref() == Some("recording_deadline_unavailable")
        }));

        mock.config.lock().expect("config lock").recording_deadline = Ok(());
        let session_id = start_session(&orchestrator);
        assert_eq!(
            block_on(orchestrator.cancel_recording(session_id))
                .expect("a later recording should recover"),
            CancelOutcome::Cancelled
        );
    }

    #[test]
    fn hud_show_failure_rejects_only_after_capture_is_cancelled() {
        let mut config = MockConfig::faithful();
        config.recording_hud_show = Err(port_error("recording_hud_unavailable"));
        let (orchestrator, mock) = harness(config);
        let (cancel_started, release_cancel) = mock.install_audio_cancel_gate();

        let start_orchestrator = Arc::clone(&orchestrator);
        let start_worker = thread::spawn(move || {
            block_on(start_orchestrator.start()).expect("HUD rejection should remain valid")
        });
        cancel_started
            .recv_timeout(Duration::from_secs(2))
            .expect("HUD failure must close the microphone before rejection");

        let session_id = mock.calls().recording_hud_shows[0].0;
        assert_eq!(
            block_on(orchestrator.start()).expect("cleanup in progress should remain typed"),
            StartOutcome::Busy {
                active_session_id: session_id,
            }
        );

        release_cancel
            .send(())
            .expect("capture cancellation should still be pending");
        assert_eq!(
            start_worker.join().expect("start thread should not panic"),
            StartOutcome::Rejected(RejectReason::RecordingHudUnavailable)
        );

        let calls = mock.calls();
        assert_eq!(calls.audio_starts, 1);
        assert_eq!(calls.audio_cancels, 1);
        assert_eq!(calls.recording_hud_hides, vec![session_id]);
        assert!(calls.asr_requests.is_empty());
        assert!(calls.llm_requests.is_empty());
        assert!(calls.inserted_texts.is_empty());
        assert!(calls.history_records.is_empty());
        assert!(calls.diagnostics.iter().any(|event| {
            event.session_id == Some(session_id)
                && event.phase.as_deref() == Some("recording_hud_show")
                && event.error_code.as_deref() == Some("recording_hud_unavailable")
        }));
    }

    #[test]
    fn hud_show_failure_with_failed_cancel_retains_cleanup_debt_and_single_flight() {
        let mut config = MockConfig::faithful();
        config.recording_hud_show = Err(port_error("recording_hud_unavailable"));
        config.audio_cancel_failures_remaining = 1;
        let (orchestrator, mock) = harness(config);

        let error = block_on(orchestrator.start())
            .expect_err("failed microphone cancellation must retain ownership");
        let session_id = mock.calls().recording_hud_shows[0].0;
        assert!(matches!(
            error,
            OrchestratorError::AudioCleanupFailed {
                session_id: failed_session_id,
                phase: "audio_cancel",
                ..
            } if failed_session_id == session_id
        ));
        assert_eq!(
            block_on(orchestrator.start()).expect("cleanup debt should occupy Single-flight"),
            StartOutcome::Busy {
                active_session_id: session_id,
            }
        );
        assert!(mock.calls().recording_hud_hides.is_empty());

        assert_eq!(
            block_on(orchestrator.retry_audio_cleanup(session_id))
                .expect("retained capture should be retryable"),
            AudioCleanupOutcome::Recovered
        );
        let calls = mock.calls();
        assert_eq!(calls.audio_cancels, 2);
        assert_eq!(calls.recording_hud_hides, vec![session_id]);
        assert!(calls.asr_requests.is_empty());
        assert!(calls.llm_requests.is_empty());

        mock.config.lock().expect("config lock").recording_hud_show = Ok(());
        let next_session = start_session(&orchestrator);
        assert_ne!(next_session, session_id);
        assert_eq!(
            block_on(orchestrator.cancel_recording(next_session))
                .expect("recovered slot should accept a new Session"),
            CancelOutcome::Cancelled
        );
    }

    #[test]
    fn hud_ready_update_failure_cancels_capture_before_rejection() {
        let mut config = MockConfig::faithful();
        config.recording_hud_update = Err(port_error("recording_hud_ready_failed"));
        let (orchestrator, mock) = harness(config);

        assert_eq!(
            block_on(orchestrator.start()).expect("HUD ready rejection should remain valid"),
            StartOutcome::Rejected(RejectReason::RecordingHudUnavailable)
        );

        let calls = mock.calls();
        let session_id = calls.recording_hud_shows[0].0;
        assert_eq!(calls.audio_starts, 1);
        assert_eq!(calls.audio_cancels, 1);
        assert_eq!(
            calls.recording_hud_shows,
            vec![(session_id, RecordingHudState::Preparing)]
        );
        assert_eq!(
            calls.recording_hud_updates,
            vec![(session_id, RecordingHudState::Recording)]
        );
        assert!(calls.recording_hud_terminals.is_empty());
        assert_eq!(calls.recording_hud_hides, vec![session_id]);
        assert!(calls.asr_requests.is_empty());
        assert!(calls.llm_requests.is_empty());
        assert!(calls.history_records.is_empty());
        assert!(calls.diagnostics.iter().any(|event| {
            event.session_id == Some(session_id)
                && event.phase.as_deref() == Some("recording_hud_update_ready")
                && event.error_code.as_deref() == Some("recording_hud_ready_failed")
        }));
    }

    #[test]
    fn hud_update_and_hide_failures_are_diagnostic_only() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let session_id = start_session(&orchestrator);
        {
            let mut config = mock.config.lock().expect("config lock");
            config.recording_hud_update = Err(port_error("recording_hud_update_failed"));
            config.recording_hud_terminal = Err(port_error("recording_hud_terminal_failed"));
            config.recording_hud_hide = Err(port_error("recording_hud_hide_failed"));
        }

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("non-critical HUD projection failures must not break delivery");
        assert!(matches!(outcome, FinishOutcome::Completed(_)));

        let calls = mock.calls();
        assert_eq!(calls.inserted_texts, vec!["clean text"]);
        assert_eq!(calls.recording_hud_updates.len(), 6);
        assert_eq!(
            calls.recording_hud_terminals,
            vec![(session_id, TerminalOutcome::Completed)]
        );
        assert_eq!(calls.recording_hud_hides, vec![session_id]);
        assert!(calls.diagnostics.iter().any(|event| {
            event.phase.as_deref() == Some("recording_hud_update")
                && event.error_code.as_deref() == Some("recording_hud_update_failed")
        }));
        assert!(calls.diagnostics.iter().any(|event| {
            event.phase.as_deref() == Some("recording_hud_publish_terminal")
                && event.error_code.as_deref() == Some("recording_hud_terminal_failed")
        }));
        assert!(calls.diagnostics.iter().any(|event| {
            event.phase.as_deref() == Some("recording_hud_hide")
                && event.error_code.as_deref() == Some("recording_hud_hide_failed")
        }));
    }

    #[test]
    fn unknown_security_uses_user_directed_paste_when_compatibility_is_enabled() {
        let mut config = MockConfig::faithful();
        config.settings = settings_with_clipboard(ProcessingMode::Faithful, true, true, true);
        config.target = Ok(target(TargetSecurity::Unknown, true));
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("unknown target should use user-directed paste");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected completion, got {outcome:?}");
        };

        assert_eq!(completion.delivery, DeliveryKind::ClipboardBridge);
        let calls = mock.calls();
        assert_eq!(calls.selection_reads, 0);
        assert_eq!(calls.target_revalidations, 0);
        assert_eq!(calls.clipboard_insert_attempts, 0);
        assert_eq!(calls.user_directed_paste_attempts, 1);
        assert_eq!(calls.user_directed_pasted_texts, vec!["clean text"]);
        assert!(calls.inserted_texts.is_empty());
        assert!(calls.temporary_texts.is_empty());
        assert_eq!(calls.history_records.len(), 1);
    }

    #[test]
    fn unknown_security_still_uses_temporary_text_when_compatibility_is_disabled() {
        let mut config = MockConfig::faithful();
        config.target = Ok(target(TargetSecurity::Unknown, true));
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("disabled compatibility mode should preserve the visible fallback");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected completion, got {outcome:?}");
        };

        assert_eq!(completion.delivery, DeliveryKind::TemporaryText);
        let calls = mock.calls();
        assert_eq!(calls.user_directed_paste_attempts, 0);
        assert_eq!(calls.temporary_texts, vec!["clean text"]);
        assert_eq!(
            calls.temporary_text_statuses,
            vec![TemporaryTextStatus::NotInserted]
        );
    }

    #[test]
    fn recording_cancel_has_zero_asr_llm_output_and_history_calls() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let session_id = start_session(&orchestrator);

        assert_eq!(
            block_on(orchestrator.cancel_recording(session_id))
                .expect("recording cancellation should be valid"),
            CancelOutcome::Cancelled
        );
        assert_eq!(
            block_on(orchestrator.cancel_recording(session_id))
                .expect("duplicate cancellation should be harmless"),
            CancelOutcome::NotFound
        );

        let calls = mock.calls();
        assert_eq!(calls.audio_cancels, 1);
        assert_eq!(calls.audio_finishes, 0);
        assert_eq!(
            calls.recording_hud_shows,
            vec![(session_id, RecordingHudState::Preparing)]
        );
        assert_eq!(
            calls.recording_hud_updates,
            vec![(session_id, RecordingHudState::Recording)]
        );
        assert_eq!(
            calls.recording_hud_terminals,
            vec![(session_id, TerminalOutcome::Cancelled)]
        );
        assert_eq!(calls.recording_hud_hides, vec![session_id]);
        assert!(calls.asr_requests.is_empty());
        assert!(calls.llm_requests.is_empty());
        assert!(calls.inserted_texts.is_empty());
        assert!(calls.temporary_texts.is_empty());
        assert!(calls.history_records.is_empty());
    }

    #[test]
    fn cancellation_retains_single_flight_slot_until_audio_cleanup_finishes() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let session_id = start_session(&orchestrator);
        let (cancel_started, release_cancel) = mock.install_audio_cancel_gate();

        let cancel_orchestrator = Arc::clone(&orchestrator);
        let cancel_worker = thread::spawn(move || {
            block_on(cancel_orchestrator.cancel_recording(session_id))
                .expect("cancel workflow should remain valid")
        });
        cancel_started
            .recv_timeout(Duration::from_secs(2))
            .expect("audio cancellation should reach the cleanup gate");

        assert_eq!(
            block_on(orchestrator.start()).expect("concurrent start should be typed"),
            StartOutcome::Busy {
                active_session_id: session_id
            }
        );

        release_cancel
            .send(())
            .expect("audio cancellation should still be pending");
        assert_eq!(
            cancel_worker
                .join()
                .expect("cancel thread should not panic"),
            CancelOutcome::Cancelled
        );

        let next_session = start_session(&orchestrator);
        assert_eq!(mock.calls().audio_cancels, 1);
        assert_eq!(
            block_on(orchestrator.cancel_recording(next_session))
                .expect("new session should own the released slot"),
            CancelOutcome::Cancelled
        );
    }

    #[test]
    fn failed_audio_cancel_keeps_single_flight_until_a_confirmed_retry() {
        let mut config = MockConfig::faithful();
        config.audio_cancel_failures_remaining = 1;
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let error = block_on(orchestrator.cancel_recording(session_id))
            .expect_err("a failed cancellation must not be reported as cancelled");
        assert!(matches!(
            error,
            OrchestratorError::AudioCleanupFailed {
                phase: "audio_cancel",
                ..
            }
        ));
        assert_eq!(
            block_on(orchestrator.start()).expect("cleanup debt should be typed as busy"),
            StartOutcome::Busy {
                active_session_id: session_id
            }
        );

        assert_eq!(
            block_on(orchestrator.cancel_recording(session_id))
                .expect("the retained capture should be retryable"),
            CancelOutcome::Cancelled
        );
        assert_eq!(mock.calls().audio_cancels, 2);

        let next_session = start_session(&orchestrator);
        assert_eq!(
            block_on(orchestrator.cancel_recording(next_session))
                .expect("a confirmed cleanup may release the slot"),
            CancelOutcome::Cancelled
        );
    }

    #[test]
    fn concurrent_cancel_does_not_duplicate_an_in_flight_adapter_call() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let session_id = start_session(&orchestrator);
        let (cancel_started, release_cancel) = mock.install_audio_cancel_gate();

        let first_orchestrator = Arc::clone(&orchestrator);
        let first = thread::spawn(move || {
            block_on(first_orchestrator.cancel_recording(session_id))
                .expect("first cancellation should remain valid")
        });
        cancel_started
            .recv_timeout(Duration::from_secs(2))
            .expect("first cancellation should reach the adapter");

        assert_eq!(
            block_on(orchestrator.cancel_recording(session_id))
                .expect("the duplicate should receive an explicit state"),
            CancelOutcome::CleanupInProgress
        );
        assert_eq!(mock.calls().audio_cancels, 1);

        release_cancel
            .send(())
            .expect("first cancellation should still be pending");
        assert_eq!(
            first.join().expect("cancel thread should not panic"),
            CancelOutcome::Cancelled
        );
        assert_eq!(mock.calls().audio_cancels, 1);
    }

    #[test]
    fn failed_audio_finish_cancels_retained_capture_before_releasing_session() {
        let mut config = MockConfig::faithful();
        config.audio_finish = Err(port_error("audio_finish_failed"));
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        assert_eq!(
            block_on(orchestrator.finish_recording(session_id))
                .expect("audio finish failure should be terminal"),
            FinishOutcome::Failed(FailureCategory::Audio)
        );

        let calls = mock.calls();
        assert_eq!(calls.audio_finishes, 1);
        assert_eq!(calls.audio_cancels, 1);
        assert!(calls.asr_requests.is_empty());
        assert_eq!(
            calls.recording_hud_terminals,
            vec![(session_id, TerminalOutcome::Failed(FailureCategory::Audio))]
        );

        let next_session = start_session(&orchestrator);
        assert_eq!(
            block_on(orchestrator.cancel_recording(next_session))
                .expect("failed finish must release single-flight after cleanup"),
            CancelOutcome::Cancelled
        );
    }

    #[test]
    fn empty_audio_capture_from_fast_finish_is_a_benign_no_output_result() {
        let mut config = MockConfig::faithful();
        config.audio_finish = Err(port_error(AUDIO_EMPTY_CAPTURE_CODE));
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        assert_eq!(
            block_on(orchestrator.finish_recording(session_id))
                .expect("an empty capture should be discarded safely"),
            FinishOutcome::NoSpeech
        );

        let calls = mock.calls();
        assert_eq!(calls.audio_finishes, 1);
        assert_eq!(calls.audio_cancels, 1);
        assert_eq!(calls.recording_cues.last(), Some(&RecordingCue::Cancel));
        assert!(calls.asr_requests.is_empty());
        assert!(calls.llm_requests.is_empty());
        assert!(calls.inserted_texts.is_empty());
        assert!(calls.temporary_texts.is_empty());
        assert!(calls.history_records.is_empty());
        assert!(calls.user_notifications.is_empty());
        assert_eq!(
            calls.recording_hud_terminals,
            vec![(session_id, TerminalOutcome::Cancelled)]
        );
        assert!(calls.diagnostics.iter().any(|event| {
            event.phase.as_deref() == Some("audio_finish")
                && event.state.as_deref() == Some("no_speech")
                && event.error_code.is_none()
        }));

        let next_session = start_session(&orchestrator);
        assert_eq!(
            block_on(orchestrator.cancel_recording(next_session))
                .expect("empty capture cleanup must release single-flight"),
            CancelOutcome::Cancelled
        );
    }

    #[test]
    fn failed_finish_and_failed_cancel_retain_capture_until_recovery() {
        let mut config = MockConfig::faithful();
        config.audio_finish = Err(port_error("audio_finish_failed"));
        config.audio_cancel_failures_remaining = 1;
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        assert!(matches!(
            block_on(orchestrator.finish_recording(session_id)),
            Err(OrchestratorError::AudioCleanupFailed {
                phase: "audio_cancel",
                ..
            })
        ));
        assert_eq!(
            block_on(orchestrator.start()).expect("failed cleanup must keep the slot"),
            StartOutcome::Busy {
                active_session_id: session_id
            }
        );
        assert_eq!(
            block_on(orchestrator.cancel_recording(session_id))
                .expect("retained finish capture should be recoverable"),
            CancelOutcome::CleanupRecovered
        );
        assert_eq!(mock.calls().audio_finishes, 1);
        assert_eq!(mock.calls().audio_cancels, 2);
    }

    #[test]
    fn asr_failure_cleans_audio_and_never_retries_whisper_in_same_session() {
        let mut config = MockConfig::faithful();
        config.asr_behavior = AsrBehavior::Failure(port_error("asr_failed"));
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        assert_eq!(
            block_on(orchestrator.finish_recording(session_id))
                .expect("ASR failure should be represented as a terminal outcome"),
            FinishOutcome::Failed(FailureCategory::Asr)
        );

        let calls = mock.calls();
        assert_eq!(calls.asr_requests.len(), 1);
        assert_eq!(calls.asr_requests[0].engine, AsrEngine::Qwen);
        assert_eq!(calls.audio_cleanups, 1);
        assert!(calls.llm_requests.is_empty());
        assert!(calls.inserted_texts.is_empty());
        assert!(calls.history_records.is_empty());
        assert_eq!(
            calls.recording_hud_terminals,
            vec![(session_id, TerminalOutcome::Failed(FailureCategory::Asr))]
        );
        assert_eq!(
            calls.user_notifications,
            vec![(session_id, UserNotificationKind::Asr)]
        );
        assert_eq!(
            calls.presentation_events,
            vec!["terminal", "hide", "notification"]
        );
    }

    #[test]
    fn explicit_no_speech_asr_signal_finishes_without_error_feedback() {
        let mut config = MockConfig::faithful();
        config.asr_behavior = AsrBehavior::Failure(port_error(ASR_NO_SPEECH_CODE));
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        assert_eq!(
            block_on(orchestrator.finish_recording(session_id))
                .expect("no speech should be a valid no-output outcome"),
            FinishOutcome::NoSpeech
        );

        let calls = mock.calls();
        assert_eq!(calls.asr_requests.len(), 1);
        assert_eq!(calls.audio_cleanups, 1);
        assert!(calls.llm_requests.is_empty());
        assert!(calls.inserted_texts.is_empty());
        assert!(calls.temporary_texts.is_empty());
        assert!(calls.history_records.is_empty());
        assert!(calls.user_notifications.is_empty());
        assert_eq!(
            calls.recording_hud_terminals,
            vec![(session_id, TerminalOutcome::Cancelled)]
        );
        assert_eq!(calls.presentation_events, vec!["terminal", "hide"]);
        assert!(calls.diagnostics.iter().any(|event| {
            event.phase.as_deref() == Some("asr")
                && event.state.as_deref() == Some("no_speech")
                && event.error_code.is_none()
        }));
    }

    #[test]
    fn defensive_empty_asr_success_is_also_silent_and_never_delivered() {
        let mut config = MockConfig::faithful();
        config.asr_behavior = AsrBehavior::Success("  \n".to_owned());
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        assert_eq!(
            block_on(orchestrator.finish_recording(session_id))
                .expect("an empty successful transcript should be discarded"),
            FinishOutcome::NoSpeech
        );

        let calls = mock.calls();
        assert_eq!(calls.asr_requests.len(), 1);
        assert_eq!(calls.audio_cleanups, 1);
        assert!(calls.llm_requests.is_empty());
        assert!(calls.inserted_texts.is_empty());
        assert!(calls.temporary_texts.is_empty());
        assert!(calls.history_records.is_empty());
        assert!(calls.user_notifications.is_empty());
        assert_eq!(
            calls.recording_hud_terminals,
            vec![(session_id, TerminalOutcome::Cancelled)]
        );
    }

    #[test]
    fn finalized_audio_cleanup_failure_blocks_delivery_and_is_retryable() {
        let mut config = MockConfig::faithful();
        config.audio_cleanup_failures_remaining = 1;
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        assert!(matches!(
            block_on(orchestrator.finish_recording(session_id)),
            Err(OrchestratorError::AudioCleanupFailed {
                phase: "audio_cleanup",
                ..
            })
        ));
        let calls = mock.calls();
        assert_eq!(calls.asr_requests.len(), 1);
        assert_eq!(calls.audio_cleanups, 1);
        assert!(calls.llm_requests.is_empty());
        assert!(calls.inserted_texts.is_empty());
        assert!(calls.temporary_texts.is_empty());
        assert!(calls.history_records.is_empty());
        assert_eq!(
            block_on(orchestrator.start()).expect("artifact debt must retain single-flight"),
            StartOutcome::Busy {
                active_session_id: session_id
            }
        );

        assert_eq!(
            block_on(orchestrator.retry_audio_cleanup(session_id))
                .expect("artifact deletion should be retryable"),
            AudioCleanupOutcome::Recovered
        );
        assert_eq!(mock.calls().audio_cleanups, 2);
        assert_eq!(mock.calls().asr_requests.len(), 1, "ASR must not rerun");
        let next_session = start_session(&orchestrator);
        assert_eq!(
            block_on(orchestrator.cancel_recording(next_session))
                .expect("recovered cleanup may release the slot"),
            CancelOutcome::Cancelled
        );
    }

    #[test]
    fn llm_failure_preserves_raw_asr_text_in_temporary_output_and_history() {
        let mut config = MockConfig::faithful();
        config.llm_behavior = LlmBehavior::Failure(port_error("llm_failed"));
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("LLM failure should preserve the local transcript");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected fallback completion, got {outcome:?}");
        };

        assert_eq!(completion.final_text, "raw text");
        assert_eq!(completion.delivery, DeliveryKind::TemporaryText);
        let calls = mock.calls();
        assert!(calls.inserted_texts.is_empty());
        assert_eq!(calls.temporary_texts, vec!["raw text"]);
        assert_eq!(
            calls.temporary_text_statuses,
            vec![TemporaryTextStatus::LlmFallback]
        );
        assert_eq!(calls.history_records[0].final_text, "raw text");
        assert_eq!(
            calls.user_notifications,
            vec![(session_id, UserNotificationKind::Llm)]
        );
        assert_eq!(
            calls.presentation_events,
            vec!["terminal", "hide", "notification"]
        );
    }

    #[test]
    fn notification_failure_does_not_rollback_a_safe_llm_fallback() {
        let mut config = MockConfig::faithful();
        config.llm_behavior = LlmBehavior::Failure(port_error("llm_failed"));
        config.user_notification = Err(port_error("notification_unavailable"));
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("feedback is ancillary after the safe fallback commits");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected fallback completion, got {outcome:?}");
        };
        assert_eq!(completion.final_text, "raw text");
        assert_eq!(completion.delivery, DeliveryKind::TemporaryText);

        let calls = mock.calls();
        assert_eq!(
            calls.user_notifications,
            vec![(session_id, UserNotificationKind::Llm)]
        );
        assert!(calls.diagnostics.iter().any(|event| {
            event.phase.as_deref() == Some("user_notification")
                && event.error_code.as_deref() == Some("notification_unavailable")
        }));
    }

    #[test]
    fn mismatched_llm_correlation_discards_untrusted_text_and_preserves_raw_fallback() {
        let mut config = MockConfig::faithful();
        config.llm_behavior = LlmBehavior::CorrelationMismatch("untrusted text".to_owned());
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("correlation mismatch should preserve the local transcript");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected safe fallback completion, got {outcome:?}");
        };
        assert_eq!(completion.final_text, "raw text");
        assert_eq!(completion.delivery, DeliveryKind::TemporaryText);

        let calls = mock.calls();
        assert_eq!(calls.llm_requests.len(), 1);
        assert!(calls.inserted_texts.is_empty());
        assert_eq!(calls.temporary_texts, vec!["raw text"]);
        assert_eq!(
            calls.temporary_text_statuses,
            vec![TemporaryTextStatus::LlmFallback]
        );
        assert_eq!(calls.history_records[0].final_text, "raw text");
        assert!(
            calls
                .temporary_texts
                .iter()
                .all(|text| text != "untrusted text")
        );
    }

    #[test]
    fn correlated_empty_llm_response_preserves_raw_text_in_safe_fallback() {
        let mut config = MockConfig::faithful();
        config.llm_behavior = LlmBehavior::Success("   ".to_owned());
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("empty but correlated response may use raw fallback");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected safe fallback, got {outcome:?}");
        };
        assert_eq!(completion.final_text, "raw text");
        assert_eq!(completion.delivery, DeliveryKind::TemporaryText);
        let calls = mock.calls();
        assert_eq!(calls.temporary_texts, vec!["raw text"]);
        assert_eq!(
            calls.temporary_text_statuses,
            vec![TemporaryTextStatus::LlmFallback]
        );
        assert_eq!(calls.history_records[0].final_text, "raw text");
    }

    #[test]
    fn authorized_clipboard_bridge_runs_only_after_proven_native_non_insertion() {
        let mut config = MockConfig::faithful();
        config.settings = settings_with_clipboard(ProcessingMode::Faithful, true, true, true);
        config.insert = Ok(InsertOutcome::NotInserted);
        config.clipboard_insert = Ok(InsertOutcome::Inserted);
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("authorized clipboard bridge should be a supported delivery route");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected completed bridge delivery, got {outcome:?}");
        };
        assert_eq!(completion.delivery, DeliveryKind::ClipboardBridge);
        let calls = mock.calls();
        assert_eq!(calls.insert_attempts, 1);
        assert_eq!(calls.clipboard_insert_attempts, 1);
        assert_eq!(calls.clipboard_inserted_texts, vec!["clean text"]);
        assert!(calls.temporary_texts.is_empty());
    }

    #[test]
    fn indeterminate_native_insertion_never_attempts_clipboard_second_write() {
        let mut config = MockConfig::faithful();
        config.settings = settings_with_clipboard(ProcessingMode::Faithful, true, true, true);
        config.insert = Ok(InsertOutcome::Indeterminate);
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("indeterminate insertion must preserve text without retrying");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected safe fallback, got {outcome:?}");
        };
        assert_eq!(completion.delivery, DeliveryKind::TemporaryText);
        let calls = mock.calls();
        assert_eq!(calls.clipboard_insert_attempts, 0);
        assert_eq!(calls.user_directed_paste_attempts, 0);
        assert_eq!(
            calls.temporary_text_statuses,
            vec![TemporaryTextStatus::Indeterminate]
        );
        assert!(calls.user_notifications.is_empty());
    }

    #[test]
    fn insertion_failure_falls_back_once_without_second_automatic_write() {
        let mut config = MockConfig::faithful();
        config.insert = Ok(InsertOutcome::NotInserted);
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("not-inserted result should use the safe fallback");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected fallback completion, got {outcome:?}");
        };

        assert_eq!(completion.delivery, DeliveryKind::TemporaryText);
        let calls = mock.calls();
        assert_eq!(calls.insert_attempts, 1);
        assert!(calls.inserted_texts.is_empty());
        assert_eq!(calls.temporary_texts, vec!["clean text"]);
        assert_eq!(
            calls.temporary_text_statuses,
            vec![TemporaryTextStatus::NotInserted]
        );
        assert_eq!(calls.history_records.len(), 1);
        assert!(calls.user_notifications.is_empty());
    }

    #[test]
    fn indeterminate_insertion_warns_the_temporary_text_surface() {
        let mut config = MockConfig::faithful();
        config.insert = Ok(InsertOutcome::Indeterminate);
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("indeterminate insertion should preserve the final text");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected fallback completion, got {outcome:?}");
        };

        assert_eq!(completion.delivery, DeliveryKind::TemporaryText);
        let calls = mock.calls();
        assert_eq!(calls.insert_attempts, 1);
        assert!(calls.inserted_texts.is_empty());
        assert_eq!(calls.temporary_texts, vec!["clean text"]);
        assert_eq!(
            calls.temporary_text_statuses,
            vec![TemporaryTextStatus::Indeterminate]
        );
        assert_eq!(calls.history_records.len(), 1);
        assert!(calls.user_notifications.is_empty());
    }

    #[test]
    fn temporary_text_failure_terminates_delivery_without_history() {
        let mut config = MockConfig::faithful();
        config.insert = Ok(InsertOutcome::NotInserted);
        config.temporary_text = Err(port_error("temporary_text_failed"));
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        assert_eq!(
            block_on(orchestrator.finish_recording(session_id))
                .expect("temporary text failure should remain a typed delivery failure"),
            FinishOutcome::Failed(FailureCategory::Delivery)
        );

        let calls = mock.calls();
        assert_eq!(calls.insert_attempts, 1);
        assert!(calls.temporary_texts.is_empty());
        assert!(calls.temporary_text_statuses.is_empty());
        assert_eq!(calls.history_save_attempts, 0);
        assert!(calls.history_records.is_empty());
        assert_eq!(
            calls.recording_hud_terminals,
            vec![(
                session_id,
                TerminalOutcome::Failed(FailureCategory::Delivery)
            )]
        );
        assert!(
            calls.user_notifications.is_empty(),
            "generic delivery failure must not reuse the indeterminate-write copy"
        );
    }

    // QA-VS1-001: 失焦。录音结束时目标已失焦/消失（复验返回 Invalid），
    // 必须回退到临时文本框，绝不向已改变的目标写入（零误写）。
    #[test]
    fn lost_focus_at_delivery_falls_back_to_temporary_text_without_inserting() {
        let mut config = MockConfig::faithful();
        // Raw 模式隔离交付路径，避免选区/LLM 干扰。
        config.settings = settings(ProcessingMode::Raw, false, false);
        // 目标在录音期间失焦/消失。
        config.revalidation = Ok(TargetRevalidation::Invalid);
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("lost focus must degrade to a temporary-text delivery, not an error");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected temporary-text completion, got {outcome:?}");
        };

        // 交付到临时文本框，而非插入目标。
        assert_eq!(completion.delivery, DeliveryKind::TemporaryText);
        let calls = mock.calls();
        // 关键不变量：目标已复验，但绝不发生插入（零误写）。
        assert_eq!(calls.target_revalidations, 1);
        assert_eq!(calls.insert_attempts, 0);
        assert!(calls.inserted_texts.is_empty());
        // ASR 原始文本落到临时文本框，状态为 NotInserted。
        assert_eq!(calls.temporary_texts, vec!["raw text"]);
        assert_eq!(
            calls.temporary_text_statuses,
            vec![TemporaryTextStatus::NotInserted]
        );
    }

    #[test]
    fn lost_exact_target_uses_current_focus_when_compatibility_is_enabled() {
        let mut config = MockConfig::faithful();
        config.settings = settings_with_clipboard(ProcessingMode::Raw, false, false, true);
        config.revalidation = Ok(TargetRevalidation::Invalid);
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("pre-dispatch target loss may use the user-selected current focus");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected user-directed completion, got {outcome:?}");
        };

        assert_eq!(completion.delivery, DeliveryKind::ClipboardBridge);
        let calls = mock.calls();
        assert_eq!(calls.target_revalidations, 1);
        assert_eq!(calls.insert_attempts, 0);
        assert_eq!(calls.clipboard_insert_attempts, 0);
        assert_eq!(calls.user_directed_paste_attempts, 1);
        assert_eq!(calls.user_directed_pasted_texts, vec!["raw text"]);
        assert!(calls.temporary_texts.is_empty());
    }

    #[test]
    fn indeterminate_user_directed_dispatch_is_never_claimed_as_delivery() {
        let mut config = MockConfig::faithful();
        config.settings = settings_with_clipboard(ProcessingMode::Faithful, true, true, true);
        config.target = Ok(target(TargetSecurity::Unknown, false));
        config.user_directed_paste = Ok(UserDirectedPasteOutcome::Indeterminate);
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("dispatch uncertainty should preserve the final text visibly");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected temporary-text completion, got {outcome:?}");
        };

        assert_eq!(completion.delivery, DeliveryKind::TemporaryText);
        let calls = mock.calls();
        assert_eq!(calls.user_directed_paste_attempts, 1);
        assert!(calls.user_directed_pasted_texts.is_empty());
        assert_eq!(calls.temporary_texts, vec!["clean text"]);
        assert_eq!(
            calls.temporary_text_statuses,
            vec![TemporaryTextStatus::Indeterminate]
        );
    }

    // QA-VS1-001: 重复结束。对同一 Session 二次调用 finish_recording，
    // 第二次必须是无副作用的空操作（NotRecording），不产生第二次交付/历史。
    #[test]
    fn finishing_an_already_finished_session_is_a_noop() {
        let config = MockConfig::faithful();
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        // 第一次结束：正常完成并插入。
        let first = block_on(orchestrator.finish_recording(session_id))
            .expect("first finish should complete");
        assert!(matches!(first, FinishOutcome::Completed(_)));

        let after_first = mock.calls();
        let inserts_after_first = after_first.insert_attempts;
        let history_after_first = after_first.history_save_attempts;
        let terminals_after_first = after_first.recording_hud_terminals.len();

        // 第二次结束同一 Session：必须是空操作，无下游副作用。
        let second = block_on(orchestrator.finish_recording(session_id))
            .expect("second finish must be a no-op, not an error");
        assert_eq!(second, FinishOutcome::NotRecording);

        let after_second = mock.calls();
        // 关键不变量：第二次不产生任何新的插入或历史写入（零重复插入）。
        assert_eq!(after_second.insert_attempts, inserts_after_first);
        assert_eq!(after_second.history_save_attempts, history_after_first);
        assert_eq!(
            after_second.recording_hud_terminals.len(),
            terminals_after_first,
            "a duplicate finish must not publish another terminal event"
        );
    }

    #[test]
    fn late_asr_result_after_quit_is_discarded_without_output_or_history() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let (asr_started, release_asr) = mock.install_asr_gate();
        let session_id = start_session(&orchestrator);

        let worker_orchestrator = Arc::clone(&orchestrator);
        let worker = thread::spawn(move || {
            block_on(worker_orchestrator.finish_recording(session_id))
                .expect("finish future should not violate invariants")
        });
        asr_started
            .recv_timeout(Duration::from_secs(2))
            .expect("ASR mock should reach the controlled pending point");

        let quit_orchestrator = Arc::clone(&orchestrator);
        let (quit_done_sender, quit_done_receiver) = mpsc::channel();
        let quit_worker = thread::spawn(move || {
            let outcome = block_on(quit_orchestrator.quit())
                .expect("quit should terminate the active session");
            let _ = quit_done_sender.send(outcome);
        });
        assert!(
            quit_done_receiver
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "quit must await the already-entered finish workflow"
        );
        release_asr
            .send(())
            .expect("pending ASR mock should still be waiting");
        assert_eq!(
            worker.join().expect("finish thread should not panic"),
            FinishOutcome::Discarded
        );
        assert_eq!(
            quit_done_receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("quit should finish after ASR operation drains"),
            QuitOutcome::Terminated(session_id)
        );
        quit_worker.join().expect("quit thread should not panic");
        assert_eq!(
            block_on(orchestrator.start()).expect("quitting state should remain stable"),
            StartOutcome::Quitting
        );

        let calls = mock.calls();
        assert_eq!(calls.asr_cancels, 1);
        assert_eq!(calls.audio_cleanups, 1);
        assert!(calls.llm_requests.is_empty());
        assert!(calls.inserted_texts.is_empty());
        assert!(calls.temporary_texts.is_empty());
        assert!(calls.history_records.is_empty());
    }

    #[test]
    fn quit_waits_for_starting_audio_cleanup_before_returning() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let (audio_start_entered, release_audio_start) = mock.install_audio_start_gate();

        let start_orchestrator = Arc::clone(&orchestrator);
        let start_worker = thread::spawn(move || {
            block_on(start_orchestrator.start()).expect("start workflow should remain valid")
        });
        audio_start_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("audio start should reach controlled pending point");

        let (quit_worker, quit_done) = spawn_quit(Arc::clone(&orchestrator));
        wait_until(|| orchestrator.runtime().expect("runtime lock").quitting);
        assert!(
            quit_done.recv_timeout(Duration::from_millis(50)).is_err(),
            "quit cannot return while audio start may still open a microphone"
        );

        release_audio_start
            .send(())
            .expect("audio start should still be pending");
        assert_eq!(
            start_worker.join().expect("start thread should not panic"),
            StartOutcome::Quitting
        );
        assert!(matches!(
            quit_done
                .recv_timeout(Duration::from_secs(2))
                .expect("quit should finish after start cleanup"),
            QuitOutcome::Terminated(_)
        ));
        quit_worker.join().expect("quit thread should not panic");

        let calls = mock.calls();
        assert_eq!(calls.audio_starts, 1);
        assert_eq!(calls.audio_cancels, 1);
        assert!(calls.asr_requests.is_empty());
    }

    #[test]
    fn concurrent_quit_waits_for_one_blocked_audio_cancel_and_reuses_result() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let session_id = start_session(&orchestrator);
        let (cancel_entered, release_cancel) = mock.install_audio_cancel_gate();

        let (leader_worker, leader_done) = spawn_quit(Arc::clone(&orchestrator));
        cancel_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("leader should reach blocked audio cancellation");
        let (follower_worker, follower_done) = spawn_quit(Arc::clone(&orchestrator));

        assert!(
            follower_done
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "follower quit must not return before leader cancellation"
        );
        assert_eq!(mock.calls().audio_cancels, 1);

        release_cancel
            .send(())
            .expect("audio cancellation should remain blocked");
        let expected = QuitOutcome::Terminated(session_id);
        assert_eq!(
            leader_done
                .recv_timeout(Duration::from_secs(2))
                .expect("leader quit should complete"),
            expected
        );
        assert_eq!(
            follower_done
                .recv_timeout(Duration::from_secs(2))
                .expect("follower should receive the same result"),
            expected
        );
        leader_worker
            .join()
            .expect("leader thread should not panic");
        follower_worker
            .join()
            .expect("follower thread should not panic");
        assert_eq!(
            block_on(orchestrator.quit()).expect("completed quit should be cached"),
            expected
        );
        assert_eq!(mock.calls().audio_cancels, 1);
    }

    #[test]
    fn failed_quit_cleanup_is_shared_then_a_new_exit_attempt_can_retry() {
        let mut config = MockConfig::faithful();
        // The leader performs one direct cancellation and one post-barrier retry.
        // Both must fail so the first exit attempt remains blocked by cleanup.
        config.audio_cancel_failures_remaining = 2;
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);
        let (cancel_entered, release_cancel) = mock.install_audio_cancel_gate();

        let (leader_sender, leader_receiver) = mpsc::channel();
        let leader_orchestrator = Arc::clone(&orchestrator);
        let leader = thread::spawn(move || {
            let _ = leader_sender.send(block_on(leader_orchestrator.quit()));
        });
        cancel_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("leader should reach the first cleanup attempt");

        let (follower_sender, follower_receiver) = mpsc::channel();
        let follower_orchestrator = Arc::clone(&orchestrator);
        let follower = thread::spawn(move || {
            let _ = follower_sender.send(block_on(follower_orchestrator.quit()));
        });
        assert!(
            follower_receiver
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "the follower must wait for the same exit attempt"
        );

        release_cancel
            .send(())
            .expect("the first cleanup should still be pending");
        let leader_error = leader_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("leader should publish a cleanup result")
            .expect_err("cleanup debt must block termination");
        let follower_error = follower_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("follower should receive the same attempt result")
            .expect_err("follower must not observe termination");
        assert_eq!(leader_error, follower_error);
        assert!(matches!(
            leader_error,
            OrchestratorError::AudioCleanupFailed {
                phase: "audio_cancel",
                ..
            }
        ));
        leader.join().expect("leader thread should not panic");
        follower.join().expect("follower thread should not panic");
        assert_eq!(mock.calls().audio_cancels, 2);

        let recovered = block_on(orchestrator.quit())
            .expect("a new exit attempt should retry retained cleanup");
        assert_eq!(recovered, QuitOutcome::Terminated(session_id));
        assert_eq!(mock.calls().audio_cancels, 3);
        assert_eq!(
            block_on(orchestrator.quit()).expect("successful exit should be cached"),
            recovered
        );
        assert_eq!(mock.calls().audio_cancels, 3);
    }

    #[test]
    fn concurrent_quit_waits_for_one_blocked_asr_cancel_and_reuses_result() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let session_id = start_session(&orchestrator);
        let (asr_entered, release_asr) = mock.install_asr_gate();

        let finish_orchestrator = Arc::clone(&orchestrator);
        let finish_worker = thread::spawn(move || {
            block_on(finish_orchestrator.finish_recording(session_id))
                .expect("finish workflow should remain valid")
        });
        asr_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("ASR should reach controlled pending point");

        let (cancel_entered, release_cancel) = mock.install_asr_cancel_gate();
        let (leader_worker, leader_done) = spawn_quit(Arc::clone(&orchestrator));
        cancel_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("leader should reach blocked ASR cancellation");
        let (follower_worker, follower_done) = spawn_quit(Arc::clone(&orchestrator));

        assert!(
            follower_done
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "follower quit must wait for the single ASR cancellation"
        );
        assert_eq!(mock.calls().asr_cancels, 1);
        release_cancel
            .send(())
            .expect("ASR cancellation should remain blocked");
        assert!(
            follower_done
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );
        release_asr
            .send(())
            .expect("ASR response should still be pending");

        assert_eq!(
            finish_worker
                .join()
                .expect("finish thread should not panic"),
            FinishOutcome::Discarded
        );
        let expected = QuitOutcome::Terminated(session_id);
        assert_eq!(
            leader_done
                .recv_timeout(Duration::from_secs(2))
                .expect("leader quit should complete"),
            expected
        );
        assert_eq!(
            follower_done
                .recv_timeout(Duration::from_secs(2))
                .expect("follower should receive the same result"),
            expected
        );
        leader_worker
            .join()
            .expect("leader thread should not panic");
        follower_worker
            .join()
            .expect("follower thread should not panic");
        assert_eq!(mock.calls().asr_cancels, 1);
    }

    #[test]
    fn concurrent_quit_waits_for_one_blocked_llm_cancel_and_reuses_result() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let session_id = start_session(&orchestrator);
        let (llm_entered, release_llm) = mock.install_llm_gate();

        let finish_orchestrator = Arc::clone(&orchestrator);
        let finish_worker = thread::spawn(move || {
            block_on(finish_orchestrator.finish_recording(session_id))
                .expect("finish workflow should remain valid")
        });
        llm_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("LLM should reach controlled pending point");

        let (cancel_entered, release_cancel) = mock.install_llm_cancel_gate();
        let (leader_worker, leader_done) = spawn_quit(Arc::clone(&orchestrator));
        cancel_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("leader should reach blocked LLM cancellation");
        let (follower_worker, follower_done) = spawn_quit(Arc::clone(&orchestrator));

        assert!(
            follower_done
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "follower quit must wait for the single LLM cancellation"
        );
        assert_eq!(mock.calls().llm_cancels, 1);
        release_cancel
            .send(())
            .expect("LLM cancellation should remain blocked");
        assert!(
            follower_done
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );
        release_llm
            .send(())
            .expect("LLM response should still be pending");

        assert_eq!(
            finish_worker
                .join()
                .expect("finish thread should not panic"),
            FinishOutcome::Discarded
        );
        let expected = QuitOutcome::Terminated(session_id);
        assert_eq!(
            leader_done
                .recv_timeout(Duration::from_secs(2))
                .expect("leader quit should complete"),
            expected
        );
        assert_eq!(
            follower_done
                .recv_timeout(Duration::from_secs(2))
                .expect("follower should receive the same result"),
            expected
        );
        leader_worker
            .join()
            .expect("leader thread should not panic");
        follower_worker
            .join()
            .expect("follower thread should not panic");
        assert_eq!(mock.calls().llm_cancels, 1);
    }

    #[test]
    fn quit_cancels_processing_llm_and_waits_for_workflow_quiescence() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let session_id = start_session(&orchestrator);
        let (llm_entered, release_llm) = mock.install_llm_gate();

        let finish_orchestrator = Arc::clone(&orchestrator);
        let finish_worker = thread::spawn(move || {
            block_on(finish_orchestrator.finish_recording(session_id))
                .expect("finish workflow should remain valid")
        });
        llm_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("LLM should reach controlled pending point");

        let (quit_worker, quit_done) = spawn_quit(Arc::clone(&orchestrator));
        wait_until(|| mock.calls().llm_cancels == 1);
        assert!(quit_done.recv_timeout(Duration::from_millis(50)).is_err());
        release_llm
            .send(())
            .expect("LLM response should still be pending");

        assert_eq!(
            finish_worker
                .join()
                .expect("finish thread should not panic"),
            FinishOutcome::Discarded
        );
        assert_eq!(
            quit_done
                .recv_timeout(Duration::from_secs(2))
                .expect("quit should finish after Processing drains"),
            QuitOutcome::Terminated(session_id)
        );
        quit_worker.join().expect("quit thread should not panic");
        let calls = mock.calls();
        assert_eq!(calls.llm_cancels, 1);
        assert!(calls.inserted_texts.is_empty());
        assert!(calls.temporary_texts.is_empty());
        assert!(calls.history_records.is_empty());
    }

    #[test]
    fn quit_waits_for_external_operation_and_rejects_new_entries() {
        let (orchestrator, _) = harness(MockConfig::faithful());
        let operation = orchestrator
            .enter_external_operation()
            .expect("history operation may enter before shutdown");

        let (quit_worker, quit_done) = spawn_quit(Arc::clone(&orchestrator));
        wait_until(|| orchestrator.runtime().expect("runtime lock").quitting);

        assert!(
            orchestrator.enter_external_operation().is_none(),
            "quiescing must reject new history mutations"
        );
        assert!(
            quit_done.recv_timeout(Duration::from_millis(50)).is_err(),
            "quit must wait for an already-entered history mutation"
        );

        drop(operation);
        assert_eq!(
            quit_done
                .recv_timeout(Duration::from_secs(2))
                .expect("quit should complete after history mutation drains"),
            QuitOutcome::Idle
        );
        quit_worker.join().expect("quit thread should not panic");
    }

    #[test]
    fn quit_rejects_output_that_has_not_crossed_irreversible_commit_point() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let session_id = start_session(&orchestrator);
        let (output_entered, release_output) = mock.install_output_precommit_gate();

        let finish_orchestrator = Arc::clone(&orchestrator);
        let finish_worker = thread::spawn(move || {
            block_on(finish_orchestrator.finish_recording(session_id))
                .expect("finish workflow should remain valid")
        });
        output_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("output should pause before commit");

        let (quit_worker, quit_done) = spawn_quit(Arc::clone(&orchestrator));
        wait_until(|| orchestrator.runtime().expect("runtime lock").quitting);
        assert!(quit_done.recv_timeout(Duration::from_millis(50)).is_err());
        release_output
            .send(())
            .expect("output should still be pending before commit");

        assert_eq!(
            finish_worker
                .join()
                .expect("finish thread should not panic"),
            FinishOutcome::Discarded
        );
        assert_eq!(
            quit_done
                .recv_timeout(Duration::from_secs(2))
                .expect("quit should finish after Delivering drains"),
            QuitOutcome::Terminated(session_id)
        );
        quit_worker.join().expect("quit thread should not panic");
        let calls = mock.calls();
        assert_eq!(calls.insert_attempts, 1);
        assert!(calls.inserted_texts.is_empty());
        assert!(calls.temporary_texts.is_empty());
        assert!(calls.history_records.is_empty());
    }

    #[test]
    fn quit_rejects_temporary_text_that_has_not_crossed_commit_point() {
        let mut config = MockConfig::faithful();
        config.target = Ok(target(TargetSecurity::Unknown, false));
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);
        let (temporary_entered, release_temporary) = mock.install_temporary_precommit_gate();

        let finish_orchestrator = Arc::clone(&orchestrator);
        let finish_worker = thread::spawn(move || {
            block_on(finish_orchestrator.finish_recording(session_id))
                .expect("finish workflow should remain valid")
        });
        temporary_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("temporary text should pause before commit");

        let (quit_worker, quit_done) = spawn_quit(Arc::clone(&orchestrator));
        wait_until(|| orchestrator.runtime().expect("runtime lock").quitting);
        release_temporary
            .send(())
            .expect("temporary output should still be pending");

        assert_eq!(
            finish_worker
                .join()
                .expect("finish thread should not panic"),
            FinishOutcome::Discarded
        );
        assert_eq!(
            quit_done
                .recv_timeout(Duration::from_secs(2))
                .expect("quit should finish after temporary output drains"),
            QuitOutcome::Terminated(session_id)
        );
        quit_worker.join().expect("quit thread should not panic");
        let calls = mock.calls();
        assert!(calls.inserted_texts.is_empty());
        assert!(calls.temporary_texts.is_empty());
        assert!(calls.history_records.is_empty());
    }

    #[test]
    fn quit_waits_for_history_commit_that_started_before_invalidation() {
        let (orchestrator, mock) = harness(MockConfig::faithful());
        let session_id = start_session(&orchestrator);
        let (history_commit_entered, release_history_commit) = mock.install_history_commit_gate();

        let finish_orchestrator = Arc::clone(&orchestrator);
        let finish_worker = thread::spawn(move || {
            block_on(finish_orchestrator.finish_recording(session_id))
                .expect("finish workflow should remain valid")
        });
        history_commit_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("history should pause after acquiring CommitGuard");
        assert!(mock.calls().history_records.is_empty());

        let (quit_worker, quit_done) = spawn_quit(Arc::clone(&orchestrator));
        wait_until(|| orchestrator.runtime().expect("runtime lock").quitting);
        assert!(
            quit_done.recv_timeout(Duration::from_millis(50)).is_err(),
            "quit must await an already-started irreversible commit"
        );
        release_history_commit
            .send(())
            .expect("history commit should still be pending");

        assert_eq!(
            finish_worker
                .join()
                .expect("finish thread should not panic"),
            FinishOutcome::Discarded
        );
        assert_eq!(
            quit_done
                .recv_timeout(Duration::from_secs(2))
                .expect("quit should finish only after history commit"),
            QuitOutcome::Terminated(session_id)
        );
        quit_worker.join().expect("quit thread should not panic");
        let committed = mock.calls();
        assert_eq!(committed.history_records.len(), 1);
        assert_eq!(committed.history_policy_enforcements, 1);
        thread::sleep(Duration::from_millis(20));
        assert_eq!(mock.calls().history_records.len(), 1);
    }

    #[test]
    fn history_failure_does_not_rollback_successful_text_delivery() {
        let mut config = MockConfig::faithful();
        config.history_save = Err(port_error("history_failed"));
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("history is an ancillary operation");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected completed delivery, got {outcome:?}");
        };

        assert_eq!(completion.delivery, DeliveryKind::Inserted);
        assert_eq!(
            completion.warnings,
            vec![FinalizationWarning::HistorySaveFailed]
        );
        let calls = mock.calls();
        assert_eq!(calls.inserted_texts, vec!["clean text"]);
        assert_eq!(calls.history_save_attempts, 1);
        assert_eq!(calls.history_policy_enforcements, 0);
    }

    #[test]
    fn enabled_auto_copy_writes_only_the_final_result_after_delivery() {
        let mut config = MockConfig::faithful();
        let mut input = config.settings.clone().into_input();
        input.auto_copy_result = true;
        config.settings = SettingsSnapshot::new(input).expect("valid auto-copy setting");
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("auto-copy is an ancillary finalization step");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected completed delivery, got {outcome:?}");
        };

        assert!(completion.warnings.is_empty());
        assert_eq!(mock.calls().clipboard_written_texts, vec!["clean text"]);
    }

    #[test]
    fn auto_copy_failure_does_not_rollback_delivery_or_history() {
        let mut config = MockConfig::faithful();
        let mut input = config.settings.clone().into_input();
        input.auto_copy_result = true;
        config.settings = SettingsSnapshot::new(input).expect("valid auto-copy setting");
        config.clipboard_write = Err(port_error("clipboard.write_failed"));
        let (orchestrator, mock) = harness(config);
        let session_id = start_session(&orchestrator);

        let outcome = block_on(orchestrator.finish_recording(session_id))
            .expect("clipboard failure must stay ancillary");
        let FinishOutcome::Completed(completion) = outcome else {
            panic!("expected completed delivery, got {outcome:?}");
        };

        assert_eq!(completion.delivery, DeliveryKind::Inserted);
        assert_eq!(
            completion.warnings,
            vec![FinalizationWarning::AutoCopyFailed]
        );
        let calls = mock.calls();
        assert!(calls.clipboard_written_texts.is_empty());
        assert_eq!(calls.history_records.len(), 1);
    }

    fn harness(config: MockConfig) -> (Arc<TranscriptionOrchestrator>, Arc<MockPorts>) {
        let mock = Arc::new(MockPorts::new(config));
        let ports = OrchestratorPorts {
            settings: mock.clone(),
            targets: mock.clone(),
            microphone_permission: mock.clone(),
            audio: mock.clone(),
            recording_cue: mock.clone(),
            recording_deadline: mock.clone(),
            recording_hud: mock.clone(),
            asr: mock.clone(),
            llm: mock.clone(),
            output: mock.clone(),
            clipboard: mock.clone(),
            clipboard_text_writer: mock.clone(),
            temporary_text: mock.clone(),
            user_notifications: mock.clone(),
            history: mock.clone(),
            diagnostics: mock.clone(),
            clock: mock.clone(),
            ids: mock.clone(),
        };
        (Arc::new(TranscriptionOrchestrator::new(ports)), mock)
    }

    fn spawn_quit(
        orchestrator: Arc<TranscriptionOrchestrator>,
    ) -> (thread::JoinHandle<()>, mpsc::Receiver<QuitOutcome>) {
        let (done_sender, done_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let outcome = block_on(orchestrator.quit()).expect("quit should remain valid");
            let _ = done_sender.send(outcome);
        });
        (worker, done_receiver)
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !condition() {
            assert!(Instant::now() < deadline, "condition did not become true");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn start_session(orchestrator: &Arc<TranscriptionOrchestrator>) -> SessionId {
        match block_on(orchestrator.start()).expect("start should not violate invariants") {
            StartOutcome::Started { session_id } => session_id,
            outcome => panic!("expected started session, got {outcome:?}"),
        }
    }

    fn settings(
        processing_mode: ProcessingMode,
        llm_configured: bool,
        history_enabled: bool,
    ) -> SettingsSnapshot {
        settings_with_clipboard(processing_mode, llm_configured, history_enabled, false)
    }

    fn settings_with_clipboard(
        processing_mode: ProcessingMode,
        llm_configured: bool,
        history_enabled: bool,
        clipboard_bridge_allowed: bool,
    ) -> SettingsSnapshot {
        SettingsSnapshot::new(SettingsSnapshotInput {
            version: 1,
            recording_mode: RecordingMode::Toggle,
            max_recording_duration: Duration::from_secs(600),
            recording_shortcut: None,
            processing_mode,
            asr_preference: AsrPreference::Qwen,
            llm: llm_configured.then(|| {
                remtene_domain::LlmNonSecretSettings::new("https://provider.invalid/v1", "model")
                    .expect("test LLM settings")
            }),
            read_selected_text: true,
            clipboard_bridge_allowed,
            auto_copy_result: false,
            local_diagnostics_enabled: true,
            history_policy: HistoryPolicy {
                enabled: history_enabled,
                limit: 10,
                retention_days: None,
            },
        })
        .expect("test settings must be valid")
    }

    fn target(security: TargetSecurity, has_selection: bool) -> CapturedTarget {
        CapturedTarget {
            target_ref: TargetSnapshotRef::new("target"),
            security,
            has_selection,
            display_hint: None,
        }
    }

    fn port_error(code: &str) -> PortError {
        PortError {
            code: code.to_owned(),
            safe_message_key: format!("error.{code}"),
            retryable: false,
        }
    }

    fn retryable_port_error(code: &str) -> PortError {
        PortError {
            retryable: true,
            ..port_error(code)
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        struct ThreadWake(thread::Thread);

        impl Wake for ThreadWake {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.0.unpark();
            }
        }

        let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
        let mut context = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => thread::park_timeout(Duration::from_millis(1)),
            }
        }
    }

    #[derive(Clone)]
    enum AsrBehavior {
        Success(String),
        Failure(PortError),
    }

    #[derive(Clone)]
    enum LlmBehavior {
        Success(String),
        CorrelationMismatch(String),
        Failure(PortError),
    }

    #[derive(Clone)]
    struct MockConfig {
        settings: SettingsSnapshot,
        target: Result<CapturedTarget, PortError>,
        selection: Result<SelectionSnapshot, PortError>,
        clipboard_selection: Result<SelectionSnapshot, PortError>,
        microphone_access: Result<MicrophoneAccess, PortError>,
        qwen_health: EngineHealth,
        whisper_health: EngineHealth,
        audio_finish: Result<(), PortError>,
        recording_cue: Result<(), PortError>,
        audio_cancel_failures_remaining: usize,
        audio_cleanup_failures_remaining: usize,
        recording_deadline: Result<(), PortError>,
        recording_hud_show: Result<(), PortError>,
        recording_hud_update: Result<(), PortError>,
        recording_hud_terminal: Result<(), PortError>,
        recording_hud_hide: Result<(), PortError>,
        user_notification: Result<(), PortError>,
        asr_behavior: AsrBehavior,
        llm_route_error: Option<PortError>,
        llm_behavior: LlmBehavior,
        revalidation: Result<TargetRevalidation, PortError>,
        insert: Result<InsertOutcome, PortError>,
        clipboard_insert: Result<InsertOutcome, PortError>,
        user_directed_paste: Result<UserDirectedPasteOutcome, PortError>,
        temporary_text: Result<(), PortError>,
        clipboard_write: Result<(), PortError>,
        history_save: Result<(), PortError>,
        history_policy: Result<(), PortError>,
    }

    impl MockConfig {
        fn faithful() -> Self {
            Self {
                settings: settings(ProcessingMode::Faithful, true, true),
                target: Ok(target(TargetSecurity::Safe, true)),
                selection: Ok(SelectionSnapshot {
                    text: Some("context".to_owned()),
                    anchor_normalized_to_end: true,
                    exceeded_limit: false,
                }),
                clipboard_selection: Ok(SelectionSnapshot {
                    text: Some("clipboard context".to_owned()),
                    anchor_normalized_to_end: true,
                    exceeded_limit: false,
                }),
                microphone_access: Ok(MicrophoneAccess::Granted),
                qwen_health: EngineHealth::Healthy,
                whisper_health: EngineHealth::Healthy,
                audio_finish: Ok(()),
                recording_cue: Ok(()),
                audio_cancel_failures_remaining: 0,
                audio_cleanup_failures_remaining: 0,
                recording_deadline: Ok(()),
                recording_hud_show: Ok(()),
                recording_hud_update: Ok(()),
                recording_hud_terminal: Ok(()),
                recording_hud_hide: Ok(()),
                user_notification: Ok(()),
                asr_behavior: AsrBehavior::Success("raw text".to_owned()),
                llm_route_error: None,
                llm_behavior: LlmBehavior::Success("clean text".to_owned()),
                revalidation: Ok(TargetRevalidation::Valid(ValidatedTargetRef::new("valid"))),
                insert: Ok(InsertOutcome::Inserted),
                clipboard_insert: Ok(InsertOutcome::Inserted),
                user_directed_paste: Ok(UserDirectedPasteOutcome::Dispatched),
                temporary_text: Ok(()),
                clipboard_write: Ok(()),
                history_save: Ok(()),
                history_policy: Ok(()),
            }
        }
    }

    #[derive(Clone, Default)]
    struct MockCalls {
        target_captures: usize,
        selection_reads: usize,
        target_revalidations: usize,
        clipboard_selection_reads: usize,
        clipboard_insert_attempts: usize,
        clipboard_inserted_texts: Vec<String>,
        user_directed_paste_attempts: usize,
        user_directed_pasted_texts: Vec<String>,
        microphone_permission_requests: usize,
        health_checks: usize,
        audio_starts: usize,
        audio_finishes: usize,
        audio_cancels: usize,
        audio_cleanups: usize,
        recording_cues: Vec<RecordingCue>,
        recording_lifecycle_events: Vec<&'static str>,
        recording_hud_shows: Vec<(SessionId, RecordingHudState)>,
        recording_hud_show_hints: Vec<Option<TargetDisplayHint>>,
        recording_hud_updates: Vec<(SessionId, RecordingHudState)>,
        recording_hud_terminals: Vec<(SessionId, TerminalOutcome)>,
        recording_hud_hides: Vec<SessionId>,
        user_notifications: Vec<(SessionId, UserNotificationKind)>,
        presentation_events: Vec<&'static str>,
        asr_requests: Vec<AsrRequest>,
        asr_cancels: usize,
        llm_route_resolutions: usize,
        llm_routes: Vec<ResolvedLlmRoute>,
        llm_requests: Vec<TextProcessingRequest>,
        llm_cancels: usize,
        insert_attempts: usize,
        inserted_texts: Vec<String>,
        temporary_texts: Vec<String>,
        temporary_text_statuses: Vec<TemporaryTextStatus>,
        clipboard_written_texts: Vec<String>,
        history_save_attempts: usize,
        history_records: Vec<HistoryRecord>,
        history_policy_enforcements: usize,
        diagnostics: Vec<DiagnosticEvent>,
    }

    struct MockPorts {
        config: Mutex<MockConfig>,
        calls: Mutex<MockCalls>,
        audio_start_gate: TestGate,
        audio_cancel_gate: TestGate,
        health_gate: TestGate,
        asr_gate: TestGate,
        asr_cancel_gate: TestGate,
        llm_gate: TestGate,
        llm_cancel_gate: TestGate,
        output_precommit_gate: TestGate,
        temporary_precommit_gate: TestGate,
        history_commit_gate: TestGate,
    }

    impl MockPorts {
        fn new(config: MockConfig) -> Self {
            Self {
                config: Mutex::new(config),
                calls: Mutex::new(MockCalls::default()),
                audio_start_gate: TestGate::default(),
                audio_cancel_gate: TestGate::default(),
                health_gate: TestGate::default(),
                asr_gate: TestGate::default(),
                asr_cancel_gate: TestGate::default(),
                llm_gate: TestGate::default(),
                llm_cancel_gate: TestGate::default(),
                output_precommit_gate: TestGate::default(),
                temporary_precommit_gate: TestGate::default(),
                history_commit_gate: TestGate::default(),
            }
        }

        fn calls(&self) -> MockCalls {
            self.calls.lock().expect("calls lock").clone()
        }

        fn install_asr_gate(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
            self.asr_gate.install()
        }

        fn install_asr_cancel_gate(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
            self.asr_cancel_gate.install()
        }

        fn install_audio_start_gate(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
            self.audio_start_gate.install()
        }

        fn install_audio_cancel_gate(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
            self.audio_cancel_gate.install()
        }

        fn install_health_gate(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
            self.health_gate.install()
        }

        fn install_llm_gate(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
            self.llm_gate.install()
        }

        fn install_llm_cancel_gate(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
            self.llm_cancel_gate.install()
        }

        fn install_output_precommit_gate(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
            self.output_precommit_gate.install()
        }

        fn install_temporary_precommit_gate(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
            self.temporary_precommit_gate.install()
        }

        fn install_history_commit_gate(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
            self.history_commit_gate.install()
        }
    }

    #[derive(Default)]
    struct TestGate {
        started: Mutex<Option<mpsc::Sender<()>>>,
        release: Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl TestGate {
        fn install(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
            let (started_sender, started_receiver) = mpsc::channel();
            let (release_sender, release_receiver) = mpsc::channel();
            *self.started.lock().expect("gate started lock") = Some(started_sender);
            *self.release.lock().expect("gate release lock") = Some(release_receiver);
            (started_receiver, release_sender)
        }

        fn take(&self) -> (Option<mpsc::Sender<()>>, Option<mpsc::Receiver<()>>) {
            (
                self.started.lock().expect("gate started lock").take(),
                self.release.lock().expect("gate release lock").take(),
            )
        }
    }

    fn wait_at_gate(started: Option<mpsc::Sender<()>>, release: Option<mpsc::Receiver<()>>) {
        if let Some(started) = started {
            let _ = started.send(());
        }
        if let Some(release) = release {
            let _ = release.recv();
        }
    }

    impl SettingsStore for MockPorts {
        fn load(&self) -> PortFuture<'_, Result<SettingsSnapshot, PortError>> {
            let settings = self.config.lock().expect("config lock").settings.clone();
            Box::pin(async move { Ok(settings) })
        }

        fn replace(
            &self,
            _expected_version: u64,
            settings: SettingsSnapshot,
        ) -> PortFuture<'_, Result<SettingsSnapshot, PortError>> {
            self.config.lock().expect("config lock").settings = settings.clone();
            Box::pin(async move { Ok(settings) })
        }
    }

    impl TargetContextPort for MockPorts {
        fn capture(&self) -> PortFuture<'_, Result<CapturedTarget, PortError>> {
            self.calls.lock().expect("calls lock").target_captures += 1;
            let result = self.config.lock().expect("config lock").target.clone();
            Box::pin(async move { result })
        }

        fn read_selected_text(
            &self,
            _target: &TargetSnapshotRef,
        ) -> PortFuture<'_, Result<SelectionSnapshot, PortError>> {
            self.calls.lock().expect("calls lock").selection_reads += 1;
            let result = self.config.lock().expect("config lock").selection.clone();
            Box::pin(async move { result })
        }

        fn revalidate(
            &self,
            _target: &TargetSnapshotRef,
        ) -> PortFuture<'_, Result<TargetRevalidation, PortError>> {
            self.calls.lock().expect("calls lock").target_revalidations += 1;
            let result = self
                .config
                .lock()
                .expect("config lock")
                .revalidation
                .clone();
            Box::pin(async move { result })
        }
    }

    impl MicrophonePermissionPort for MockPorts {
        fn request_recording_access(&self) -> PortFuture<'_, Result<MicrophoneAccess, PortError>> {
            self.calls
                .lock()
                .expect("calls lock")
                .microphone_permission_requests += 1;
            let result = self
                .config
                .lock()
                .expect("config lock")
                .microphone_access
                .clone();
            Box::pin(async move { result })
        }
    }

    impl AudioCapture for MockPorts {
        fn start(
            &self,
            _session_id: SessionId,
        ) -> PortFuture<'_, Result<AudioCaptureRef, PortError>> {
            let mut calls = self.calls.lock().expect("calls lock");
            calls.audio_starts += 1;
            calls.recording_lifecycle_events.push("audio_start");
            drop(calls);
            let gate = self.audio_start_gate.take();
            Box::pin(async move {
                wait_at_gate(gate.0, gate.1);
                Ok(AudioCaptureRef::new("capture"))
            })
        }

        fn finish(
            &self,
            _capture: AudioCaptureRef,
        ) -> PortFuture<'_, Result<FinalizedAudio, PortError>> {
            let mut calls = self.calls.lock().expect("calls lock");
            calls.audio_finishes += 1;
            calls.recording_lifecycle_events.push("audio_finish");
            drop(calls);
            let result = self
                .config
                .lock()
                .expect("config lock")
                .audio_finish
                .clone();
            Box::pin(async move {
                result?;
                Ok(FinalizedAudio {
                    audio_ref: AudioRef::new("audio"),
                    format: AudioFormat {
                        sample_rate_hz: 16_000,
                        channels: 1,
                        bits_per_sample: 16,
                    },
                    duration_ms: 1_000,
                })
            })
        }

        fn cancel(&self, _capture: AudioCaptureRef) -> PortFuture<'_, Result<(), PortError>> {
            let mut calls = self.calls.lock().expect("calls lock");
            calls.audio_cancels += 1;
            calls.recording_lifecycle_events.push("audio_cancel");
            drop(calls);
            let gate = self.audio_cancel_gate.take();
            let result = {
                let mut config = self.config.lock().expect("config lock");
                if config.audio_cancel_failures_remaining == 0 {
                    Ok(())
                } else {
                    config.audio_cancel_failures_remaining -= 1;
                    Err(retryable_port_error("audio_cancel_failed"))
                }
            };
            Box::pin(async move {
                wait_at_gate(gate.0, gate.1);
                result
            })
        }

        fn cleanup(&self, _audio_ref: AudioRef) -> PortFuture<'_, Result<(), PortError>> {
            self.calls.lock().expect("calls lock").audio_cleanups += 1;
            let result = {
                let mut config = self.config.lock().expect("config lock");
                if config.audio_cleanup_failures_remaining == 0 {
                    Ok(())
                } else {
                    config.audio_cleanup_failures_remaining -= 1;
                    Err(retryable_port_error("audio_cleanup_failed"))
                }
            };
            Box::pin(async move { result })
        }
    }

    impl RecordingCuePort for MockPorts {
        fn play(&self, cue: RecordingCue) -> PortFuture<'_, Result<(), PortError>> {
            let mut calls = self.calls.lock().expect("calls lock");
            calls.recording_cues.push(cue);
            calls.recording_lifecycle_events.push(match cue {
                RecordingCue::Start => "cue_start",
                RecordingCue::Finish => "cue_finish",
                RecordingCue::Cancel => "cue_cancel",
            });
            drop(calls);
            let result = self
                .config
                .lock()
                .expect("config lock")
                .recording_cue
                .clone();
            Box::pin(async move { result })
        }
    }

    impl RecordingDeadlinePort for MockPorts {
        fn schedule(
            &self,
            _duration: Duration,
            _on_elapsed: crate::ports::RecordingDeadlineTask,
        ) -> Result<RecordingDeadlineGuard, PortError> {
            self.config
                .lock()
                .expect("config lock")
                .recording_deadline
                .clone()
                .map(|()| RecordingDeadlineGuard::new(|| {}))
        }
    }

    impl RecordingHudPort for MockPorts {
        fn show(
            &self,
            session_id: SessionId,
            state: RecordingHudState,
            display_hint: Option<TargetDisplayHint>,
            _recording_limit: Duration,
        ) -> PortFuture<'_, Result<(), PortError>> {
            let mut calls = self.calls.lock().expect("calls lock");
            calls.recording_hud_shows.push((session_id, state));
            calls.recording_hud_show_hints.push(display_hint);
            drop(calls);
            let result = self
                .config
                .lock()
                .expect("config lock")
                .recording_hud_show
                .clone();
            Box::pin(async move { result })
        }

        fn update(
            &self,
            session_id: SessionId,
            state: RecordingHudState,
        ) -> PortFuture<'_, Result<(), PortError>> {
            self.calls
                .lock()
                .expect("calls lock")
                .recording_hud_updates
                .push((session_id, state));
            let result = self
                .config
                .lock()
                .expect("config lock")
                .recording_hud_update
                .clone();
            Box::pin(async move { result })
        }

        fn publish_terminal(
            &self,
            session_id: SessionId,
            outcome: TerminalOutcome,
        ) -> PortFuture<'_, Result<(), PortError>> {
            let mut calls = self.calls.lock().expect("calls lock");
            calls.recording_hud_terminals.push((session_id, outcome));
            calls.presentation_events.push("terminal");
            drop(calls);
            let result = self
                .config
                .lock()
                .expect("config lock")
                .recording_hud_terminal
                .clone();
            Box::pin(async move { result })
        }

        fn hide(&self, session_id: SessionId) -> PortFuture<'_, Result<(), PortError>> {
            let mut calls = self.calls.lock().expect("calls lock");
            calls.recording_hud_hides.push(session_id);
            calls.presentation_events.push("hide");
            drop(calls);
            let result = self
                .config
                .lock()
                .expect("config lock")
                .recording_hud_hide
                .clone();
            Box::pin(async move { result })
        }
    }

    impl UserNotificationPort for MockPorts {
        fn raise(
            &self,
            session_id: SessionId,
            kind: UserNotificationKind,
        ) -> PortFuture<'_, Result<(), PortError>> {
            let mut calls = self.calls.lock().expect("calls lock");
            calls.user_notifications.push((session_id, kind));
            calls.presentation_events.push("notification");
            drop(calls);
            let result = self
                .config
                .lock()
                .expect("config lock")
                .user_notification
                .clone();
            Box::pin(async move { result })
        }
    }

    impl AsrEnginePort for MockPorts {
        fn health(&self, engine: AsrEngine) -> PortFuture<'_, Result<EngineHealth, PortError>> {
            self.calls.lock().expect("calls lock").health_checks += 1;
            let gate = self.health_gate.take();
            let config = self.config.lock().expect("config lock");
            let health = match engine {
                AsrEngine::Qwen => config.qwen_health,
                AsrEngine::Whisper => config.whisper_health,
            };
            Box::pin(async move {
                wait_at_gate(gate.0, gate.1);
                Ok(health)
            })
        }

        fn transcribe(&self, request: AsrRequest) -> PortFuture<'_, Result<AsrResult, PortError>> {
            self.calls
                .lock()
                .expect("calls lock")
                .asr_requests
                .push(request.clone());
            let behavior = self
                .config
                .lock()
                .expect("config lock")
                .asr_behavior
                .clone();
            let gate = self.asr_gate.take();
            Box::pin(async move {
                wait_at_gate(gate.0, gate.1);
                match behavior {
                    AsrBehavior::Success(final_text) => Ok(AsrResult {
                        session_id: request.session_id,
                        request_id: request.request_id,
                        engine: request.engine,
                        final_text,
                        detected_language: Some("zh".to_owned()),
                        inference_duration_ms: 10,
                    }),
                    AsrBehavior::Failure(error) => Err(error),
                }
            })
        }

        fn cancel(&self, _request_id: RequestId) -> PortFuture<'_, Result<(), PortError>> {
            self.calls.lock().expect("calls lock").asr_cancels += 1;
            let gate = self.asr_cancel_gate.take();
            Box::pin(async move {
                wait_at_gate(gate.0, gate.1);
                Ok(())
            })
        }
    }

    impl LlmProvider for MockPorts {
        fn resolve_route(
            &self,
            candidate: Option<LlmRouteCandidate>,
        ) -> PortFuture<'_, LlmRouteResolution> {
            self.calls.lock().expect("calls lock").llm_route_resolutions += 1;
            if let Some(error) = self
                .config
                .lock()
                .expect("config lock")
                .llm_route_error
                .clone()
            {
                return Box::pin(
                    async move { LlmRouteResolution::Unavailable { route: None, error } },
                );
            }
            let resolution = candidate.map_or(LlmRouteResolution::NoConfiguration, |candidate| {
                LlmRouteResolution::Ready(ResolvedLlmRoute::new(
                    "primary",
                    format!(
                        "{}/chat/completions",
                        candidate.base_url().trim_end_matches('/')
                    ),
                    candidate.model(),
                    "llm.openai_compatible.test-fingerprint",
                ))
            });
            Box::pin(async move { resolution })
        }

        fn process(
            &self,
            route: ResolvedLlmRoute,
            request: TextProcessingRequest,
        ) -> PortFuture<'_, Result<TextProcessingResult, PortError>> {
            let mut calls = self.calls.lock().expect("calls lock");
            calls.llm_routes.push(route);
            calls.llm_requests.push(request.clone());
            drop(calls);
            let behavior = self
                .config
                .lock()
                .expect("config lock")
                .llm_behavior
                .clone();
            let gate = self.llm_gate.take();
            Box::pin(async move {
                wait_at_gate(gate.0, gate.1);
                match behavior {
                    LlmBehavior::Success(final_text) => Ok(TextProcessingResult {
                        session_id: request.session_id,
                        request_id: request.request_id,
                        intent: IntentDecision::Dictation,
                        final_text,
                    }),
                    LlmBehavior::CorrelationMismatch(final_text) => Ok(TextProcessingResult {
                        session_id: SessionId::new(),
                        request_id: RequestId::new(),
                        intent: IntentDecision::Dictation,
                        final_text,
                    }),
                    LlmBehavior::Failure(error) => Err(error),
                }
            })
        }

        fn cancel(&self, _request_id: RequestId) -> PortFuture<'_, Result<(), PortError>> {
            self.calls.lock().expect("calls lock").llm_cancels += 1;
            let gate = self.llm_cancel_gate.take();
            Box::pin(async move {
                wait_at_gate(gate.0, gate.1);
                Ok(())
            })
        }
    }

    impl OutputAdapter for MockPorts {
        fn insert(
            &self,
            _target: ValidatedTargetRef,
            text: String,
            _delivery_id: DeliveryId,
            lifecycle: LifecycleFence,
        ) -> PortFuture<'_, Result<InsertOutcome, PortError>> {
            let result = self.config.lock().expect("config lock").insert.clone();
            self.calls.lock().expect("calls lock").insert_attempts += 1;
            let gate = self.output_precommit_gate.take();
            Box::pin(async move {
                wait_at_gate(gate.0, gate.1);
                let Some(_commit_guard) = lifecycle.begin_commit() else {
                    return Err(port_error("lifecycle_invalidated"));
                };
                if matches!(result, Ok(InsertOutcome::Inserted)) {
                    self.calls
                        .lock()
                        .expect("calls lock")
                        .inserted_texts
                        .push(text);
                }
                result
            })
        }
    }

    impl ClipboardBridge for MockPorts {
        fn read_selected_text(
            &self,
            _target: &TargetSnapshotRef,
        ) -> PortFuture<'_, Result<SelectionSnapshot, PortError>> {
            self.calls
                .lock()
                .expect("calls lock")
                .clipboard_selection_reads += 1;
            let result = self
                .config
                .lock()
                .expect("config lock")
                .clipboard_selection
                .clone();
            Box::pin(async move { result })
        }

        fn insert_and_restore(
            &self,
            _target: ValidatedTargetRef,
            text: String,
            _delivery_id: DeliveryId,
            lifecycle: LifecycleFence,
        ) -> PortFuture<'_, Result<InsertOutcome, PortError>> {
            self.calls
                .lock()
                .expect("calls lock")
                .clipboard_insert_attempts += 1;
            let result = self
                .config
                .lock()
                .expect("config lock")
                .clipboard_insert
                .clone();
            Box::pin(async move {
                let Some(_commit_guard) = lifecycle.begin_commit() else {
                    return Err(port_error("lifecycle_invalidated"));
                };
                if matches!(result, Ok(InsertOutcome::Inserted)) {
                    self.calls
                        .lock()
                        .expect("calls lock")
                        .clipboard_inserted_texts
                        .push(text);
                }
                result
            })
        }

        fn insert_at_current_focus_and_restore(
            &self,
            text: String,
            _delivery_id: DeliveryId,
            lifecycle: LifecycleFence,
        ) -> PortFuture<'_, Result<UserDirectedPasteOutcome, PortError>> {
            self.calls
                .lock()
                .expect("calls lock")
                .user_directed_paste_attempts += 1;
            let result = self
                .config
                .lock()
                .expect("config lock")
                .user_directed_paste
                .clone();
            Box::pin(async move {
                let Some(_commit_guard) = lifecycle.begin_commit() else {
                    return Err(port_error("lifecycle_invalidated"));
                };
                if matches!(result, Ok(UserDirectedPasteOutcome::Dispatched)) {
                    self.calls
                        .lock()
                        .expect("calls lock")
                        .user_directed_pasted_texts
                        .push(text);
                }
                result
            })
        }
    }

    impl ClipboardTextWriter for MockPorts {
        fn write_text(&self, text: String) -> PortFuture<'_, Result<(), PortError>> {
            let result = self
                .config
                .lock()
                .expect("config lock")
                .clipboard_write
                .clone();
            Box::pin(async move {
                result?;
                self.calls
                    .lock()
                    .expect("calls lock")
                    .clipboard_written_texts
                    .push(text);
                Ok(())
            })
        }
    }

    impl TemporaryTextOutput for MockPorts {
        fn show(
            &self,
            _session_id: SessionId,
            _delivery_id: DeliveryId,
            final_text: String,
            status: TemporaryTextStatus,
            lifecycle: LifecycleFence,
        ) -> PortFuture<'_, Result<(), PortError>> {
            let result = self
                .config
                .lock()
                .expect("config lock")
                .temporary_text
                .clone();
            let gate = self.temporary_precommit_gate.take();
            Box::pin(async move {
                result?;
                wait_at_gate(gate.0, gate.1);
                let Some(_commit_guard) = lifecycle.begin_commit() else {
                    return Err(port_error("lifecycle_invalidated"));
                };
                self.calls
                    .lock()
                    .expect("calls lock")
                    .temporary_texts
                    .push(final_text);
                self.calls
                    .lock()
                    .expect("calls lock")
                    .temporary_text_statuses
                    .push(status);
                Ok(())
            })
        }
    }

    impl HistoryStore for MockPorts {
        fn save_with_policy(
            &self,
            record: HistoryRecord,
            _settings: &SettingsSnapshot,
            lifecycle: LifecycleFence,
        ) -> PortFuture<'_, Result<(), PortError>> {
            let save_result = self
                .config
                .lock()
                .expect("config lock")
                .history_save
                .clone();
            let policy_result = self
                .config
                .lock()
                .expect("config lock")
                .history_policy
                .clone();
            self.calls.lock().expect("calls lock").history_save_attempts += 1;
            let gate = self.history_commit_gate.take();
            Box::pin(async move {
                save_result?;
                policy_result?;
                let Some(_commit_guard) = lifecycle.begin_commit() else {
                    return Err(port_error("lifecycle_invalidated"));
                };
                wait_at_gate(gate.0, gate.1);
                let mut calls = self.calls.lock().expect("calls lock");
                calls.history_records.push(record);
                calls.history_policy_enforcements += 1;
                Ok(())
            })
        }

        fn list(&self) -> PortFuture<'_, Result<Vec<HistoryRecord>, PortError>> {
            let records = self
                .calls
                .lock()
                .expect("calls lock")
                .history_records
                .clone();
            Box::pin(async move { Ok(records) })
        }

        fn clear_all(&self) -> PortFuture<'_, Result<(), PortError>> {
            self.calls
                .lock()
                .expect("calls lock")
                .history_records
                .clear();
            Box::pin(async { Ok(()) })
        }

        fn enforce_policy(
            &self,
            _settings: &SettingsSnapshot,
            lifecycle: LifecycleFence,
        ) -> PortFuture<'_, Result<(), PortError>> {
            let result = self
                .config
                .lock()
                .expect("config lock")
                .history_policy
                .clone();
            Box::pin(async move {
                result?;
                let Some(_commit_guard) = lifecycle.begin_commit() else {
                    return Err(port_error("lifecycle_invalidated"));
                };
                self.calls
                    .lock()
                    .expect("calls lock")
                    .history_policy_enforcements += 1;
                Ok(())
            })
        }
    }

    impl DiagnosticsSink for MockPorts {
        fn record(&self, event: DiagnosticEvent) {
            self.calls
                .lock()
                .expect("calls lock")
                .diagnostics
                .push(event);
        }
    }

    impl Clock for MockPorts {
        fn now(&self) -> TimestampMs {
            TimestampMs::new(100)
        }
    }

    impl IdGenerator for MockPorts {
        fn session_id(&self) -> SessionId {
            SessionId::new()
        }

        fn request_id(&self) -> RequestId {
            RequestId::new()
        }

        fn delivery_id(&self) -> DeliveryId {
            DeliveryId::new()
        }
    }
}
