//! Minimal macOS Accessibility boundary for target capture and insert-only output.
//!
//! All raw pointers and Apple FFI calls are contained in this module. The rest
//! of the crate receives owned, opaque identities only. The only writable AX
//! attributes named here are `AXSelectedTextRange`, used to collapse a selection
//! to its end, and `AXSelectedText`, used to insert at that proven empty range.
//! `AXValue`, full-control replacement, and deletion are deliberately absent.

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::{
    ffi::c_void,
    ptr,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use core_foundation::{
    attributed_string::{CFAttributedString, CFAttributedStringGetString},
    base::{Boolean, CFIndex, CFRange, CFType, CFTypeID, CFTypeRef, TCFType},
    boolean::CFBoolean,
    dictionary::{CFDictionary, CFDictionaryRef},
    number::CFNumber,
    string::{CFString, CFStringRef},
};
use libc::pid_t;
use objc2_app_kit::NSWorkspace;
use remtene_application::ports::{
    CapturedTarget, InsertOutcome, LifecycleFence, OutputAdapter, PortError, PortFuture,
    SelectionSnapshot, TargetContextPort, TargetDisplayHint, TargetRevalidation, TargetSnapshotRef,
    ValidatedTargetRef,
};
use remtene_domain::DeliveryId;

use super::{
    ClipboardInsertObservation, ControlKind, IdentityComparison, InsertDispatch,
    InsertVerification, Probe, SafeTargetContext, TargetContextBackend, TargetIdentityComparison,
    TargetObservation, TargetSecurity, TextRange, classify_security,
};

const AX_FOCUSED_APPLICATION: &str = "AXFocusedApplication";
const AX_FOCUSED_WINDOW: &str = "AXFocusedWindow";
const AX_FOCUSED_UI_ELEMENT: &str = "AXFocusedUIElement";
const AX_WINDOW: &str = "AXWindow";
const AX_ROLE: &str = "AXRole";
const AX_SUBROLE: &str = "AXSubrole";
const AX_ENABLED: &str = "AXEnabled";
const AX_SELECTED_TEXT: &str = "AXSelectedText";
const AX_SELECTED_TEXT_RANGE: &str = "AXSelectedTextRange";
const AX_POSITION: &str = "AXPosition";
const AX_SIZE: &str = "AXSize";
const AX_NUMBER_OF_CHARACTERS: &str = "AXNumberOfCharacters";
const AX_STRING_FOR_RANGE: &str = "AXStringForRange";
const AX_ATTRIBUTED_STRING_FOR_RANGE: &str = "AXAttributedStringForRange";

const AX_TEXT_FIELD_ROLE: &str = "AXTextField";
const AX_TEXT_AREA_ROLE: &str = "AXTextArea";
const AX_SECURE_TEXT_FIELD_SUBROLE: &str = "AXSecureTextField";

const AX_ERROR_SUCCESS: AxError = 0;
const AX_ERROR_ATTRIBUTE_UNSUPPORTED: AxError = -25_205;
const AX_ERROR_NO_VALUE: AxError = -25_212;
const AX_VALUE_CG_POINT: AxValueType = 1;
const AX_VALUE_CG_SIZE: AxValueType = 2;
const AX_VALUE_CF_RANGE: AxValueType = 4;
const AX_MESSAGING_TIMEOUT_SECONDS: f32 = 0.25;
const VERIFY_ATTEMPTS: usize = 10;
const VERIFY_INTERVAL: Duration = Duration::from_millis(10);

/// Serializes every AX call across adapter instances, including Apple's
/// process-global, explicitly non-thread-safe Secure Event Input probe.
static AX_FFI_GATE: Mutex<()> = Mutex::new(());
#[cfg(test)]
static AX_SELECTED_TEXT_SETTER_CALLS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
fn selected_text_setter_call_count() -> usize {
    AX_SELECTED_TEXT_SETTER_CALLS.load(AtomicOrdering::SeqCst)
}

type AxError = i32;
type AxValueType = u32;

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct AxPoint {
    x: f64,
    y: f64,
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct AxSize {
    width: f64,
    height: f64,
}

#[repr(C)]
struct AxUiElementOpaque {
    _private: [u8; 0],
}

#[repr(C)]
struct AxValueOpaque {
    _private: [u8; 0],
}

type AxUiElementRef = *const AxUiElementOpaque;
type AxValueRef = *const AxValueOpaque;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    #[link_name = "AXIsProcessTrusted"]
    fn ax_is_process_trusted() -> Boolean;

    /// Private but stable since 10.14 and used by every app that must tell a
    /// real TCC grant from one inherited from its launcher.
    #[link_name = "responsibility_get_pid_responsible_for_pid"]
    fn responsibility_get_pid_responsible_for_pid(pid: pid_t) -> pid_t;

    #[link_name = "AXIsProcessTrustedWithOptions"]
    fn ax_is_process_trusted_with_options(options: CFDictionaryRef) -> Boolean;

    #[link_name = "kAXTrustedCheckOptionPrompt"]
    static AX_TRUSTED_CHECK_OPTION_PROMPT: CFStringRef;

    #[link_name = "AXUIElementGetTypeID"]
    fn ax_ui_element_get_type_id() -> CFTypeID;

    #[link_name = "AXUIElementCreateSystemWide"]
    fn ax_ui_element_create_system_wide() -> AxUiElementRef;

    #[link_name = "AXUIElementCreateApplication"]
    fn ax_ui_element_create_application(pid: pid_t) -> AxUiElementRef;

    #[link_name = "AXUIElementCopyAttributeValue"]
    fn ax_ui_element_copy_attribute_value(
        element: AxUiElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AxError;

    #[link_name = "AXUIElementCopyParameterizedAttributeValue"]
    fn ax_ui_element_copy_parameterized_attribute_value(
        element: AxUiElementRef,
        attribute: CFStringRef,
        parameter: CFTypeRef,
        result: *mut CFTypeRef,
    ) -> AxError;

    #[link_name = "AXUIElementIsAttributeSettable"]
    fn ax_ui_element_is_attribute_settable(
        element: AxUiElementRef,
        attribute: CFStringRef,
        settable: *mut Boolean,
    ) -> AxError;

    #[link_name = "AXUIElementSetAttributeValue"]
    fn ax_ui_element_set_attribute_value(
        element: AxUiElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> AxError;

    #[link_name = "AXUIElementGetPid"]
    fn ax_ui_element_get_pid(element: AxUiElementRef, pid: *mut pid_t) -> AxError;

    #[link_name = "AXUIElementSetMessagingTimeout"]
    fn ax_ui_element_set_messaging_timeout(
        element: AxUiElementRef,
        timeout_seconds: f32,
    ) -> AxError;

    #[link_name = "AXValueGetTypeID"]
    fn ax_value_get_type_id() -> CFTypeID;

    #[link_name = "AXValueCreate"]
    fn ax_value_create(value_type: AxValueType, value: *const c_void) -> AxValueRef;

    #[link_name = "AXValueGetValue"]
    fn ax_value_get_value(
        value: AxValueRef,
        value_type: AxValueType,
        destination: *mut c_void,
    ) -> Boolean;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    #[link_name = "IsSecureEventInputEnabled"]
    fn is_secure_event_input_enabled() -> Boolean;
}

/// Ready-to-use macOS target and output adapter. Native objects stay inside
/// this type and are never serialized or exposed to the renderer.
pub struct MacTargetContext {
    inner: SafeTargetContext<MacBackend>,
}

impl MacTargetContext {
    #[must_use]
    pub fn new() -> Self {
        // Delivery goes through `SafeTargetContext::insert`, which re-observes
        // the focused application, window, control, security, and caret before
        // the element-bound AX write. Any mismatch fails closed to
        // `NotInserted`; an unconfirmed external effect becomes `Indeterminate`
        // so the orchestrator degrades to the temporary-text surface.
        Self {
            inner: SafeTargetContext::new(Arc::new(MacBackend::new())),
        }
    }

    /// Non-prompting Accessibility trust probe for status presentation.
    ///
    /// Never call this from a path that should surface the system dialog; use
    /// [`Self::request_accessibility_permission_prompt`] for that.
    #[must_use]
    pub fn accessibility_trusted() -> bool {
        let _gate = lock_ax_ffi();
        process_is_trusted()
    }

    /// Classifies *how* Accessibility trust was obtained.
    ///
    /// `accessibility_trusted` alone cannot answer the question that matters
    /// during development: a grant inherited from the launching terminal or IDE
    /// reports trusted while cross-process AX queries still fail. Callers that
    /// present status to the user must use this instead of the bare boolean.
    #[must_use]
    pub fn accessibility_trust() -> AccessibilityTrust {
        let _gate = lock_ax_ffi();
        let trusted = process_is_trusted();
        let owned = process_owns_its_trust();
        let trust = match (trusted, owned) {
            (false, _) => AccessibilityTrust::NotTrusted,
            (true, true) => AccessibilityTrust::Granted,
            (true, false) => AccessibilityTrust::Inherited,
        };
        // 「系统设置里显示已授权」和「本进程真的持有授权」是两件事。责任进程
        // 才是判据，所以把两个原始事实都写出来，而不只写结论。
        crate::trace::delivery(
            "授权归属",
            trust.label(),
            &format!(
                "AXIsProcessTrusted={trusted} 自有授权={owned} 本进程pid={} 责任进程pid={}",
                std::process::id(),
                // SAFETY: see `process_owns_its_trust`.
                unsafe { responsibility_get_pid_responsible_for_pid(std::process::id() as pid_t) },
            ),
        );
        trust
    }

    /// Requests the operating-system Accessibility prompt.
    ///
    /// This is the only prompting entry point. Call it only from an explicit
    /// user action in permission settings. Construction, capture, and status
    /// checks use the non-prompting `AXIsProcessTrusted` API instead. The
    /// return value is the current trust state; macOS may show the prompt and
    /// grant access asynchronously after this method returns.
    pub fn request_accessibility_permission_prompt() -> bool {
        let _gate = lock_ax_ffi();
        request_accessibility_prompt()
    }

    /// Whether a validated capability still points at the focused control.
    ///
    /// The clipboard backend calls this immediately before ⌘V. Sharing this one
    /// registry is what makes the paste target-bound rather than a blind
    /// keystroke: a token minted here is meaningless to any other instance.
    #[must_use]
    pub fn clipboard_insert_status(
        &self,
        target: &ValidatedTargetRef,
    ) -> crate::clipboard::ClipboardTargetStatus {
        self.inner.clipboard_insert_status(target)
    }

    /// Whether a captured snapshot still points at the focused control.
    #[must_use]
    pub fn clipboard_selection_status(
        &self,
        target: &TargetSnapshotRef,
    ) -> crate::clipboard::ClipboardTargetStatus {
        self.inner.clipboard_selection_status(target)
    }

    pub(crate) fn dispatch_and_verify_clipboard_insert<E>(
        &self,
        target: &ValidatedTargetRef,
        text: &str,
        attempts: usize,
        interval: Duration,
        dispatch: impl FnOnce() -> Result<(), E>,
    ) -> Result<crate::clipboard::ClipboardPasteOutcome, E> {
        self.inner
            .dispatch_and_verify_clipboard_insert(target, text, attempts, interval, dispatch)
    }
}

impl Default for MacTargetContext {
    fn default() -> Self {
        Self::new()
    }
}

impl TargetContextPort for MacTargetContext {
    fn capture(&self) -> PortFuture<'_, Result<CapturedTarget, PortError>> {
        self.inner.capture()
    }

    fn read_selected_text(
        &self,
        target: &TargetSnapshotRef,
    ) -> PortFuture<'_, Result<SelectionSnapshot, PortError>> {
        self.inner.read_selected_text(target)
    }

    fn revalidate(
        &self,
        target: &TargetSnapshotRef,
    ) -> PortFuture<'_, Result<TargetRevalidation, PortError>> {
        self.inner.revalidate(target)
    }
}

