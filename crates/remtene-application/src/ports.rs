//! Stable capabilities used by application workflows.

use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    time::Duration,
};

use remtene_domain::{
    AsrEngine, DeliveryId, IntentDecision, ProcessingMode, RecordingShortcut, RequestId, SessionId,
    SettingsSnapshot, TerminalOutcome, TimestampMs,
};
use thiserror::Error;
use zeroize::Zeroizing;

pub type PortFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type RecordingDeadlineTask = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// 单个录音 Deadline（截止时间）任务的取消所有权。
///
/// 丢弃 Guard（守卫）必须阻止尚未开始的任务。已经赢得竞态的任务仍然安全，
/// 因为 Orchestrator（编排器）会在提交前复核 Session 和 `Recording` 阶段。
pub struct RecordingDeadlineGuard {
    cancel: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl RecordingDeadlineGuard {
    #[must_use]
    pub fn new(cancel: impl FnOnce() + Send + 'static) -> Self {
        Self {
            cancel: Some(Box::new(cancel)),
        }
    }
}

impl fmt::Debug for RecordingDeadlineGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecordingDeadlineGuard")
            .field("armed", &self.cancel.is_some())
            .finish()
    }
}

impl Drop for RecordingDeadlineGuard {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel();
        }
    }
}

/// A session-scoped barrier guarding irreversible external side effects.
///
/// Output, temporary-text, and history adapters must call [`Self::begin_commit`]
/// immediately before their irreversible commit point and hold the returned
/// [`CommitGuard`] until that commit has finished. Once invalidated, no new
/// commit can begin. The quiescence future is runtime-independent and wakes
/// when every commit that began before invalidation has completed.
#[derive(Clone, Debug)]
pub struct LifecycleFence {
    inner: Arc<LifecycleFenceInner>,
}

#[derive(Debug)]
struct LifecycleFenceInner {
    state: Mutex<LifecycleFenceState>,
}

#[derive(Debug)]
struct LifecycleFenceState {
    open: bool,
    in_flight: usize,
    waiters: Vec<Waker>,
}

impl LifecycleFence {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(LifecycleFenceInner {
                state: Mutex::new(LifecycleFenceState {
                    open: true,
                    in_flight: 0,
                    waiters: Vec::new(),
                }),
            }),
        }
    }

    /// Starts an irreversible commit if this session is still valid.
    #[must_use]
    pub fn begin_commit(&self) -> Option<CommitGuard> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.open {
            return None;
        }
        state.in_flight += 1;
        Some(CommitGuard {
            inner: Arc::clone(&self.inner),
        })
    }

    /// Prevents any future commit from beginning.
    pub fn invalidate(&self) {
        let waiters = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.open = false;
            if state.in_flight == 0 {
                std::mem::take(&mut state.waiters)
            } else {
                Vec::new()
            }
        };
        wake_all(waiters);
    }

    /// Resolves after every commit already in progress has completed.
    #[must_use]
    pub fn wait_quiescent(&self) -> LifecycleQuiescence {
        LifecycleQuiescence {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Invalidates the session and resolves once all prior commits finish.
    #[must_use]
    pub fn invalidate_and_wait(&self) -> LifecycleQuiescence {
        self.invalidate();
        self.wait_quiescent()
    }
}

impl Default for LifecycleFence {
    fn default() -> Self {
        Self::new()
    }
}

/// Proof that an irreversible commit began before session invalidation.
#[derive(Debug)]
pub struct CommitGuard {
    inner: Arc<LifecycleFenceInner>,
}

impl Drop for CommitGuard {
    fn drop(&mut self) {
        let waiters = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            debug_assert!(state.in_flight > 0, "commit guard count underflow");
            state.in_flight = state.in_flight.saturating_sub(1);
            if state.in_flight == 0 {
                std::mem::take(&mut state.waiters)
            } else {
                Vec::new()
            }
        };
        wake_all(waiters);
    }
}

/// Runtime-independent future used by shutdown to await commit quiescence.
#[derive(Debug)]
pub struct LifecycleQuiescence {
    inner: Arc<LifecycleFenceInner>,
}

