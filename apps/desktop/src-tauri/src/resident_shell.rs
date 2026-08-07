//! macOS 常驻入口与开机自启动。
//!
//! MenuBar 只负责恢复控制面板与请求正式退出；退出仍统一进入 `RunEvent::ExitRequested`
//! 的 Application 清理屏障。自启动状态只来自操作系统插件的复核结果，Renderer
//! 不直接取得插件权限。

#[cfg(target_os = "macos")]
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use remtene_contracts::{
    AppError, AutostartStatusView, CONTRACT_VERSION, ErrorCategory, ErrorSeverity,
    SetAutostartCommand, SetAutostartResult,
};
use tauri::{
    App, AppHandle, Emitter, EventTarget, Manager, WebviewWindow,
    image::Image,
    menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tauri_plugin_autostart::ManagerExt;

use crate::{
    AppRuntime, CONTROL_PANEL_LABEL, WindowCommandClass, authorize_window,
    commands::model::APP_SNAPSHOT_CHANGED_EVENT, restore_control_panel,
};

const TRAY_ID: &str = "remtene-menu-bar";
const OPEN_MENU_ID: &str = "remtene-menu-open";
const QUIT_MENU_ID: &str = "remtene-menu-quit";
pub(crate) const AUTOSTART_ENTRY_NAME: &str = "辑语";

#[cfg(target_os = "macos")]
const AUTOSTART_BUNDLE_NAME: &str = "辑语.app";
#[cfg(target_os = "macos")]
const LEGACY_AUTOSTART_ENTRY_NAME: &str = "吟誦";
#[cfg(target_os = "macos")]
const LEGACY_AUTOSTART_BUNDLE_NAME: &str = "吟誦.app";
#[cfg(target_os = "macos")]
const LEGACY_AUTOSTART_EXECUTABLE_NAME: &str = "bard-desktop";
#[cfg(target_os = "macos")]
const MAX_AUTOSTART_PLIST_BYTES: u64 = 64 * 1024;
#[cfg(target_os = "macos")]
const COMPILED_MACOS_BUILD_FLAVOR: &str = env!("REMTENE_COMPILED_MACOS_BUILD_FLAVOR");

pub(crate) fn initialize(app: &mut App) -> tauri::Result<()> {
    let status = MenuItemBuilder::with_id("remtene-menu-status", "辑语正在后台运行")
        .enabled(false)
        .build(app)?;
    let open = MenuItemBuilder::with_id(OPEN_MENU_ID, "打开辑语").build(app)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItemBuilder::with_id(QUIT_MENU_ID, "退出辑语").build(app)?;
    let menu = MenuBuilder::new(app)
        .items(&[&status, &open, &separator, &quit])
        .build()?;
    let icon = Image::from_bytes(include_bytes!("../icons/menu-bar-template@2x.png"))?;

    TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .icon(icon)
        .icon_as_template(true)
        .tooltip("辑语")
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            OPEN_MENU_ID => restore_control_panel(app, "menu_bar_open"),
            QUIT_MENU_ID => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                restore_control_panel(tray.app_handle(), "menu_bar_click");
            }
        })
        .build(app)?;

    #[cfg(target_os = "macos")]
    if let Err(error) = migrate_legacy_autostart_entry(app.handle()) {
        eprintln!("autostart brand migration unavailable: {}", error.code);
    }

    match read_enabled(app.handle()) {
        Ok(enabled) => sync_public_snapshot(app.handle(), enabled),
        Err(error) => eprintln!("autostart state unavailable at startup: {}", error.code),
    }
    Ok(())
}

fn autostart_error(code: &'static str, retryable: bool) -> AppError {
    AppError::new(
        code,
        ErrorCategory::Lifecycle,
        ErrorSeverity::Error,
        retryable,
        match code {
            "autostart.state_mismatch" => "errors.autostart.state_mismatch",
            "autostart.update_failed" => "errors.autostart.update_failed",
            _ => "errors.autostart.read_failed",
        },
    )
}

