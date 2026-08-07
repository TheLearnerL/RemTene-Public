#![allow(unsafe_code)]

use tauri::{Manager, WebviewWindow};
use tauri_nspanel::{StyleMask, WebviewWindowExt, tauri_panel};

tauri_panel! {
    RecordingHudPanel {
        config: {
            can_become_key_window: false,
            can_become_main_window: false,
            is_floating_panel: true,
            becomes_key_only_if_needed: true
        }
    }
}

/// Converts the existing React/WebView HUD into a real non-activating macOS panel.
///
/// `focusable(false)` on a regular `NSWindow` only prevents keyboard focus; clicking
/// that window can still activate RemTene and displace the target application. AppKit
/// grants click-without-activation semantics only to an `NSPanel` carrying the
/// `NonactivatingPanel` style.
pub(super) fn configure(window: &WebviewWindow, corner_radius: f64) -> tauri::Result<()> {
    let panel = window.to_panel::<RecordingHudPanel>()?;
    panel.set_style_mask(recording_hud_style_mask().value());
    panel.set_floating_panel(true);
    panel.set_becomes_key_only_if_needed(true);
    panel.set_hides_on_deactivate(false);
    panel.set_has_shadow(false);
    panel.set_transparent(true);
    panel.set_corner_radius(corner_radius);

    // `tauri-nspanel` 会设置 content view 的 cornerRadius，但 CALayer 默认不会
    // 裁切子层。显式开启 masksToBounds，确保 WKWebView 的矩形底色也被裁成胶囊，
    // 无需为整个应用启用 Tauri 的 macos-private-api feature。
    let content_view = panel.content_view();
    unsafe {
        let layer: tauri_nspanel::objc2::rc::Retained<tauri_nspanel::objc2_foundation::NSObject> =
            tauri_nspanel::objc2::msg_send![&*content_view, layer];
        let _: () = tauri_nspanel::objc2::msg_send![&*layer, setMasksToBounds: true];
    }
    Ok(())
}

fn recording_hud_style_mask() -> StyleMask {
    StyleMask::empty().borderless().nonactivating_panel()
}

#[cfg(test)]
mod tests {
    use tauri_nspanel::panel::NSWindowStyleMask;

    use super::recording_hud_style_mask;

    #[test]
    fn hud_style_is_borderless_and_nonactivating() {
        let style = recording_hud_style_mask().value();

        assert!(style.contains(NSWindowStyleMask::NonactivatingPanel));
        assert!(!style.contains(NSWindowStyleMask::Titled));
        assert!(!style.contains(NSWindowStyleMask::Closable));
        assert!(!style.contains(NSWindowStyleMask::Resizable));
    }
}