impl Future for LifecycleQuiescence {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.in_flight == 0 {
            return Poll::Ready(());
        }
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

fn wake_all(waiters: Vec<Waker>) {
    for waiter in waiters {
        waiter.wake();
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{code}")]
pub struct PortError {
    pub code: String,
    pub safe_message_key: String,
    pub retryable: bool,
}

/// Stable Port error code for a capture that finalized without any audio frames.
pub const AUDIO_EMPTY_CAPTURE_CODE: &str = "audio.empty_capture";

/// Stable Port result code for audio that contains no recognizable speech.
pub const ASR_NO_SPEECH_CODE: &str = "asr.no_speech";

macro_rules! opaque_ref {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, PartialEq)]
        pub struct $name(String);

        impl $name {
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

opaque_ref!(AudioCaptureRef);
opaque_ref!(AudioRef);
opaque_ref!(TargetSnapshotRef);
opaque_ref!(ValidatedTargetRef);
opaque_ref!(ModelOperationId);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetSecurity {
    Safe,
    SecureInput,
    Unknown,
}

/// Content-free global logical point used only to place transient system UI
/// on the display containing the captured target window.
///
/// The hint contains no target identity, application metadata, selection, or
/// text. It remains inside the Rust application boundary and must never be
/// serialized to the renderer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TargetDisplayHint {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedTarget {
    pub target_ref: TargetSnapshotRef,
    pub security: TargetSecurity,
    pub has_selection: bool,
    pub display_hint: Option<TargetDisplayHint>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectionSnapshot {
    pub text: Option<String>,
    pub anchor_normalized_to_end: bool,
    pub exceeded_limit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TargetRevalidation {
    Valid(ValidatedTargetRef),
    Invalid,
    Indeterminate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioFormat {
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub bits_per_sample: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalizedAudio {
    pub audio_ref: AudioRef,
    pub format: AudioFormat,
    pub duration_ms: u64,
}

/// The result of an explicit, user-triggered microphone authorization check.
///
/// `NotDetermined` is deliberately absent: a Permission Adapter must resolve
/// the operating-system prompt before this future completes. Restricted and
/// unknown platform states remain distinct for diagnostics, but neither may
/// open an input device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MicrophoneAccess {
    Granted,
    Denied,
    Restricted,
    Unavailable,
}

pub trait MicrophonePermissionPort: Send + Sync {
    /// Checks current authorization and, only for this explicit recording
    /// attempt, requests system access when it has not yet been determined.
    fn request_recording_access(&self) -> PortFuture<'_, Result<MicrophoneAccess, PortError>>;
}

pub trait AudioCapture: Send + Sync {
    fn start(&self, session_id: SessionId) -> PortFuture<'_, Result<AudioCaptureRef, PortError>>;
    /// Success guarantees that microphone capture is closed and the audio is finalized.
    /// An error does not prove closure; the caller retains the capture reference and must cancel it.
    fn finish(&self, capture: AudioCaptureRef)
    -> PortFuture<'_, Result<FinalizedAudio, PortError>>;
    /// Success guarantees that microphone capture is closed and every temporary artifact owned by
    /// the capture has been removed. An error leaves ownership with the caller and must be retryable.
    fn cancel(&self, capture: AudioCaptureRef) -> PortFuture<'_, Result<(), PortError>>;
    /// Success guarantees that the finalized audio artifact has been removed. An error leaves
    /// ownership with the caller and must be retryable.
    fn cleanup(&self, audio_ref: AudioRef) -> PortFuture<'_, Result<(), PortError>>;
}

/// Short, content-free audible feedback for the microphone lifecycle.
///
/// Implementations must follow the system output volume and mute state. Cue
/// playback is deliberately separated from microphone capture so Application
/// can guarantee that the start cue finishes before the input device opens.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingCue {
    Start,
    Finish,
    Cancel,
}

pub trait RecordingCuePort: Send + Sync {
    fn play(&self, cue: RecordingCue) -> PortFuture<'_, Result<(), PortError>>;
}

/// 安排在 Session 开始时冻结的录音时长上限。
///
/// Runtime 特有的任务启动方式留在此 Port（接口）之后。任务始终调用与用户确认相同的
/// Application 结束用例，因此截止时间、快捷键和 HUD 确认会竞争同一次原子状态迁移。
pub trait RecordingDeadlinePort: Send + Sync {
    fn schedule(
        &self,
        duration: Duration,
        on_elapsed: RecordingDeadlineTask,
    ) -> Result<RecordingDeadlineGuard, PortError>;
}

/// Content-free projection of the active Session for the Recording HUD.
///
/// This type deliberately cannot carry transcript text, selected text, target
/// identity, audio samples, file paths, or secrets across the Presentation
/// boundary. Interactive controls are derived from the state instead of being
/// supplied as independently mutable flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingHudState {
    Preparing,
    Recording,
    Recognizing,
    Processing,
    Delivering,
    Finalizing,
    Completed,
}

impl RecordingHudState {
    #[must_use]
    pub const fn can_finish(self) -> bool {
        matches!(self, Self::Recording)
    }

    #[must_use]
    pub const fn can_cancel(self) -> bool {
        matches!(self, Self::Preparing | Self::Recording)
    }
}

pub trait RecordingHudPort: Send + Sync {
    /// Creates the session-scoped HUD without taking focus from the captured
    /// target. An error means the required recording controls are unavailable.
    fn show(
        &self,
        session_id: SessionId,
        state: RecordingHudState,
        display_hint: Option<TargetDisplayHint>,
        recording_limit: Duration,
    ) -> PortFuture<'_, Result<(), PortError>>;

    /// Updates an already-created HUD. Implementations must not create or
    /// retarget a different session when the supplied Session is stale.
    fn update(
        &self,
        session_id: SessionId,
        state: RecordingHudState,
    ) -> PortFuture<'_, Result<(), PortError>>;

    /// Publishes a content-free terminal signal only after the Domain Session
    /// accepted a terminal transition. Window cleanup remains a separate,
    /// idempotent operation because stale/preflight cleanup is not a business
    /// terminal outcome.
    fn publish_terminal(
        &self,
        session_id: SessionId,
        outcome: TerminalOutcome,
    ) -> PortFuture<'_, Result<(), PortError>>;

    /// Hides only the HUD owned by the supplied Session. This operation must be
    /// idempotent so late cleanup cannot hide a newer Session's HUD.
    fn hide(&self, session_id: SessionId) -> PortFuture<'_, Result<(), PortError>>;
}

/// Content-free user feedback that is rendered outside the Recording HUD.
///
/// These variants intentionally describe only the four approved recovery
/// surfaces. Presentation owns the exact copy and action label; Application
/// only selects a stable semantic outcome and never sends transcript text,
/// provider responses, paths, or arbitrary error strings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserNotificationKind {
    MicrophonePermission,
    Asr,
    Llm,
    Delivery,
}

pub trait UserNotificationPort: Send + Sync {
    /// Raises one session-scoped, content-free recovery surface.
    ///
    /// The operation is best-effort from the workflow's point of view: a
    /// presentation failure must not roll back an already completed delivery.
    fn raise(
        &self,
        session_id: SessionId,
        kind: UserNotificationKind,
    ) -> PortFuture<'_, Result<(), PortError>>;
}

pub trait TargetContextPort: Send + Sync {
    fn capture(&self) -> PortFuture<'_, Result<CapturedTarget, PortError>>;
    fn read_selected_text(
        &self,
        target: &TargetSnapshotRef,
    ) -> PortFuture<'_, Result<SelectionSnapshot, PortError>>;
    fn revalidate(
        &self,
        target: &TargetSnapshotRef,
    ) -> PortFuture<'_, Result<TargetRevalidation, PortError>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineHealth {
    Healthy,
    Unhealthy,
    Missing,
    Incompatible,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AsrRequest {
    pub session_id: SessionId,
    pub request_id: RequestId,
    pub engine: AsrEngine,
    pub audio: FinalizedAudio,
    pub language_hint: Option<String>,
    pub deadline_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AsrResult {
    pub session_id: SessionId,
    pub request_id: RequestId,
    pub engine: AsrEngine,
    pub final_text: String,
    pub detected_language: Option<String>,
    pub inference_duration_ms: u64,
}

pub trait AsrEnginePort: Send + Sync {
    fn health(&self, engine: AsrEngine) -> PortFuture<'_, Result<EngineHealth, PortError>>;
    fn transcribe(&self, request: AsrRequest) -> PortFuture<'_, Result<AsrResult, PortError>>;
    fn cancel(&self, request_id: RequestId) -> PortFuture<'_, Result<(), PortError>>;
}

/// 用户明确选择模型前，Adapter 对本地包与 Worker 配置的定向准备结果。
///
/// Application 只理解产品可见的失败分类，不接触 manifest、路径或平台 Runtime。
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum AsrModelPreparationError {
    #[error("the selected model package is missing or invalid")]
    Missing,
    #[error("the selected model package hash does not match its manifest")]
    HashMismatch,
    #[error("the ASR model runtime could not be prepared: {0}")]
    Runtime(PortError),
}

/// 准备用户明确选择的本地 ASR 模型。
///
/// 实现必须只接纳通过完整性验证的固定模型包；必要时可在空闲状态重配 Worker，
/// 但不得启动录音、读取选区或访问网络。成功只表示目标可进入 Health／预热，
/// 最终选择仍由 Application 在 Health 成功后持久化。
pub trait AsrModelControlPort: Send + Sync {
    fn prepare(&self, engine: AsrEngine) -> PortFuture<'_, Result<(), AsrModelPreparationError>>;
}

#[derive(Clone, Eq, PartialEq)]
pub struct TextProcessingRequest {
    pub session_id: SessionId,
    pub request_id: RequestId,
    pub processing_mode: ProcessingMode,
    pub raw_transcript: String,
    pub selected_text: Option<String>,
}

impl fmt::Debug for TextProcessingRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TextProcessingRequest")
            .field("session_id", &self.session_id)
            .field("request_id", &self.request_id)
            .field("processing_mode", &self.processing_mode)
            .field("raw_transcript", &"[REDACTED]")
            .field("selected_text_present", &self.selected_text.is_some())
            .finish()
    }
}

/// Non-secret LLM settings proposed for a future Session.
///
/// The caller represents incomplete ordinary settings as `None` when resolving
/// a route. No API key or user-editable Provider identifier is permitted in
/// this value.
#[derive(Clone, Eq, PartialEq)]
pub struct LlmRouteCandidate {
    base_url: String,
    model: String,
}

impl LlmRouteCandidate {
    #[must_use]
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
        }
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}

impl fmt::Debug for LlmRouteCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmRouteCandidate")
            .field("base_url", &"[REDACTED]")
            .field("model", &"[REDACTED]")
            .finish()
    }
}