impl OutputAdapter for MacTargetContext {
    fn insert(
        &self,
        target: ValidatedTargetRef,
        text: String,
        delivery_id: DeliveryId,
        lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<InsertOutcome, PortError>> {
        // Delivery is authorized only through the fail-closed state machine in
        // `SafeTargetContext::insert`, which re-observes the target and collapses
        // the selection to its caret before any dispatch. See ADR-0005.
        self.inner.insert(target, text, delivery_id, lifecycle)
    }
}

struct MacBackend {}

impl MacBackend {
    const fn new() -> Self {
        Self {}
    }
}

#[derive(Clone)]
struct MacIdentity {
    pid: pid_t,
    application: AxElement,
    window: AxElement,
    control: AxElement,
    role: MacTextRole,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MacTextRole {
    SingleLine,
    MultiLine,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MacInsertReceipt {
    anchor: usize,
    utf16_length: usize,
    before_character_count: usize,
    expected_character_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InsertedTextMatch {
    Exact,
    LineBreakEquivalent,
    Mismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InsertionLengthObservation {
    Pending,
    Candidates([Option<usize>; 2]),
    Unverified,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InsertionMarkerPolicy {
    /// Clipboard verification preserves its existing rule: either the caret or
    /// the character count may independently authorize an exact range read.
    Any,
    /// Element-bound AX insertion preserves its stronger rule: both markers
    /// must agree before the setter result is accepted as verified.
    Both,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InsertedTextReadback {
    Exact,
    LineBreakEquivalent,
    Mismatch,
    Unavailable,
}

/// AppKit text controls do not expose one universal line-break representation.
/// A pasted logical newline may be read back as LF, CR, CRLF, U+2028, or U+2029.
/// No other whitespace or content normalization is allowed here.
fn normalize_ax_line_breaks(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                normalized.push('\n');
            }
            '\n' | '\u{2028}' | '\u{2029}' => normalized.push('\n'),
            other => normalized.push(other),
        }
    }
    normalized
}

fn inserted_text_match(expected: &str, actual: &str) -> InsertedTextMatch {
    if actual == expected {
        return InsertedTextMatch::Exact;
    }
    if normalize_ax_line_breaks(actual) == normalize_ax_line_breaks(expected) {
        InsertedTextMatch::LineBreakEquivalent
    } else {
        InsertedTextMatch::Mismatch
    }
}

/// Returns the only UTF-16 length range that line-break representation alone
/// can produce: one unit per canonical newline, or two for CRLF.
fn compatible_utf16_length_bounds(text: &str) -> Option<(usize, usize)> {
    let normalized = normalize_ax_line_breaks(text);
    let minimum = normalized.encode_utf16().count();
    let line_breaks = normalized
        .chars()
        .filter(|character| *character == '\n')
        .count();
    Some((minimum, minimum.checked_add(line_breaks)?))
}

fn insertion_length_observation(
    anchor: usize,
    text: &str,
    current_range: Option<TextRange>,
    before_character_count: Option<usize>,
    current_character_count: Option<usize>,
    marker_policy: InsertionMarkerPolicy,
) -> InsertionLengthObservation {
    let caret_unchanged = current_range == Some(TextRange::caret_at(anchor));
    let count_unchanged =
        before_character_count.is_none_or(|before| current_character_count == Some(before));
    if caret_unchanged && count_unchanged {
        return InsertionLengthObservation::Pending;
    }

    let Some((minimum, maximum)) = compatible_utf16_length_bounds(text) else {
        return InsertionLengthObservation::Unverified;
    };
    if anchor.checked_add(maximum).is_none() {
        return InsertionLengthObservation::Unverified;
    }

    let caret_delta = current_range.and_then(|range| {
        (range.length == 0)
            .then(|| range.location.checked_sub(anchor))
            .flatten()
    });
    let count_delta = before_character_count
        .zip(current_character_count)
        .and_then(|(before, current)| current.checked_sub(before));

    if marker_policy == InsertionMarkerPolicy::Both {
        return match (caret_delta, count_delta) {
            (Some(caret), Some(count))
                if caret == count && (minimum..=maximum).contains(&caret) =>
            {
                InsertionLengthObservation::Candidates([Some(caret), None])
            }
            _ => InsertionLengthObservation::Unverified,
        };
    }

    let mut candidates = [None, None];
    for candidate in [caret_delta, count_delta].into_iter().flatten() {
        if !(minimum..=maximum).contains(&candidate) || candidates[0] == Some(candidate) {
            continue;
        }
        if candidates[0].is_none() {
            candidates[0] = Some(candidate);
        } else {
            candidates[1] = Some(candidate);
        }
    }

    if candidates[0].is_some() {
        InsertionLengthObservation::Candidates(candidates)
    } else {
        InsertionLengthObservation::Unverified
    }
}

fn read_inserted_text(
    control: &AxElement,
    anchor: usize,
    text: &str,
    candidates: [Option<usize>; 2],
) -> InsertedTextReadback {
    let mut read_any_candidate = false;
    for length in candidates.into_iter().flatten() {
        let inserted_range = TextRange {
            location: anchor,
            length,
        };
        let Ok(inserted) = copy_text_for_range(control, inserted_range) else {
            continue;
        };
        read_any_candidate = true;
        match inserted_text_match(text, &inserted) {
            InsertedTextMatch::Exact => return InsertedTextReadback::Exact,
            InsertedTextMatch::LineBreakEquivalent => {
                return InsertedTextReadback::LineBreakEquivalent;
            }
            InsertedTextMatch::Mismatch => {}
        }
    }

    if read_any_candidate {
        InsertedTextReadback::Mismatch
    } else {
        InsertedTextReadback::Unavailable
    }
}

impl TargetContextBackend for MacBackend {
    type Identity = MacIdentity;
    type Receipt = MacInsertReceipt;

    fn observe_focused_target(&self) -> TargetObservation<Self::Identity> {
        let _gate = self.lock_gate();
        self.observe_locked()
    }

    fn compare_identity(
        &self,
        expected: &Self::Identity,
        actual: &Self::Identity,
    ) -> TargetIdentityComparison {
        let _gate = self.lock_gate();
        compare_identities(expected, actual)
    }

    fn read_selected_text(&self, target: &Self::Identity, range: TextRange) -> Result<String, ()> {
        let _gate = self.lock_gate();
        let before = self.observe_locked();
        if !observation_matches(&before, target, range) {
            return Err(());
        }

        let selected_text = copy_string(&target.control, AX_SELECTED_TEXT).map_err(|_| ())?;
        if selected_text.encode_utf16().count() != range.length {
            return Err(());
        }

        // Recheck after the read so a selection that moved during the AX call
        // never becomes authorized context.
        let after = self.observe_locked();
        if !observation_matches(&after, target, range) {
            return Err(());
        }
        Ok(selected_text)
    }

    fn collapse_selection(
        &self,
        target: &Self::Identity,
        expected_range: TextRange,
        anchor: usize,
    ) -> Result<(), ()> {
        let _gate = self.lock_gate();
        if expected_range.length == 0 || expected_range.end() != Some(anchor) {
            return Err(());
        }
        let before = self.observe_locked();
        if !observation_matches(&before, target, expected_range) {
            return Err(());
        }

        set_selected_range(&target.control, TextRange::caret_at(anchor)).map_err(|_| ())?;

        let after = self.observe_locked();
        if observation_matches(&after, target, TextRange::caret_at(anchor)) {
            Ok(())
        } else {
            Err(())
        }
    }

    fn dispatch_insert(
        &self,
        target: &Self::Identity,
        anchor: usize,
        text: &str,
    ) -> InsertDispatch<Self::Receipt> {
        let _gate = self.lock_gate();
        if !text_supported_by_role(target.role, text) {
            crate::trace::delivery(
                "dispatch",
                "拒绝",
                &format!(
                    "角色 {:?} 不接受此文本（含换行或不支持的字符）",
                    target.role
                ),
            );
            return InsertDispatch::NotDispatched;
        }

        // Refuse controls that do not explicitly expose an element-bound
        // selected-text setter. This check occurs at the real write point
        // because support can differ by application and concrete control.
        match attribute_is_settable(&target.control, AX_SELECTED_TEXT) {
            Ok(true) => {}
            Ok(false) => {
                // AX 直写路线最常见的失败：控件根本不声明 AXSelectedText 可写。
                // 目标应用不实现该接口时，我们无路可走，只能回退。
                crate::trace::delivery("dispatch", "拒绝", "控件声明 AXSelectedText 不可写");
                return InsertDispatch::NotDispatched;
            }
            Err(_) => {
                crate::trace::delivery("dispatch", "拒绝", "无法读取 AXSelectedText 可写性");
                return InsertDispatch::NotDispatched;
            }
        }
        crate::trace::checkpoint("dispatch.settable", "AXSelectedText 可写");

        let current = self.observe_locked();
        if !observation_matches(&current, target, TextRange::caret_at(anchor)) {
            crate::trace::delivery("dispatch", "拒绝", "写入前目标或光标位置已变");
            return InsertDispatch::NotDispatched;
        }

        let utf16_length = text.encode_utf16().count();
        let Ok(before_character_count) = copy_character_count(&target.control) else {
            crate::trace::delivery("dispatch", "拒绝", "无法读取控件当前字符数");
            return InsertDispatch::NotDispatched;
        };
        let Some(expected_character_count) = before_character_count.checked_add(utf16_length)
        else {
            crate::trace::delivery("dispatch", "拒绝", "字符数溢出");
            return InsertDispatch::NotDispatched;
        };
        if copy_character_count(&target.control) != Ok(before_character_count) {
            crate::trace::delivery("dispatch", "拒绝", "两次读取字符数不一致（控件正在变动）");
            return InsertDispatch::NotDispatched;
        }

        // Prove that this control supports exact range reads before posting an
        // irreversible event. A zero-length query does not expose document text.
        match copy_text_for_range(&target.control, TextRange::caret_at(anchor)) {
            Ok(probe) if probe.is_empty() => {}
            Ok(_) => {
                crate::trace::delivery("dispatch", "拒绝", "零长度范围查询返回了非空内容");
                return InsertDispatch::NotDispatched;
            }
            Err(_) => {
                // 无法按范围回读就无法验证写入结果，因此不允许写。
                crate::trace::delivery("dispatch", "拒绝", "控件不支持按范围读取文本");
                return InsertDispatch::NotDispatched;
            }
        }
        crate::trace::checkpoint("dispatch.rangeread", "范围读取可用");

        // Re-observe immediately before the setter. The setter targets the
        // captured AX element itself, so a focus change after this point cannot
        // redirect the text to a different control.
        let current = self.observe_locked();
        if !observation_matches(&current, target, TextRange::caret_at(anchor)) {
            crate::trace::delivery("dispatch", "拒绝", "临写入前复核失败");
            return InsertDispatch::NotDispatched;
        }

        if set_selected_text(&target.control, text).is_err() {
            crate::trace::delivery("dispatch", "不确定", "AXSelectedText 写入调用返回错误");
            // Once the external setter has been called, its effect cannot be
            // proven absent from metadata alone. A non-standard control could
            // have changed content without changing its character count, so an
            // error is always indeterminate and must never trigger a second
            // automatic delivery path.
            return InsertDispatch::Indeterminate;
        }

        InsertDispatch::Dispatched(MacInsertReceipt {
            anchor,
            utf16_length,
            before_character_count,
            expected_character_count,
        })
    }

    fn character_count(&self, target: &Self::Identity) -> Option<usize> {
        let _gate = self.lock_gate();
        copy_character_count(&target.control).ok()
    }

    fn clipboard_insert_observation(
        &self,
        target: &Self::Identity,
        anchor: usize,
        text: &str,
        before_character_count: Option<usize>,
    ) -> ClipboardInsertObservation {
        let _gate = self.lock_gate();
        let current = self.observe_locked();
        let Some(identity) = current.identity.as_ref() else {
            crate::trace::delivery(
                "clipboard.verify",
                "不确定",
                "reason=target_identity_missing",
            );
            return ClipboardInsertObservation::Unverified;
        };
        if classify_security(&current) != TargetSecurity::Safe
            || !identities_are_exact(target, identity)
        {
            crate::trace::delivery(
                "clipboard.verify",
                "不确定",
                "reason=target_changed_or_unsafe",
            );
            return ClipboardInsertObservation::Unverified;
        }

        let current_range = current.selected_range;
        let current_character_count = copy_character_count(&target.control).ok();
        match insertion_length_observation(
            anchor,
            text,
            current_range,
            before_character_count,
            current_character_count,
            InsertionMarkerPolicy::Any,
        ) {
            InsertionLengthObservation::Pending => ClipboardInsertObservation::Pending,
            InsertionLengthObservation::Unverified => {
                crate::trace::delivery(
                    "clipboard.verify",
                    "不确定",
                    &format!(
                        "reason=marker_mismatch expected_utf16={} range_present={} count_present={}",
                        text.encode_utf16().count(),
                        current_range.is_some(),
                        current_character_count.is_some()
                    ),
                );
                ClipboardInsertObservation::Unverified
            }
            InsertionLengthObservation::Candidates(candidates) => {
                match read_inserted_text(&target.control, anchor, text, candidates) {
                    InsertedTextReadback::Exact => {
                        crate::trace::delivery(
                            "clipboard.verify",
                            "已验证",
                            "reason=exact_readback",
                        );
                        ClipboardInsertObservation::Verified
                    }
                    InsertedTextReadback::LineBreakEquivalent => {
                        crate::trace::delivery(
                            "clipboard.verify",
                            "已验证",
                            "reason=line_break_equivalent_readback",
                        );
                        ClipboardInsertObservation::Verified
                    }
                    InsertedTextReadback::Mismatch => {
                        crate::trace::delivery(
                            "clipboard.verify",
                            "不确定",
                            &format!(
                                "reason=readback_mismatch expected_utf16={}",
                                text.encode_utf16().count()
                            ),
                        );
                        ClipboardInsertObservation::Unverified
                    }
                    InsertedTextReadback::Unavailable => {
                        ClipboardInsertObservation::ReadbackPending
                    }
                }
            }
        }
    }

    fn verify_insert(
        &self,
        target: &Self::Identity,
        anchor: usize,
        text: &str,
        receipt: &Self::Receipt,
    ) -> InsertVerification {
        let _gate = self.lock_gate();
        if receipt.anchor != anchor
            || receipt.utf16_length != text.encode_utf16().count()
            || receipt.expected_character_count
                != receipt
                    .before_character_count
                    .saturating_add(receipt.utf16_length)
        {
            crate::trace::delivery("verify", "未验证", "回执与请求不一致");
            return InsertVerification::Unverified;
        }
        let Some((_, maximum_utf16_length)) = compatible_utf16_length_bounds(text) else {
            crate::trace::delivery("verify", "未验证", "reason=line_break_length_overflow");
            return InsertVerification::Unverified;
        };
        if anchor.checked_add(maximum_utf16_length).is_none() {
            crate::trace::delivery("verify", "未验证", "reason=expected_range_overflow");
            return InsertVerification::Unverified;
        }

        for attempt in 0..VERIFY_ATTEMPTS {
            let current = self.observe_locked();
            let Some(identity) = current.identity.as_ref() else {
                crate::trace::delivery("verify", "未验证", "验证时已读不到目标身份");
                return InsertVerification::Unverified;
            };
            if classify_security(&current) != TargetSecurity::Safe
                || !identities_are_exact(target, identity)
            {
                crate::trace::delivery("verify", "未验证", "验证时目标已换（焦点变更）");
                return InsertVerification::Unverified;
            }

            let range = current.selected_range;
            let count = copy_character_count(&target.control).ok();
            match insertion_length_observation(
                anchor,
                text,
                range,
                Some(receipt.before_character_count),
                count,
                InsertionMarkerPolicy::Both,
            ) {
                InsertionLengthObservation::Pending => {}
                InsertionLengthObservation::Unverified => {
                    crate::trace::delivery(
                        "verify",
                        "未验证",
                        &format!(
                            "reason=marker_mismatch attempt={} range_present={} count_present={}",
                            attempt + 1,
                            range.is_some(),
                            count.is_some()
                        ),
                    );
                    return InsertVerification::Unverified;
                }
                InsertionLengthObservation::Candidates(candidates) => {
                    return match read_inserted_text(&target.control, anchor, text, candidates) {
                        InsertedTextReadback::Exact => {
                            crate::trace::delivery("verify", "已验证", "reason=exact_readback");
                            InsertVerification::Verified
                        }
                        InsertedTextReadback::LineBreakEquivalent => {
                            crate::trace::delivery(
                                "verify",
                                "已验证",
                                "reason=line_break_equivalent_readback",
                            );
                            InsertVerification::Verified
                        }
                        InsertedTextReadback::Mismatch => {
                            crate::trace::delivery("verify", "未验证", "reason=readback_mismatch");
                            InsertVerification::Unverified
                        }
                        InsertedTextReadback::Unavailable => {
                            crate::trace::delivery(
                                "verify",
                                "未验证",
                                "reason=readback_unavailable",
                            );
                            InsertVerification::Unverified
                        }
                    };
                }
            }
            if attempt + 1 < VERIFY_ATTEMPTS {
                thread::sleep(VERIFY_INTERVAL);
            }
        }
        // 轮询用尽，且每一轮都确认光标仍在原锚点、字符总数仍等于写入前的值
        // ——循环内任何偏离都已提前返回 `Unverified`。所以到这里能给出比
        // 「不知道」更强的结论：文档确证未被改动，回退是安全的。
        //
        // Chromium／Electron 走的就是这条路径：`AXSelectedText` 声明可写，
        // 写入调用返回成功，实际被丢弃，控件纹丝不动。
        crate::trace::delivery(
            "verify",
            "确定未插入",
            &format!("轮询 {VERIFY_ATTEMPTS} 次控件始终为写入前状态（光标与字符数均未变）"),
        );
        InsertVerification::ProvenNotInserted
    }
}

impl MacBackend {
    fn lock_gate(&self) -> std::sync::MutexGuard<'_, ()> {
        lock_ax_ffi()
    }

    fn observe_locked(&self) -> TargetObservation<MacIdentity> {
        let accessibility_trusted = process_is_trusted();
        let secure_event_input = secure_event_input_is_enabled();
        // Inherited trust passes `process_is_trusted` but fails every
        // cross-process AX query, which previously surfaced as the misleading
        // `no_focused_control`. Reject it here so the real cause is named.
        let trust_is_owned = !accessibility_trusted || process_owns_its_trust();
        if !accessibility_trusted || secure_event_input || !trust_is_owned {
            let reason = if !accessibility_trusted {
                UnknownReason::NotTrusted
            } else if secure_event_input {
                UnknownReason::SecureEventInput
            } else {
                UnknownReason::TrustInheritedFromLauncher
            };
            return unknown_observation(accessibility_trusted, secure_event_input, None, reason);
        }

        self.try_observe_locked().unwrap_or_else(|| {
            unknown_observation(true, false, None, UnknownReason::NoFrontmostChain)
        })
    }

    fn try_observe_locked(&self) -> Option<TargetObservation<MacIdentity>> {
        let system = AxElement::system_wide()?;
        set_global_messaging_timeout(&system).ok()?;
        // The system-wide element's focus attributes are deliberately not
        // queried: on macOS 26 they return kAXErrorCannotComplete for
        // processes without a window-server registration, and inconsistent
        // error codes even with one. The frontmost application therefore
        // comes from NSWorkspace, and every subsequent hop stays on the
        // per-application AX chain.
        let pid = frontmost_application_pid()?;
        let application = AxElement::application(pid)?;
        let window = copy_element(&application, AX_FOCUSED_WINDOW).ok()?;
        let display_hint = window_display_hint(&window).ok();
        let control = match copy_element(&application, AX_FOCUSED_UI_ELEMENT) {
            Ok(control) => control,
            Err(error) => {
                // Diagnostic only, no behaviour change: record which application
                // was observed and whether the focused control is reachable from
                // the window even though the application element refused. Those
                // two answers decide whether this is a wrong-element bug or the
                // target genuinely holding no caret.
                let window_control = copy_element(&window, AX_FOCUSED_UI_ELEMENT);
                eprintln!(
                    "  ↳ pid={pid} self={} ax_err={error:?} window_focus={}",
                    pid == current_process_id(),
                    window_control.is_ok()
                );
                return Some(unknown_observation(
                    true,
                    false,
                    display_hint,
                    UnknownReason::NoFocusedControl,
                ));
            }
        };
        let control_window = match copy_element(&control, AX_WINDOW) {
            Ok(window) => window,
            Err(_) => {
                return Some(unknown_observation(
                    true,
                    false,
                    display_hint,
                    UnknownReason::ControlWindowMismatch,
                ));
            }
        };
        if !window.same(&control_window) {
            return Some(unknown_observation(
                true,
                false,
                display_hint,
                UnknownReason::ControlWindowMismatch,
            ));
        }

        if element_pid(&window).ok() != Some(pid) || element_pid(&control).ok() != Some(pid) {
            return Some(unknown_observation(
                true,
                false,
                display_hint,
                UnknownReason::PidMismatch,
            ));
        }

        let role_name = match copy_string(&control, AX_ROLE) {
            Ok(role) => role,
            Err(_) => {
                return Some(unknown_observation(
                    true,
                    false,
                    display_hint,
                    UnknownReason::RoleUnreadable,
                ));
            }
        };
        let role = match role_name.as_str() {
            AX_TEXT_FIELD_ROLE => MacTextRole::SingleLine,
            AX_TEXT_AREA_ROLE => MacTextRole::MultiLine,
            _ => MacTextRole::Unsupported,
        };
        let subrole = match copy_optional_string(&control, AX_SUBROLE) {
            Ok(subrole) => subrole,
            Err(_) => {
                return Some(unknown_observation(
                    true,
                    false,
                    display_hint,
                    UnknownReason::SubroleUnreadable,
                ));
            }
        };
        let identity = MacIdentity {
            pid,
            application,
            window,
            control,
            role,
        };

        if subrole.as_deref() == Some(AX_SECURE_TEXT_FIELD_SUBROLE) {
            return Some(TargetObservation {
                identity: Some(identity),
                display_hint,
                accessibility_trusted: Probe::Known(true),
                secure_event_input: Probe::Known(false),
                control_kind: ControlKind::SecureText,
                selected_range: None,
                belongs_to_this_process: pid == current_process_id(),
            });
        }

        if role == MacTextRole::Unsupported {
            // Not a failure: the focused control is real but is not a text field
            // or text area, so there is no defined place to insert. Logged
            // because from the user's side it is indistinguishable from a bug.
            eprintln!("· 目标观测不可用：control_role_unsupported（role={role_name}）");
            return Some(TargetObservation {
                identity: Some(identity),
                display_hint,
                accessibility_trusted: Probe::Known(true),
                secure_event_input: Probe::Known(false),
                control_kind: ControlKind::Unsupported,
                selected_range: None,
                belongs_to_this_process: pid == current_process_id(),
            });
        }

        let enabled = match copy_bool(&identity.control, AX_ENABLED) {
            Ok(enabled) => enabled,
            // NSTextView-backed text areas (TextEdit among them) do not
            // implement AXEnabled at all. Only an explicit `false` disqualifies
            // the control; editability keeps being proven by the role and a
            // settable selection range.
            Err(AxCallFailure::Unsupported) => true,
            Err(_) => {
                return Some(unknown_observation(
                    true,
                    false,
                    display_hint,
                    UnknownReason::EnabledUnreadable,
                ));
            }
        };
        let range_settable = match attribute_is_settable(&identity.control, AX_SELECTED_TEXT_RANGE)
        {
            Ok(settable) => settable,
            Err(_) => {
                return Some(unknown_observation(
                    true,
                    false,
                    display_hint,
                    UnknownReason::RangeSettabilityUnreadable,
                ));
            }
        };
        if !enabled || !range_settable {
            return Some(TargetObservation {
                identity: Some(identity),
                display_hint,
                accessibility_trusted: Probe::Known(true),
                secure_event_input: Probe::Known(false),
                control_kind: ControlKind::Unsupported,
                selected_range: None,
                belongs_to_this_process: pid == current_process_id(),
            });
        }

        let selected_range = match copy_range(&identity.control, AX_SELECTED_TEXT_RANGE) {
            Ok(range) => range,
            Err(_) => {
                return Some(unknown_observation(
                    true,
                    false,
                    display_hint,
                    UnknownReason::RangeUnreadable,
                ));
            }
        };
        let belongs_to_this_process = pid == current_process_id();
        if belongs_to_this_process {
            // Every probe passed, yet `classify_security` will still return
            // Unknown: writing into our own control is refused by design. This is
            // the expected outcome when recording is triggered from the control
            // panel rather than from the target application, and without this
            // line it looks identical to a broken insertion path.
            eprintln!("· 目标观测不可用：target_is_self（前台应用是本应用自身）");
        }
        Some(TargetObservation {
            identity: Some(identity),
            display_hint,
            accessibility_trusted: Probe::Known(true),
            secure_event_input: Probe::Known(false),
            control_kind: ControlKind::EditableText,
            selected_range: Some(selected_range),
            belongs_to_this_process,
        })
    }
}

/// How this process came to hold (or not hold) Accessibility trust.
///
/// The distinction exists because `AXIsProcessTrusted` conflates two states
/// that behave differently: a grant this app owns, and one inherited from the
/// process that launched it. Only the former makes cross-process AX writes work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessibilityTrust {
    /// No trust at all. The app must request it.
    NotTrusted,
    /// This app owns the grant; it appears in System Settings under its own name.
    Granted,
    /// Trust is borrowed from the launching terminal or IDE. AX reports trusted,
    /// the app is absent from System Settings, and cross-process AX writes fail.
    /// Only reachable when running the binary directly rather than the bundle.
    Inherited,
}

impl AccessibilityTrust {
    /// Whether AX operations owned by this app can be expected to work.
    #[must_use]
    pub fn is_usable(self) -> bool {
        self == Self::Granted
    }

