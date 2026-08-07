//! 设置读写命令 - Tauri IPC 入口点
//!
//! 本模块只投影 Control Panel 所需的非秘密设置；API Key 继续走专用
//! Secret IPC。剪贴板兼容投递默认关闭；启用后，
//! Electron/Chromium 类应用即使不暴露可验证的 `AXSelectedText` 控件，
//! 也能向用户当前选择的键盘焦点派发一次 ⌘V。这个开关必须由用户明确
//! 打开，因为通用路径不检查密码框，也可能遵循目标应用语义替换当前选区。

use remtene_application::{
    HistorySettingsError, LlmConfigurationError, RecordingSettingsController,
    RecordingSettingsError, SystemSettingsError,
};
use remtene_contracts::{
    AppError, CONTRACT_VERSION, ErrorCategory, ErrorSeverity, HistoryPolicyView, LlmSettingsView,
    ProcessingModeView, RecordingModeView, SetAutoCopyResultCommand, SetAutoCopyResultResult,
    SetHistoryEnabledCommand, SetHistoryEnabledResult, SetHistoryLimitCommand,
    SetHistoryLimitResult, SetHistoryRetentionCommand, SetHistoryRetentionResult,
    SetLlmSettingsCommand, SetLlmSettingsResult, SetLocalDiagnosticsCommand,
    SetLocalDiagnosticsResult, SetRecordingPreferencesCommand, SetRecordingPreferencesResult,
    SetRecordingShortcutCommand, SetRecordingShortcutResult, SetTextProcessingSettingsCommand,
    SetTextProcessingSettingsResult, SettingsView,
};
use remtene_domain::{
    LlmNonSecretSettings, ProcessingMode, RecordingMode, RecordingShortcut, SettingsSnapshot,
};
use tauri::{Emitter, EventTarget, Manager, State, WebviewWindow};

use crate::composition_root::CompositionRoot;
use crate::{
    AppRuntime, CONTROL_PANEL_LABEL, WindowCommandClass, authorize_window,
    commands::model::APP_SNAPSHOT_CHANGED_EVENT,
};

fn settings_view(snapshot: &SettingsSnapshot) -> SettingsView {
    SettingsView {
        contract_version: CONTRACT_VERSION,
        version: snapshot.version(),
        recording_mode: match snapshot.recording_mode() {
            RecordingMode::Toggle => RecordingModeView::Toggle,
            RecordingMode::PushToTalk => RecordingModeView::PushToTalk,
        },
        max_recording_duration_seconds: snapshot.max_recording_duration().as_secs(),
        recording_shortcut: snapshot
            .recording_shortcut()
            .map(|shortcut| shortcut.as_str().to_owned()),
        processing_mode: match snapshot.processing_mode() {
            ProcessingMode::Raw => ProcessingModeView::Raw,
            ProcessingMode::Faithful => ProcessingModeView::Faithful,
            ProcessingMode::Structured => ProcessingModeView::Structured,
        },
        read_selected_text: snapshot.read_selected_text(),
        clipboard_bridge_allowed: snapshot.clipboard_bridge_allowed(),
        auto_copy_result: snapshot.auto_copy_result(),
        local_diagnostics_enabled: snapshot.local_diagnostics_enabled(),
        history_policy: {
            let policy = snapshot.history_policy();
            HistoryPolicyView {
                enabled: policy.enabled,
                limit: policy.limit,
                retention_days: policy.retention_days,
            }
        },
        llm: snapshot.llm().map(|llm| LlmSettingsView {
            base_url: llm.base_url().to_owned(),
            model: llm.model().to_owned(),
        }),
    }
}

#[tauri::command]
pub async fn settings_set_recording_preferences(
    window: WebviewWindow,
    controller: State<'_, RecordingSettingsController>,
    command: SetRecordingPreferencesCommand,
) -> Result<SetRecordingPreferencesResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;
    if command.contract_version != CONTRACT_VERSION {
        return Err(contract_error());
    }

    let recording_mode = match command.recording_mode {
        RecordingModeView::Toggle => RecordingMode::Toggle,
        RecordingModeView::PushToTalk => RecordingMode::PushToTalk,
    };
    let stored = controller
        .set_recording_preferences(
            command.expected_version,
            recording_mode,
            command.max_recording_duration_seconds,
        )
        .await
        .map_err(recording_settings_error)?;

    Ok(SetRecordingPreferencesResult {
        contract_version: CONTRACT_VERSION,
        request_id: command.request_id,
        settings: settings_view(&stored),
    })
}

