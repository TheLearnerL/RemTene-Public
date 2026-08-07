//! Explicit permission prompts for control-panel first-run readiness.
//!
//! Microphone and Accessibility must only be requested from a user click.
//! Status probes never show a system dialog.

use remtene_contracts::{
    AppError, CONTRACT_VERSION, ErrorCategory, ErrorSeverity, MicrophonePermission,
    SystemPermission,
};
use serde::Serialize;
use tauri::{State, WebviewWindow};

use crate::{AppRuntime, WindowCommandClass, authorize_window};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PermissionStatusView {
    pub contract_version: u16,
    pub microphone: MicrophonePermission,
    pub accessibility: SystemPermission,
    /// Bundle product name shown in System Settings (may differ from binary name).
    pub app_display_name: String,
    /// Executable name that sometimes appears instead of the product name.
    pub process_name: String,
}

#[tauri::command]
pub fn permission_get_status(
    window: WebviewWindow,
    runtime: State<'_, AppRuntime>,
) -> Result<PermissionStatusView, AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;
    let status = probe_permission_status();
    runtime.update_permissions(status.microphone, status.accessibility);
    Ok(status)
}

#[tauri::command]
pub async fn permission_request_microphone(
    window: WebviewWindow,
    runtime: State<'_, AppRuntime>,
) -> Result<PermissionStatusView, AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;

    // 只触发系统权限弹框；绝不要在「请求」路径里打开系统设置。
    // 已拒绝时 macOS 不会再弹框——由前端提示用户改点「打开设置」。
    #[cfg(target_os = "macos")]
    {
        use remtene_application::ports::MicrophonePermissionPort;

        let mic = remtene_platform::MacOsMicrophonePermission;
        mic.request_recording_access().await.map_err(|error| {
            AppError::new(
                "permission.microphone_request_failed",
                ErrorCategory::Permission,
                ErrorSeverity::Error,
                true,
                format!("microphone permission request failed: {}", error.code),
            )
        })?;
    }

    let status = probe_permission_status();
    runtime.update_permissions(status.microphone, status.accessibility);
    Ok(status)
}

#[tauri::command]
pub fn permission_request_accessibility(
    window: WebviewWindow,
    runtime: State<'_, AppRuntime>,
) -> Result<PermissionStatusView, AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;

    // 只触发 AX 系统提示；打开设置留给独立的「打开设置」按钮。
    //
    // macOS 在已 trusted 时不显示提示框。继承来的授权同样算 trusted，所以在那种
    // 状态下调用这里只会静默返回——按钮看起来失效。此时唯一的出路是让用户以 .app
    // 身份重启，因此不发这个注定无效的请求，把状态原样报回去让界面说明清楚。
    #[cfg(target_os = "macos")]
    {
        use remtene_platform::AccessibilityTrust;
        if remtene_platform::MacTargetContext::accessibility_trust()
            == AccessibilityTrust::NotTrusted
        {
            let _trusted =
                remtene_platform::MacTargetContext::request_accessibility_permission_prompt();
        }
    }

    let status = probe_permission_status();
    runtime.update_permissions(status.microphone, status.accessibility);
    Ok(status)
}

#[tauri::command]
pub fn permission_open_accessibility_settings(window: WebviewWindow) -> Result<(), AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;
    open_macos_privacy_pane("Privacy_Accessibility")
}

#[tauri::command]
pub fn permission_open_microphone_settings(window: WebviewWindow) -> Result<(), AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;
    open_macos_privacy_pane("Privacy_Microphone")
}

/// Live OS permission probe used by snapshot refresh and permission commands.
pub(crate) fn probe_permission_status() -> PermissionStatusView {
    PermissionStatusView {
        contract_version: CONTRACT_VERSION,
        microphone: current_microphone_permission(),
        accessibility: current_accessibility_permission(),
        app_display_name: "辑语".to_owned(),
        process_name: std::env::current_exe()
            .ok()
            .and_then(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "remtene-desktop".to_owned()),
    }
}

fn current_microphone_permission() -> MicrophonePermission {
    #[cfg(target_os = "macos")]
    {
        use remtene_platform::MicrophoneAuthorizationStatus;
        match remtene_platform::MacOsMicrophonePermission.current_status() {
            MicrophoneAuthorizationStatus::Authorized => MicrophonePermission::Granted,
            MicrophoneAuthorizationStatus::NotDetermined => MicrophonePermission::NotDetermined,
            MicrophoneAuthorizationStatus::Denied
            | MicrophoneAuthorizationStatus::Restricted
            | MicrophoneAuthorizationStatus::Unavailable => MicrophonePermission::Denied,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        MicrophonePermission::Unknown
    }
}

fn current_accessibility_permission() -> SystemPermission {
    #[cfg(target_os = "macos")]
    {
        use remtene_platform::AccessibilityTrust;
        match remtene_platform::MacTargetContext::accessibility_trust() {
            AccessibilityTrust::Granted => SystemPermission::Granted,
            // Reporting this as Granted is what made the panel show a green light
            // while System Settings had no entry for the app at all.
            AccessibilityTrust::Inherited => SystemPermission::InheritedFromLauncher,
            // AX does not distinguish not-determined from denied.
            AccessibilityTrust::NotTrusted => SystemPermission::NotDetermined,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        SystemPermission::NotRequired
    }
}

pub(crate) fn open_macos_privacy_pane(pane: &str) -> Result<(), AppError> {
    #[cfg(target_os = "macos")]
    {
        // Sequoia+ deep link; older macOS still accepts the legacy preference URL.
        let modern = format!(
            "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?{pane}"
        );
        let legacy = format!("x-apple.systempreferences:com.apple.preference.security?{pane}");
        let opened = std::process::Command::new("open")
            .arg(&modern)
            .status()
            .ok()
            .filter(|status| status.success())
            .is_some()
            || std::process::Command::new("open")
                .arg(&legacy)
                .status()
                .ok()
                .filter(|status| status.success())
                .is_some();
        if opened {
            Ok(())
        } else {
            Err(AppError::new(
                "permission.open_settings_failed",
                ErrorCategory::Permission,
                ErrorSeverity::Error,
                true,
                "errors.permission.open_settings_failed",
            ))
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pane;
        Err(AppError::new(
            "permission.unsupported_platform",
            ErrorCategory::Permission,
            ErrorSeverity::Error,
            false,
            "errors.permission.unsupported_platform",
        ))
    }
}