    /// Stable, greppable label for logs and diagnostics.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::NotTrusted => "not_trusted",
            Self::Granted => "granted",
            Self::Inherited => "inherited_from_launcher",
        }
    }
}

/// Why an observation collapsed to `ControlKind::Unknown`.
///
/// The insertion path has thirteen distinct ways to fail and all of them used to
/// produce the same opaque `Unknown`, so a real failure was indistinguishable
/// from "the user focused something we deliberately refuse to write into". This
/// enum makes each exit point name itself; the value is logged once per
/// observation and never reaches the UI, which still sees only Safe/Unknown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnknownReason {
    /// Accessibility (辅助功能) authorization is missing — nothing else was probed.
    NotTrusted,
    /// Secure Event Input（安全输入模式）is active; refusing to observe is by design.
    SecureEventInput,
    /// AX reports trusted, but the grant belongs to the launching terminal or IDE
    /// rather than this app. Cross-process AX queries fail under this state, so
    /// observing would only produce a misleading `NoFocusedControl`.
    TrustInheritedFromLauncher,
    /// `NSWorkspace` reported no frontmost application, or the AX chain to its
    /// focused window could not be built.
    NoFrontmostChain,
    /// The application reports no focused UI element (no text cursor anywhere).
    NoFocusedControl,
    /// The focused control claims a window other than the focused one.
    ControlWindowMismatch,
    /// Window or control belongs to a different process than the frontmost app.
    PidMismatch,
    /// `AXRole` could not be read.
    RoleUnreadable,
    /// `AXSubrole` could not be read.
    SubroleUnreadable,
    /// `AXEnabled` returned an error other than "unsupported".
    EnabledUnreadable,
    /// Settability of `AXSelectedTextRange` could not be determined.
    RangeSettabilityUnreadable,
    /// `AXSelectedTextRange` could not be read.
    RangeUnreadable,
}