#[tauri::command]
pub async fn settings_set_recording_shortcut(
    window: WebviewWindow,
    runtime: State<'_, AppRuntime>,
    controller: State<'_, RecordingSettingsController>,
    command: SetRecordingShortcutCommand,
) -> Result<SetRecordingShortcutResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;
    if command.contract_version != CONTRACT_VERSION {
        return Err(contract_error());
    }

    let recording_shortcut = command
        .recording_shortcut
        .map(RecordingShortcut::new)
        .transpose()
        .map_err(|_| recording_settings_error(RecordingSettingsError::InvalidShortcut))?;
    let stored = controller
        .set_recording_shortcut(command.expected_version, recording_shortcut)
        .await
        .map_err(recording_settings_error)?;

    runtime.update_shortcut_configured(stored.recording_shortcut().is_some());
    if let Err(error) = window.app_handle().emit_to(
        EventTarget::webview_window(CONTROL_PANEL_LABEL),
        APP_SNAPSHOT_CHANGED_EVENT,
        runtime.snapshot(),
    ) {
        eprintln!("shortcut snapshot event failed: {error}");
    }

    Ok(SetRecordingShortcutResult {
        contract_version: CONTRACT_VERSION,
        request_id: command.request_id,
        settings: settings_view(&stored),
    })
}

fn store_error(detail: &str) -> AppError {
    AppError::new(
        "settings.store_unavailable",
        ErrorCategory::Storage,
        ErrorSeverity::Error,
        true,
        detail,
    )
}

#[tauri::command]
pub async fn settings_get(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
) -> Result<SettingsView, AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;
    let snapshot = composition
        .llm_configuration
        .get_settings()
        .await
        .map_err(|_| store_error("errors.settings.load_failed"))?;
    Ok(settings_view(&snapshot))
}

#[tauri::command]
pub async fn settings_set_history_enabled(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
    command: SetHistoryEnabledCommand,
) -> Result<SetHistoryEnabledResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;
    if command.contract_version != CONTRACT_VERSION {
        return Err(contract_error());
    }

    let stored = composition
        .history_settings
        .set_enabled(command.expected_version, command.enabled)
        .await
        .map_err(history_settings_error)?;
    Ok(SetHistoryEnabledResult {
        contract_version: CONTRACT_VERSION,
        request_id: command.request_id,
        settings: settings_view(&stored),
    })
}

#[tauri::command]
pub async fn settings_set_history_limit(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
    command: SetHistoryLimitCommand,
) -> Result<SetHistoryLimitResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;
    if command.contract_version != CONTRACT_VERSION {
        return Err(contract_error());
    }

    let stored = composition
        .history_settings
        .set_limit(
            command.expected_version,
            command.limit,
            command.acknowledge_data_loss,
        )
        .await
        .map_err(history_settings_error)?;
    Ok(SetHistoryLimitResult {
        contract_version: CONTRACT_VERSION,
        request_id: command.request_id,
        settings: settings_view(&stored),
    })
}

#[tauri::command]
pub async fn settings_set_history_retention(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
    command: SetHistoryRetentionCommand,
) -> Result<SetHistoryRetentionResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;
    if command.contract_version != CONTRACT_VERSION {
        return Err(contract_error());
    }

    let stored = composition
        .history_settings
        .set_retention_days(
            command.expected_version,
            command.retention_days,
            command.acknowledge_data_loss,
        )
        .await
        .map_err(history_settings_error)?;
    Ok(SetHistoryRetentionResult {
        contract_version: CONTRACT_VERSION,
        request_id: command.request_id,
        settings: settings_view(&stored),
    })
}

#[tauri::command]
pub async fn settings_set_auto_copy_result(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
    command: SetAutoCopyResultCommand,
) -> Result<SetAutoCopyResultResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;
    if command.contract_version != CONTRACT_VERSION {
        return Err(contract_error());
    }
    let stored = composition
        .system_settings
        .set_auto_copy_result(command.expected_version, command.enabled)
        .await
        .map_err(system_settings_error)?;
    Ok(SetAutoCopyResultResult {
        contract_version: CONTRACT_VERSION,
        request_id: command.request_id,
        settings: settings_view(&stored),
    })
}

#[tauri::command]
pub async fn settings_set_local_diagnostics(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
    command: SetLocalDiagnosticsCommand,
) -> Result<SetLocalDiagnosticsResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;
    if command.contract_version != CONTRACT_VERSION {
        return Err(contract_error());
    }
    let stored = composition
        .system_settings
        .set_local_diagnostics_enabled(command.expected_version, command.enabled)
        .await
        .map_err(system_settings_error)?;
    Ok(SetLocalDiagnosticsResult {
        contract_version: CONTRACT_VERSION,
        request_id: command.request_id,
        settings: settings_view(&stored),
    })
}

