use std::{
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

use remtene_application::FinishOutcome;
use remtene_application::ports::{
    PortError, PortFuture, RecordingHudPort, RecordingHudState, TargetDisplayHint,
};
use remtene_contracts::{
    AppError, CONTRACT_VERSION, CommandAccepted, ErrorCategory, ErrorSeverity, SessionCommand,
    SessionPhaseView, SessionPublicSnapshot, SessionTerminalEvent, SessionTerminalOutcomeView,
    SessionUserState,
};
use remtene_domain::{SessionId, TerminalOutcome};
use tauri::{
    App, AppHandle, Emitter, EventTarget, Manager, PhysicalPosition, State, WebviewUrl,
    WebviewWindow, WebviewWindowBuilder,
};

use crate::session_projection::{failure_error_code, reject_error_code};
use crate::{CONTROL_PANEL_LABEL, RECORDING_HUD_LABEL, WindowCommandClass, authorize_window};

#[cfg(target_os = "macos")]
mod macos_panel;

const SESSION_STATE_CHANGED_EVENT: &str = "session:state-changed";
/// 兼容的会话结束通知。只表示 HUD 生命周期已收敛。
const SESSION_ENDED_EVENT: &str = "session:ended";
/// Domain 已确认终态后发布的无内容业务结果。
const SESSION_TERMINAL_EVENT: &str = "session:terminal";

#[derive(Clone, serde::Serialize)]
struct SessionEnded {
    contract_version: u16,
    session_id: String,
}
const HUD_WIDTH_LOGICAL: f64 = 144.0;
const HUD_HEIGHT_LOGICAL: f64 = 40.0;
const HUD_CORNER_RADIUS_LOGICAL: f64 = HUD_HEIGHT_LOGICAL / 2.0;
const HUD_EDGE_MARGIN_LOGICAL: f64 = 24.0;
const HUD_COMPLETED_HOLD_DURATION: Duration = Duration::from_millis(240);
const HUD_NATIVE_HIDE_DELAY: Duration = Duration::from_millis(240);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingHudIntentKind {
    Finish,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordingHudIntent {
    pub kind: RecordingHudIntentKind,
    pub command: SessionCommand,
}

#[derive(Default)]
struct RecordingHudRuntime {
    current: Option<SessionPublicSnapshot>,
    pending_intent: Option<RecordingHudIntent>,
    recording_started_at: Option<Instant>,
    recording_limit: Option<Duration>,
    visibility_generation: u64,
}

/// Tauri-backed Presentation adapter for the session-scoped Recording HUD.
///
/// The controller stores only the content-free public projection. Audio,
/// transcript text, target identity, selections, paths, and secrets never
/// enter this boundary.
pub struct RecordingHudController {
    app: AppHandle,
    runtime: Mutex<RecordingHudRuntime>,
}

impl RecordingHudController {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            runtime: Mutex::new(RecordingHudRuntime::default()),
        }
    }

    pub fn current(&self) -> Option<SessionPublicSnapshot> {
        let runtime = self.runtime();
        runtime.current.as_ref().map(|current| {
            let mut snapshot = current.clone();
            if matches!(
                snapshot.user_state,
                SessionUserState::Preparing | SessionUserState::Recording
            ) {
                snapshot.recording_elapsed_ms = runtime.recording_started_at.map(elapsed_millis);
            }
            snapshot.recording_limit_ms = runtime.recording_limit.map(duration_millis);
            snapshot
        })
    }

    fn runtime(&self) -> MutexGuard<'_, RecordingHudRuntime> {
        self.runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn window(&self) -> Result<WebviewWindow, PortError> {
        self.app
            .get_webview_window(RECORDING_HUD_LABEL)
            .ok_or_else(|| hud_port_error("recording_hud.window_missing", false))
    }

    fn show_state(
        &self,
        session_id: SessionId,
        state: RecordingHudState,
        display_hint: Option<TargetDisplayHint>,
        recording_limit: Duration,
    ) -> Result<(), PortError> {
        let snapshot = session_snapshot(session_id, state, Some(0), Some(recording_limit));
        let mut runtime = self.runtime();
        if runtime.current.is_some() {
            return Err(hud_port_error(
                "recording_hud.session_already_visible",
                false,
            ));
        }

        let window = self.window()?;
        position_on_target_monitor(&window, display_hint)?;
        runtime.visibility_generation = runtime.visibility_generation.wrapping_add(1);
        runtime.current = Some(snapshot.clone());
        runtime.pending_intent = None;
        runtime.recording_started_at = Some(Instant::now());
        runtime.recording_limit = Some(recording_limit);
        if let Err(error) = self.emit_state(snapshot) {
            runtime.current = None;
            runtime.recording_started_at = None;
            runtime.recording_limit = None;
            return Err(error);
        }
        if window.show().is_err() {
            runtime.current = None;
            return Err(hud_port_error("recording_hud.show_failed", true));
        }
        Ok(())
    }

    fn update_state(
        &self,
        session_id: SessionId,
        state: RecordingHudState,
    ) -> Result<(), PortError> {
        let mut runtime = self.runtime();
        let Some(current) = runtime.current.as_ref() else {
            return Err(hud_port_error("recording_hud.session_missing", false));
        };
        if current.session_id != session_id.as_uuid() {
            return Err(hud_port_error("recording_hud.stale_session", false));
        }
        if !hud_transition_allowed(current.phase, state) {
            return Err(hud_port_error("recording_hud.state_regression", false));
        }
        let elapsed = runtime.recording_started_at.map(elapsed_millis);
        let snapshot = session_snapshot(session_id, state, elapsed, runtime.recording_limit);
        self.emit_state(snapshot.clone())?;
        runtime.current = Some(snapshot);
        if !matches!(
            state,
            RecordingHudState::Preparing | RecordingHudState::Recording
        ) {
            runtime.pending_intent = None;
        }
        Ok(())
    }

    async fn hide_session(&self, session_id: SessionId) -> Result<(), PortError> {
        let (visibility_generation, show_completed_feedback) = {
            let mut runtime = self.runtime();
            let Some(current) = runtime.current.as_ref() else {
                return Ok(());
            };
            if current.session_id != session_id.as_uuid() {
                return Ok(());
            }
            let show_completed_feedback = current.user_state == SessionUserState::Completed
                && current.phase == SessionPhaseView::Terminated;
            runtime.visibility_generation = runtime.visibility_generation.wrapping_add(1);
            let visibility_generation = runtime.visibility_generation;
            runtime.current = None;
            runtime.pending_intent = None;
            runtime.recording_started_at = None;
            runtime.recording_limit = None;
            (visibility_generation, show_completed_feedback)
        };

        let window = self.window()?;
        // 控制面板立即收敛业务状态；HUD 的视觉退场由下方独立安排，不能反向阻塞 Core。
        self.emit_session_ended_to(session_id, CONTROL_PANEL_LABEL);

        if show_completed_feedback {
            wait_for_hud_motion(HUD_COMPLETED_HOLD_DURATION).await?;
            if !self.native_hide_is_current(visibility_generation) {
                return Ok(());
            }
        }

        // Renderer 收到逻辑结束事件后播放既有的 210ms 退场。原生窗口多保留一个
        // 合成余量，避免在 WebView 尚未画完最后一帧时直接 orderOut。
        self.emit_session_ended_to(session_id, RECORDING_HUD_LABEL);
        wait_for_hud_motion(HUD_NATIVE_HIDE_DELAY).await?;

        // 新会话会推进代次。检查和 hide 在同一把锁下完成，确保旧任务永远不能
        // 在检查后、真正隐藏前误伤刚显示的新 HUD。
        let runtime = self.runtime();
        if !native_hide_is_current(&runtime, visibility_generation) {
            return Ok(());
        }
        window
            .hide()
            .map_err(|_| hud_port_error("recording_hud.hide_failed", true))?;
        Ok(())
    }

    fn native_hide_is_current(&self, visibility_generation: u64) -> bool {
        native_hide_is_current(&self.runtime(), visibility_generation)
    }

    fn emit_state(&self, snapshot: SessionPublicSnapshot) -> Result<(), PortError> {
        // 控制面板与 HUD 是同一个会话的两个视图，必须看到同一份状态：任一处提交后，
        // 另一处的按钮都要跟着变。快照本身不含转录文本、目标身份或选区，
        // 与控制面板既有的 `app_get_snapshot` 数据面等价，不扩大边界。
        //
        // 控制面板只是镜像视图，投递失败不得中断录音——HUD 才是本次会话的权威可见面。
        let _ = self.app.emit_to(
            EventTarget::webview_window(CONTROL_PANEL_LABEL),
            SESSION_STATE_CHANGED_EVENT,
            snapshot.clone(),
        );
        self.app
            .emit_to(
                EventTarget::webview_window(RECORDING_HUD_LABEL),
                SESSION_STATE_CHANGED_EVENT,
                snapshot,
            )
            .map_err(|_error| {
                #[cfg(debug_assertions)]
                eprintln!("recording HUD state event failed: {_error}");
                hud_port_error("recording_hud.state_event_failed", true)
            })
    }

    /// 会话逻辑结束时通知指定窗口；载荷不包含转录内容或失败细节。
    ///
    /// 控制面板用它收敛活动状态；HUD 用同一事件启动视觉退场。它不承诺原生窗口
    /// 已经隐藏，真正的 orderOut 会在动画和合成余量结束后发生。
    fn emit_session_ended_to(&self, session_id: SessionId, window_label: &'static str) {
        let _ = self.app.emit_to(
            EventTarget::webview_window(window_label),
            SESSION_ENDED_EVENT,
            SessionEnded {
                contract_version: CONTRACT_VERSION,
                session_id: session_id.as_uuid().to_string(),
            },
        );
    }

    fn emit_session_terminal(
        &self,
        session_id: SessionId,
        outcome: TerminalOutcome,
    ) -> Result<(), PortError> {
        self.app
            .emit_to(
                EventTarget::webview_window(CONTROL_PANEL_LABEL),
                SESSION_TERMINAL_EVENT,
                session_terminal(session_id, outcome),
            )
            .map_err(|_error| {
                #[cfg(debug_assertions)]
                eprintln!("session terminal event failed: {_error}");
                hud_port_error("recording_hud.terminal_event_failed", true)
            })
    }

    fn accept_intent(
        &self,
        command: SessionCommand,
        kind: RecordingHudIntentKind,
    ) -> Result<IntentAcceptance, AppError> {
        accept_runtime_intent(&mut self.runtime(), command, kind)
    }
}