/// Immutable, non-secret Provider route frozen for one Session.
///
/// The endpoint is already normalized by the concrete Provider Adapter. The
/// logical secret ID binds the API key to that normalized endpoint but never
/// contains the key itself. This value stays inside the Rust application
/// boundary and is deliberately absent from Renderer and Worker DTOs.
#[derive(Clone, Eq, PartialEq)]
pub struct ResolvedLlmRoute {
    provider_ref: String,
    endpoint: String,
    model: String,
    secret_id: String,
}

impl ResolvedLlmRoute {
    /// Creates a route from parts already validated by a Provider Adapter.
    ///
    /// Concrete adapters must validate the parts again at the irreversible
    /// network boundary instead of treating construction as a security proof.
    #[must_use]
    pub fn new(
        provider_ref: impl Into<String>,
        endpoint: impl Into<String>,
        model: impl Into<String>,
        secret_id: impl Into<String>,
    ) -> Self {
        Self {
            provider_ref: provider_ref.into(),
            endpoint: endpoint.into(),
            model: model.into(),
            secret_id: secret_id.into(),
        }
    }

    #[must_use]
    pub fn provider_ref(&self) -> &str {
        &self.provider_ref
    }

    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    pub fn secret_id(&self) -> &str {
        &self.secret_id
    }
}

