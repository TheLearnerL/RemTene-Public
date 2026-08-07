use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
use std::{
    fs::{File, OpenOptions},
    path::Path,
};

use remtene_application::{
    RecordingSettingsController,
    ports::{DiagnosticEvent, EngineHealth, RecordingShortcutPort},
};
use remtene_contracts::{
    AppError, AppSnapshot, AsrReadiness, ErrorCategory, ErrorSeverity, LifecycleState,
    ModelSummary, SessionPublicSnapshot,
};
use remtene_domain::AsrPreference;
use tauri::{AppHandle, Manager, RunEvent, State, WebviewWindow};

use composition_root::CompositionRoot;

mod asr_runtime;
mod asr_status;
mod commands;
pub mod composition_root;
mod identity_migration;
mod recording_deadline;
mod recording_hud;
mod resident_shell;
mod session_feedback;
mod session_projection;
mod shortcuts;
mod temporary_text;

pub(crate) const CONTROL_PANEL_LABEL: &str = "control-panel";
pub(crate) const RECORDING_HUD_LABEL: &str = "recording-hud";
const TEMPORARY_TEXT_BOX_LABEL: &str = "temporary-text-box";
pub(crate) const SESSION_FEEDBACK_LABEL: &str = "session-feedback";

const INNER_SIZE_TOLERANCE_LOGICAL: f64 = 0.5;

fn inner_size_is_undersized(
    actual_width: f64,
    actual_height: f64,
    required_width: f64,
    required_height: f64,
) -> bool {
    actual_width + INNER_SIZE_TOLERANCE_LOGICAL < required_width
        || actual_height + INNER_SIZE_TOLERANCE_LOGICAL < required_height
}

/// Best-effort correction for an auxiliary WebView's requested logical content
/// area. Some decorated macOS windows have been observed with the requested
/// height applied to the outer frame during construction, clipping the bottom
/// controls. Runtime `set_size` targets the inner/client area. Failure stays
/// diagnostic-only because the responsive CSS must remain the final fallback.
pub(crate) fn ensure_minimum_inner_size(
    window: &WebviewWindow,
    required_width: f64,
    required_height: f64,
) {
    let Ok(scale_factor) = window.scale_factor() else {
        eprintln!(
            "[窗口] auxiliary.inner_size → 无法校正（label={} reason=scale_factor_unavailable）",
            window.label()
        );
        return;
    };
    let Ok(current) = window.inner_size() else {
        eprintln!(
            "[窗口] auxiliary.inner_size → 无法校正（label={} reason=inner_size_unavailable）",
            window.label()
        );
        return;
    };
    let current = current.to_logical::<f64>(scale_factor);
    if !inner_size_is_undersized(
        current.width,
        current.height,
        required_width,
        required_height,
    ) {
        return;
    }

    if window
        .set_size(tauri::LogicalSize::new(required_width, required_height))
        .is_err()
    {
        eprintln!(
            "[窗口] auxiliary.inner_size → 无法校正（label={} reason=set_size_failed）",
            window.label()
        );
        return;
    }

    // Keep a content-free diagnostic if the platform still refuses the size.
    // CSS also remains responsive so the action row stays visible in this case.
    let Ok(corrected) = window.inner_size() else {
        eprintln!(
            "[窗口] auxiliary.inner_size → 无法复核（label={} reason=corrected_size_unavailable）",
            window.label()
        );
        return;
    };
    let corrected = corrected.to_logical::<f64>(scale_factor);
    if inner_size_is_undersized(
        corrected.width,
        corrected.height,
        required_width,
        required_height,
    ) {
        eprintln!(
            "[窗口] auxiliary.inner_size → 未达到要求（label={} required={}x{} actual={:.1}x{:.1}）",
            window.label(),
            required_width,
            required_height,
            corrected.width,
            corrected.height
        );
    }
}

fn should_hide_window_on_close(label: &str) -> bool {
    label == CONTROL_PANEL_LABEL
}

#[cfg(target_os = "macos")]
fn should_restore_control_panel_on_reopen(_has_visible_windows: bool) -> bool {
    // A Dock click is explicit intent to open the control panel. Auxiliary
    // surfaces such as the recording HUD may keep this flag true while the
    // control panel itself is hidden, so it must not gate restoration.
    true
}