fn read_enabled(app: &AppHandle) -> Result<bool, AppError> {
    app.autolaunch()
        .is_enabled()
        .map_err(|_| autostart_error("autostart.read_failed", true))
}

#[cfg(target_os = "macos")]
fn migrate_legacy_autostart_entry(app: &AppHandle) -> Result<(), AppError> {
    let current_executable = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|_| autostart_error("autostart.read_failed", true))?;
    let home = app
        .path()
        .home_dir()
        .map_err(|_| autostart_error("autostart.read_failed", true))?;
    if !autostart_migration_is_authorized(COMPILED_MACOS_BUILD_FLAVOR, &current_executable, &home) {
        // A Tauri dev, CI, ad-hoc, translocated or not-yet-installed bundle
        // must never replace an installed formal app's LaunchAgent.
        return Ok(());
    }
    let legacy_entry = home
        .join("Library/LaunchAgents")
        .join(format!("{LEGACY_AUTOSTART_ENTRY_NAME}.plist"));
    if !legacy_autostart_entry_is_owned(&legacy_entry, &home)? {
        return Ok(());
    }

    // The plugin keys LaunchAgent state by product name and stores an absolute
    // executable path, so preserving the Bundle ID alone cannot migrate it.
    // Create and verify the current entry before removing the recognized legacy one.
    let manager = app.autolaunch();
    manager
        .enable()
        .map_err(|_| autostart_error("autostart.update_failed", true))?;
    let current_entry = home
        .join("Library/LaunchAgents")
        .join(format!("{AUTOSTART_ENTRY_NAME}.plist"));
    if !autostart_entry_matches(&current_entry, AUTOSTART_ENTRY_NAME, &current_executable)? {
        return Err(autostart_error("autostart.state_mismatch", false));
    }
    let enabled = manager
        .is_enabled()
        .map_err(|_| autostart_error("autostart.read_failed", true))?;
    if !enabled {
        return Err(autostart_error("autostart.state_mismatch", false));
    }
    fs::remove_file(legacy_entry).map_err(|_| autostart_error("autostart.update_failed", true))
}

#[cfg(target_os = "macos")]
fn is_bundled_macos_executable(path: &Path) -> bool {
    bundled_executable_is_named(path, "remtene-desktop", AUTOSTART_BUNDLE_NAME)
}

#[cfg(target_os = "macos")]
fn autostart_migration_is_authorized(build_flavor: &str, executable: &Path, home: &Path) -> bool {
    build_flavor == "formal"
        && is_bundled_macos_executable(executable)
        && (executable.starts_with("/Applications")
            || executable.starts_with(home.join("Applications")))
}

#[cfg(target_os = "macos")]
fn bundled_executable_is_named(path: &Path, executable_name: &str, bundle_name: &str) -> bool {
    let Some(bundle) = path.parent().and_then(Path::parent).and_then(Path::parent) else {
        return false;
    };
    path.is_absolute()
        && path.file_name().is_some_and(|name| name == executable_name)
        && path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "MacOS")
        && path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .is_some_and(|name| name == "Contents")
        && bundle.file_name().is_some_and(|name| name == bundle_name)
}

#[cfg(target_os = "macos")]
fn is_installed_application_path(path: &Path, home: &Path) -> bool {
    path.starts_with("/Applications") || path.starts_with(home.join("Applications"))
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, Eq, PartialEq)]
struct AutostartEntry {
    label: String,
    executable: PathBuf,
    run_at_load: bool,
}