impl fmt::Debug for ResolvedLlmRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedLlmRoute")
            .field("provider_ref", &"[REDACTED]")
            .field("endpoint", &"[REDACTED]")
            .field("model", &"[REDACTED]")
            .field("secret_id", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LlmRouteResolution {
    NoConfiguration,
    MissingSecret(ResolvedLlmRoute),
    Ready(ResolvedLlmRoute),
    /// The candidate could not become ready.
    ///
    /// `route` is retained only when the non-secret endpoint and model were
    /// successfully validated before SecretStore failed. Controllers need that
    /// logical secret ID to distinguish recoverable encrypted material from an
    /// ordinary infrastructure outage. Invalid URLs never receive a route.
    Unavailable {
        route: Option<ResolvedLlmRoute>,
        error: PortError,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub struct TextProcessingResult {
    pub session_id: SessionId,
    pub request_id: RequestId,
    pub intent: IntentDecision,
    pub final_text: String,
}

impl fmt::Debug for TextProcessingResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TextProcessingResult")
            .field("session_id", &self.session_id)
            .field("request_id", &self.request_id)
            .field("intent", &self.intent)
            .field("final_text", &"[REDACTED]")
            .finish()
    }
}

/// Bounded upstream response returned only by an explicit ControlPanel
/// connection probe.
///
/// This value must never enter the formal text-processing result, logs,
/// history, settings, AppSnapshot, or cross-window events. Concrete adapters
/// are responsible for removing credential echoes before construction.
#[derive(Clone, Eq, PartialEq)]
pub struct LlmUpstreamError {
    http_status: u16,
    response_body: String,
    truncated: bool,
}

impl LlmUpstreamError {
    #[must_use]
    pub fn new(http_status: u16, response_body: impl Into<String>, truncated: bool) -> Self {
        Self {
            http_status,
            response_body: response_body.into(),
            truncated,
        }
    }

    #[must_use]
    pub const fn http_status(&self) -> u16 {
        self.http_status
    }

    #[must_use]
    pub fn response_body(&self) -> &str {
        &self.response_body
    }

    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

impl fmt::Debug for LlmUpstreamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmUpstreamError")
            .field("http_status", &self.http_status)
            .field("response_body", &"[REDACTED]")
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// Failure envelope for the explicit connection probe.
///
/// The stable [`PortError`] remains suitable for ordinary control flow. The
/// optional upstream response is a transient diagnostic projection and its
/// custom `Debug` implementation prevents accidental body logging.
#[derive(Clone, Eq, PartialEq)]
pub struct LlmConnectionProbeError {
    pub error: PortError,
    pub upstream: Option<LlmUpstreamError>,
}

impl LlmConnectionProbeError {
    #[must_use]
    pub fn from_port(error: PortError) -> Self {
        Self {
            error,
            upstream: None,
        }
    }

    #[must_use]
    pub fn with_upstream(error: PortError, upstream: LlmUpstreamError) -> Self {
        Self {
            error,
            upstream: Some(upstream),
        }
    }

    #[must_use]
    pub fn into_port_error(self) -> PortError {
        self.error
    }
}

impl fmt::Debug for LlmConnectionProbeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmConnectionProbeError")
            .field("error", &self.error)
            .field("upstream", &self.upstream)
            .finish()
    }
}

