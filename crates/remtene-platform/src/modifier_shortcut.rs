//! macOS 纯修饰键全局监听。
//!
//! AppKit 的全局 monitor 不接收发往本应用的事件，本地 monitor 又只接收本应用
//! 事件，因此两者必须成对安装。原生 token 只能在主线程创建和释放；调用方通过
//! Tauri 主线程调度进入本模块，业务层只看到普通的按下／松开回调。

#![allow(unsafe_code)]

use std::{cell::RefCell, ptr::NonNull, rc::Rc, sync::Arc};

use block2::RcBlock;
use objc2::{MainThreadMarker, rc::Retained, runtime::AnyObject};
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags};
use thiserror::Error;

use crate::MacTargetContext;

type ShortcutCallback = Arc<dyn Fn() + Send + Sync + 'static>;

/// 浏览器 `KeyboardEvent.code` 与 macOS 物理修饰键的一一对应。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MacModifierKey {
    CommandLeft,
    CommandRight,
    ControlLeft,
    ControlRight,
    OptionLeft,
    OptionRight,
    ShiftLeft,
    ShiftRight,
}

impl MacModifierKey {
    #[must_use]
    pub fn from_binding(binding: &str) -> Option<Self> {
        match binding {
            "MetaLeft" => Some(Self::CommandLeft),
            "MetaRight" => Some(Self::CommandRight),
            "ControlLeft" => Some(Self::ControlLeft),
            "ControlRight" => Some(Self::ControlRight),
            "AltLeft" => Some(Self::OptionLeft),
            "AltRight" => Some(Self::OptionRight),
            "ShiftLeft" => Some(Self::ShiftLeft),
            "ShiftRight" => Some(Self::ShiftRight),
            _ => None,
        }
    }

    #[must_use]
    pub const fn binding(self) -> &'static str {
        match self {
            Self::CommandLeft => "MetaLeft",
            Self::CommandRight => "MetaRight",
            Self::ControlLeft => "ControlLeft",
            Self::ControlRight => "ControlRight",
            Self::OptionLeft => "AltLeft",
            Self::OptionRight => "AltRight",
            Self::ShiftLeft => "ShiftLeft",
            Self::ShiftRight => "ShiftRight",
        }
    }

    const fn key_code(self) -> u16 {
        match self {
            Self::CommandLeft => 0x37,
            Self::CommandRight => 0x36,
            Self::ControlLeft => 0x3b,
            Self::ControlRight => 0x3e,
            Self::OptionLeft => 0x3a,
            Self::OptionRight => 0x3d,
            Self::ShiftLeft => 0x38,
            Self::ShiftRight => 0x3c,
        }
    }

    const fn aggregate_flag(self) -> NSEventModifierFlags {
        match self {
            Self::CommandLeft | Self::CommandRight => NSEventModifierFlags::Command,
            Self::ControlLeft | Self::ControlRight => NSEventModifierFlags::Control,
            Self::OptionLeft | Self::OptionRight => NSEventModifierFlags::Option,
            Self::ShiftLeft | Self::ShiftRight => NSEventModifierFlags::Shift,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModifierTransition {
    Pressed,
    Released,
}

#[derive(Debug)]
struct ModifierPressState {
    pressed: bool,
    suppress_initial_release: bool,
}

impl ModifierPressState {
    fn new(initially_pressed: bool) -> Self {
        Self {
            pressed: initially_pressed,
            // 保存绑定时，录入用的修饰键通常还没松开；第一次松开只负责归零，
            // 不能把它当成一次 Push-to-Talk 结束事件。
            suppress_initial_release: initially_pressed,
        }
    }

    fn toggle(&mut self) -> Option<ModifierTransition> {
        if self.pressed {
            self.pressed = false;
            if self.suppress_initial_release {
                self.suppress_initial_release = false;
                None
            } else {
                Some(ModifierTransition::Released)
            }
        } else {
            self.pressed = true;
            Some(ModifierTransition::Pressed)
        }
    }
}

/// 纯修饰键平台注册失败原因。
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum MacModifierShortcutError {
    #[error("modifier shortcut operations must run on the main thread")]
    WrongThread,
    #[error("accessibility permission is required for global modifier events")]
    PermissionRequired,
    #[error("macOS did not create the modifier event monitor")]
    MonitorUnavailable,
}

struct MacModifierMonitor {
    key: MacModifierKey,
    global_monitor: Retained<AnyObject>,
    local_monitor: Retained<AnyObject>,
}

impl MacModifierMonitor {
    fn install(
        key: MacModifierKey,
        on_press: ShortcutCallback,
        on_release: ShortcutCallback,
    ) -> Result<Self, MacModifierShortcutError> {
        if !MacTargetContext::accessibility_trust().is_usable() {
            return Err(MacModifierShortcutError::PermissionRequired);
        }

        let initially_pressed = NSEvent::modifierFlags_class().contains(key.aggregate_flag());
        let state = Rc::new(RefCell::new(ModifierPressState::new(initially_pressed)));

        let global_state = Rc::clone(&state);
        let global_press = Arc::clone(&on_press);
        let global_release = Arc::clone(&on_release);
        let global_block = RcBlock::new(move |event: NonNull<NSEvent>| {
            dispatch_event(event, key, &global_state, &global_press, &global_release);
        });
        let global_monitor = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(
            NSEventMask::FlagsChanged,
            &global_block,
        )
        .ok_or(MacModifierShortcutError::MonitorUnavailable)?;

        let local_block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            dispatch_event(event, key, &state, &on_press, &on_release);
            event.as_ptr()
        });
        // SAFETY: The block returns the same live NSEvent pointer it received and never
        // retains or mutates it. The monitor is removed before its token is released.
        let Some(local_monitor) = (unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(
                NSEventMask::FlagsChanged,
                &local_block,
            )
        }) else {
            // SAFETY: `global_monitor` is the token returned by the matching AppKit API
            // on this main thread and has not previously been removed.
            unsafe { NSEvent::removeMonitor(&global_monitor) };
            return Err(MacModifierShortcutError::MonitorUnavailable);
        };

        Ok(Self {
            key,
            global_monitor,
            local_monitor,
        })
    }
}