async fn wait_for_hud_motion(duration: Duration) -> Result<(), PortError> {
    tauri::async_runtime::spawn_blocking(move || std::thread::sleep(duration))
        .await
        .map_err(|_| hud_port_error("recording_hud.motion_wait_failed", true))
}

fn native_hide_is_current(runtime: &RecordingHudRuntime, visibility_generation: u64) -> bool {
    runtime.current.is_none() && runtime.visibility_generation == visibility_generation
}

/// Result of validating one HUD intent.
///
/// `dispatch` is true only for the first acceptance of an intent, so an exact retry of the
/// same command stays idempotent and cannot start a second end-to-end task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IntentAcceptance {
    accepted: CommandAccepted,
    dispatch: bool,
}

fn accept_runtime_intent(
    runtime: &mut RecordingHudRuntime,
    command: SessionCommand,
    kind: RecordingHudIntentKind,
) -> Result<IntentAcceptance, AppError> {
    if command.contract_version != CONTRACT_VERSION {
        return Err(command_error(
            "ipc.contract_version_mismatch",
            ErrorCategory::Security,
            false,
        ));
    }

    let Some(current) = runtime.current.as_ref() else {
        return Err(command_error(
            "recording_hud.session_missing",
            ErrorCategory::Lifecycle,
            false,
        ));
    };
    if current.session_id != command.session_id {
        return Err(command_error(
            "recording_hud.command_not_recording",
            ErrorCategory::Lifecycle,
            false,
        ));
    }
    let allowed = match kind {
        RecordingHudIntentKind::Finish => {
            current.user_state == SessionUserState::Recording
                && current.phase == SessionPhaseView::Recording
                && current.can_finish
        }
        RecordingHudIntentKind::Cancel => {
            matches!(
                current.user_state,
                SessionUserState::Preparing | SessionUserState::Recording
            ) && matches!(
                current.phase,
                SessionPhaseView::Preparing | SessionPhaseView::Recording
            ) && current.can_cancel
        }
    };
    if !allowed {
        return Err(command_error(
            "recording_hud.command_not_allowed",
            ErrorCategory::Lifecycle,
            false,
        ));
    }

    let intent = RecordingHudIntent { kind, command };
    let dispatch = match runtime.pending_intent {
        Some(pending) if pending == intent => false,
        Some(_) => {
            return Err(command_error(
                "recording_hud.command_already_pending",
                ErrorCategory::Lifecycle,
                true,
            ));
        }
        None => {
            runtime.pending_intent = Some(intent);
            true
        }
    };

    Ok(IntentAcceptance {
        accepted: CommandAccepted {
            contract_version: CONTRACT_VERSION,
            request_id: command.request_id,
        },
        dispatch,
    })
}