pub trait LlmProvider: Send + Sync {
    /// Resolves and authenticates a non-secret candidate before a Session
    /// freezes its route.
    ///
    /// `Ready` proves that the endpoint is valid and the bound secret can be
    /// authenticated now. It does not perform a network request. Raw mode must
    /// bypass this method entirely.
    fn resolve_route(
        &self,
        candidate: Option<LlmRouteCandidate>,
    ) -> PortFuture<'_, LlmRouteResolution>;

    fn process(
        &self,
        route: ResolvedLlmRoute,
        request: TextProcessingRequest,
    ) -> PortFuture<'_, Result<TextProcessingResult, PortError>>;

    /// Performs the fixed, user-triggered ControlPanel connection probe.
    ///
    /// The default keeps test doubles and unavailable adapters content-free.
    /// A concrete network adapter may override this method to attach a
    /// bounded, credential-redacted upstream HTTP response. Formal Session
    /// processing must continue to use [`Self::process`].
    fn probe_connection(
        &self,
        route: ResolvedLlmRoute,
        request: TextProcessingRequest,
    ) -> PortFuture<'_, Result<TextProcessingResult, LlmConnectionProbeError>> {
        Box::pin(async move {
            self.process(route, request)
                .await
                .map_err(LlmConnectionProbeError::from_port)
        })
    }

    fn cancel(&self, request_id: RequestId) -> PortFuture<'_, Result<(), PortError>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InsertOutcome {
    Inserted,
    NotInserted,
    Indeterminate,
}

/// Result of a user-directed compatibility paste.
///
/// Unlike [`InsertOutcome::Inserted`], `Dispatched` proves only that the
/// operating-system paste shortcut was posted to the keyboard focus selected
/// by the user. Targets without an accessible text element cannot provide
/// content or caret evidence after the event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserDirectedPasteOutcome {
    Dispatched,
    NotDispatched,
    Indeterminate,
}

