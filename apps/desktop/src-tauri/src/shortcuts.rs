//! 全局快捷键触发层（UI-020）
//!
//! 快捷键只是触发来源，本身不承载产品规则：按下与放开都转成对同一个 Application
//! 用例的调用，权限、安全输入、目标校验与单工约束仍由编排器和平台适配层负责。
//!
//! 绑定由普通设置持久化；运行时替换必须先通过真实系统注册，再由 Application
//! 提交设置。未配置时保持未绑定，遵守首次安装不预占系统快捷键的正式产品合同。

use std::sync::Arc;

#[cfg(target_os = "macos")]
use std::sync::mpsc;

use remtene_application::ports::{PortError, PortFuture, RecordingShortcutPort, SettingsStore};
use remtene_application::{FinishOutcome, StartOutcome, TranscriptionOrchestrator};
use remtene_domain::{RecordingMode, RecordingShortcut, SessionId};
use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Shortcut, ShortcutState};

#[cfg(target_os = "macos")]
use remtene_platform::{MacModifierKey, MacModifierShortcutError, replace_mac_modifier_shortcut};

use crate::composition_root::CompositionRoot;
use crate::recording_hud::RecordingHudController;
use crate::session_projection::failure_error_code;

/// 建立带触发处理器的全局快捷键插件。
pub(crate) fn plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri_plugin_global_shortcut::Builder::new()
        .with_handler(
            |app: &AppHandle, _shortcut: &Shortcut, event| match event.state {
                ShortcutState::Pressed => handle_press(app),
                ShortcutState::Released => handle_release(app),
            },
        )
        .build()
}

/// Tauri 全局快捷键 Port。系统冲突只有真实调用 `register` 才能证明，
/// `is_registered` 只知道本应用，不能拿来冒充跨应用冲突检测。
pub(crate) struct TauriRecordingShortcutPort {
    app: AppHandle,
}

impl TauriRecordingShortcutPort {
    #[must_use]
    pub(crate) fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl RecordingShortcutPort for TauriRecordingShortcutPort {
    fn replace_binding(
        &self,
        current: Option<RecordingShortcut>,
        next: Option<RecordingShortcut>,
    ) -> PortFuture<'_, Result<(), PortError>> {
        let app = self.app.clone();
        Box::pin(async move { replace_registered_binding(&app, current.as_ref(), next.as_ref()) })
    }
}

/// 注册设置文件中的录音快捷键。
///
/// 返回已注册的绑定字符串；未配置时返回 `None`，此时只能通过控制面板触发。
pub(crate) fn register_recording_shortcut(
    app: &AppHandle,
    binding: Option<&RecordingShortcut>,
) -> Result<Option<String>, PortError> {
    let Some(binding) = binding else {
        return Ok(None);
    };

    let shortcut = parse_shortcut(binding)?;
    register_binding(app, shortcut)?;
    Ok(Some(binding.as_str().to_owned()))
}