impl RecordingHudPort for RecordingHudController {
    fn show(
        &self,
        session_id: SessionId,
        state: RecordingHudState,
        display_hint: Option<TargetDisplayHint>,
        recording_limit: Duration,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move { self.show_state(session_id, state, display_hint, recording_limit) })
    }

    fn update(
        &self,
        session_id: SessionId,
        state: RecordingHudState,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move { self.update_state(session_id, state) })
    }

    fn publish_terminal(
        &self,
        session_id: SessionId,
        outcome: TerminalOutcome,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move { self.emit_session_terminal(session_id, outcome) })
    }

    fn hide(&self, session_id: SessionId) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move { self.hide_session(session_id).await })
    }
}

pub(crate) fn build_recording_hud(app: &mut App) -> tauri::Result<()> {
    let window = WebviewWindowBuilder::new(
        app,
        RECORDING_HUD_LABEL,
        WebviewUrl::App("index.html?surface=recording-hud".into()),
    )
    .title("辑语 · 录音")
    .inner_size(HUD_WIDTH_LOGICAL, HUD_HEIGHT_LOGICAL)
    .min_inner_size(HUD_WIDTH_LOGICAL, HUD_HEIGHT_LOGICAL)
    .max_inner_size(HUD_WIDTH_LOGICAL, HUD_HEIGHT_LOGICAL)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .closable(false)
    .decorations(false)
    // NSPanel 透明只处理原生窗口；退出动画缩小并下沉胶囊时还会露出
    // WKWebView 本身，因此两层都必须在创建时启用透明背景。
    .transparent(true)
    .shadow(false)
    .always_on_top(true)
    .visible_on_all_workspaces(true)
    .skip_taskbar(true)
    .focused(false)
    .focusable(false)
    .accept_first_mouse(true)
    .visible(false)
    .build()?;

    #[cfg(target_os = "macos")]
    macos_panel::configure(&window, HUD_CORNER_RADIUS_LOGICAL)?;

    Ok(())
}