pub trait OutputAdapter: Send + Sync {
    /// Inserts at an already revalidated anchor. `target` must remain bound to
    /// the original target identity and exact insertion anchor; it is not a
    /// transferable permission to whichever field later gains focus. The
    /// adapter must revalidate that binding at the real OS write point, acquire
    /// `lifecycle.begin_commit()` immediately before writing, and hold the
    /// resulting guard until the write has completed. `Err` and `NotInserted`
    /// both mean no text was committed; if the adapter cannot prove that, it
    /// must return `Indeterminate`. No replace or delete operation exists by
    /// design.
    fn insert(
        &self,
        target: ValidatedTargetRef,
        text: String,
        delivery_id: DeliveryId,
        lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<InsertOutcome, PortError>>;
}

pub trait ClipboardBridge: Send + Sync {
    fn read_selected_text(
        &self,
        target: &TargetSnapshotRef,
    ) -> PortFuture<'_, Result<SelectionSnapshot, PortError>>;
    /// Uses the clipboard only as an implementation mechanism for inserting at
    /// the original validated target and exact anchor. The adapter must
    /// revalidate that binding at the real OS write point, acquire
    /// `lifecycle.begin_commit()` immediately before the first irreversible
    /// clipboard or input mutation, and hold the guard through insertion and
    /// clipboard restoration.
    fn insert_and_restore(
        &self,
        target: ValidatedTargetRef,
        text: String,
        delivery_id: DeliveryId,
        lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<InsertOutcome, PortError>>;

    /// Temporarily stages `text`, posts one paste shortcut to the keyboard
    /// focus chosen by the user at the real dispatch boundary, then restores
    /// the prior clipboard when possible.
    ///
    /// This compatibility path deliberately carries no target or caret proof.
    /// Implementations must still serialize transactions, honor the lifecycle
    /// fence, and make `delivery_id` single-use across both clipboard insertion
    /// methods. A successful return must not be described as verified content
    /// insertion; it proves only that the shortcut was dispatched once.
    fn insert_at_current_focus_and_restore(
        &self,
        text: String,
        delivery_id: DeliveryId,
        lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<UserDirectedPasteOutcome, PortError>>;
}

/// Writes user-selected text to the system clipboard without dispatching a
/// paste shortcut, reading existing clipboard content, or restoring it later.
///
/// This Port is intentionally separate from [`ClipboardBridge`]. It represents
/// the explicit “复制全部” action in the temporary-text surface, not a delivery
/// fallback, and therefore must never synthesize input or claim that text was
/// inserted into another application.
pub trait ClipboardTextWriter: Send + Sync {
    fn write_text(&self, text: String) -> PortFuture<'_, Result<(), PortError>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TemporaryTextStatus {
    NotInserted,
    Indeterminate,
    LlmFallback,
}

pub trait TemporaryTextOutput: Send + Sync {
    /// Creates the one-time fallback surface. `status` must be preserved so the
    /// UI can warn when the original insertion result was indeterminate.
    /// Returning `Err` means no visible fallback surface was committed.
    fn show(
        &self,
        session_id: SessionId,
        delivery_id: DeliveryId,
        final_text: String,
        status: TemporaryTextStatus,
        lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<(), PortError>>;
}

pub trait SettingsStore: Send + Sync {
    fn load(&self) -> PortFuture<'_, Result<SettingsSnapshot, PortError>>;
    fn replace(
        &self,
        expected_version: u64,
        settings: SettingsSnapshot,
    ) -> PortFuture<'_, Result<SettingsSnapshot, PortError>>;
}

/// 在操作系统中替换录音快捷键绑定。
///
/// 实现必须具备事务语义：返回 `Ok` 时 `next` 已成为唯一生效绑定；返回 `Err`
/// 时应尽力保持 `current` 仍然生效。持久化若随后失败，Application 会用反向调用
/// 恢复旧绑定。
pub trait RecordingShortcutPort: Send + Sync {
    fn replace_binding(
        &self,
        current: Option<RecordingShortcut>,
        next: Option<RecordingShortcut>,
    ) -> PortFuture<'_, Result<(), PortError>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryRecord {
    pub delivery_id: DeliveryId,
    pub final_text: String,
    pub created_at: TimestampMs,
}

pub trait HistoryStore: Send + Sync {
    /// Persists one final record and applies the frozen Session policy in one
    /// adapter transaction.
    ///
    /// Implementations must hold one storage lock, acquire one lifecycle
    /// commit guard, and cross one irreversible commit point. Saving first and
    /// trimming later would let shutdown strand data beyond the configured
    /// limit.
    fn save_with_policy(
        &self,
        record: HistoryRecord,
        settings: &SettingsSnapshot,
        lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<(), PortError>>;
    fn list(&self) -> PortFuture<'_, Result<Vec<HistoryRecord>, PortError>>;
    fn clear_all(&self) -> PortFuture<'_, Result<(), PortError>>;
    fn enforce_policy(
        &self,
        settings: &SettingsSnapshot,
        lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<(), PortError>>;
}

pub struct SecretValue(Zeroizing<String>);

impl SecretValue {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue([REDACTED])")
    }
}

/// Authenticated state of one logical secret.
///
/// `Configured` proves that the current encrypted record can be authenticated
/// and decoded with the current master key. `RecoveryRequired` is reserved for
/// persisted material that is deterministically unreadable and may therefore
/// be cleared only by an explicit destructive recovery action. Ordinary I/O,
/// locking, permission, or storage availability failures remain `PortError`
/// instead of being mislabeled as recoverable data loss.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretMaterialState {
    NotConfigured,
    Configured,
    RecoveryRequired,
}

pub trait SecretStore: Send + Sync {
    /// Reports whether a persisted record exists.
    ///
    /// This legacy existence probe is deliberately not a health claim and must
    /// never be used to derive LLM readiness. Use [`Self::inspect`] instead.
    fn is_configured(&self, secret_id: &str) -> PortFuture<'_, Result<bool, PortError>>;

    /// Authenticates the current material without exposing its plaintext.
    ///
    /// The fail-closed default performs a real `read`, which is sufficient for
    /// stores that do not support destructive recovery. Stores with encrypted
    /// persisted material should override this method so deterministic
    /// authentication failures can become `RecoveryRequired`.
    fn inspect(&self, secret_id: &str) -> PortFuture<'_, Result<SecretMaterialState, PortError>> {
        let secret_id = secret_id.to_owned();
        Box::pin(async move {
            self.read(&secret_id).await.map(|value| {
                value.map_or(SecretMaterialState::NotConfigured, |_| {
                    SecretMaterialState::Configured
                })
            })
        })
    }

    /// Authenticates the encrypted store as a whole.
    ///
    /// This catches orphaned ciphertext whose former endpoint is no longer in
    /// ordinary settings. A global recovery action must be based on this state,
    /// never solely on whether the currently configured secret ID exists.
    fn inspect_store(&self) -> PortFuture<'_, Result<SecretMaterialState, PortError>> {
        Box::pin(async {
            Err(PortError {
                code: "secret.store_inspection_unsupported".to_owned(),
                safe_message_key: "errors.secret.store_inspection_unsupported".to_owned(),
                retryable: false,
            })
        })
    }

    fn read(&self, secret_id: &str) -> PortFuture<'_, Result<Option<SecretValue>, PortError>>;
    fn replace(&self, secret_id: &str, value: SecretValue)
    -> PortFuture<'_, Result<(), PortError>>;

    /// Atomically converges one internal namespace to exactly one secret.
    ///
    /// Implementations must not emulate this as `delete_namespace` followed by
    /// `replace`: losing power or failing between those calls would discard the
    /// old credential. The fail-closed default forces stores to provide a real
    /// atomic implementation before callers may use this capability.
    fn replace_namespace(
        &self,
        _namespace: &str,
        _secret_id: &str,
        _value: SecretValue,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async {
            Err(PortError {
                code: "secret.namespace_replace_unsupported".to_owned(),
                safe_message_key: "errors.secret.namespace_replace_unsupported".to_owned(),
                retryable: false,
            })
        })
    }