#[tauri::command]
pub fn diagnostics_open_directory(window: WebviewWindow) -> Result<(), AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;
    let directory = window
        .app_handle()
        .path()
        .app_cache_dir()
        .map_err(|_| diagnostics_directory_error("errors.diagnostics.path_unavailable"))?
        .join("RemTene")
        .join("logs");
    std::fs::create_dir_all(&directory)
        .map_err(|_| diagnostics_directory_error("errors.diagnostics.directory_unavailable"))?;

    #[cfg(target_os = "macos")]
    let status = std::process::Command::new("/usr/bin/open")
        .arg(&directory)
        .status();
    #[cfg(target_os = "windows")]
    let status = std::process::Command::new("explorer.exe")
        .arg(&directory)
        .status();
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let status = std::process::Command::new("xdg-open")
        .arg(&directory)
        .status();

    match status {
        Ok(status) if status.success() => Ok(()),
        _ => Err(diagnostics_directory_error(
            "errors.diagnostics.open_directory_failed",
        )),
    }
}

/// 翻转剪贴板桥接开关。
///
/// 走 `replace(expected_version, ..)` 而不是盲写：设置可能被别处改过，
/// 版本不匹配时宁可让前端重读也不覆盖别人的修改。
#[tauri::command]
pub async fn settings_set_clipboard_bridge(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
    allowed: bool,
) -> Result<SettingsView, AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;

    let stored = composition
        .llm_configuration
        .set_clipboard_bridge_allowed(allowed)
        .await
        .map_err(|_| store_error("errors.settings.save_failed"))?;

    Ok(settings_view(&stored))
}

/// 原子更新状态页中的处理方式与选区读取偏好。
///
/// 两项共享一次 CAS（比较并交换）写入；活动任务期间由 Application 的配置
/// 门闩拒绝更新，保证当前 Session 继续使用启动时冻结的完整设置。
#[tauri::command]
pub async fn settings_set_text_processing(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
    command: SetTextProcessingSettingsCommand,
) -> Result<SetTextProcessingSettingsResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;
    if command.contract_version != CONTRACT_VERSION {
        return Err(contract_error());
    }

    let processing_mode = match command.processing_mode {
        ProcessingModeView::Raw => ProcessingMode::Raw,
        ProcessingModeView::Faithful => ProcessingMode::Faithful,
        ProcessingModeView::Structured => ProcessingMode::Structured,
    };
    let stored = composition
        .llm_configuration
        .set_text_processing_settings(
            command.expected_version,
            processing_mode,
            command.read_selected_text,
        )
        .await
        .map_err(text_processing_error)?;

    Ok(SetTextProcessingSettingsResult {
        contract_version: CONTRACT_VERSION,
        request_id: command.request_id,
        settings: settings_view(&stored),
    })
}

#[tauri::command]
pub async fn settings_set_llm(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
    command: SetLlmSettingsCommand,
) -> Result<SetLlmSettingsResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;
    if command.contract_version != CONTRACT_VERSION {
        return Err(contract_error());
    }

    let llm = command
        .llm
        .map(|llm| LlmNonSecretSettings::new(llm.base_url, llm.model))
        .transpose()
        .map_err(|_| {
            AppError::new(
                "settings.llm_invalid",
                ErrorCategory::Llm,
                ErrorSeverity::Error,
                false,
                "errors.settings.llm_invalid",
            )
        })?;
    let stored = composition
        .llm_configuration
        .set_llm_settings(command.expected_version, llm)
        .await
        .map_err(super::secrets::configuration_error)?;

    Ok(SetLlmSettingsResult {
        contract_version: CONTRACT_VERSION,
        request_id: command.request_id,
        settings: settings_view(&stored),
    })
}

fn contract_error() -> AppError {
    AppError::new(
        "ipc.contract_mismatch",
        ErrorCategory::Security,
        ErrorSeverity::Error,
        false,
        "errors.ipc.contract_mismatch",
    )
}