fn restore_control_panel(app: &AppHandle, reason: &str) {
    // `WebviewWindow::show` restores the window itself. macOS can also hide the
    // application as a whole, so unhide the app before trying to focus it.
    #[cfg(target_os = "macos")]
    if app.show().is_err() {
        eprintln!("control panel restore failed: lifecycle.app_show_failed reason={reason}");
    }

    let Some(control_panel) = app.get_webview_window(CONTROL_PANEL_LABEL) else {
        eprintln!(
            "control panel restore failed: lifecycle.control_panel_unavailable reason={reason}"
        );
        return;
    };
    if control_panel.unminimize().is_err() {
        eprintln!(
            "control panel restore failed: lifecycle.control_panel_unminimize_failed reason={reason}"
        );
    }
    if control_panel.show().is_err() {
        eprintln!(
            "control panel restore failed: lifecycle.control_panel_show_failed reason={reason}"
        );
    }
    if control_panel.set_focus().is_err() {
        eprintln!(
            "control panel restore failed: lifecycle.control_panel_focus_failed reason={reason}"
        );
    }
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
struct InstanceLease {
    _locks: Vec<File>,
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn acquire_instance_lock(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        let metadata = std::fs::symlink_metadata(parent)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(std::io::Error::other(
                "instance lease parent must be a real directory",
            ));
        }
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(std::io::Error::other(
                "instance lease must be a real regular file",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let lock = options.open(path)?;
    if !lock.metadata()?.is_file() {
        return Err(std::io::Error::other(
            "instance lease must be a real regular file",
        ));
    }
    lock.try_lock().map_err(|error| {
        let kind = match error {
            std::fs::TryLockError::WouldBlock => std::io::ErrorKind::WouldBlock,
            std::fs::TryLockError::Error(error) => error.kind(),
        };
        std::io::Error::new(
            kind,
            "another RemTene process is active or the instance lease is unavailable",
        )
    })?;
    Ok(lock)
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
#[cfg(test)]
fn acquire_instance_lease(path: &Path) -> std::io::Result<InstanceLease> {
    Ok(InstanceLease {
        _locks: vec![acquire_instance_lock(path)?],
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowRole {
    ControlPanel,
    RecordingHud,
    TemporaryTextBox,
    SessionFeedback,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WindowCommandClass {
    PublicSnapshot,
    RecordingHudState,
    RecordingControl,
    TemporaryTextControl,
    NotificationControl,
    Secret,
    Settings,
    History,
    Model,
}

fn window_role(label: &str) -> WindowRole {
    match label {
        CONTROL_PANEL_LABEL => WindowRole::ControlPanel,
        RECORDING_HUD_LABEL => WindowRole::RecordingHud,
        TEMPORARY_TEXT_BOX_LABEL => WindowRole::TemporaryTextBox,
        SESSION_FEEDBACK_LABEL => WindowRole::SessionFeedback,
        _ => WindowRole::Unknown,
    }
}

pub(crate) fn authorize_window(label: &str, command: WindowCommandClass) -> Result<(), AppError> {
    let allowed = matches!(
        (window_role(label), command),
        (
            WindowRole::ControlPanel,
            WindowCommandClass::PublicSnapshot
                | WindowCommandClass::RecordingControl
                | WindowCommandClass::Secret
                | WindowCommandClass::Settings
                | WindowCommandClass::History
                | WindowCommandClass::Model
        ) | (
            WindowRole::RecordingHud,
            WindowCommandClass::RecordingHudState | WindowCommandClass::RecordingControl
        ) | (
            WindowRole::TemporaryTextBox,
            WindowCommandClass::TemporaryTextControl
        ) | (
            WindowRole::SessionFeedback,
            WindowCommandClass::NotificationControl
        )
    );

    if allowed {
        Ok(())
    } else {
        Err(AppError::new(
            "ipc.window_forbidden",
            ErrorCategory::Security,
            ErrorSeverity::Error,
            false,
            "errors.ipc.window_forbidden",
        ))
    }
}

pub(crate) struct AppRuntime {
    public_snapshot: std::sync::Mutex<AppSnapshot>,
    shutdown_started: AtomicBool,
    shutdown_complete: AtomicBool,
}

impl AppRuntime {
    fn ready() -> Self {
        let mut public_snapshot = AppSnapshot::bootstrap();
        public_snapshot.lifecycle_state = LifecycleState::Ready;
        Self {
            public_snapshot: std::sync::Mutex::new(public_snapshot),
            shutdown_started: AtomicBool::new(false),
            shutdown_complete: AtomicBool::new(false),
        }
    }

    pub(crate) fn snapshot(&self) -> AppSnapshot {
        self.public_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn update_permissions(
        &self,
        microphone: remtene_contracts::MicrophonePermission,
        accessibility: remtene_contracts::SystemPermission,
    ) {
        let mut snapshot = self
            .public_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.microphone_permission = microphone;
        snapshot.accessibility_permission = accessibility;
    }

    fn refresh_operational_state(
        &self,
        active_session: Option<SessionPublicSnapshot>,
        asr_health: asr_status::AsrHealthSnapshot,
        preference: Option<AsrPreference>,
        llm_configured: bool,
    ) {
        let mut snapshot = self
            .public_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.active_session = active_session;
        snapshot.llm_configured = llm_configured;
        let (models, readiness) = public_asr_state(asr_health, preference);
        snapshot.model_summary = models;
        snapshot.asr_readiness = readiness;
    }

    fn update_shortcut_configured(&self, configured: bool) {
        self.public_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shortcut_configured = configured;
    }

    fn update_autostart_enabled(&self, enabled: bool) -> bool {
        let mut snapshot = self
            .public_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if snapshot.autostart_enabled == enabled {
            return false;
        }
        snapshot.autostart_enabled = enabled;
        true
    }

    fn begin_shutdown(&self) -> bool {
        if self
            .shutdown_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.public_snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .lifecycle_state = LifecycleState::Quitting;
        true
    }

    fn complete_shutdown(&self) {
        self.shutdown_complete.store(true, Ordering::Release);
    }

    fn shutdown_is_complete(&self) -> bool {
        self.shutdown_complete.load(Ordering::Acquire)
    }

    fn allow_shutdown_retry(&self) {
        self.shutdown_started.store(false, Ordering::Release);
    }
}

fn public_asr_state(
    health: asr_status::AsrHealthSnapshot,
    preference: Option<AsrPreference>,
) -> (ModelSummary, AsrReadiness) {
    let qwen_ready = health.qwen == Some(EngineHealth::Healthy);
    let whisper_ready = health.whisper == Some(EngineHealth::Healthy);
    let preference = preference.unwrap_or(AsrPreference::Qwen);
    let active_model_id = match preference {
        AsrPreference::Qwen => qwen_ready.then(|| asr_runtime::DEFAULT_QWEN_MODEL_ID.to_owned()),
        AsrPreference::Whisper => {
            whisper_ready.then(|| asr_runtime::DEFAULT_WHISPER_MODEL_ID.to_owned())
        }
    };
    let readiness = match active_model_id.as_deref() {
        Some(asr_runtime::DEFAULT_QWEN_MODEL_ID) => AsrReadiness::QwenReady,
        Some(asr_runtime::DEFAULT_WHISPER_MODEL_ID) => AsrReadiness::WhisperReady,
        _ => match preference {
            AsrPreference::Qwen if health.qwen.is_none() => AsrReadiness::Discovering,
            AsrPreference::Whisper if health.whisper.is_none() => AsrReadiness::Discovering,
            AsrPreference::Qwen | AsrPreference::Whisper => AsrReadiness::Unavailable,
        },
    };
    let models = ModelSummary {
        selected_model: match preference {
            AsrPreference::Qwen => remtene_contracts::LocalAsrModel::Qwen,
            AsrPreference::Whisper => remtene_contracts::LocalAsrModel::Whisper,
        },
        active_model_id,
        qwen_ready,
        whisper_ready,
    };
    (models, readiness)
}

pub(crate) async fn refresh_public_snapshot(
    runtime: &AppRuntime,
    composition: &CompositionRoot,
    recording_hud: &Arc<recording_hud::RecordingHudController>,
) -> AppSnapshot {
    refresh_public_snapshot_inner(runtime, composition, recording_hud, None).await
}

pub(crate) async fn refresh_public_snapshot_with_asr_preference(
    runtime: &AppRuntime,
    composition: &CompositionRoot,
    recording_hud: &Arc<recording_hud::RecordingHudController>,
    preference: AsrPreference,
) -> AppSnapshot {
    refresh_public_snapshot_inner(runtime, composition, recording_hud, Some(preference)).await
}

async fn refresh_public_snapshot_inner(
    runtime: &AppRuntime,
    composition: &CompositionRoot,
    recording_hud: &Arc<recording_hud::RecordingHudController>,
    checked_preference: Option<AsrPreference>,
) -> AppSnapshot {
    // Refresh live OS permission probes on every snapshot read.
    let status = commands::permissions::probe_permission_status();
    runtime.update_permissions(status.microphone, status.accessibility);
    let preference = match checked_preference {
        Some(preference) => Some(preference),
        None => composition
            .settings
            .load()
            .await
            .ok()
            .map(|settings| settings.asr_preference()),
    };
    let llm_configured = composition.llm_configuration.is_llm_ready().await;
    // 所有异步读取完成后再取得 Session／ASR 现场投影，避免等待 LLM 状态期间
    // 已结束的任务被旧快照重新写回公开状态。
    runtime.refresh_operational_state(
        recording_hud.current(),
        composition.asr_status.snapshot(),
        preference,
        llm_configured,
    );
    runtime.snapshot()
}

#[tauri::command]
async fn app_get_snapshot(
    window: WebviewWindow,
    runtime: State<'_, AppRuntime>,
    composition: State<'_, CompositionRoot>,
    recording_hud: State<'_, Arc<recording_hud::RecordingHudController>>,
) -> Result<AppSnapshot, AppError> {
    authorize_window(window.label(), WindowCommandClass::PublicSnapshot)?;
    Ok(refresh_public_snapshot(runtime.inner(), composition.inner(), recording_hud.inner()).await)
}

fn handle_run_event(app: &AppHandle, event: RunEvent) {
    #[cfg(target_os = "macos")]
    if let RunEvent::Reopen {
        has_visible_windows,
        ..
    } = &event
    {
        if should_restore_control_panel_on_reopen(*has_visible_windows) {
            restore_control_panel(app, "dock_reopen");
        }
        return;
    }

    let RunEvent::ExitRequested { api, .. } = event else {
        return;
    };
    let Some(runtime) = app.try_state::<AppRuntime>() else {
        return;
    };
    if runtime.shutdown_is_complete() {
        return;
    }

    // The process may exit only after the Application workflow has invalidated
    // the active Session, cancelled remote work, cleaned audio, and drained
    // every already-started irreversible commit.
    api.prevent_exit();
    if !runtime.begin_shutdown() {
        return;
    }
    let Some(composition) = app.try_state::<CompositionRoot>() else {
        runtime.allow_shutdown_retry();
        eprintln!("formal shutdown failed: lifecycle.composition_unavailable");
        return;
    };
    let orchestrator = Arc::clone(&composition.orchestrator);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match orchestrator.quit().await {
            Ok(_) => {
                app.state::<AppRuntime>().complete_shutdown();
                app.exit(0);
            }
            Err(_) => {
                app.state::<AppRuntime>().allow_shutdown_retry();
                eprintln!("formal shutdown failed: lifecycle.cleanup_incomplete");
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    // This must be the first desktop plugin: every process must resolve to the
    // same CompositionRoot so Session/configuration gates cannot be bypassed
    // by launching a second application instance.
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(
        |app, _arguments, _working_directory| {
            restore_control_panel(app, "second_instance");
        },
    ));
    let builder = builder.plugin(shortcuts::plugin());
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    let autostart_builder =
        tauri_plugin_autostart::Builder::new().app_name(resident_shell::AUTOSTART_ENTRY_NAME);
    #[cfg(target_os = "macos")]
    let autostart_builder =
        autostart_builder.macos_launcher(tauri_plugin_autostart::MacosLauncher::LaunchAgent);
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    let builder = builder.plugin(autostart_builder.build());
    let builder = builder.on_window_event(|window, event| {
        if window.label() == SESSION_FEEDBACK_LABEL
            && matches!(event, tauri::WindowEvent::CloseRequested { .. })
            && let Some(controller) =
                window.try_state::<Arc<session_feedback::SessionFeedbackController>>()
        {
            controller.clear_pending_after_user_close();
        }
        if !should_hide_window_on_close(window.label()) {
            return;
        }
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            // 关闭控制面板只收起设置入口；正式退出仍只经过 RunEvent::ExitRequested
            // 和 Application 的资源清理屏障。
            api.prevent_close();
            let _ = window.hide();
        }
    });
    #[cfg(target_os = "macos")]
    let builder = builder.plugin(tauri_nspanel::init());

    let app = builder
        .manage(AppRuntime::ready())
        .setup(|app| {
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            let desktop_storage = {
                let app_data_base = app
                    .path()
                    .app_data_dir()
                    .map_err(|_| std::io::Error::other("application data directory unavailable"))?;
                let app_cache_base = app
                    .path()
                    .app_cache_dir()
                    .map_err(|_| std::io::Error::other("application cache directory unavailable"))?;
                let legacy_data_root =
                    app_data_base.join(identity_migration::LEGACY_STORAGE_DIRECTORY);
                let legacy_lock = identity_migration::real_directory_exists(&legacy_data_root)?
                    .then(|| {
                    acquire_instance_lock(
                        &legacy_data_root.join(identity_migration::INSTANCE_LEASE_FILE),
                    )
                });
                let legacy_lock = legacy_lock.transpose()?;
                let paths = identity_migration::prepare_desktop_storage(
                    &app_data_base,
                    &app_cache_base,
                )
                .map_err(|_| std::io::Error::other("brand storage migration failed"))?;
                let current_lease = paths
                    .data_root()
                    .join(identity_migration::INSTANCE_LEASE_FILE);
                let mut locks = Vec::with_capacity(2);
                if let Some(lock) = legacy_lock {
                    locks.push(lock);
                }
                locks.push(acquire_instance_lock(&current_lease)?);
                let leases = InstanceLease { _locks: locks };
                if !app.manage(leases) {
                    return Err(Box::new(std::io::Error::other(
                        "instance lease state was already registered",
                    )));
                }
                paths
            };

            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            resident_shell::initialize(app)?;

            let session_feedback_controller = Arc::new(
                session_feedback::SessionFeedbackController::new(app.handle().clone()),
            );

            // Initialize ASR Worker (desktop platforms only)
            #[cfg(target_os = "macos")]
            {
                // 准备音频存储目录（缓存，用后即删）
                let cache_dir = desktop_storage.cache_root().to_path_buf();
                let audio_root = cache_dir.join("audio");
                let diagnostics_root = cache_dir.join("logs");

                // 准备历史存储文件（持久化数据，仅存最终文本与时间）
                let data_dir = desktop_storage.data_root().to_path_buf();
                let history_path = data_dir.join("history.json");
                let settings_path = data_dir.join("settings.json");
                let secret_root = data_dir.join("secrets");

                // 创建 RecordingHud 控制器
                let recording_hud_controller = Arc::new(recording_hud::RecordingHudController::new(
                    app.handle().clone(),
                ));

                // 先建立录音适配器：它是音频工件的权威登记处，Worker 只能经它解析音频。
                let audio_capture = match remtene_platform::create_default_macos_audio_capture(audio_root) {
                    Ok(capture) => Arc::new(capture),
                    Err(error) => {
                        eprintln!("✗ audio capture initialization failed: {}", error.code);
                        return Err(Box::new(std::io::Error::other(format!(
                            "Failed to initialize audio capture: {}",
                            error.code
                        ))));
                    }
                };

                // 尝试初始化 Worker
                let (asr_port, asr_model_control, asr_worker_state, asr_worker_error): (
                    Arc<dyn remtene_application::ports::AsrEnginePort>,
                    Arc<dyn remtene_application::ports::AsrModelControlPort>,
                    &'static str,
                    Option<String>,
                ) = match asr_runtime::initialize_macos_worker(app.handle(), Arc::clone(&audio_capture)) {
                    Ok(worker) => {
                        eprintln!("✓ ASR Worker initialized successfully");
                        let runtime = Arc::new(worker);
                        (
                            Arc::clone(&runtime) as Arc<dyn remtene_application::ports::AsrEnginePort>,
                            runtime as Arc<dyn remtene_application::ports::AsrModelControlPort>,
                            "initialized",
                            None,
                        )
                    }
                    Err(e) => {
                        eprintln!("⚠ ASR Worker initialization: {} (code: {})", e.user_message_key, e.code);
                        eprintln!("  Application will continue, but ASR will report unhealthy and no session can start.");
                        // 使用 Stub ASR Port 作为降级
                        (
                            Arc::new(remtene_adapters::stub_ports::StubAsrEngine::new()),
                            Arc::new(remtene_adapters::stub_ports::StubAsrModelControl::new()),
                            "unavailable",
                            Some(e.code),
                        )
                    }
                };

                // 临时文本框是锚点不可验证与结果不确定时唯一允许的交付面
                let temporary_text_controller = Arc::new(
                    temporary_text::TemporaryTextBoxController::new(app.handle().clone()),
                );

                // 创建 CompositionRoot（使用真实 macOS 平台 Ports）
                match CompositionRoot::new_macos(
                    audio_capture as Arc<dyn remtene_application::ports::AudioCapture>,
                    history_path,
                    settings_path,
                    diagnostics_root,
                    secret_root,
                    Arc::clone(&recording_hud_controller) as Arc<dyn remtene_application::ports::RecordingHudPort>,
                    asr_port,
                    asr_model_control,
                    Arc::clone(&temporary_text_controller) as Arc<dyn remtene_application::ports::TemporaryTextOutput>,
                    Arc::clone(&session_feedback_controller) as Arc<dyn remtene_application::ports::UserNotificationPort>,
                ) {
                    Ok(composition_root) => {
                        eprintln!("✓ CompositionRoot initialized with real macOS platform Ports");
                        composition_root.diagnostics.record(DiagnosticEvent {
                            session_id: None,
                            phase: Some("asr.worker".to_owned()),
                            state: Some(asr_worker_state.to_owned()),
                            duration_ms: None,
                            error_code: asr_worker_error,
                            detail: None,
                        });
                        composition_root.diagnostics.record(DiagnosticEvent {
                            session_id: None,
                            phase: Some("application.startup".to_owned()),
                            state: Some("ready".to_owned()),
                            duration_ms: None,
                            error_code: None,
                            detail: None,
                        });
                        // 直接托管 CompositionRoot：会话命令请求 State<'_, CompositionRoot>，
                        // Tauri 按 TypeId 精确匹配，不能再包一层 Arc（否则运行时查找失败）。
                        app.manage(composition_root);
                        app.manage(recording_hud_controller);
                        // 临时文本框的拉取与关闭命令要拿到同一个控制器实例。
                        app.manage(temporary_text_controller);
                    }
                    Err(e) => {
                        eprintln!("✗ CompositionRoot initialization failed: {}", e.user_message_key);
                        return Err(Box::new(std::io::Error::other(
                            format!("Failed to initialize application: {}", e.code),
                        )));
                    }
                }
            }

            #[cfg(target_os = "windows")]
            {
                let secret_root = Some(desktop_storage.data_root().join("secrets"));

                // 创建 RecordingHud 控制器
                let recording_hud_controller = Arc::new(recording_hud::RecordingHudController::new(
                    app.handle().clone(),
                ));

                // 尝试初始化 Worker
                let asr_port: Arc<dyn remtene_application::ports::AsrEnginePort> = match asr_runtime::initialize_windows_worker(app.handle()) {
                    Ok(worker) => {
                        eprintln!("✓ ASR Worker initialized successfully");
                        Arc::new(worker)
                    }
                    Err(e) => {
                        eprintln!("⚠ ASR Worker initialization: {} (code: {})", e.user_message_key, e.code);
                        eprintln!("  Windows ASR Worker is not yet implemented (ASR-WIN-001).");
                        eprintln!("  Using Stub ASR Port");
                        Arc::new(remtene_adapters::stub_ports::StubAsrEngine::new())
                    }
                };

                let temporary_text_controller = Arc::new(
                    temporary_text::TemporaryTextBoxController::new(app.handle().clone()),
                );

                // 创建 CompositionRoot（使用 Stub Windows 平台 Ports）
                match CompositionRoot::new_windows(
                    secret_root,
                    Arc::clone(&recording_hud_controller) as Arc<dyn remtene_application::ports::RecordingHudPort>,
                    asr_port,
                    Arc::new(remtene_adapters::stub_ports::StubAsrModelControl::new()),
                    Arc::clone(&temporary_text_controller) as Arc<dyn remtene_application::ports::TemporaryTextOutput>,
                    Arc::clone(&session_feedback_controller) as Arc<dyn remtene_application::ports::UserNotificationPort>,
                ) {
                    Ok(composition_root) => {
                        eprintln!("✓ CompositionRoot initialized with Windows stub Ports");
                        // 与 macOS 一致：直接托管，匹配 State<'_, CompositionRoot>。
                        app.manage(composition_root);
                        app.manage(recording_hud_controller);
                        // 临时文本框的拉取与关闭命令要拿到同一个控制器实例。
                        app.manage(temporary_text_controller);
                    }
                    Err(e) => {
                        eprintln!("✗ CompositionRoot initialization failed: {}", e.user_message_key);
                        return Err(Box::new(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("Failed to initialize application: {}", e.code),
                        )));
                    }
                }
            }

            // Build Recording HUD window (if not already managed)
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                let recording_hud = Arc::new(recording_hud::RecordingHudController::new(
                    app.handle().clone(),
                ));
                app.manage(recording_hud);
            }

            app.manage(session_feedback_controller);
            recording_hud::build_recording_hud(app)?;
            // 临时文本框不再预建：它关闭即销毁，改由交付时按需创建。

            #[cfg(any(target_os = "macos", target_os = "windows"))]
            let persisted_shortcut = {
                let (orchestrator, settings) = {
                    let composition = app.state::<CompositionRoot>();
                    (
                        Arc::clone(&composition.orchestrator),
                        Arc::clone(&composition.settings),
                    )
                };

                let shortcut_port: Arc<dyn RecordingShortcutPort> = Arc::new(
                    shortcuts::TauriRecordingShortcutPort::new(app.handle().clone()),
                );
                if !app.manage(RecordingSettingsController::new(
                    orchestrator,
                    Arc::clone(&settings),
                    shortcut_port,
                )) {
                    return Err(Box::new(std::io::Error::other(
                        "recording settings controller was already registered",
                    )));
                }

                match tauri::async_runtime::block_on(settings.load()) {
                    Ok(settings) => settings.recording_shortcut().cloned(),
                    Err(error) => {
                        eprintln!("⚠ 录音快捷键设置读取失败：{}", error.code);
                        None
                    }
                }
            };

            // 快捷键是跨应用输入的前提：从控制面板触发时前台应用与键盘焦点都属于
            // 本应用，精确 AX 与使用者导向兼容贴上都无法代表用户原本的外部输入位置。
            // 注册失败因此不能静默。
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            let shortcut_configured = match shortcuts::register_recording_shortcut(
                app.handle(),
                persisted_shortcut.as_ref(),
            ) {
                Ok(Some(binding)) => {
                    eprintln!("✓ 录音快捷键已绑定：{binding}");
                    true
                }
                Ok(None) => {
                    eprintln!("· 未配置录音快捷键，当前只能从控制面板触发，无法保持外部应用为输入焦点");
                    false
                }
                Err(error) => {
                    eprintln!("⚠ 录音快捷键注册失败：{}", error.code);
                    false
                }
            };
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            let shortcut_configured = false;
            app.state::<AppRuntime>()
                .update_shortcut_configured(shortcut_configured);

            // 每次应用进程启动后自动执行一次与“重新检查”按钮完全相同的本地模型校验。
            // 异步运行避免模型加载与静音预热阻塞控制面板创建；完成后只向控制面板发送
            // 不含正文或秘密的公开快照。
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            commands::model::spawn_startup_health_check(app.handle().clone());

            #[cfg(debug_assertions)]
            if std::env::var("REMTENE_RECORDING_HUD_PREVIEW").as_deref() == Ok("recording") {
                // Get the recording_hud from managed state for preview
                if let Some(preview_controller) = app.try_state::<Arc<recording_hud::RecordingHudController>>() {
                    let app_handle = app.handle().clone();
                    let preview_controller = Arc::clone(&preview_controller);
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        let main_thread_handle = app_handle.clone();
                        let _ = app_handle.run_on_main_thread(move || {
                            if let Err(error) = recording_hud::show_debug_preview(&preview_controller) {
                                eprintln!("recording HUD preview failed: {}", error.code);
                                return;
                            }
                            if let Some(control_panel) =
                                main_thread_handle.get_webview_window(CONTROL_PANEL_LABEL)
                            {
                                let _ = control_panel.hide();
                            }
                        });
                    });
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_get_snapshot,
            commands::model::model_check_health,
            commands::model::model_switch_engine,
            commands::model::model_open_directory,
            recording_hud::recording_hud_get_state,
            recording_hud::recording_finish,
            recording_hud::recording_cancel,
            commands::session::session_start,
            commands::session::session_cancel,
            commands::session::session_finish,
            commands::permissions::permission_get_status,
            commands::permissions::permission_request_microphone,
            commands::permissions::permission_request_accessibility,
            commands::permissions::permission_open_accessibility_settings,
            commands::permissions::permission_open_microphone_settings,
            commands::settings::settings_get,
            commands::settings::settings_set_clipboard_bridge,
            commands::settings::settings_set_recording_preferences,
            commands::settings::settings_set_recording_shortcut,
            commands::settings::settings_set_history_enabled,
            commands::settings::settings_set_history_limit,
            commands::settings::settings_set_history_retention,
            commands::settings::settings_set_auto_copy_result,
            commands::settings::settings_set_local_diagnostics,
            commands::settings::diagnostics_open_directory,
            commands::settings::settings_set_text_processing,
            commands::settings::settings_set_llm,
            commands::history::history_list,
            commands::history::history_copy,
            commands::history::history_clear_all,
            resident_shell::autostart_get_status,
            resident_shell::autostart_set_enabled,
            commands::secrets::secret_get_llm_api_key_status,
            commands::secrets::secret_set_llm_api_key,
            commands::secrets::secret_reveal_llm_api_key,
            commands::secrets::secret_delete_llm_api_key,
            commands::secrets::secret_reset_unrecoverable_llm_secrets,
            commands::secrets::llm_test_connection,
            temporary_text::temporary_text_get_pending,
            temporary_text::temporary_text_dismiss,
            temporary_text::temporary_text_copy_all,
            session_feedback::notification_get_pending,
            session_feedback::notification_apply_action,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");
    app.run(handle_run_event);
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    #[cfg(target_os = "macos")]
    use std::{path::PathBuf, process::Command};

    use super::*;

    #[test]
    fn auxiliary_window_size_correction_only_expands_undersized_content() {
        assert!(inner_size_is_undersized(420.0, 210.0, 420.0, 240.0));
        assert!(inner_size_is_undersized(390.0, 240.0, 420.0, 240.0));
        assert!(!inner_size_is_undersized(420.0, 240.0, 420.0, 240.0));
        assert!(!inner_size_is_undersized(419.75, 239.75, 420.0, 240.0));
        assert!(!inner_size_is_undersized(440.0, 260.0, 420.0, 240.0));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dock_reopen_restores_the_control_panel_even_with_an_auxiliary_window_visible() {
        assert!(should_restore_control_panel_on_reopen(false));
        assert!(should_restore_control_panel_on_reopen(true));
    }

    #[test]
    fn public_snapshot_maps_live_model_health_and_selected_engine() {
        let runtime = AppRuntime::ready();
        runtime.refresh_operational_state(
            None,
            asr_status::AsrHealthSnapshot {
                qwen: Some(EngineHealth::Healthy),
                whisper: Some(EngineHealth::Healthy),
            },
            Some(AsrPreference::Whisper),
            true,
        );
        runtime.update_shortcut_configured(true);

        let snapshot = runtime.snapshot();
        assert_eq!(snapshot.asr_readiness, AsrReadiness::WhisperReady);
        assert_eq!(
            snapshot.model_summary.active_model_id.as_deref(),
            Some(asr_runtime::DEFAULT_WHISPER_MODEL_ID)
        );
        assert!(snapshot.model_summary.qwen_ready);
        assert!(snapshot.model_summary.whisper_ready);
        assert!(snapshot.llm_configured);
        assert!(snapshot.shortcut_configured);
    }

    #[test]
    fn public_snapshot_keeps_unobserved_asr_state_discovering() {
        let runtime = AppRuntime::ready();
        runtime.refresh_operational_state(
            None,
            asr_status::AsrHealthSnapshot::default(),
            Some(AsrPreference::Qwen),
            false,
        );

        let snapshot = runtime.snapshot();
        assert_eq!(snapshot.asr_readiness, AsrReadiness::Discovering);
        assert_eq!(snapshot.model_summary.active_model_id, None);
        assert!(!snapshot.model_summary.qwen_ready);
        assert!(!snapshot.model_summary.whisper_ready);
    }

    #[test]
    fn qwen_selection_never_presents_healthy_whisper_as_the_active_fallback() {
        let (models, readiness) = public_asr_state(
            asr_status::AsrHealthSnapshot {
                qwen: Some(EngineHealth::Missing),
                whisper: Some(EngineHealth::Healthy),
            },
            Some(AsrPreference::Qwen),
        );

        assert_eq!(readiness, AsrReadiness::Unavailable);
        assert_eq!(
            models.selected_model,
            remtene_contracts::LocalAsrModel::Qwen
        );
        assert_eq!(models.active_model_id, None);
        assert!(models.whisper_ready);
    }

    #[test]
    fn formal_shutdown_is_single_leader_and_stays_quitting_until_complete() {
        let runtime = AppRuntime::ready();
        assert!(runtime.begin_shutdown());
        assert!(!runtime.begin_shutdown());
        assert_eq!(runtime.snapshot().lifecycle_state, LifecycleState::Quitting);
        assert!(!runtime.shutdown_is_complete());

        runtime.complete_shutdown();
        assert!(runtime.shutdown_is_complete());
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    #[test]
    fn process_instance_lease_fails_closed_until_the_owner_exits() {
        let directory =
            std::env::temp_dir().join(format!("remtene-instance-lease-{}", uuid::Uuid::new_v4()));
        let path = directory.join("instance.lock");
        let first = acquire_instance_lease(&path).expect("first process lease");

        let error = acquire_instance_lease(&path)
            .err()
            .expect("a second independent handle must not acquire the lease");
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::PermissionDenied
            ),
            "unexpected lock conflict: {error}"
        );

        drop(first);
        let next = acquire_instance_lease(&path).expect("lease releases with its owner");
        drop(next);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(directory);
    }

    #[cfg(unix)]
    #[test]
    fn process_instance_lease_refuses_a_symbolic_link() {
        use std::os::unix::fs::symlink;

        let directory =
            std::env::temp_dir().join(format!("remtene-instance-lease-{}", uuid::Uuid::new_v4()));
        let external = directory.join("external.lock");
        let path = directory.join("instance.lock");
        std::fs::create_dir_all(&directory).expect("create lease directory");
        std::fs::write(&external, b"external").expect("write external lock target");
        symlink(&external, &path).expect("create lease symlink");

        let error = acquire_instance_lease(&path)
            .err()
            .expect("instance lease symlink must fail closed");
        assert_ne!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(
            std::fs::read(&external).expect("read external lock target"),
            b"external"
        );

        std::fs::remove_dir_all(directory).expect("remove lease directory");
    }

    #[test]
    fn command_matrix_rejects_cross_window_privilege_use() {
        assert!(authorize_window(CONTROL_PANEL_LABEL, WindowCommandClass::PublicSnapshot).is_ok());
        assert!(
            authorize_window(RECORDING_HUD_LABEL, WindowCommandClass::RecordingControl).is_ok()
        );
        assert!(
            authorize_window(RECORDING_HUD_LABEL, WindowCommandClass::RecordingHudState).is_ok()
        );
        assert!(
            authorize_window(
                TEMPORARY_TEXT_BOX_LABEL,
                WindowCommandClass::TemporaryTextControl,
            )
            .is_ok()
        );
        for label in [
            CONTROL_PANEL_LABEL,
            RECORDING_HUD_LABEL,
            SESSION_FEEDBACK_LABEL,
            "unknown",
        ] {
            assert!(
                authorize_window(label, WindowCommandClass::TemporaryTextControl).is_err(),
                "{label} must not call temporary-text commands"
            );
        }
        assert!(
            authorize_window(
                SESSION_FEEDBACK_LABEL,
                WindowCommandClass::NotificationControl,
            )
            .is_ok()
        );
        assert!(authorize_window(CONTROL_PANEL_LABEL, WindowCommandClass::History).is_ok());

        for label in [
            RECORDING_HUD_LABEL,
            TEMPORARY_TEXT_BOX_LABEL,
            SESSION_FEEDBACK_LABEL,
            "unknown",
        ] {
            let error = authorize_window(label, WindowCommandClass::PublicSnapshot)
                .expect_err("non-control-panel windows must not read the app snapshot");
            assert_eq!(error.code, "ipc.window_forbidden");
        }

        for command in [
            WindowCommandClass::Secret,
            WindowCommandClass::Settings,
            WindowCommandClass::History,
            WindowCommandClass::Model,
        ] {
            assert!(authorize_window(RECORDING_HUD_LABEL, command).is_err());
            assert!(authorize_window(TEMPORARY_TEXT_BOX_LABEL, command).is_err());
            assert!(authorize_window(SESSION_FEEDBACK_LABEL, command).is_err());
        }
    }

    #[test]
    fn capabilities_are_window_specific_and_do_not_use_core_default() {
        let fixtures = [
            (
                include_str!("../capabilities/control-panel.json"),
                CONTROL_PANEL_LABEL,
                Some("allow-app-get-snapshot"),
            ),
            (
                include_str!("../capabilities/recording-hud.json"),
                RECORDING_HUD_LABEL,
                Some("allow-recording-hud-get-state"),
            ),
            (
                include_str!("../capabilities/temporary-text-box.json"),
                TEMPORARY_TEXT_BOX_LABEL,
                None,
            ),
            (
                include_str!("../capabilities/session-feedback.json"),
                SESSION_FEEDBACK_LABEL,
                None,
            ),
        ];

        for (source, expected_label, required_app_permission) in fixtures {
            let capability: Value =
                serde_json::from_str(source).expect("capability JSON must remain valid");
            assert_eq!(capability["identifier"], expected_label);
            assert_eq!(capability["windows"][0], expected_label);

            let permissions = capability["permissions"]
                .as_array()
                .expect("permissions must be an array");
            assert!(
                permissions
                    .iter()
                    .all(|permission| permission != "core:default")
            );
            if let Some(permission) = required_app_permission {
                assert!(permissions.iter().any(|candidate| candidate == permission));
            } else {
                assert!(
                    permissions
                        .iter()
                        .all(|candidate| candidate != "allow-app-get-snapshot")
                );
            }
        }

        let recording_hud: Value =
            serde_json::from_str(include_str!("../capabilities/recording-hud.json"))
                .expect("recording HUD capability must remain valid");
        let permissions = recording_hud["permissions"]
            .as_array()
            .expect("permissions must be an array");
        for required in [
            "allow-recording-hud-get-state",
            "allow-recording-finish",
            "allow-recording-cancel",
            "core:event:allow-listen",
            "core:event:allow-unlisten",
        ] {
            assert!(permissions.iter().any(|candidate| candidate == required));
        }

        let temporary_text: Value =
            serde_json::from_str(include_str!("../capabilities/temporary-text-box.json"))
                .expect("temporary-text capability must remain valid");
        let temporary_text_permissions = temporary_text["permissions"]
            .as_array()
            .expect("permissions must be an array");
        for required in [
            "allow-temporary-text-get-pending",
            "allow-temporary-text-dismiss",
            "allow-temporary-text-copy-all",
            "core:event:allow-listen",
            "core:event:allow-unlisten",
        ] {
            assert!(
                temporary_text_permissions
                    .iter()
                    .any(|candidate| candidate == required)
            );
        }

        let session_feedback: Value =
            serde_json::from_str(include_str!("../capabilities/session-feedback.json"))
                .expect("session feedback capability must remain valid");
        let feedback_permissions = session_feedback["permissions"]
            .as_array()
            .expect("permissions must be an array");
        for required in [
            "allow-notification-get-pending",
            "allow-notification-apply-action",
            "core:event:allow-listen",
            "core:event:allow-unlisten",
        ] {
            assert!(
                feedback_permissions
                    .iter()
                    .any(|candidate| candidate == required)
            );
        }

        let control_panel: Value =
            serde_json::from_str(include_str!("../capabilities/control-panel.json"))
                .expect("control-panel capability must remain valid");
        let control_permissions = control_panel["permissions"]
            .as_array()
            .expect("permissions must be an array");
        for required in [
            "core:window:allow-start-dragging",
            "allow-model-check-health",
            "allow-model-switch-engine",
            "allow-model-open-directory",
            "allow-settings-set-recording-preferences",
            "allow-settings-set-recording-shortcut",
            "allow-settings-set-history-enabled",
            "allow-settings-set-text-processing",
            "allow-settings-set-llm",
            "allow-history-list",
            "allow-history-copy",
            "allow-history-clear-all",
            "allow-secret-get-llm-api-key-status",
            "allow-secret-set-llm-api-key",
            "allow-secret-reveal-llm-api-key",
            "allow-secret-delete-llm-api-key",
            "allow-secret-reset-unrecoverable-llm-secrets",
            "allow-llm-test-connection",
        ] {
            assert!(
                control_permissions
                    .iter()
                    .any(|candidate| candidate == required)
            );
        }
        for source in [
            include_str!("../capabilities/recording-hud.json"),
            include_str!("../capabilities/temporary-text-box.json"),
            include_str!("../capabilities/session-feedback.json"),
        ] {
            let capability: Value =
                serde_json::from_str(source).expect("capability JSON must remain valid");
            let permissions = capability["permissions"]
                .as_array()
                .expect("permissions must be an array");
            assert!(permissions.iter().all(|permission| {
                permission.as_str().is_none_or(|permission| {
                    !permission.contains("secret")
                        && permission != "allow-history-list"
                        && permission != "allow-history-copy"
                        && permission != "allow-history-clear-all"
                        && permission != "allow-llm-test-connection"
                        && permission != "allow-settings-set-llm"
                        && permission != "allow-settings-set-recording-preferences"
                        && permission != "allow-settings-set-recording-shortcut"
                        && permission != "allow-settings-set-history-enabled"
                        && permission != "allow-settings-set-text-processing"
                        && permission != "allow-model-check-health"
                        && permission != "allow-model-switch-engine"
                        && permission != "allow-model-open-directory"
                        && permission != "core:window:allow-start-dragging"
                })
            }));
        }

        for source in [
            include_str!("../capabilities/control-panel.json"),
            include_str!("../capabilities/recording-hud.json"),
            include_str!("../capabilities/session-feedback.json"),
        ] {
            let capability: Value =
                serde_json::from_str(source).expect("capability JSON must remain valid");
            let permissions = capability["permissions"]
                .as_array()
                .expect("permissions must be an array");
            assert!(
                permissions
                    .iter()
                    .all(|permission| permission != "allow-temporary-text-copy-all")
            );
        }
    }

    #[test]
    fn model_open_directory_acl_is_generated_and_registered() {
        let build_script = include_str!("../build.rs");
        assert!(build_script.contains("\"model_open_directory\""));

        let permission = include_str!("../permissions/autogenerated/model_open_directory.toml");
        assert!(permission.contains("commands.allow = [\"model_open_directory\"]"));
        assert!(permission.contains("commands.deny = [\"model_open_directory\"]"));
    }

    #[test]
    fn model_switch_acl_is_generated_and_control_panel_only() {
        let build_script = include_str!("../build.rs");
        assert!(build_script.contains("\"model_switch_engine\""));

        let permission = include_str!("../permissions/autogenerated/model_switch_engine.toml");
        assert!(permission.contains("commands.allow = [\"model_switch_engine\"]"));
        assert!(permission.contains("commands.deny = [\"model_switch_engine\"]"));
    }

    #[test]
    fn personal_feature_acls_are_generated_and_control_panel_only() {
        let build_script = include_str!("../build.rs");
        let control_panel = include_str!("../capabilities/control-panel.json");
        let permissions = [
            (
                "settings_set_history_limit",
                "allow-settings-set-history-limit",
                include_str!("../permissions/autogenerated/settings_set_history_limit.toml"),
            ),
            (
                "autostart_get_status",
                "allow-autostart-get-status",
                include_str!("../permissions/autogenerated/autostart_get_status.toml"),
            ),
            (
                "autostart_set_enabled",
                "allow-autostart-set-enabled",
                include_str!("../permissions/autogenerated/autostart_set_enabled.toml"),
            ),
        ];

        for (command, identifier, permission) in permissions {
            assert!(build_script.contains(&format!("\"{command}\"")));
            assert!(control_panel.contains(&format!("\"{identifier}\"")));
            assert!(permission.contains(&format!("commands.allow = [\"{command}\"]")));
            assert!(permission.contains(&format!("commands.deny = [\"{command}\"]")));

            for restricted in [
                include_str!("../capabilities/recording-hud.json"),
                include_str!("../capabilities/temporary-text-box.json"),
                include_str!("../capabilities/session-feedback.json"),
            ] {
                assert!(!restricted.contains(identifier));
            }
        }
    }

    #[test]
    fn startup_and_recheck_share_the_model_health_use_case() {
        let desktop_source = include_str!("lib.rs");
        let model_source = include_str!("commands/model.rs");
        let startup_call = ["spawn_startup_health_check", "(app.handle().clone())"].concat();

        assert_eq!(
            desktop_source.matches(&startup_call).count(),
            1,
            "desktop startup must schedule exactly one automatic model health check"
        );
        assert!(model_source.contains("pub async fn model_check_health"));
        assert!(model_source.contains("pub(crate) fn spawn_startup_health_check"));
        assert_eq!(
            model_source.matches("check_and_refresh(").count(),
            3,
            "startup, explicit recheck, and the shared implementation must stay connected"
        );
    }

    #[test]
    fn desktop_surface_has_a_restrictive_csp_and_no_template_branding() {
        let config: Value = serde_json::from_str(include_str!("../tauri.conf.json"))
            .expect("Tauri config must remain valid JSON");
        assert_eq!(config["identifier"], "io.github.TheLearnerL.bard");
        let control_panel = &config["app"]["windows"][0];
        assert_eq!(control_panel["label"], CONTROL_PANEL_LABEL);
        assert_eq!(control_panel["width"], 960);
        assert_eq!(control_panel["height"], 680);
        assert_eq!(control_panel["minWidth"], 640);
        assert_eq!(control_panel["minHeight"], 520);
        assert_eq!(control_panel["decorations"], true);
        assert_eq!(control_panel["titleBarStyle"], "Overlay");
        assert_eq!(control_panel["hiddenTitle"], true);
        assert_eq!(control_panel["trafficLightPosition"]["x"], 24);
        assert_eq!(control_panel["trafficLightPosition"]["y"], 18);
        assert!(should_hide_window_on_close(CONTROL_PANEL_LABEL));
        assert!(!should_hide_window_on_close(RECORDING_HUD_LABEL));
        assert!(!should_hide_window_on_close(TEMPORARY_TEXT_BOX_LABEL));
        assert!(!should_hide_window_on_close(SESSION_FEEDBACK_LABEL));
        let csp = config["app"]["security"]["csp"]
            .as_str()
            .expect("desktop CSP must not be disabled");
        for directive in [
            "script-src 'self'",
            "object-src 'none'",
            "base-uri 'none'",
            "frame-ancestors 'none'",
        ] {
            assert!(
                csp.contains(directive),
                "missing CSP directive: {directive}"
            );
        }

        let html = include_str!("../../index.html");
        assert!(html.contains("<html lang=\"zh-CN\">"));
        assert!(html.contains("<title>辑语</title>"));
        assert!(!html.contains("vite.svg"));
        assert!(!html.contains("Tauri + React"));
    }

    #[test]
    fn release_helper_config_bundles_the_sandboxed_asr_worker() {
        let config: Value = serde_json::from_str(include_str!("../tauri.sidecar.conf.json"))
            .expect("Tauri Helper config must remain valid JSON");
        assert_eq!(
            config["bundle"]["macOS"]["files"]["Helpers/RemTeneASRWorker.app"],
            "binaries/RemTeneASRWorker.app"
        );
        assert_eq!(
            config["bundle"]["macOS"]["entitlements"],
            "binaries/macos/main.entitlements.plist"
        );
        assert!(config["bundle"].get("externalBin").is_none());
        assert!(config.get("plugins").is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn formal_macos_signing_defaults_to_the_official_bundle_identity() {
        let signing_environment = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../scripts/macos-asr-signing-env.sh");
        let output = Command::new("/bin/sh")
            .args([
                "-c",
                ". \"$1\"; printf '%s\\n%s\\n%s\\n' \"$REMTENE_MACOS_MAIN_BUNDLE_ID\" \"$REMTENE_MACOS_WORKER_BUNDLE_ID\" \"$REMTENE_MACOS_APP_GROUP_ID\"",
                "remtene-signing-test",
            ])
            .arg(signing_environment)
            .env_clear()
            .env("REMTENE_MACOS_BUILD_FLAVOR", "formal")
            .env("REMTENE_APPLE_TEAM_ID", "TESTTEAM00")
            .env("REMTENE_MACOS_SIGNING_IDENTITY", "test-identity")
            .output()
            .expect("macOS signing environment must execute");

        assert!(
            output.status.success(),
            "signing environment failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).expect("signing identity output must be UTF-8"),
            concat!(
                "io.github.TheLearnerL.bard\n",
                "io.github.TheLearnerL.bard.asr-worker\n",
                "TESTTEAM00.io.github.TheLearnerL.bard.asr\n"
            )
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn formal_macos_signing_rejects_pre_and_post_rename_development_identities() {
        let signing_environment = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../scripts/macos-asr-signing-env.sh");

        for development_bundle_id in ["com.bard.desktop.dev", "com.remtene.desktop.dev"] {
            let output = Command::new("/bin/sh")
                .arg(&signing_environment)
                .env_clear()
                .env("REMTENE_MACOS_BUILD_FLAVOR", "formal")
                .env("REMTENE_APPLE_TEAM_ID", "TESTTEAM00")
                .env("REMTENE_MACOS_SIGNING_IDENTITY", "test-identity")
                .env("REMTENE_MACOS_MAIN_BUNDLE_ID", development_bundle_id)
                .output()
                .expect("macOS signing environment must execute");

            assert!(
                !output.status.success(),
                "development Bundle ID must fail formal signing: {development_bundle_id}"
            );
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("formal builds require a stable non-development Bundle ID"),
                "formal signing must fail at the development identity gate"
            );
        }
    }
}