    fn delete(&self, secret_id: &str) -> PortFuture<'_, Result<(), PortError>>;

    /// Deletes every secret in one internal namespace.
    ///
    /// Renderer IPC must never supply this prefix. The V1 LLM controller uses
    /// its fixed namespace so deleting the single global Provider credential
    /// also removes endpoint-scoped leftovers.
    fn delete_namespace(&self, _namespace: &str) -> PortFuture<'_, Result<u64, PortError>> {
        Box::pin(async {
            Err(PortError {
                code: "secret.namespace_delete_unsupported".to_owned(),
                safe_message_key: "errors.secret.namespace_delete_unsupported".to_owned(),
                retryable: false,
            })
        })
    }

    /// Clears every encrypted record only after the implementation proves that
    /// `secret_id` is currently unrecoverable.
    ///
    /// The default is intentionally unsupported so test doubles and platform
    /// stores cannot accidentally turn a transient failure into data loss.
    fn reset_unrecoverable(&self, _secret_id: &str) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async {
            Err(PortError {
                code: "secret.reset_unsupported".to_owned(),
                safe_message_key: "errors.secret.reset_unsupported".to_owned(),
                retryable: false,
            })
        })
    }

    /// Clears all encrypted records only after a store-wide authentication
    /// pass proves that at least one existing record is unrecoverable and no
    /// ordinary availability/incompatible-version error is present.
    fn reset_unrecoverable_store(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async {
            Err(PortError {
                code: "secret.reset_unsupported".to_owned(),
                safe_message_key: "errors.secret.reset_unsupported".to_owned(),
                retryable: false,
            })
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelHealth {
    Unknown,
    Healthy,
    Unhealthy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelReadiness {
    pub engine: AsrEngine,
    pub logical_model_id: String,
    pub version: String,
    pub health: ModelHealth,
}

pub trait ModelManager: Send + Sync {
    fn active_models(&self) -> PortFuture<'_, Result<Vec<ModelReadiness>, PortError>>;
    fn begin_managed_update(
        &self,
        logical_model_id: &str,
    ) -> PortFuture<'_, Result<ModelOperationId, PortError>>;
    fn cancel_update(
        &self,
        operation_id: ModelOperationId,
    ) -> PortFuture<'_, Result<(), PortError>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticEvent {
    pub session_id: Option<SessionId>,
    pub phase: Option<String>,
    pub state: Option<String>,
    pub duration_ms: Option<u64>,
    pub error_code: Option<String>,
    pub detail: Option<String>,
}

pub trait DiagnosticsSink: Send + Sync {
    fn record(&self, event: DiagnosticEvent);
}

/// Runtime control for the local, content-free diagnostics sink.
///
/// This is separate from [`DiagnosticsSink`] so Application can change the
/// persisted user preference without learning where or how logs are stored.
pub trait DiagnosticsControl: Send + Sync {
    fn enabled(&self) -> bool;
    fn set_enabled(&self, enabled: bool);
}

pub trait Clock: Send + Sync {
    fn now(&self) -> TimestampMs;
}

pub trait IdGenerator: Send + Sync {
    fn session_id(&self) -> SessionId;
    fn request_id(&self) -> RequestId;
    fn delivery_id(&self) -> DeliveryId;
}

#[cfg(test)]
mod tests {
    use remtene_domain::{IntentDecision, ProcessingMode, RequestId, SessionId};

    use super::{SecretValue, TextProcessingRequest, TextProcessingResult};

    #[test]
    fn secret_value_debug_does_not_expose_plaintext() {
        let plaintext = "sensitive-test-secret";
        let debug_output = format!("{:?}", SecretValue::new(plaintext));

        assert_eq!(debug_output, "SecretValue([REDACTED])");
        assert!(!debug_output.contains(plaintext));
    }

    #[test]
    fn llm_content_debug_projections_are_redacted() {
        let request = TextProcessingRequest {
            session_id: SessionId::new(),
            request_id: RequestId::new(),
            processing_mode: ProcessingMode::Faithful,
            raw_transcript: "private transcript marker".to_owned(),
            selected_text: Some("private selection marker".to_owned()),
        };
        let result = TextProcessingResult {
            session_id: request.session_id,
            request_id: request.request_id,
            intent: IntentDecision::Dictation,
            final_text: "private final marker".to_owned(),
        };

        let request_debug = format!("{request:?}");
        let result_debug = format!("{result:?}");
        for forbidden in [
            "private transcript marker",
            "private selection marker",
            "private final marker",
        ] {
            assert!(!request_debug.contains(forbidden));
            assert!(!result_debug.contains(forbidden));
        }
        assert!(request_debug.contains("selected_text_present: true"));
    }
}