fn text_processing_error(error: LlmConfigurationError) -> AppError {
    match error {
        LlmConfigurationError::Busy => AppError::new(
            "settings.busy",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Warning,
            true,
            "errors.settings.busy",
        ),
        LlmConfigurationError::RuntimeUnavailable => AppError::new(
            "settings.runtime_unavailable",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Error,
            true,
            "errors.settings.runtime_unavailable",
        ),
        LlmConfigurationError::InvalidConfiguration(error) | LlmConfigurationError::Port(error) => {
            AppError::new(
                error.code,
                ErrorCategory::Storage,
                ErrorSeverity::Error,
                error.retryable,
                error.safe_message_key,
            )
        }
        LlmConfigurationError::NotConfigured
        | LlmConfigurationError::RecoveryRequired
        | LlmConfigurationError::InvalidSecret
        | LlmConfigurationError::SecretVerificationFailed => AppError::new(
            "settings.update_failed",
            ErrorCategory::Storage,
            ErrorSeverity::Error,
            false,
            "errors.settings.update_failed",
        ),
    }
}

fn recording_settings_error(error: RecordingSettingsError) -> AppError {
    match error {
        RecordingSettingsError::Busy => AppError::new(
            "settings.busy",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Warning,
            true,
            "errors.settings.busy",
        ),
        RecordingSettingsError::RuntimeUnavailable => AppError::new(
            "settings.runtime_unavailable",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Error,
            true,
            "errors.settings.runtime_unavailable",
        ),
        RecordingSettingsError::InvalidDuration => AppError::new(
            "settings.recording_duration_invalid",
            ErrorCategory::Storage,
            ErrorSeverity::Warning,
            false,
            "errors.settings.recording_duration_invalid",
        ),
        RecordingSettingsError::InvalidShortcut => AppError::new(
            "shortcut.invalid",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Warning,
            false,
            "errors.shortcut.invalid",
        ),
        RecordingSettingsError::ShortcutRollbackFailed { .. } => AppError::new(
            "shortcut.rollback_failed",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Blocking,
            false,
            "errors.shortcut.rollback_failed",
        ),
        RecordingSettingsError::Port(error) => AppError::new(
            error.code,
            if error.safe_message_key.starts_with("errors.shortcut.") {
                ErrorCategory::Lifecycle
            } else {
                ErrorCategory::Storage
            },
            ErrorSeverity::Error,
            error.retryable,
            error.safe_message_key,
        ),
    }
}

fn history_settings_error(error: HistorySettingsError) -> AppError {
    match error {
        HistorySettingsError::Busy => AppError::new(
            "settings.busy",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Warning,
            true,
            "errors.settings.busy",
        ),
        HistorySettingsError::RuntimeUnavailable => AppError::new(
            "settings.runtime_unavailable",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Error,
            true,
            "errors.settings.runtime_unavailable",
        ),
        HistorySettingsError::Quitting => AppError::new(
            "settings.quitting",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Warning,
            true,
            "errors.settings.quitting",
        ),
        HistorySettingsError::ConfirmationRequired => AppError::new(
            "settings.history_limit_confirmation_required",
            ErrorCategory::Storage,
            ErrorSeverity::Warning,
            false,
            "errors.settings.history_limit_confirmation_required",
        ),
        HistorySettingsError::RetentionConfirmationRequired => AppError::new(
            "settings.history_retention_confirmation_required",
            ErrorCategory::Storage,
            ErrorSeverity::Warning,
            false,
            "errors.settings.history_retention_confirmation_required",
        ),
        HistorySettingsError::InvalidPolicy => AppError::new(
            "settings.history_policy_invalid",
            ErrorCategory::Storage,
            ErrorSeverity::Error,
            false,
            "errors.settings.history_policy_invalid",
        ),
        HistorySettingsError::Port(error) => AppError::new(
            error.code,
            ErrorCategory::Storage,
            ErrorSeverity::Error,
            error.retryable,
            error.safe_message_key,
        ),
    }
}

fn system_settings_error(error: SystemSettingsError) -> AppError {
    match error {
        SystemSettingsError::Busy => AppError::new(
            "settings.busy",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Warning,
            true,
            "errors.settings.busy",
        ),
        SystemSettingsError::RuntimeUnavailable => AppError::new(
            "settings.runtime_unavailable",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Error,
            true,
            "errors.settings.runtime_unavailable",
        ),
        SystemSettingsError::Port(error) => AppError::new(
            error.code,
            ErrorCategory::Storage,
            ErrorSeverity::Error,
            error.retryable,
            error.safe_message_key,
        ),
    }
}

fn diagnostics_directory_error(message_key: &str) -> AppError {
    AppError::new(
        "diagnostics.directory_unavailable",
        ErrorCategory::Storage,
        ErrorSeverity::Error,
        true,
        message_key,
    )
}
