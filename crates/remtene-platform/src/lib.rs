//! Operating-system adapters. Shared product rules must not branch in this crate's callers.

pub mod asr_shared_data;
pub mod audio;
pub mod clipboard;
pub mod model_manifest;
pub mod model_registry;
#[cfg(target_os = "macos")]
pub mod modifier_shortcut;
pub mod permissions;
pub mod recording_cue;
pub mod resident_shell;
pub mod target_context;
mod trace;

/// 将平台适配层 trace 连接到应用的统一诊断 Sink。
pub fn configure_diagnostics_sink(
    sink: &std::sync::Arc<dyn remtene_application::ports::DiagnosticsSink>,
) {
    trace::configure(sink);
}

pub use recording_cue::create_default_recording_cue;

// Re-export commonly used types for convenience
#[cfg(target_os = "macos")]
pub use audio::create_default_macos_audio_capture;
#[cfg(target_os = "macos")]
pub use modifier_shortcut::{
    MacModifierKey, MacModifierShortcutError, replace_mac_modifier_shortcut,
};
#[cfg(target_os = "macos")]
pub use permissions::{MacOsMicrophonePermission, MicrophoneAuthorizationStatus};
#[cfg(target_os = "macos")]
pub use target_context::{AccessibilityTrust, MacTargetContext};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformFamily {
    MacOs,
    Windows,
    Unsupported,
}

impl PlatformFamily {
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Unsupported
        }
    }
}