#[cfg(target_os = "macos")]
fn read_autostart_entry(path: &Path) -> Result<Option<AutostartEntry>, AppError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(autostart_error("autostart.read_failed", true)),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_AUTOSTART_PLIST_BYTES
    {
        return Err(autostart_error("autostart.read_failed", false));
    }

    let mut options = fs::OpenOptions::new();
    options.read(true);
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .map_err(|_| autostart_error("autostart.read_failed", true))?;
    let opened_metadata = file
        .metadata()
        .map_err(|_| autostart_error("autostart.read_failed", true))?;
    if !opened_metadata.is_file() || opened_metadata.len() > MAX_AUTOSTART_PLIST_BYTES {
        return Err(autostart_error("autostart.read_failed", false));
    }

    let mut source = Vec::new();
    file.take(MAX_AUTOSTART_PLIST_BYTES + 1)
        .read_to_end(&mut source)
        .map_err(|_| autostart_error("autostart.read_failed", true))?;
    if source.len() as u64 > MAX_AUTOSTART_PLIST_BYTES {
        return Err(autostart_error("autostart.read_failed", false));
    }
    let value = plist::Value::from_reader_xml(source.as_slice())
        .map_err(|_| autostart_error("autostart.read_failed", false))?;
    let dictionary = value
        .as_dictionary()
        .ok_or_else(|| autostart_error("autostart.read_failed", false))?;
    let label = dictionary
        .get("Label")
        .and_then(plist::Value::as_string)
        .ok_or_else(|| autostart_error("autostart.read_failed", false))?;
    let arguments = dictionary
        .get("ProgramArguments")
        .and_then(plist::Value::as_array)
        .ok_or_else(|| autostart_error("autostart.read_failed", false))?;
    let [executable] = arguments.as_slice() else {
        return Err(autostart_error("autostart.read_failed", false));
    };
    let executable = executable
        .as_string()
        .ok_or_else(|| autostart_error("autostart.read_failed", false))?;
    let run_at_load = dictionary
        .get("RunAtLoad")
        .and_then(plist::Value::as_boolean)
        .ok_or_else(|| autostart_error("autostart.read_failed", false))?;

    Ok(Some(AutostartEntry {
        label: label.to_owned(),
        executable: PathBuf::from(executable),
        run_at_load,
    }))
}

#[cfg(target_os = "macos")]
fn autostart_entry_matches(
    path: &Path,
    expected_label: &str,
    expected_executable: &Path,
) -> Result<bool, AppError> {
    let Some(entry) = read_autostart_entry(path)? else {
        return Ok(false);
    };
    Ok(entry.label == expected_label
        && entry.executable == expected_executable
        && entry.run_at_load)
}

#[cfg(target_os = "macos")]
fn legacy_autostart_entry_is_owned(path: &Path, home: &Path) -> Result<bool, AppError> {
    let Some(entry) = read_autostart_entry(path)? else {
        return Ok(false);
    };
    let executable = entry.executable.as_path();
    if entry.label == LEGACY_AUTOSTART_ENTRY_NAME
        && entry.run_at_load
        && bundled_executable_is_named(
            executable,
            LEGACY_AUTOSTART_EXECUTABLE_NAME,
            LEGACY_AUTOSTART_BUNDLE_NAME,
        )
        && is_installed_application_path(executable, home)
    {
        Ok(true)
    } else {
        Err(autostart_error("autostart.read_failed", false))
    }
}

fn sync_public_snapshot(app: &AppHandle, enabled: bool) {
    let Some(runtime) = app.try_state::<AppRuntime>() else {
        return;
    };
    if !runtime.update_autostart_enabled(enabled) {
        return;
    }
    if let Err(error) = app.emit_to(
        EventTarget::webview_window(CONTROL_PANEL_LABEL),
        APP_SNAPSHOT_CHANGED_EVENT,
        runtime.snapshot(),
    ) {
        eprintln!("autostart snapshot event failed: {error}");
    }
}

#[tauri::command]
pub(crate) fn autostart_get_status(window: WebviewWindow) -> Result<AutostartStatusView, AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;
    let enabled = read_enabled(window.app_handle())?;
    sync_public_snapshot(window.app_handle(), enabled);
    Ok(AutostartStatusView {
        contract_version: CONTRACT_VERSION,
        enabled,
    })
}