impl Drop for MacModifierMonitor {
    fn drop(&mut self) {
        debug_assert!(MainThreadMarker::new().is_some());
        // SAFETY: Both values are live monitor tokens created by AppKit on the same
        // main thread. This object is owned by a main-thread thread-local and drops once.
        unsafe {
            NSEvent::removeMonitor(&self.local_monitor);
            NSEvent::removeMonitor(&self.global_monitor);
        }
    }
}

fn dispatch_event(
    event: NonNull<NSEvent>,
    key: MacModifierKey,
    state: &RefCell<ModifierPressState>,
    on_press: &ShortcutCallback,
    on_release: &ShortcutCallback,
) {
    // SAFETY: AppKit supplies a non-null NSEvent pointer valid for the duration of
    // the monitor callback. We only read its physical key code synchronously.
    let event = unsafe { event.as_ref() };
    if event.keyCode() != key.key_code() {
        return;
    }

    match state.borrow_mut().toggle() {
        Some(ModifierTransition::Pressed) => on_press(),
        Some(ModifierTransition::Released) => on_release(),
        None => {}
    }
}

thread_local! {
    static ACTIVE_MONITOR: RefCell<Option<MacModifierMonitor>> = const { RefCell::new(None) };
}

/// 在 macOS 主线程上以事务方式替换当前纯修饰键监听。
///
/// 新 monitor 创建失败时旧 monitor 保持不变；传入 `None` 则移除当前监听。
pub fn replace_mac_modifier_shortcut(
    next: Option<MacModifierKey>,
    on_press: ShortcutCallback,
    on_release: ShortcutCallback,
) -> Result<(), MacModifierShortcutError> {
    if MainThreadMarker::new().is_none() {
        return Err(MacModifierShortcutError::WrongThread);
    }

    ACTIVE_MONITOR.with_borrow_mut(|active| {
        if active.as_ref().map(|monitor| monitor.key) == next {
            return Ok(());
        }

        let replacement = next
            .map(|key| MacModifierMonitor::install(key, on_press, on_release))
            .transpose()?;
        *active = replacement;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::{MacModifierKey, ModifierPressState, ModifierTransition};

    #[test]
    fn browser_codes_preserve_left_and_right_modifier_identity() {
        assert_eq!(
            MacModifierKey::from_binding("MetaLeft"),
            Some(MacModifierKey::CommandLeft)
        );
        assert_eq!(
            MacModifierKey::from_binding("AltRight"),
            Some(MacModifierKey::OptionRight)
        );
        assert_eq!(MacModifierKey::from_binding("Fn"), None);
    }

    #[test]
    fn initially_held_capture_key_does_not_emit_a_release() {
        let mut state = ModifierPressState::new(true);
        assert_eq!(state.toggle(), None);
        assert_eq!(state.toggle(), Some(ModifierTransition::Pressed));
        assert_eq!(state.toggle(), Some(ModifierTransition::Released));
    }

    #[test]
    fn idle_modifier_emits_press_then_release() {
        let mut state = ModifierPressState::new(false);
        assert_eq!(state.toggle(), Some(ModifierTransition::Pressed));
        assert_eq!(state.toggle(), Some(ModifierTransition::Released));
    }
}