#[cfg(debug_assertions)]
pub(crate) fn show_debug_preview(controller: &RecordingHudController) -> Result<(), PortError> {
    if std::env::var("REMTENE_RECORDING_HUD_PREVIEW").as_deref() != Ok("recording") {
        return Ok(());
    }

    controller.show_state(
        SessionId::new(),
        RecordingHudState::Recording,
        None,
        Duration::from_secs(600),
    )
}

#[tauri::command]
pub(crate) fn recording_hud_get_state(
    window: WebviewWindow,
    controller: State<'_, std::sync::Arc<RecordingHudController>>,
) -> Result<Option<SessionPublicSnapshot>, AppError> {
    authorize_window(window.label(), WindowCommandClass::RecordingHudState)?;
    Ok(controller.current())
}

#[tauri::command]
pub(crate) fn recording_finish(
    window: WebviewWindow,
    app: AppHandle,
    controller: State<'_, std::sync::Arc<RecordingHudController>>,
    command: SessionCommand,
) -> Result<CommandAccepted, AppError> {
    authorize_window(window.label(), WindowCommandClass::RecordingControl)?;
    let acceptance = controller.accept_intent(command, RecordingHudIntentKind::Finish)?;
    if acceptance.dispatch {
        dispatch_intent(&app, command.session_id, RecordingHudIntentKind::Finish)?;
    }
    Ok(acceptance.accepted)
}

#[tauri::command]
pub(crate) fn recording_cancel(
    window: WebviewWindow,
    app: AppHandle,
    controller: State<'_, std::sync::Arc<RecordingHudController>>,
    command: SessionCommand,
) -> Result<CommandAccepted, AppError> {
    authorize_window(window.label(), WindowCommandClass::RecordingControl)?;
    let acceptance = controller.accept_intent(command, RecordingHudIntentKind::Cancel)?;
    if acceptance.dispatch {
        dispatch_intent(&app, command.session_id, RecordingHudIntentKind::Cancel)?;
    }
    Ok(acceptance.accepted)
}

/// Hand the validated intent to the single Application use case.
///
/// The command answers as soon as the intent is accepted; ASR, delivery and cleanup keep
/// running on the async runtime so the HUD never blocks on a full transcription.
fn dispatch_intent(
    app: &AppHandle,
    session_id: uuid::Uuid,
    kind: RecordingHudIntentKind,
) -> Result<(), AppError> {
    let Some(root) = app.try_state::<crate::composition_root::CompositionRoot>() else {
        return Err(command_error(
            "recording_hud.orchestrator_unavailable",
            ErrorCategory::Lifecycle,
            false,
        ));
    };
    let orchestrator = std::sync::Arc::clone(&root.orchestrator);
    let session_id = SessionId::from_uuid(session_id);
    tauri::async_runtime::spawn(async move {
        match kind {
            RecordingHudIntentKind::Finish => {
                report_async_finish_outcome(orchestrator.finish_recording(session_id).await);
            }
            RecordingHudIntentKind::Cancel => {
                if let Err(error) = orchestrator.cancel_recording(session_id).await {
                    eprintln!("session cancel failed: {error}");
                }
            }
        }
    });
    Ok(())
}