#[tauri::command]
pub(crate) fn autostart_set_enabled(
    window: WebviewWindow,
    command: SetAutostartCommand,
) -> Result<SetAutostartResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::Settings)?;
    if command.contract_version != CONTRACT_VERSION {
        return Err(AppError::new(
            "ipc.contract_mismatch",
            ErrorCategory::Security,
            ErrorSeverity::Error,
            false,
            "errors.ipc.contract_mismatch",
        ));
    }

    let manager = window.app_handle().autolaunch();
    let update = if command.enabled {
        manager.enable()
    } else {
        manager.disable()
    };
    update.map_err(|_| autostart_error("autostart.update_failed", true))?;

    let enabled = read_enabled(window.app_handle())?;
    if enabled != command.enabled {
        return Err(autostart_error("autostart.state_mismatch", false));
    }
    sync_public_snapshot(window.app_handle(), enabled);

    Ok(SetAutostartResult {
        contract_version: CONTRACT_VERSION,
        request_id: command.request_id,
        status: AutostartStatusView {
            contract_version: CONTRACT_VERSION,
            enabled,
        },
    })
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn test_entry(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "remtene-autostart-migration-{name}-{}.plist",
            Uuid::new_v4()
        ))
    }

    #[test]
    fn recognizes_only_the_pre_rename_launch_agent_owned_by_the_app() {
        let path = test_entry("owned");
        fs::write(
            &path,
            r#"<?xml version="1.0"?><plist><dict><key>Label</key><string>吟誦</string><key>ProgramArguments</key><array><string>/Applications/吟誦.app/Contents/MacOS/bard-desktop</string></array><key>RunAtLoad</key><true/></dict></plist>"#,
        )
        .expect("write legacy LaunchAgent");

        let home = Path::new("/Users/tester");
        assert!(legacy_autostart_entry_is_owned(&path, home).expect("inspect legacy LaunchAgent"));
        fs::remove_file(path).expect("remove test LaunchAgent");
    }

    #[test]
    fn migration_requires_a_formal_bundle_in_an_installed_application_directory() {
        let bundled = Path::new("/Applications/辑语.app/Contents/MacOS/remtene-desktop");
        let home = Path::new("/Users/tester");
        assert!(autostart_migration_is_authorized("formal", bundled, home));
        assert!(!autostart_migration_is_authorized("adhoc", bundled, home));
        assert!(!autostart_migration_is_authorized(
            "unverified",
            bundled,
            home
        ));
        assert!(autostart_migration_is_authorized(
            "formal",
            Path::new("/Users/tester/Applications/辑语.app/Contents/MacOS/remtene-desktop"),
            home
        ));
        assert!(!autostart_migration_is_authorized(
            "formal",
            Path::new("/workspace/target/debug/remtene-desktop"),
            home
        ));
        assert!(!autostart_migration_is_authorized(
            "formal",
            Path::new("/Applications/辑语.app/Contents/MacOS/bard-desktop"),
            home
        ));
        assert!(!autostart_migration_is_authorized(
            "formal",
            Path::new(
                "/private/var/folders/AppTranslocation/辑语.app/Contents/MacOS/remtene-desktop"
            ),
            home
        ));
    }

    #[test]
    fn rejects_an_unrecognized_or_symlinked_legacy_launch_agent() {
        use std::os::unix::fs::symlink;

        let unrelated = test_entry("unrelated");
        fs::write(
            &unrelated,
            r#"<?xml version="1.0"?><plist><dict><key>Label</key><string>other</string><key>ProgramArguments</key><array><string>/Applications/吟誦.app/Contents/MacOS/bard-desktop</string></array><key>RunAtLoad</key><true/></dict></plist>"#,
        )
        .expect("write unrelated LaunchAgent");
        let home = Path::new("/Users/tester");
        assert!(legacy_autostart_entry_is_owned(&unrelated, home).is_err());

        let symlink_path = test_entry("symlink");
        symlink(&unrelated, &symlink_path).expect("create LaunchAgent symlink");
        assert!(legacy_autostart_entry_is_owned(&symlink_path, home).is_err());

        fs::remove_file(symlink_path).expect("remove symlink");
        fs::remove_file(unrelated).expect("remove unrelated LaunchAgent");
    }
}