fn replace_registered_binding(
    app: &AppHandle,
    current: Option<&RecordingShortcut>,
    next: Option<&RecordingShortcut>,
) -> Result<(), PortError> {
    let current = current.map(parse_shortcut).transpose()?;
    let next = next.map(parse_shortcut).transpose()?;
    if current == next {
        if let Some(next) = next {
            ensure_registered(app, next)?;
        }
        return Ok(());
    }

    // AppKit 可以先创建新 monitor，再释放旧 monitor。纯修饰键之间直接走这条
    // 原子替换，避免“先注销旧键、再尝试新键”扩大失败窗口。
    #[cfg(target_os = "macos")]
    if let (Some(ParsedShortcut::Modifier(_)), Some(ParsedShortcut::Modifier(next_modifier))) =
        (current, next)
    {
        return set_modifier_binding(app, Some(next_modifier));
    }

    if let Some(current) = current {
        unregister_binding(app, current)?;
    }

    if let Some(next) = next
        && let Err(next_error) = register_binding(app, next)
    {
        if let Some(current) = current
            && register_binding(app, current).is_err()
        {
            return Err(shortcut_error("shortcut.rollback_failed", false));
        }
        return Err(next_error);
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParsedShortcut {
    System(Shortcut),
    #[cfg(target_os = "macos")]
    Modifier(MacModifierKey),
}

fn parse_shortcut(binding: &RecordingShortcut) -> Result<ParsedShortcut, PortError> {
    if is_pure_modifier_binding(binding.as_str()) {
        #[cfg(target_os = "macos")]
        {
            let modifier = MacModifierKey::from_binding(binding.as_str())
                .ok_or_else(|| shortcut_error("shortcut.invalid", false))?;
            return Ok(ParsedShortcut::Modifier(modifier));
        }
        #[cfg(not(target_os = "macos"))]
        {
            return Err(shortcut_error("shortcut.unsupported", false));
        }
    }

    let shortcut: Shortcut = binding
        .as_str()
        .parse()
        .map_err(|_| shortcut_error("shortcut.invalid", false))?;
    if shortcut.mods.is_empty() && !is_safe_bare_key(shortcut.key) {
        return Err(shortcut_error("shortcut.unsafe_bare_key", false));
    }
    Ok(ParsedShortcut::System(shortcut))
}

fn is_pure_modifier_binding(binding: &str) -> bool {
    matches!(
        binding,
        "MetaLeft"
            | "MetaRight"
            | "ControlLeft"
            | "ControlRight"
            | "AltLeft"
            | "AltRight"
            | "ShiftLeft"
            | "ShiftRight"
    )
}

fn is_safe_bare_key(key: Code) -> bool {
    matches!(
        key,
        Code::F1
            | Code::F2
            | Code::F3
            | Code::F4
            | Code::F5
            | Code::F6
            | Code::F7
            | Code::F8
            | Code::F9
            | Code::F10
            | Code::F11
            | Code::F12
            | Code::F13
            | Code::F14
            | Code::F15
            | Code::F16
            | Code::F17
            | Code::F18
            | Code::F19
            | Code::F20
    )
}

fn ensure_registered(app: &AppHandle, binding: ParsedShortcut) -> Result<(), PortError> {
    match binding {
        ParsedShortcut::System(shortcut) => {
            if !app.global_shortcut().is_registered(shortcut) {
                app.global_shortcut()
                    .register(shortcut)
                    .map_err(|_| shortcut_error("shortcut.register_failed", false))?;
            }
            Ok(())
        }
        #[cfg(target_os = "macos")]
        ParsedShortcut::Modifier(modifier) => set_modifier_binding(app, Some(modifier)),
    }
}

fn register_binding(app: &AppHandle, binding: ParsedShortcut) -> Result<(), PortError> {
    match binding {
        ParsedShortcut::System(shortcut) => app
            .global_shortcut()
            .register(shortcut)
            .map_err(|_| shortcut_error("shortcut.register_failed", false)),
        #[cfg(target_os = "macos")]
        ParsedShortcut::Modifier(modifier) => set_modifier_binding(app, Some(modifier)),
    }
}

fn unregister_binding(app: &AppHandle, binding: ParsedShortcut) -> Result<(), PortError> {
    match binding {
        ParsedShortcut::System(shortcut) => app
            .global_shortcut()
            .unregister(shortcut)
            .map_err(|_| shortcut_error("shortcut.unregister_failed", true)),
        #[cfg(target_os = "macos")]
        ParsedShortcut::Modifier(_) => set_modifier_binding(app, None)
            .map_err(|_| shortcut_error("shortcut.unregister_failed", true)),
    }
}

#[cfg(target_os = "macos")]
fn set_modifier_binding(
    app: &AppHandle,
    modifier: Option<MacModifierKey>,
) -> Result<(), PortError> {
    let (sender, receiver) = mpsc::channel();
    let press_app = app.clone();
    let release_app = app.clone();
    app.run_on_main_thread(move || {
        let result = replace_mac_modifier_shortcut(
            modifier,
            Arc::new(move || handle_press(&press_app)),
            Arc::new(move || handle_release(&release_app)),
        );
        let _ = sender.send(result);
    })
    .map_err(|_| shortcut_error("shortcut.register_failed", false))?;

    receiver
        .recv()
        .map_err(|_| shortcut_error("shortcut.register_failed", false))?
        .map_err(|error| match error {
            MacModifierShortcutError::PermissionRequired => {
                shortcut_error("shortcut.accessibility_required", false)
            }
            MacModifierShortcutError::WrongThread
            | MacModifierShortcutError::MonitorUnavailable => {
                shortcut_error("shortcut.register_failed", false)
            }
        })
}

fn shortcut_error(code: &str, retryable: bool) -> PortError {
    PortError {
        code: code.to_owned(),
        safe_message_key: format!("errors.{code}"),
        retryable,
    }
}

/// 当前可结束的 Session；HUD 投影是 Presentation 侧唯一的公开会话状态。
fn finishable_session(app: &AppHandle) -> Option<SessionId> {
    let controller = app.try_state::<Arc<RecordingHudController>>()?;
    let snapshot = controller.current()?;
    snapshot
        .can_finish
        .then(|| SessionId::from_uuid(snapshot.session_id))
}

fn runtime_parts(
    app: &AppHandle,
) -> Option<(Arc<TranscriptionOrchestrator>, Arc<dyn SettingsStore>)> {
    let root = app.try_state::<CompositionRoot>()?;
    Some((Arc::clone(&root.orchestrator), Arc::clone(&root.settings)))
}

fn handle_press(app: &AppHandle) {
    let Some((orchestrator, settings)) = runtime_parts(app) else {
        return;
    };
    // Toggle 模式的第二次按键与 HUD 对号是等价结束事件；Push-to-Talk 只在放开时结束。
    let finishable = finishable_session(app);

    tauri::async_runtime::spawn(async move {
        if let Some(session_id) = finishable {
            if matches!(recording_mode(&settings).await, Some(RecordingMode::Toggle)) {
                finish(&orchestrator, session_id).await;
            }
            return;
        }

        match orchestrator.start().await {
            Ok(StartOutcome::Started { .. }) => {}
            Ok(outcome) => eprintln!("快捷键触发未开始录音：{outcome:?}"),
            Err(error) => eprintln!("快捷键触发失败：{error}"),
        }
    });
}

fn handle_release(app: &AppHandle) {
    let Some((orchestrator, settings)) = runtime_parts(app) else {
        return;
    };
    let Some(session_id) = finishable_session(app) else {
        return;
    };

    tauri::async_runtime::spawn(async move {
        if matches!(
            recording_mode(&settings).await,
            Some(RecordingMode::PushToTalk)
        ) {
            finish(&orchestrator, session_id).await;
        }
    });
}

async fn finish(orchestrator: &TranscriptionOrchestrator, session_id: SessionId) {
    match orchestrator.finish_recording(session_id).await {
        Ok(FinishOutcome::Failed(category)) => {
            eprintln!("结束录音失败：{}", failure_error_code(category));
        }
        Ok(
            FinishOutcome::Completed(_)
            | FinishOutcome::NoSpeech
            | FinishOutcome::Discarded
            | FinishOutcome::NotRecording,
        ) => {}
        Err(error) => eprintln!("结束录音失败：{error}"),
    }
}

async fn recording_mode(settings: &Arc<dyn SettingsStore>) -> Option<RecordingMode> {
    match settings.load().await {
        Ok(snapshot) => Some(snapshot.recording_mode()),
        Err(error) => {
            eprintln!("读取录音模式失败：{}", error.code);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ParsedShortcut, parse_shortcut};
    use remtene_domain::RecordingShortcut;
    #[cfg(target_os = "macos")]
    use remtene_platform::MacModifierKey;

    #[test]
    fn valid_binding_is_accepted() {
        let binding = RecordingShortcut::new("CommandOrControl+Shift+KeyR").unwrap();
        assert!(matches!(
            parse_shortcut(&binding),
            Ok(ParsedShortcut::System(_))
        ));
    }

    #[test]
    fn bare_function_key_is_accepted() {
        let binding = RecordingShortcut::new("F20").unwrap();
        assert!(matches!(
            parse_shortcut(&binding),
            Ok(ParsedShortcut::System(_))
        ));
    }

    #[test]
    fn daily_bare_keys_are_rejected_to_avoid_hijacking_normal_input() {
        for value in [
            "KeyR",
            "Digit4",
            "Space",
            "Enter",
            "ArrowLeft",
            "Backspace",
            "F21",
        ] {
            let binding = RecordingShortcut::new(value).unwrap();
            assert_eq!(
                parse_shortcut(&binding).unwrap_err().code,
                "shortcut.unsafe_bare_key"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pure_modifier_preserves_physical_side() {
        let binding = RecordingShortcut::new("MetaRight").unwrap();
        assert_eq!(
            parse_shortcut(&binding),
            Ok(ParsedShortcut::Modifier(MacModifierKey::CommandRight))
        );
    }
}
