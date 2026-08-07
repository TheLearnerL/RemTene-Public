//! Stub implementations for platform-specific ports
//!
//! These are temporary implementations used for architectural integration.
//! They should be replaced with real platform implementations from remtene-platform.

use remtene_application::ports::{
    AudioCapture, AudioCaptureRef, AudioRef, CapturedTarget, FinalizedAudio, MicrophoneAccess,
    MicrophonePermissionPort, PortError, PortFuture, SelectionSnapshot, TargetContextPort,
    TargetRevalidation, TargetSnapshotRef,
};
use remtene_domain::SessionId;

/// Stub target context implementation
pub struct StubTargetContext;

impl StubTargetContext {
    pub fn new() -> Self {
        Self
    }
}

impl Default for StubTargetContext {
    fn default() -> Self {
        Self::new()
    }
}

impl TargetContextPort for StubTargetContext {
    fn capture(&self) -> PortFuture<'_, Result<CapturedTarget, PortError>> {
        Box::pin(async {
            Err(PortError {
                code: "target.stub".to_string(),
                safe_message_key: "errors.target.stub".to_string(),
                retryable: false,
            })
        })
    }

    fn read_selected_text(
        &self,
        _target: &TargetSnapshotRef,
    ) -> PortFuture<'_, Result<SelectionSnapshot, PortError>> {
        Box::pin(async {
            Err(PortError {
                code: "target.stub".to_string(),
                safe_message_key: "errors.target.stub".to_string(),
                retryable: false,
            })
        })
    }

    fn revalidate(
        &self,
        _target: &TargetSnapshotRef,
    ) -> PortFuture<'_, Result<TargetRevalidation, PortError>> {
        Box::pin(async {
            Err(PortError {
                code: "target.stub".to_string(),
                safe_message_key: "errors.target.stub".to_string(),
                retryable: false,
            })
        })
    }
}

/// Stub microphone permission implementation
pub struct StubMicrophonePermission;

impl StubMicrophonePermission {
    pub fn new() -> Self {
        Self
    }
}

impl Default for StubMicrophonePermission {
    fn default() -> Self {
        Self::new()
    }
}

impl MicrophonePermissionPort for StubMicrophonePermission {
    fn request_recording_access(&self) -> PortFuture<'_, Result<MicrophoneAccess, PortError>> {
        Box::pin(async { Ok(MicrophoneAccess::Unavailable) })
    }
}

/// Stub audio capture implementation
pub struct StubAudioCapture;

impl StubAudioCapture {
    pub fn new() -> Self {
        Self
    }
}

impl Default for StubAudioCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioCapture for StubAudioCapture {
    fn start(&self, _session_id: SessionId) -> PortFuture<'_, Result<AudioCaptureRef, PortError>> {
        Box::pin(async {
            Err(PortError {
                code: "audio.stub".to_string(),
                safe_message_key: "errors.audio.stub".to_string(),
                retryable: false,
            })
        })
    }

    fn finish(
        &self,
        _capture_ref: AudioCaptureRef,
    ) -> PortFuture<'_, Result<FinalizedAudio, PortError>> {
        Box::pin(async {
            Err(PortError {
                code: "audio.stub".to_string(),
                safe_message_key: "errors.audio.stub".to_string(),
                retryable: false,
            })
        })
    }

    fn cancel(&self, _capture_ref: AudioCaptureRef) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async {
            Err(PortError {
                code: "audio.stub".to_string(),
                safe_message_key: "errors.audio.stub".to_string(),
                retryable: false,
            })
        })
    }

    fn cleanup(&self, _audio_ref: AudioRef) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Stub ASR engine implementation
pub struct StubAsrEngine;

impl StubAsrEngine {
    pub fn new() -> Self {
        Self
    }
}

impl Default for StubAsrEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl remtene_application::ports::AsrEnginePort for StubAsrEngine {
    fn health(
        &self,
        _engine: remtene_domain::AsrEngine,
    ) -> PortFuture<'_, Result<remtene_application::ports::EngineHealth, PortError>> {
        Box::pin(async {
            Err(PortError {
                code: "asr.stub".to_string(),
                safe_message_key: "errors.asr.stub".to_string(),
                retryable: false,
            })
        })
    }

    fn transcribe(
        &self,
        _request: remtene_application::ports::AsrRequest,
    ) -> PortFuture<'_, Result<remtene_application::ports::AsrResult, PortError>> {
        Box::pin(async {
            Err(PortError {
                code: "asr.stub".to_string(),
                safe_message_key: "errors.asr.stub".to_string(),
                retryable: false,
            })
        })
    }

    fn cancel(
        &self,
        _request_id: remtene_domain::RequestId,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }
}

/// 未提供正式模型 Runtime 时使用的 fail-closed 模型控制实现。
pub struct StubAsrModelControl;

impl StubAsrModelControl {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for StubAsrModelControl {
    fn default() -> Self {
        Self::new()
    }
}

impl remtene_application::ports::AsrModelControlPort for StubAsrModelControl {
    fn prepare(
        &self,
        _engine: remtene_domain::AsrEngine,
    ) -> PortFuture<'_, Result<(), remtene_application::ports::AsrModelPreparationError>> {
        Box::pin(async { Err(remtene_application::ports::AsrModelPreparationError::Missing) })
    }
}

/// Stub recording HUD implementation.
///
/// Accepts all show/update/hide calls as no-ops. Used for assembly tests and
/// non-desktop fallbacks where no real HUD window exists.
pub struct StubRecordingHud;

impl StubRecordingHud {
    pub fn new() -> Self {
        Self
    }
}

impl Default for StubRecordingHud {
    fn default() -> Self {
        Self::new()
    }
}

impl remtene_application::ports::RecordingHudPort for StubRecordingHud {
    fn show(
        &self,
        _session_id: SessionId,
        _state: remtene_application::ports::RecordingHudState,
        _display_hint: Option<remtene_application::ports::TargetDisplayHint>,
        _recording_limit: std::time::Duration,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }

    fn update(
        &self,
        _session_id: SessionId,
        _state: remtene_application::ports::RecordingHudState,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }

    fn publish_terminal(
        &self,
        _session_id: SessionId,
        _outcome: remtene_domain::TerminalOutcome,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }

    fn hide(&self, _session_id: SessionId) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Stub user notification surface used only by assembly tests and unsupported
/// platform compositions.
pub struct StubUserNotification;

impl StubUserNotification {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for StubUserNotification {
    fn default() -> Self {
        Self::new()
    }
}

impl remtene_application::ports::UserNotificationPort for StubUserNotification {
    fn raise(
        &self,
        _session_id: SessionId,
        _kind: remtene_application::ports::UserNotificationKind,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }
}