impl UnknownReason {
    /// Stable, greppable label. Deliberately free of window titles and text
    /// content: this goes to the log, and the log must stay free of user data.
    fn label(self) -> &'static str {
        match self {
            Self::NotTrusted => "accessibility_not_trusted",
            Self::SecureEventInput => "secure_event_input_active",
            Self::TrustInheritedFromLauncher => "trust_inherited_from_launcher",
            Self::NoFrontmostChain => "no_frontmost_ax_chain",
            Self::NoFocusedControl => "no_focused_control",
            Self::ControlWindowMismatch => "control_window_mismatch",
            Self::PidMismatch => "pid_mismatch",
            Self::RoleUnreadable => "role_unreadable",
            Self::SubroleUnreadable => "subrole_unreadable",
            Self::EnabledUnreadable => "enabled_unreadable",
            Self::RangeSettabilityUnreadable => "range_settability_unreadable",
            Self::RangeUnreadable => "range_unreadable",
        }
    }
}

fn unknown_observation(
    accessibility_trusted: bool,
    secure_event_input: bool,
    display_hint: Option<TargetDisplayHint>,
    reason: UnknownReason,
) -> TargetObservation<MacIdentity> {
    // 这是十三种失败塌缩成 `ControlKind::Unknown` 之前唯一能说出自己身份的地方，
    // 所以诊断必须在此发出；一旦返回，具体原因就永久丢失了。
    crate::trace::delivery("observe", "不可用", reason.label());
    TargetObservation {
        identity: None,
        display_hint,
        accessibility_trusted: Probe::Known(accessibility_trusted),
        secure_event_input: Probe::Known(secure_event_input),
        control_kind: ControlKind::Unknown,
        selected_range: None,
        belongs_to_this_process: false,
    }
}

fn observation_matches(
    observation: &TargetObservation<MacIdentity>,
    expected: &MacIdentity,
    range: TextRange,
) -> bool {
    classify_security(observation) == TargetSecurity::Safe
        && observation.selected_range == Some(range)
        && observation
            .identity
            .as_ref()
            .is_some_and(|current| identities_are_exact(expected, current))
}

fn compare_identities(expected: &MacIdentity, actual: &MacIdentity) -> TargetIdentityComparison {
    TargetIdentityComparison {
        process: if expected.pid == actual.pid {
            IdentityComparison::Same
        } else {
            IdentityComparison::Different
        },
        application: compare_elements(&expected.application, &actual.application),
        window: compare_elements(&expected.window, &actual.window),
        control: if expected.role == actual.role {
            compare_elements(&expected.control, &actual.control)
        } else {
            IdentityComparison::Different
        },
    }
}

fn identities_are_exact(expected: &MacIdentity, actual: &MacIdentity) -> bool {
    let comparison = compare_identities(expected, actual);
    comparison.application == IdentityComparison::Same
        && comparison.window == IdentityComparison::Same
        && comparison.control == IdentityComparison::Same
}