fn report_async_finish_outcome(
    result: Result<FinishOutcome, remtene_application::OrchestratorError>,
) {
    match result {
        Ok(FinishOutcome::Failed(category)) => {
            eprintln!("session finish failed: {}", failure_error_code(category));
        }
        Ok(
            FinishOutcome::Completed(_)
            | FinishOutcome::NoSpeech
            | FinishOutcome::Discarded
            | FinishOutcome::NotRecording,
        ) => {}
        Err(error) => eprintln!("session finish failed: {error}"),
    }
}

fn session_snapshot(
    session_id: SessionId,
    state: RecordingHudState,
    recording_elapsed_ms: Option<u64>,
    recording_limit: Option<Duration>,
) -> SessionPublicSnapshot {
    let (user_state, phase, can_finish, can_cancel, status_code) = match state {
        RecordingHudState::Preparing => (
            SessionUserState::Preparing,
            SessionPhaseView::Preparing,
            false,
            true,
            "session.preparing",
        ),
        RecordingHudState::Recording => (
            SessionUserState::Recording,
            SessionPhaseView::Recording,
            true,
            true,
            "session.recording",
        ),
        RecordingHudState::Recognizing => (
            SessionUserState::Processing,
            SessionPhaseView::Recognizing,
            false,
            false,
            "session.recognizing",
        ),
        RecordingHudState::Processing => (
            SessionUserState::Processing,
            SessionPhaseView::Processing,
            false,
            false,
            "session.processing",
        ),
        RecordingHudState::Delivering => (
            SessionUserState::Processing,
            SessionPhaseView::Delivering,
            false,
            false,
            "session.delivering",
        ),
        RecordingHudState::Finalizing => (
            SessionUserState::Processing,
            SessionPhaseView::Finalizing,
            false,
            false,
            "session.finalizing",
        ),
        RecordingHudState::Completed => (
            SessionUserState::Completed,
            SessionPhaseView::Terminated,
            false,
            false,
            "session.completed",
        ),
    };
    SessionPublicSnapshot {
        contract_version: CONTRACT_VERSION,
        session_id: session_id.as_uuid(),
        user_state,
        phase,
        recording_elapsed_ms,
        recording_limit_ms: recording_limit.map(duration_millis),
        can_finish,
        can_cancel,
        status_code: status_code.to_owned(),
    }
}

fn session_terminal(session_id: SessionId, terminal: TerminalOutcome) -> SessionTerminalEvent {
    let (outcome, error_code) = match terminal {
        TerminalOutcome::Completed => (SessionTerminalOutcomeView::Completed, None),
        TerminalOutcome::Cancelled => (SessionTerminalOutcomeView::Cancelled, None),
        TerminalOutcome::Rejected(reason) => (
            SessionTerminalOutcomeView::Rejected,
            Some(reject_error_code(reason).to_owned()),
        ),
        TerminalOutcome::Failed(category) => (
            SessionTerminalOutcomeView::Failed,
            Some(failure_error_code(category).to_owned()),
        ),
    };
    SessionTerminalEvent {
        contract_version: CONTRACT_VERSION,
        session_id: session_id.as_uuid(),
        outcome,
        error_code,
    }
}