fn compare_elements(expected: &AxElement, actual: &AxElement) -> IdentityComparison {
    if expected.same(actual) {
        IdentityComparison::Same
    } else {
        IdentityComparison::Different
    }
}

fn text_supported_by_role(role: MacTextRole, text: &str) -> bool {
    !text.is_empty()
        && role != MacTextRole::Unsupported
        && text.chars().all(|character| {
            !character.is_control() || (role == MacTextRole::MultiLine && character == '\n')
        })
}

#[derive(Clone)]
struct AxElement {
    value: CFType,
}

// SAFETY: AXUIElement is an immutable Core Foundation reference. Core
// Foundation retain/release is thread-safe, and every AX operation performed by
// this crate is additionally serialized by `AX_FFI_GATE`.
unsafe impl Send for AxElement {}

// SAFETY: See the `Send` justification. Shared references never mutate the
// wrapper, and all calls through the contained reference are serialized.
unsafe impl Sync for AxElement {}

impl AxElement {
    fn system_wide() -> Option<Self> {
        // SAFETY: `AXUIElementCreateSystemWide` follows the Create Rule and
        // returns either null or a +1 retained AXUIElementRef.
        let raw = unsafe { ax_ui_element_create_system_wide() };
        Self::from_created(raw)
    }

    fn application(pid: pid_t) -> Option<Self> {
        if pid <= 0 {
            return None;
        }
        // SAFETY: `AXUIElementCreateApplication` follows the Create Rule and
        // returns a +1 retained AXUIElementRef for any pid. Validity of the
        // target is proven by the subsequent attribute queries, not here.
        let raw = unsafe { ax_ui_element_create_application(pid) };
        Self::from_created(raw)
    }

    fn from_created(raw: AxUiElementRef) -> Option<Self> {
        if raw.is_null() {
            return None;
        }
        // SAFETY: The caller transfers a valid +1 retained Core Foundation
        // object. `CFType` assumes that ownership and releases it on drop.
        let value = unsafe { CFType::wrap_under_create_rule(raw.cast()) };
        Self::from_cf_type(value)
    }

    fn from_cf_type(value: CFType) -> Option<Self> {
        // SAFETY: The function has no preconditions and only returns the stable
        // runtime type identifier for AXUIElement.
        let expected_type = unsafe { ax_ui_element_get_type_id() };
        (value.type_of() == expected_type).then_some(Self { value })
    }

    fn as_raw(&self) -> AxUiElementRef {
        self.value.as_CFTypeRef().cast()
    }

    fn same(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AxCallFailure {
    NoValue,
    Unsupported,
    Failed,
    TypeMismatch,
    InvalidValue,
}

fn process_is_trusted() -> bool {
    // SAFETY: The function takes no pointers and performs a read-only process
    // trust query. It does not display the system prompt.
    unsafe { ax_is_process_trusted() != 0 }
}

/// Whether this process is its own TCC responsible process.
///
/// macOS attributes an Accessibility grant to the *responsible* process, not
/// necessarily the calling one. A binary launched from a terminal or an IDE
/// inherits that launcher's grant, so `AXIsProcessTrusted` answers `true` while
/// the app itself was never registered and never appears in System Settings.
/// Cross-process AX queries still fail, because they run under this process's
/// own identity. Comparing the responsible pid against our own separates a real
/// grant from a borrowed one.
fn process_owns_its_trust() -> bool {
    let me = std::process::id();
    // SAFETY: Takes a pid by value and returns a pid. No pointers involved.
    // Returns -1 (or our own pid) when the responsible pid is unavailable;
    // treating "unknown" as owned avoids a false alarm in the packaged app.
    let responsible = unsafe { responsibility_get_pid_responsible_for_pid(me as pid_t) };
    responsible <= 0 || responsible == me as pid_t
}

fn request_accessibility_prompt() -> bool {
    // SAFETY: Apple exports this as an immortal CFStringRef constant. Wrapping
    // it under the Get Rule retains it for the dictionary's lifetime.
    let prompt_key = unsafe { CFString::wrap_under_get_rule(AX_TRUSTED_CHECK_OPTION_PROMPT) };
    let prompt_value = CFBoolean::true_value();
    let options = CFDictionary::from_CFType_pairs(&[(prompt_key, prompt_value)]);
    // SAFETY: `options` is a valid immutable CFDictionary that remains alive
    // for the entire call. The API only reads the dictionary.
    unsafe { ax_is_process_trusted_with_options(options.as_concrete_TypeRef()) != 0 }
}

fn lock_ax_ffi() -> std::sync::MutexGuard<'static, ()> {
    AX_FFI_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn secure_event_input_is_enabled() -> bool {
    // SAFETY: Apple marks this process-global read as not thread-safe. Every
    // call is made while `AX_FFI_GATE` is held.
    unsafe { is_secure_event_input_enabled() != 0 }
}

fn current_process_id() -> pid_t {
    // SAFETY: `getpid` has no preconditions and cannot fail.
    unsafe { libc::getpid() }
}

fn frontmost_application_pid() -> Option<pid_t> {
    let application = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    let pid = application.processIdentifier();
    (pid > 0).then_some(pid)
}

fn copy_attribute(element: &AxElement, attribute: &str) -> Result<CFType, AxCallFailure> {
    let attribute = CFString::new(attribute);
    let mut value: CFTypeRef = ptr::null();
    // SAFETY: `element` and `attribute` own valid references for the duration
    // of the call. The non-null out pointer is initialized by the API on
    // success and follows the Copy Rule.
    let error = unsafe {
        ax_ui_element_copy_attribute_value(
            element.as_raw(),
            attribute.as_concrete_TypeRef(),
            &mut value,
        )
    };
    if error != AX_ERROR_SUCCESS {
        return Err(classify_ax_error(error));
    }
    if value.is_null() {
        return Err(AxCallFailure::InvalidValue);
    }
    // SAFETY: A successful Copy call returns a +1 retained CF object.
    Ok(unsafe { CFType::wrap_under_create_rule(value) })
}

fn copy_element(element: &AxElement, attribute: &str) -> Result<AxElement, AxCallFailure> {
    let value = copy_attribute(element, attribute)?;
    AxElement::from_cf_type(value).ok_or(AxCallFailure::TypeMismatch)
}

fn copy_string(element: &AxElement, attribute: &str) -> Result<String, AxCallFailure> {
    copy_attribute(element, attribute)?
        .downcast_into::<CFString>()
        .map(|value| value.to_string())
        .ok_or(AxCallFailure::TypeMismatch)
}

fn copy_optional_string(
    element: &AxElement,
    attribute: &str,
) -> Result<Option<String>, AxCallFailure> {
    match copy_string(element, attribute) {
        Ok(value) => Ok(Some(value)),
        Err(AxCallFailure::NoValue | AxCallFailure::Unsupported) => Ok(None),
        Err(error) => Err(error),
    }
}

fn copy_bool(element: &AxElement, attribute: &str) -> Result<bool, AxCallFailure> {
    copy_attribute(element, attribute)?
        .downcast_into::<CFBoolean>()
        .map(bool::from)
        .ok_or(AxCallFailure::TypeMismatch)
}

fn copy_character_count(element: &AxElement) -> Result<usize, AxCallFailure> {
    let count = copy_attribute(element, AX_NUMBER_OF_CHARACTERS)?
        .downcast_into::<CFNumber>()
        .and_then(|number| number.to_i64())
        .ok_or(AxCallFailure::TypeMismatch)?;
    usize::try_from(count).map_err(|_| AxCallFailure::InvalidValue)
}

fn window_display_hint(element: &AxElement) -> Result<TargetDisplayHint, AxCallFailure> {
    let position = copy_ax_point(element, AX_POSITION)?;
    let size = copy_ax_size(element, AX_SIZE)?;
    display_hint_from_geometry(position, size).ok_or(AxCallFailure::InvalidValue)
}

fn copy_ax_point(element: &AxElement, attribute: &str) -> Result<AxPoint, AxCallFailure> {
    let value = copy_attribute(element, attribute)?;
    copy_ax_value(&value, AX_VALUE_CG_POINT, AxPoint { x: 0.0, y: 0.0 })
}

fn copy_ax_size(element: &AxElement, attribute: &str) -> Result<AxSize, AxCallFailure> {
    let value = copy_attribute(element, attribute)?;
    copy_ax_value(
        &value,
        AX_VALUE_CG_SIZE,
        AxSize {
            width: 0.0,
            height: 0.0,
        },
    )
}

fn copy_ax_value<T: Copy>(
    value: &CFType,
    value_type: AxValueType,
    mut destination: T,
) -> Result<T, AxCallFailure> {
    // SAFETY: Type identifier access has no preconditions.
    if value.type_of() != unsafe { ax_value_get_type_id() } {
        return Err(AxCallFailure::TypeMismatch);
    }
    let raw: AxValueRef = value.as_CFTypeRef().cast();
    // SAFETY: `raw` was type-checked as AXValue, while the caller supplies a
    // destination matching `value_type` and keeps it alive for this call.
    let success =
        unsafe { ax_value_get_value(raw, value_type, (&raw mut destination).cast::<c_void>()) };
    (success != 0)
        .then_some(destination)
        .ok_or(AxCallFailure::InvalidValue)
}

fn display_hint_from_geometry(position: AxPoint, size: AxSize) -> Option<TargetDisplayHint> {
    if !position.x.is_finite()
        || !position.y.is_finite()
        || !size.width.is_finite()
        || !size.height.is_finite()
        || size.width < 0.0
        || size.height < 0.0
    {
        return None;
    }
    Some(TargetDisplayHint {
        x: rounded_coordinate(position.x + size.width / 2.0),
        y: rounded_coordinate(position.y + size.height / 2.0),
    })
}

fn rounded_coordinate(value: f64) -> i32 {
    value
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

fn copy_range(element: &AxElement, attribute: &str) -> Result<TextRange, AxCallFailure> {
    let value = copy_attribute(element, attribute)?;
    // SAFETY: Type identifier access has no preconditions.
    if value.type_of() != unsafe { ax_value_get_type_id() } {
        return Err(AxCallFailure::TypeMismatch);
    }
    let raw: AxValueRef = value.as_CFTypeRef().cast();
    let mut range = CFRange {
        location: 0,
        length: 0,
    };
    // SAFETY: `raw` was type-checked as AXValue, and `range` is a writable
    // destination of the exact type requested.
    let success =
        unsafe { ax_value_get_value(raw, AX_VALUE_CF_RANGE, (&raw mut range).cast::<c_void>()) };
    if success == 0 {
        return Err(AxCallFailure::InvalidValue);
    }
    text_range_from_cf(range).ok_or(AxCallFailure::InvalidValue)
}

fn set_selected_range(element: &AxElement, range: TextRange) -> Result<(), AxCallFailure> {
    let range = cf_range_from_text(range).ok_or(AxCallFailure::InvalidValue)?;
    // SAFETY: `range` is a valid CFRange that remains alive for the call. The
    // Create Rule result is either null or a +1 retained AXValueRef.
    let raw = unsafe { ax_value_create(AX_VALUE_CF_RANGE, (&raw const range).cast::<c_void>()) };
    if raw.is_null() {
        return Err(AxCallFailure::Failed);
    }
    // SAFETY: `raw` is a +1 retained Core Foundation object.
    let value = unsafe { CFType::wrap_under_create_rule(raw.cast()) };
    let attribute = CFString::new(AX_SELECTED_TEXT_RANGE);
    // SAFETY: All references are valid for the duration of the call. This is
    // the only AX setter in the module and it changes selection only.
    let error = unsafe {
        ax_ui_element_set_attribute_value(
            element.as_raw(),
            attribute.as_concrete_TypeRef(),
            value.as_CFTypeRef(),
        )
    };
    ax_result(error)
}

fn set_selected_text(element: &AxElement, text: &str) -> Result<(), AxCallFailure> {
    if text.is_empty() {
        return Err(AxCallFailure::InvalidValue);
    }
    let value = CFString::new(text);
    let attribute = CFString::new(AX_SELECTED_TEXT);
    #[cfg(test)]
    AX_SELECTED_TEXT_SETTER_CALLS.fetch_add(1, AtomicOrdering::SeqCst);
    // SAFETY: The AX element and both CFString references remain valid for the
    // duration of the call. The caller has already proven an empty selected
    // range on this exact element; setting AXSelectedText therefore inserts at
    // that caret rather than replacing existing content.
    let error = unsafe {
        ax_ui_element_set_attribute_value(
            element.as_raw(),
            attribute.as_concrete_TypeRef(),
            value.as_CFTypeRef(),
        )
    };
    ax_result(error)
}

fn attribute_is_settable(element: &AxElement, attribute: &str) -> Result<bool, AxCallFailure> {
    let attribute = CFString::new(attribute);
    let mut settable: Boolean = 0;
    // SAFETY: The references and output pointer remain valid for the call.
    let error = unsafe {
        ax_ui_element_is_attribute_settable(
            element.as_raw(),
            attribute.as_concrete_TypeRef(),
            &mut settable,
        )
    };
    ax_result(error)?;
    Ok(settable != 0)
}

fn copy_string_for_range(element: &AxElement, range: TextRange) -> Result<String, AxCallFailure> {
    copy_parameterized_attribute_for_range(element, AX_STRING_FOR_RANGE, range)?
        .downcast_into::<CFString>()
        .map(|value| value.to_string())
        .ok_or(AxCallFailure::TypeMismatch)
}

fn copy_attributed_string_for_range(
    element: &AxElement,
    range: TextRange,
) -> Result<String, AxCallFailure> {
    let value =
        copy_parameterized_attribute_for_range(element, AX_ATTRIBUTED_STRING_FOR_RANGE, range)?
            .downcast_into::<CFAttributedString>()
            .ok_or(AxCallFailure::TypeMismatch)?;
    // SAFETY: `value` is a live CFAttributedString. The returned CFString is
    // borrowed from it and wrapped under the Get Rule, which retains it before
    // `value` leaves scope.
    let string = unsafe { CFAttributedStringGetString(value.as_concrete_TypeRef()) };
    if string.is_null() {
        return Err(AxCallFailure::InvalidValue);
    }
    // SAFETY: `string` is a valid borrowed CFStringRef returned by Core
    // Foundation and is retained by `wrap_under_get_rule`.
    Ok(unsafe { CFString::wrap_under_get_rule(string) }.to_string())
}

fn copy_text_for_range(element: &AxElement, range: TextRange) -> Result<String, AxCallFailure> {
    copy_string_for_range(element, range)
        .or_else(|_| copy_attributed_string_for_range(element, range))
}

fn copy_parameterized_attribute_for_range(
    element: &AxElement,
    attribute: &str,
    range: TextRange,
) -> Result<CFType, AxCallFailure> {
    let range = cf_range_from_text(range).ok_or(AxCallFailure::InvalidValue)?;
    // SAFETY: `range` remains alive for AXValueCreate and the returned object
    // follows the Create Rule.
    let parameter =
        unsafe { ax_value_create(AX_VALUE_CF_RANGE, (&raw const range).cast::<c_void>()) };
    if parameter.is_null() {
        return Err(AxCallFailure::Failed);
    }
    // SAFETY: `parameter` is a +1 retained Core Foundation object.
    let parameter = unsafe { CFType::wrap_under_create_rule(parameter.cast()) };
    let attribute = CFString::new(attribute);
    let mut result: CFTypeRef = ptr::null();
    // SAFETY: All input references are valid and `result` is a non-null out
    // pointer. A successful call returns a +1 retained result.
    let error = unsafe {
        ax_ui_element_copy_parameterized_attribute_value(
            element.as_raw(),
            attribute.as_concrete_TypeRef(),
            parameter.as_CFTypeRef(),
            &mut result,
        )
    };
    if error != AX_ERROR_SUCCESS {
        return Err(classify_ax_error(error));
    }
    if result.is_null() {
        return Err(AxCallFailure::InvalidValue);
    }
    // SAFETY: A successful Copy call returns a +1 retained CF object.
    Ok(unsafe { CFType::wrap_under_create_rule(result) })
}

fn element_pid(element: &AxElement) -> Result<pid_t, AxCallFailure> {
    let mut pid: pid_t = 0;
    // SAFETY: `element` is a valid AXUIElement and `pid` is a writable output.
    let error = unsafe { ax_ui_element_get_pid(element.as_raw(), &mut pid) };
    ax_result(error)?;
    (pid > 0).then_some(pid).ok_or(AxCallFailure::InvalidValue)
}

fn set_global_messaging_timeout(element: &AxElement) -> Result<(), AxCallFailure> {
    // SAFETY: `element` is the valid system-wide AXUIElement created by this
    // module. The positive timeout is within the API's documented domain.
    let error = unsafe {
        ax_ui_element_set_messaging_timeout(element.as_raw(), AX_MESSAGING_TIMEOUT_SECONDS)
    };
    ax_result(error)
}

fn text_range_from_cf(range: CFRange) -> Option<TextRange> {
    let location = usize::try_from(range.location).ok()?;
    let length = usize::try_from(range.length).ok()?;
    location.checked_add(length)?;
    Some(TextRange { location, length })
}

fn cf_range_from_text(range: TextRange) -> Option<CFRange> {
    range.end()?;
    Some(CFRange {
        location: CFIndex::try_from(range.location).ok()?,
        length: CFIndex::try_from(range.length).ok()?,
    })
}

fn ax_result(error: AxError) -> Result<(), AxCallFailure> {
    if error == AX_ERROR_SUCCESS {
        Ok(())
    } else {
        Err(classify_ax_error(error))
    }
}

fn classify_ax_error(error: AxError) -> AxCallFailure {
    match error {
        AX_ERROR_NO_VALUE => AxCallFailure::NoValue,
        AX_ERROR_ATTRIBUTE_UNSUPPORTED => AxCallFailure::Unsupported,
        _ => AxCallFailure::Failed,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
        time::Duration,
    };

    use super::*;

    const LIVE_SMOKE_GATE: &str = "REMTENE_RUN_LIVE_MACOS_AX_SMOKE";
    const LIVE_REVOKED_GATE: &str = "REMTENE_EXPECT_MACOS_AX_REVOKED";
    const LIVE_SECURE_GATE: &str = "REMTENE_EXPECT_MACOS_SECURE_INPUT";
    const FIXTURE_SINGLE_LINE_TEXT: &str = "FIELD_ALPHA_固定初始文字";
    const FIXTURE_MULTI_LINE_TEXT: &str = "TEXTVIEW_BETA_第一行\nTEXTVIEW_BETA_第二行";
    const FIXTURE_CARET_TEXT: &str = "FIELD_CARET_固定初始文字";
    const LIVE_INSERT_TEXT: &str = " [REMTENE_AX_M1A_7E2C]";
    const LIVE_CARET_INSERT_TEXT: &str = " [REMTENE_AX_CARET_41D9]";
    const LIVE_RACE_INSERT_TEXT: &str = " [REMTENE_AX_RACE_MUST_NOT_APPEAR]";

    fn block_on<F>(future: F) -> F::Output
    where
        F: Future,
    {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = pin!(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn assert_focused_fixture_text(expected: &str, expected_range: TextRange) {
        let backend = MacBackend::new();
        let observation = backend.observe_focused_target();
        assert_eq!(
            classify_security(&observation),
            TargetSecurity::Safe,
            "expected a trusted, non-secure editable fixture"
        );
        assert_eq!(observation.selected_range, Some(expected_range));
        assert!(
            observation.display_hint.is_some(),
            "fixture window must provide a display placement hint"
        );
        let identity = observation.identity.expect("focused fixture identity");
        let _gate = lock_ax_ffi();
        let character_count = copy_character_count(&identity.control).expect("character count");
        assert_eq!(character_count, expected.encode_utf16().count());
        let value = copy_string_for_range(
            &identity.control,
            TextRange {
                location: 0,
                length: character_count,
            },
        )
        .expect("read exact fixture text");
        assert_eq!(
            value, expected,
            "focused text is not the controlled fixture"
        );
    }

    fn run_live_target_race(expected_race: &str) {
        assert_eq!(
            std::env::var(LIVE_SMOKE_GATE).as_deref(),
            Ok("1"),
            "set {LIVE_SMOKE_GATE}=1 explicitly to run the live AX race gate"
        );
        assert_eq!(
            std::env::var("REMTENE_AX_TARGET_RACE").as_deref(),
            Ok(expected_race),
            "set REMTENE_AX_TARGET_RACE={expected_race} for this controlled gate"
        );

        eprintln!(
            "arming target-{expected_race} gate for 5 seconds; focus and fully select a documented RemTene sentinel"
        );
        thread::sleep(Duration::from_secs(5));

        let adapter = MacTargetContext::new();
        let captured = block_on(adapter.capture()).expect("capture controlled target");
        assert_eq!(captured.security, TargetSecurity::Safe);
        assert!(captured.has_selection);
        let selection = block_on(adapter.read_selected_text(&captured.target_ref))
            .expect("read controlled sentinel");
        assert!(matches!(
            selection.text.as_deref(),
            Some(FIXTURE_SINGLE_LINE_TEXT | FIXTURE_MULTI_LINE_TEXT)
        ));
        let validated = match block_on(adapter.revalidate(&captured.target_ref))
            .expect("validate controlled target before race")
        {
            TargetRevalidation::Valid(target) => target,
            other => panic!("controlled target was not valid before race: {other:?}"),
        };
        let setter_calls_before = selected_text_setter_call_count();

        eprintln!(
            "target captured; within 5 seconds {} the original TextEdit target",
            if expected_race == "switch" {
                "switch to a different controlled document from"
            } else {
                "close"
            }
        );
        thread::sleep(Duration::from_secs(5));

        let outcome = block_on(adapter.insert(
            validated,
            LIVE_RACE_INSERT_TEXT.to_owned(),
            DeliveryId::new(),
            LifecycleFence::new(),
        ))
        .expect("race delivery returns a typed outcome");
        assert_eq!(outcome, InsertOutcome::NotInserted);
        assert_eq!(
            selected_text_setter_call_count(),
            setter_calls_before,
            "target race reached AXSelectedText setter"
        );
    }

    #[test]
    fn range_conversion_rejects_negative_and_overflow_values() {
        assert_eq!(
            text_range_from_cf(CFRange {
                location: 3,
                length: 4,
            }),
            Some(TextRange {
                location: 3,
                length: 4,
            })
        );
        assert!(
            text_range_from_cf(CFRange {
                location: -1,
                length: 1,
            })
            .is_none()
        );
        assert!(
            text_range_from_cf(CFRange {
                location: 0,
                length: -1,
            })
            .is_none()
        );
        assert!(
            cf_range_from_text(TextRange {
                location: usize::MAX,
                length: 1,
            })
            .is_none()
        );
    }

    #[test]
    fn insert_text_policy_rejects_action_like_control_characters() {
        assert!(text_supported_by_role(
            MacTextRole::SingleLine,
            "hello 世界"
        ));
        assert!(!text_supported_by_role(MacTextRole::SingleLine, ""));
        assert!(!text_supported_by_role(MacTextRole::SingleLine, "a\nb"));
        assert!(!text_supported_by_role(MacTextRole::MultiLine, "a\tb"));
        assert!(text_supported_by_role(MacTextRole::MultiLine, "a\nb"));
        assert_eq!("🎵".encode_utf16().count(), 2);
    }

    #[test]
    fn ax_readback_accepts_only_exact_line_break_equivalence() {
        let expected = "第一项\n第二项\n🎵";
        assert_eq!(
            inserted_text_match(expected, expected),
            InsertedTextMatch::Exact
        );
        for equivalent in [
            "第一项\r第二项\r🎵",
            "第一项\r\n第二项\r\n🎵",
            "第一项\u{2028}第二项\u{2029}🎵",
        ] {
            assert_eq!(
                inserted_text_match(expected, equivalent),
                InsertedTextMatch::LineBreakEquivalent
            );
        }

        for changed in [
            "第一项 第二项 🎵",
            "第一项\n第二项！\n🎵",
            "第一项\n\n第二项\n🎵",
            "第一项\n第二项",
        ] {
            assert_eq!(
                inserted_text_match(expected, changed),
                InsertedTextMatch::Mismatch
            );
        }
    }

    #[test]
    fn insertion_length_candidates_cover_lf_and_crlf_without_widening_content() {
        let text = "1. 十点来。\n2. 开门记得打卡。";
        let expected_utf16 = text.encode_utf16().count();
        assert_eq!(
            compatible_utf16_length_bounds(text),
            Some((expected_utf16, expected_utf16 + 1))
        );

        assert_eq!(
            insertion_length_observation(
                10,
                text,
                Some(TextRange::caret_at(10)),
                Some(40),
                Some(40),
                InsertionMarkerPolicy::Any,
            ),
            InsertionLengthObservation::Pending
        );
        assert_eq!(
            insertion_length_observation(
                10,
                text,
                Some(TextRange::caret_at(10 + expected_utf16 + 1)),
                Some(40),
                Some(40 + expected_utf16 + 1),
                InsertionMarkerPolicy::Any,
            ),
            InsertionLengthObservation::Candidates([Some(expected_utf16 + 1), None])
        );
        assert_eq!(
            insertion_length_observation(
                10,
                text,
                Some(TextRange::caret_at(10 + expected_utf16 + 2)),
                Some(40),
                Some(40 + expected_utf16 + 2),
                InsertionMarkerPolicy::Any,
            ),
            InsertionLengthObservation::Unverified
        );
    }

    #[test]
    fn insertion_length_candidates_preserve_either_independent_marker() {
        let text = "第一行\n第二行";
        let length = text.encode_utf16().count();

        assert_eq!(
            insertion_length_observation(
                4,
                text,
                Some(TextRange::caret_at(4 + length)),
                Some(9),
                None,
                InsertionMarkerPolicy::Any,
            ),
            InsertionLengthObservation::Candidates([Some(length), None])
        );
        assert_eq!(
            insertion_length_observation(
                4,
                text,
                None,
                Some(9),
                Some(9 + length),
                InsertionMarkerPolicy::Any,
            ),
            InsertionLengthObservation::Candidates([Some(length), None])
        );
    }

    #[test]
    fn element_bound_verification_requires_both_markers_to_agree() {
        let text = "第一行\n第二行";
        let length = text.encode_utf16().count();

        assert_eq!(
            insertion_length_observation(
                4,
                text,
                Some(TextRange::caret_at(4 + length)),
                Some(9),
                Some(9 + length),
                InsertionMarkerPolicy::Both,
            ),
            InsertionLengthObservation::Candidates([Some(length), None])
        );
        assert_eq!(
            insertion_length_observation(
                4,
                text,
                Some(TextRange::caret_at(4 + length)),
                Some(9),
                None,
                InsertionMarkerPolicy::Both,
            ),
            InsertionLengthObservation::Unverified
        );
        assert_eq!(
            insertion_length_observation(
                4,
                text,
                Some(TextRange::caret_at(4 + length)),
                Some(9),
                Some(9 + length + 1),
                InsertionMarkerPolicy::Both,
            ),
            InsertionLengthObservation::Unverified
        );
    }

    #[test]
    fn derives_content_free_display_hint_from_window_center() {
        assert_eq!(
            display_hint_from_geometry(
                AxPoint {
                    x: -1_280.0,
                    y: 120.0,
                },
                AxSize {
                    width: 1_000.0,
                    height: 700.0,
                },
            ),
            Some(TargetDisplayHint { x: -780, y: 470 })
        );
    }

    #[test]
    fn rejects_non_finite_or_negative_window_geometry() {
        assert_eq!(
            display_hint_from_geometry(
                AxPoint {
                    x: f64::NAN,
                    y: 0.0
                },
                AxSize {
                    width: 100.0,
                    height: 100.0,
                },
            ),
            None
        );
        assert_eq!(
            display_hint_from_geometry(
                AxPoint { x: 0.0, y: 0.0 },
                AxSize {
                    width: -1.0,
                    height: 100.0,
                },
            ),
            None
        );
    }

    #[test]
    fn ffi_symbols_link_and_system_wide_element_has_the_ax_type() {
        // SAFETY: Type identifier functions have no preconditions.
        assert_ne!(unsafe { ax_ui_element_get_type_id() }, 0);
        // SAFETY: Type identifier functions have no preconditions.
        assert_ne!(unsafe { ax_value_get_type_id() }, 0);
        let system = AxElement::system_wide().expect("system-wide AX element");
        // SAFETY: Type identifier functions have no preconditions.
        assert_eq!(system.value.type_of(), unsafe {
            ax_ui_element_get_type_id()
        });
    }

    #[test]
    fn live_observation_is_read_only_and_fail_closed() {
        let backend = MacBackend::new();
        let observation = backend.observe_focused_target();
        match classify_security(&observation) {
            TargetSecurity::Safe => {
                assert!(observation.identity.is_some());
                assert!(observation.selected_range.is_some());
                assert!(!observation.belongs_to_this_process);
            }
            TargetSecurity::SecureInput | TargetSecurity::Unknown => {}
        }
    }

    #[test]
    #[ignore = "requires explicitly revoked macOS Accessibility permission"]
    fn live_revoked_accessibility_fails_closed_before_target_access() {
        assert_eq!(
            std::env::var(LIVE_REVOKED_GATE).as_deref(),
            Ok("1"),
            "set {LIVE_REVOKED_GATE}=1 only after explicitly revoking Accessibility permission"
        );
        assert!(
            !process_is_trusted(),
            "Accessibility permission is still trusted; refusing to claim revoked-state evidence"
        );

        let setter_calls_before = selected_text_setter_call_count();
        let observation = MacBackend::new().observe_focused_target();
        assert_eq!(classify_security(&observation), TargetSecurity::Unknown);
        assert!(observation.identity.is_none());
        assert!(observation.display_hint.is_none());
        assert!(observation.selected_range.is_none());
        assert!(!observation.belongs_to_this_process);
        assert_eq!(
            selected_text_setter_call_count(),
            setter_calls_before,
            "revoked observation reached the AXSelectedText setter"
        );
    }

    #[test]
    #[ignore = "requires explicitly active macOS Secure Event Input"]
    fn live_secure_input_fails_closed_before_target_access() {
        assert_eq!(
            std::env::var(LIVE_SECURE_GATE).as_deref(),
            Ok("1"),
            "set {LIVE_SECURE_GATE}=1 only while controlled macOS Secure Event Input is active"
        );

        eprintln!(
            "arming secure-input gate for 5 seconds; enable the controlled macOS Secure Event Input state now"
        );
        thread::sleep(Duration::from_secs(5));

        let (accessibility_trusted, secure_input_active) = {
            let _gate = lock_ax_ffi();
            (process_is_trusted(), secure_event_input_is_enabled())
        };
        assert!(
            accessibility_trusted,
            "Accessibility permission is not trusted; refusing to conflate revoked access with secure-input evidence"
        );
        assert!(
            secure_input_active,
            "macOS Secure Event Input is not active; refusing to claim secure-field evidence"
        );

        let setter_calls_before = selected_text_setter_call_count();
        let observation = MacBackend::new().observe_focused_target();
        assert_eq!(classify_security(&observation), TargetSecurity::SecureInput);
        assert!(observation.identity.is_none());
        assert!(observation.display_hint.is_none());
        assert!(observation.selected_range.is_none());
        assert!(!observation.belongs_to_this_process);
        assert_eq!(
            selected_text_setter_call_count(),
            setter_calls_before,
            "secure-input observation reached the AXSelectedText setter"
        );
    }

    #[test]
    fn insert_with_a_forged_capability_token_fails_closed_without_native_access() {
        // Under ADR-0005 (option B') `MacTargetContext::new()` routes delivery
        // through the fail-closed state machine. A token that never came from a
        // real capture/revalidate cannot resolve to a validated capability, so
        // no collapse or AX selected-text write can occur and the outcome is
        // `NotInserted`. This proves the public adapter neither prompts for
        // permission nor mutates any AX state on construction or on a bogus
        // insert.
        let adapter = MacTargetContext::new();
        let outcome = block_on(adapter.insert(
            ValidatedTargetRef::new("must-not-reach-the-native-registry"),
            "must not be dispatched".to_owned(),
            DeliveryId::new(),
            LifecycleFence::new(),
        ))
        .expect("forged capability delivery fails closed without native access");
        assert_eq!(outcome, InsertOutcome::NotInserted);
    }

    #[test]
    #[ignore = "requests the macOS Accessibility prompt; run only with the live smoke gate"]
    fn live_request_accessibility_permission_prompt() {
        assert_eq!(
            std::env::var(LIVE_SMOKE_GATE).as_deref(),
            Ok("1"),
            "set {LIVE_SMOKE_GATE}=1 explicitly to request Accessibility permission"
        );
        let trusted_before = process_is_trusted();
        let trusted_now = MacTargetContext::request_accessibility_permission_prompt();
        eprintln!(
            "accessibility permission requested: trusted_before={trusted_before} \
             trusted_now={trusted_now}; grant access in System Settings, then rerun the \
             read-only diagnostic"
        );
    }

    #[test]
    #[ignore = "diagnostic observation timeline; run only with the live smoke gate"]
    fn live_ax_debug_observation_timeline() {
        assert_eq!(
            std::env::var(LIVE_SMOKE_GATE).as_deref(),
            Ok("1"),
            "set {LIVE_SMOKE_GATE}=1 explicitly to run the live AX diagnostic"
        );

        fn probe(parent: &AxElement, attribute_name: &str) -> (AxError, Option<AxElement>) {
            let attribute = CFString::new(attribute_name);
            let mut value: CFTypeRef = ptr::null();
            // SAFETY: same contract as `copy_attribute`; valid refs, out pointer.
            let error = unsafe {
                ax_ui_element_copy_attribute_value(
                    parent.as_raw(),
                    attribute.as_concrete_TypeRef(),
                    &mut value,
                )
            };
            if error != AX_ERROR_SUCCESS || value.is_null() {
                return (error, None);
            }
            // SAFETY: successful Copy call returns a +1 retained CF object.
            let cf = unsafe { CFType::wrap_under_create_rule(value) };
            (error, AxElement::from_cf_type(cf))
        }

        eprintln!("waiting 3s for the target to come frontmost...");
        thread::sleep(Duration::from_secs(3));

        let _gate = lock_ax_ffi();
        eprintln!("trusted={}", process_is_trusted());
        eprintln!("secure_input={}", secure_event_input_is_enabled());

        let Some(system) = AxElement::system_wide() else {
            eprintln!("STEP FAIL: AXUIElementCreateSystemWide returned null");
            return;
        };
        eprintln!("system_wide: ok");

        // SAFETY: same contract as `set_global_messaging_timeout`.
        let timeout_error = unsafe {
            ax_ui_element_set_messaging_timeout(system.as_raw(), AX_MESSAGING_TIMEOUT_SECONDS)
        };
        eprintln!("set_messaging_timeout err={timeout_error}");

        // Kept as regression evidence: on macOS 26 this system-wide query
        // fails with kAXErrorCannotComplete for CLI test hosts regardless of
        // TCC trust, which is why production resolves the frontmost
        // application through NSWorkspace instead.
        let (systemwide_error, _) = probe(&system, AX_FOCUSED_APPLICATION);
        eprintln!("systemwide AXFocusedApplication err={systemwide_error} (informational)");

        let Some(pid) = frontmost_application_pid() else {
            eprintln!("STEP FAIL: NSWorkspace reported no frontmost application");
            return;
        };
        let name = std::process::Command::new("ps")
            .args(["-o", "comm=", "-p", &pid.to_string()])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
        eprintln!("frontmost pid={pid} command={name:?}");
        let Some(application) = AxElement::application(pid) else {
            eprintln!("STEP FAIL: AXUIElementCreateApplication returned null");
            return;
        };

        let (window_error, window) = probe(&application, AX_FOCUSED_WINDOW);
        eprintln!(
            "AXFocusedWindow err={window_error} got={}",
            window.is_some()
        );

        let (control_error, control) = probe(&application, AX_FOCUSED_UI_ELEMENT);
        eprintln!(
            "AXFocusedUIElement(application) err={control_error} got={}",
            control.is_some()
        );

        // Control comparison for the `no_focused_control` failure: some
        // applications expose the focused control only on the window, not on the
        // application element. If the application query fails while this one
        // succeeds, production is asking the wrong element.
        if let Some(window) = window.as_ref() {
            let (window_control_error, window_control) = probe(window, AX_FOCUSED_UI_ELEMENT);
            eprintln!(
                "AXFocusedUIElement(window) err={window_control_error} got={}",
                window_control.is_some()
            );
            if let Some(window_control) = window_control.as_ref() {
                eprintln!(
                    "window-focused role={:?} subrole={:?}",
                    copy_string(window_control, AX_ROLE),
                    copy_optional_string(window_control, AX_SUBROLE)
                );
            }
        }

        let Some(control) = control else {
            eprintln!("STEP FAIL: application reported no focused UI element");
            return;
        };

        eprintln!("role={:?}", copy_string(&control, AX_ROLE));
        eprintln!("subrole={:?}", copy_optional_string(&control, AX_SUBROLE));

        let (control_window_error, control_window) = probe(&control, AX_WINDOW);
        eprintln!(
            "control AXWindow err={control_window_error} got={}",
            control_window.is_some()
        );
        if let (Some(window), Some(control_window)) = (window.as_ref(), control_window.as_ref()) {
            eprintln!("window == control.window: {}", window.same(control_window));
        }

        eprintln!("enabled={:?}", copy_bool(&control, AX_ENABLED));
        eprintln!(
            "range settable={:?}",
            attribute_is_settable(&control, AX_SELECTED_TEXT_RANGE)
        );
        eprintln!(
            "selected text settable={:?}",
            attribute_is_settable(&control, AX_SELECTED_TEXT)
        );
        eprintln!(
            "selected range={:?}",
            copy_range(&control, AX_SELECTED_TEXT_RANGE)
        );
    }

    #[test]
    #[ignore = "mutates an explicitly focused caret fixture; run only with the live smoke gate"]
    fn live_ax_revalidated_insert_appends_at_an_unselected_caret() {
        assert_eq!(
            std::env::var(LIVE_SMOKE_GATE).as_deref(),
            Ok("1"),
            "set {LIVE_SMOKE_GATE}=1 explicitly to run the live AX caret gate"
        );

        eprintln!(
            "arming live caret gate for 5 seconds; focus the documented caret fixture with its caret exactly at the end"
        );
        thread::sleep(Duration::from_secs(5));

        let caret = FIXTURE_CARET_TEXT.encode_utf16().count();
        assert_focused_fixture_text(FIXTURE_CARET_TEXT, TextRange::caret_at(caret));
        let adapter = MacTargetContext::new();
        let captured = block_on(adapter.capture()).expect("capture controlled caret fixture");
        assert_eq!(captured.security, TargetSecurity::Safe);
        assert!(!captured.has_selection);
        assert!(captured.display_hint.is_some());

        let validated = match block_on(adapter.revalidate(&captured.target_ref))
            .expect("revalidate controlled caret fixture")
        {
            TargetRevalidation::Valid(target) => target,
            other => panic!("caret fixture was not exactly revalidated: {other:?}"),
        };
        let setter_calls_before = selected_text_setter_call_count();
        let outcome = block_on(adapter.insert(
            validated,
            LIVE_CARET_INSERT_TEXT.to_owned(),
            DeliveryId::new(),
            LifecycleFence::new(),
        ))
        .expect("perform the gated caret insert-only operation");
        assert_eq!(outcome, InsertOutcome::Inserted);
        assert_eq!(selected_text_setter_call_count(), setter_calls_before + 1);

        let expected = format!("{FIXTURE_CARET_TEXT}{LIVE_CARET_INSERT_TEXT}");
        assert_focused_fixture_text(
            &expected,
            TextRange::caret_at(expected.encode_utf16().count()),
        );
    }

    #[test]
    #[ignore = "requires a controlled target switch during the live AX gate"]
    fn live_ax_target_switch_fails_closed_before_the_setter() {
        run_live_target_race("switch");
    }

    #[test]
    #[ignore = "requires closing the controlled target during the live AX gate"]
    fn live_ax_target_close_fails_closed_before_the_setter() {
        run_live_target_race("close");
    }

    #[test]
    #[ignore = "mutates an explicitly focused sentinel; run only with the live smoke gate"]
    fn live_ax_revalidated_insert_delivers_to_the_focused_sentinel() {
        assert_eq!(
            std::env::var(LIVE_SMOKE_GATE).as_deref(),
            Ok("1"),
            "live AX smoke mutates the focused sentinel; explicitly set \
             {LIVE_SMOKE_GATE}=1 and follow pocs/platform/macos-input-fixture/README.md"
        );

        eprintln!(
            "arming live AX smoke for 5 seconds; now focus and fully select one documented \
             RemTene sentinel in a controlled editable field"
        );
        std::thread::sleep(Duration::from_secs(5));

        // The production adapter is exactly what B' ships: delivery goes through
        // the pre-dispatch revalidation path in `SafeTargetContext::insert`.
        let adapter = MacTargetContext::new();
        let captured = block_on(adapter.capture()).expect("capture the currently focused target");
        assert_eq!(
            captured.security,
            TargetSecurity::Safe,
            "expected a trusted, non-secure text control; this test never requests \
             Accessibility permission"
        );
        assert!(
            captured.has_selection,
            "target identity cannot be proven without a fixed, fully selected sentinel text"
        );

        let selection = block_on(adapter.read_selected_text(&captured.target_ref))
            .expect("read the exact captured sentinel selection");
        assert!(selection.anchor_normalized_to_end);
        assert!(!selection.exceeded_limit);
        let selected_text = selection
            .text
            .as_deref()
            .expect("target must expose its fixed selected text");
        assert!(
            matches!(
                selected_text,
                FIXTURE_SINGLE_LINE_TEXT | FIXTURE_MULTI_LINE_TEXT
            ),
            "focused selection is not a known RemTene sentinel; refusing to insert"
        );

        let validated = match block_on(adapter.revalidate(&captured.target_ref))
            .expect("revalidate the captured sentinel target")
        {
            TargetRevalidation::Valid(target) => target,
            other => panic!("sentinel target was not exactly revalidated: {other:?}"),
        };
        let outcome = block_on(adapter.insert(
            validated,
            LIVE_INSERT_TEXT.to_owned(),
            DeliveryId::new(),
            LifecycleFence::new(),
        ))
        .expect("perform the gated insert-only operation");
        assert_eq!(
            outcome,
            InsertOutcome::Inserted,
            "the adapter did not prove the exact sentinel insertion"
        );
    }
}