fn elapsed_millis(started_at: Instant) -> u64 {
    duration_millis(started_at.elapsed())
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn hud_transition_allowed(current: SessionPhaseView, requested: RecordingHudState) -> bool {
    let current_rank = match current {
        SessionPhaseView::Preparing => 0,
        SessionPhaseView::Recording => 1,
        SessionPhaseView::Recognizing => 2,
        SessionPhaseView::Processing => 3,
        SessionPhaseView::Delivering => 4,
        SessionPhaseView::Finalizing => 5,
        SessionPhaseView::Terminated => 6,
    };
    let requested_rank = match requested {
        RecordingHudState::Preparing => 0,
        RecordingHudState::Recording => 1,
        RecordingHudState::Recognizing => 2,
        RecordingHudState::Processing => 3,
        RecordingHudState::Delivering => 4,
        RecordingHudState::Finalizing => 5,
        RecordingHudState::Completed => 6,
    };
    requested_rank >= current_rank
}

fn hud_port_error(code: &str, retryable: bool) -> PortError {
    PortError {
        code: code.to_owned(),
        safe_message_key: format!("errors.{code}"),
        retryable,
    }
}

fn command_error(code: &str, category: ErrorCategory, retryable: bool) -> AppError {
    AppError::new(
        code,
        category,
        ErrorSeverity::Error,
        retryable,
        format!("errors.{code}"),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MonitorBounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

fn position_on_target_monitor(
    window: &WebviewWindow,
    display_hint: Option<TargetDisplayHint>,
) -> Result<(), PortError> {
    let monitor = if let Some(hint) = display_hint {
        let monitors = window
            .available_monitors()
            .map_err(|_| hud_port_error("recording_hud.monitor_query_failed", true))?;
        let matching = monitors.into_iter().find(|monitor| {
            monitor_contains_logical_point(
                MonitorBounds {
                    x: monitor.position().x,
                    y: monitor.position().y,
                    width: monitor.size().width,
                    height: monitor.size().height,
                },
                monitor.scale_factor(),
                hint,
            )
        });
        match matching {
            Some(monitor) => Some(monitor),
            None => window
                .primary_monitor()
                .map_err(|_| hud_port_error("recording_hud.monitor_query_failed", true))?,
        }
    } else {
        window
            .primary_monitor()
            .map_err(|_| hud_port_error("recording_hud.monitor_query_failed", true))?
    }
    .ok_or_else(|| hud_port_error("recording_hud.monitor_missing", true))?;
    let work_area = monitor.work_area();
    let hud_position = bottom_center_position(
        MonitorBounds {
            x: work_area.position.x,
            y: work_area.position.y,
            width: work_area.size.width,
            height: work_area.size.height,
        },
        monitor.scale_factor(),
    );
    window
        .set_position(PhysicalPosition::new(hud_position.0, hud_position.1))
        .map_err(|_| hud_port_error("recording_hud.position_failed", true))
}

fn monitor_contains_logical_point(
    bounds: MonitorBounds,
    scale_factor: f64,
    point: TargetDisplayHint,
) -> bool {
    let scale = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        return false;
    };
    let x = f64::from(point.x);
    let y = f64::from(point.y);
    let left = f64::from(bounds.x) / scale;
    let top = f64::from(bounds.y) / scale;
    let right = left + f64::from(bounds.width) / scale;
    let bottom = top + f64::from(bounds.height) / scale;
    x >= left && x < right && y >= top && y < bottom
}

fn bottom_center_position(bounds: MonitorBounds, scale_factor: f64) -> (i32, i32) {
    let scale = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    let hud_width = (HUD_WIDTH_LOGICAL * scale).round() as i64;
    let hud_height = (HUD_HEIGHT_LOGICAL * scale).round() as i64;
    let edge_margin = (HUD_EDGE_MARGIN_LOGICAL * scale).round() as i64;
    let available_width = i64::from(bounds.width);
    let available_height = i64::from(bounds.height);
    let x_offset = (available_width - hud_width).max(0) / 2;
    let y_offset = (available_height - hud_height - edge_margin).max(0);
    (
        clamp_i64_to_i32(i64::from(bounds.x) + x_offset),
        clamp_i64_to_i32(i64::from(bounds.y) + y_offset),
    )
}

fn clamp_i64_to_i32(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use remtene_domain::{FailureCategory, RequestId};

    fn test_snapshot(session_id: SessionId, state: RecordingHudState) -> SessionPublicSnapshot {
        session_snapshot(session_id, state, Some(0), Some(Duration::from_secs(600)))
    }

    #[test]
    fn positions_hud_at_bottom_center_on_standard_monitor() {
        assert_eq!(
            bottom_center_position(
                MonitorBounds {
                    x: 0,
                    y: 0,
                    width: 1_920,
                    height: 1_080,
                },
                1.0,
            ),
            (888, 1_016)
        );
    }

    #[test]
    fn converts_logical_hud_size_once_for_retina_monitor() {
        assert_eq!(
            bottom_center_position(
                MonitorBounds {
                    x: 0,
                    y: 0,
                    width: 3_024,
                    height: 1_964,
                },
                2.0,
            ),
            (1_368, 1_836)
        );
    }

    #[test]
    fn preserves_negative_origin_for_left_side_monitor() {
        assert_eq!(
            bottom_center_position(
                MonitorBounds {
                    x: -2_560,
                    y: -160,
                    width: 2_560,
                    height: 1_440,
                },
                1.25,
            ),
            (-1_370, 1_200)
        );
    }

    #[test]
    fn centers_compact_hud_and_keeps_the_bottom_margin_when_it_fits() {
        assert_eq!(
            bottom_center_position(
                MonitorBounds {
                    x: 240,
                    y: -80,
                    width: 300,
                    height: 100,
                },
                1.0,
            ),
            (318, -44)
        );
    }

    #[test]
    fn matches_target_hint_against_retina_logical_bounds() {
        let retina = MonitorBounds {
            x: 0,
            y: 0,
            width: 3_024,
            height: 1_964,
        };
        assert!(monitor_contains_logical_point(
            retina,
            2.0,
            TargetDisplayHint { x: 1_000, y: 700 }
        ));
        assert!(!monitor_contains_logical_point(
            retina,
            2.0,
            TargetDisplayHint { x: 1_600, y: 700 }
        ));
    }

    #[test]
    fn matches_negative_origin_monitor_and_rejects_invalid_scale() {
        let left_monitor = MonitorBounds {
            x: -2_560,
            y: -160,
            width: 2_560,
            height: 1_440,
        };
        let hint = TargetDisplayHint { x: -1_000, y: 200 };
        assert!(monitor_contains_logical_point(left_monitor, 1.25, hint));
        assert!(!monitor_contains_logical_point(left_monitor, 0.0, hint));
    }

    #[test]
    fn session_projection_is_content_free_and_controls_follow_state() {
        let session_id = SessionId::new();
        let preparing = test_snapshot(session_id, RecordingHudState::Preparing);
        assert_eq!(preparing.user_state, SessionUserState::Preparing);
        assert_eq!(preparing.phase, SessionPhaseView::Preparing);
        assert!(!preparing.can_finish);
        assert!(preparing.can_cancel);

        let recording = test_snapshot(session_id, RecordingHudState::Recording);
        assert_eq!(recording.user_state, SessionUserState::Recording);
        assert!(recording.can_finish);
        assert!(recording.can_cancel);

        let serialized = serde_json::to_string(&recording).expect("snapshot must serialize");
        for forbidden in [
            "final_text",
            "selected_text",
            "audio_ref",
            "target_ref",
            "api_key",
        ] {
            assert!(!serialized.contains(forbidden));
        }

        for (state, phase) in [
            (
                RecordingHudState::Recognizing,
                SessionPhaseView::Recognizing,
            ),
            (RecordingHudState::Processing, SessionPhaseView::Processing),
            (RecordingHudState::Delivering, SessionPhaseView::Delivering),
            (RecordingHudState::Finalizing, SessionPhaseView::Finalizing),
        ] {
            let processing = test_snapshot(session_id, state);
            assert_eq!(processing.user_state, SessionUserState::Processing);
            assert_eq!(processing.phase, phase);
            assert!(!processing.can_finish);
            assert!(!processing.can_cancel);
        }

        let completed = test_snapshot(session_id, RecordingHudState::Completed);
        assert_eq!(completed.user_state, SessionUserState::Completed);
        assert_eq!(completed.phase, SessionPhaseView::Terminated);
    }

    #[test]
    fn stale_delayed_hide_cannot_hide_a_newer_hud_generation() {
        let mut runtime = RecordingHudRuntime {
            visibility_generation: 7,
            ..RecordingHudRuntime::default()
        };
        assert!(native_hide_is_current(&runtime, 7));

        runtime.visibility_generation = 8;
        assert!(!native_hide_is_current(&runtime, 7));

        runtime.current = Some(test_snapshot(
            SessionId::new(),
            RecordingHudState::Recording,
        ));
        assert!(!native_hide_is_current(&runtime, 8));
    }

    #[test]
    fn event_name_is_valid_for_the_tauri_runtime() {
        assert!(!SESSION_STATE_CHANGED_EVENT.is_empty());
        assert!(SESSION_STATE_CHANGED_EVENT.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '/' | ':' | '_')
        }));
        assert!(SESSION_TERMINAL_EVENT.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '/' | ':' | '_')
        }));
    }

    #[test]
    fn hud_state_cannot_regress_and_may_skip_forward() {
        assert!(hud_transition_allowed(
            SessionPhaseView::Preparing,
            RecordingHudState::Recording,
        ));
        assert!(hud_transition_allowed(
            SessionPhaseView::Recording,
            RecordingHudState::Recording,
        ));
        assert!(hud_transition_allowed(
            SessionPhaseView::Recording,
            RecordingHudState::Completed,
        ));
        assert!(hud_transition_allowed(
            SessionPhaseView::Finalizing,
            RecordingHudState::Completed,
        ));
        assert!(!hud_transition_allowed(
            SessionPhaseView::Delivering,
            RecordingHudState::Processing,
        ));
        assert!(!hud_transition_allowed(
            SessionPhaseView::Processing,
            RecordingHudState::Recording,
        ));
        assert!(!hud_transition_allowed(
            SessionPhaseView::Terminated,
            RecordingHudState::Processing,
        ));
        assert!(!hud_transition_allowed(
            SessionPhaseView::Recording,
            RecordingHudState::Preparing,
        ));
    }

    #[test]
    fn terminal_projection_is_versioned_stable_and_content_free() {
        let session_id = SessionId::new();
        let completed = session_terminal(session_id, TerminalOutcome::Completed);
        assert_eq!(completed.outcome, SessionTerminalOutcomeView::Completed);
        assert_eq!(completed.error_code, None);

        let failed = session_terminal(session_id, TerminalOutcome::Failed(FailureCategory::Asr));
        assert_eq!(failed.outcome, SessionTerminalOutcomeView::Failed);
        assert_eq!(failed.error_code.as_deref(), Some("session.failed.asr"));

        let serialized = serde_json::to_string(&failed).expect("terminal event must serialize");
        for forbidden in [
            "final_text",
            "selected_text",
            "audio_ref",
            "target_ref",
            "provider",
            "api_key",
        ] {
            assert!(!serialized.contains(forbidden));
        }
    }

    #[test]
    fn preparing_hud_accepts_cancel_but_rejects_finish() {
        let session_id = SessionId::new();
        let mut runtime = RecordingHudRuntime {
            current: Some(test_snapshot(session_id, RecordingHudState::Preparing)),
            pending_intent: None,
            ..RecordingHudRuntime::default()
        };
        let finish = SessionCommand {
            contract_version: CONTRACT_VERSION,
            request_id: RequestId::new().as_uuid(),
            session_id: session_id.as_uuid(),
        };
        assert_eq!(
            accept_runtime_intent(&mut runtime, finish, RecordingHudIntentKind::Finish)
                .expect_err("preparing HUD must not finish before route freeze")
                .code,
            "recording_hud.command_not_allowed"
        );

        let cancel = SessionCommand {
            contract_version: CONTRACT_VERSION,
            request_id: RequestId::new().as_uuid(),
            session_id: session_id.as_uuid(),
        };
        accept_runtime_intent(&mut runtime, cancel, RecordingHudIntentKind::Cancel)
            .expect("preparing recording must remain cancellable");
    }

    #[test]
    fn recording_intent_is_correlated_idempotent_and_single_pending() {
        let session_id = SessionId::new();
        let request_id = RequestId::new();
        let finish = SessionCommand {
            contract_version: CONTRACT_VERSION,
            request_id: request_id.as_uuid(),
            session_id: session_id.as_uuid(),
        };
        let mut runtime = RecordingHudRuntime {
            current: Some(test_snapshot(session_id, RecordingHudState::Recording)),
            pending_intent: None,
            ..RecordingHudRuntime::default()
        };

        let accepted = accept_runtime_intent(&mut runtime, finish, RecordingHudIntentKind::Finish)
            .expect("current recording command must be accepted");
        assert_eq!(accepted.accepted.request_id, request_id.as_uuid());
        assert!(
            accepted.dispatch,
            "the first acceptance must reach the Application use case"
        );

        let retried = accept_runtime_intent(&mut runtime, finish, RecordingHudIntentKind::Finish)
            .expect("the exact retry must remain idempotent");
        assert!(
            !retried.dispatch,
            "an exact retry must not start a second end-to-end task"
        );

        let conflicting = SessionCommand {
            contract_version: CONTRACT_VERSION,
            request_id: RequestId::new().as_uuid(),
            session_id: session_id.as_uuid(),
        };
        let error =
            accept_runtime_intent(&mut runtime, conflicting, RecordingHudIntentKind::Cancel)
                .expect_err("a second distinct intent cannot replace the pending command");
        assert_eq!(error.code, "recording_hud.command_already_pending");
    }

    #[test]
    fn stale_or_version_mismatched_hud_commands_fail_closed() {
        let session_id = SessionId::new();
        let mut runtime = RecordingHudRuntime {
            current: Some(test_snapshot(session_id, RecordingHudState::Recording)),
            pending_intent: None,
            ..RecordingHudRuntime::default()
        };

        let wrong_version = SessionCommand {
            contract_version: CONTRACT_VERSION + 1,
            request_id: RequestId::new().as_uuid(),
            session_id: session_id.as_uuid(),
        };
        assert_eq!(
            accept_runtime_intent(&mut runtime, wrong_version, RecordingHudIntentKind::Cancel,)
                .expect_err("contract mismatch must fail")
                .code,
            "ipc.contract_version_mismatch"
        );

        let stale = SessionCommand {
            contract_version: CONTRACT_VERSION,
            request_id: RequestId::new().as_uuid(),
            session_id: SessionId::new().as_uuid(),
        };
        assert_eq!(
            accept_runtime_intent(&mut runtime, stale, RecordingHudIntentKind::Cancel)
                .expect_err("another session cannot control this HUD")
                .code,
            "recording_hud.command_not_recording"
        );
        assert!(runtime.pending_intent.is_none());
    }
}
