//! Objective-C and CoreGraphics calls behind the macOS clipboard backend.
//!
//! `NSPasteboard` is reached through `objc2`'s message sending rather than the
//! generated `objc2-app-kit` bindings so this module does not force a new
//! AppKit feature onto every crate in the workspace. Each helper returns a
//! structurally content-free [`ClipboardBackendError`]; pasteboard text must
//! never travel inside an error value.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{class, msg_send};
use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardWriting};
use objc2_foundation::{NSArray, NSData, NSString};

use crate::clipboard::ClipboardBackendError;

/// Virtual keycode for the `V` key on the ANSI layout (`kVK_ANSI_V`).
///
/// Keycodes address physical keys, not characters, so this value is correct on
/// non-QWERTY layouts where `V` prints something else.
const KEYCODE_V: u16 = 0x09;

/// `kCGEventFlagMaskCommand` — the ⌘ modifier bit for a synthesized event.
const EVENT_FLAG_COMMAND: u64 = 1 << 20;

/// `kCGHIDEventTap` — post at the lowest tap so the event reaches every app.
const HID_EVENT_TAP: u32 = 0;

/// `kCGEventSourceStatePrivate`.
///
/// Not `kCGEventSourceStateCombinedSessionState` (which is `0`): that state
/// inherits the modifier keys the user is physically holding, so a ⌘V posted
/// while they still hold the recording hotkey would arrive as ⌃⌥⌘V and do
/// something else entirely in the target app.
const EVENT_SOURCE_STATE_PRIVATE: i32 = -1;

/// Avoid turning an unexpectedly huge or hostile lazy pasteboard provider into
/// unbounded process memory. Exceeding this limit fails before the clipboard is
/// mutated, so the user's original data remains untouched.
const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
const MAX_SNAPSHOT_REPRESENTATIONS: usize = 16_384;

type CGEventSourceRef = *mut std::ffi::c_void;
type CGEventRef = *mut std::ffi::c_void;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    #[link_name = "CGEventSourceCreate"]
    fn cg_event_source_create(state_id: i32) -> CGEventSourceRef;

    #[link_name = "CGEventCreateKeyboardEvent"]
    fn cg_event_create_keyboard_event(
        source: CGEventSourceRef,
        virtual_key: u16,
        key_down: bool,
    ) -> CGEventRef;

    #[link_name = "CGEventSetFlags"]
    fn cg_event_set_flags(event: CGEventRef, flags: u64);

    #[link_name = "CGEventPost"]
    fn cg_event_post(tap: u32, event: CGEventRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    #[link_name = "CFRelease"]
    fn cf_release(value: *const std::ffi::c_void);
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    #[link_name = "IsSecureEventInputEnabled"]
    fn is_secure_event_input_enabled() -> u8;
}

/// True when a secure input session is swallowing synthesized key events.
pub(super) fn secure_event_input_active() -> bool {
    unsafe { is_secure_event_input_enabled() != 0 }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PasteboardRepresentation {
    kind: String,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PasteboardItemContents {
    representations: Vec<PasteboardRepresentation>,
}

/// Owned, Send-safe deep copy of all pasteboard item representations.
///
/// Objective-C pasteboard objects and lazy providers are deliberately not held
/// across the transaction. Every representation is materialized before the
/// first mutation so restoration does not depend on the source app remaining
/// alive.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct PasteboardContents {
    items: Vec<PasteboardItemContents>,
    representation_count: usize,
    total_bytes: usize,
}

impl PasteboardContents {
    pub(super) const fn item_count(&self) -> usize {
        self.items.len()
    }

    pub(super) const fn representation_count(&self) -> usize {
        self.representation_count
    }

    pub(super) const fn total_bytes(&self) -> usize {
        self.total_bytes
    }
}

/// Handle to one native pasteboard. Production uses the general pasteboard;
/// tests use an isolated uniquely named pasteboard so they never touch the
/// user's real clipboard.
pub(super) struct GeneralPasteboard {
    raw: Retained<NSPasteboard>,
}

impl GeneralPasteboard {
    /// Monotonic counter macOS bumps on every write by any process.
    pub(super) fn change_count(&self) -> isize {
        self.raw.changeCount()
    }

    /// Reads the plain-text flavor, if the pasteboard currently offers one.
    pub(super) fn read_string(&self) -> Option<String> {
        let kind = NSString::from_str("public.utf8-plain-text");
        let value: Option<Retained<NSString>> =
            unsafe { msg_send![&*self.raw, stringForType: &*kind] };
        value.map(|text| text.to_string())
    }

    /// Deep-copies every item/type/data representation before any mutation.
    pub(super) fn snapshot_contents(&self) -> Result<PasteboardContents, ClipboardBackendError> {
        let Some(items) = self.raw.pasteboardItems() else {
            // `nil` normally means an empty pasteboard. If legacy top-level
            // types are nevertheless present, treating it as empty would lose
            // data on restore, so fail before mutation instead.
            let types: Option<Retained<NSArray<NSString>>> =
                unsafe { msg_send![&*self.raw, types] };
            return match types {
                Some(types) if !types.is_empty() => Err(ClipboardBackendError::transient()),
                _ => Ok(PasteboardContents::default()),
            };
        };
        snapshot_items(items.to_vec())
    }

    /// Reconstructs the original item boundaries and raw representations.
    pub(super) fn restore_contents(
        &self,
        snapshot: &PasteboardContents,
    ) -> Result<(), ClipboardBackendError> {
        let items = materialize_items(snapshot)?;
        let items = items
            .into_iter()
            .map(ProtocolObject::<dyn NSPasteboardWriting>::from_retained)
            .collect::<Vec<_>>();

        self.raw.clearContents();
        if items.is_empty() {
            return Ok(());
        }
        let items = NSArray::from_retained_slice(&items);
        if self.raw.writeObjects(&items) {
            Ok(())
        } else {
            Err(ClipboardBackendError::transient())
        }
    }

    /// Replaces the pasteboard contents with `text` as a single write.
    pub(super) fn write_string(&self, text: &str) -> Result<(), ClipboardBackendError> {
        self.raw.clearContents();
        let kind = NSString::from_str("public.utf8-plain-text");
        let value = NSString::from_str(text);
        let wrote: bool = unsafe { msg_send![&*self.raw, setString: &*value, forType: &*kind] };
        if wrote {
            Ok(())
        } else {
            // AppKit refused the write; a retry may succeed once whichever
            // process owns the pasteboard releases it.
            Err(ClipboardBackendError::transient())
        }
    }
}

fn snapshot_items(
    items: Vec<Retained<NSPasteboardItem>>,
) -> Result<PasteboardContents, ClipboardBackendError> {
    let mut snapshot = PasteboardContents::default();
    for item in items {
        let mut representations = Vec::new();
        for kind in item.types().to_vec() {
            snapshot.representation_count = snapshot
                .representation_count
                .checked_add(1)
                .filter(|count| *count <= MAX_SNAPSHOT_REPRESENTATIONS)
                .ok_or_else(ClipboardBackendError::permanent)?;
            let data = item
                .dataForType(&kind)
                .ok_or_else(ClipboardBackendError::transient)?;
            snapshot.total_bytes = snapshot
                .total_bytes
                .checked_add(data.len())
                .filter(|bytes| *bytes <= MAX_SNAPSHOT_BYTES)
                .ok_or_else(ClipboardBackendError::permanent)?;
            representations.push(PasteboardRepresentation {
                kind: kind.to_string(),
                bytes: data.to_vec(),
            });
        }
        if representations.is_empty() {
            return Err(ClipboardBackendError::permanent());
        }
        snapshot
            .items
            .push(PasteboardItemContents { representations });
    }
    Ok(snapshot)
}

fn materialize_items(
    snapshot: &PasteboardContents,
) -> Result<Vec<Retained<NSPasteboardItem>>, ClipboardBackendError> {
    let mut items = Vec::with_capacity(snapshot.items.len());
    for saved_item in &snapshot.items {
        let item = NSPasteboardItem::new();
        for representation in &saved_item.representations {
            let kind = NSString::from_str(&representation.kind);
            let data = NSData::with_bytes(&representation.bytes);
            if !item.setData_forType(&data, &kind) {
                return Err(ClipboardBackendError::transient());
            }
        }
        items.push(item);
    }
    Ok(items)
}

/// Runs `operation` against the general pasteboard.
pub(super) fn with_general_pasteboard<T, F>(operation: F) -> Result<T, ClipboardBackendError>
where
    F: FnOnce(&GeneralPasteboard) -> Result<T, ClipboardBackendError>,
{
    let raw: Option<Retained<NSPasteboard>> =
        unsafe { msg_send![class!(NSPasteboard), generalPasteboard] };
    let Some(raw) = raw else {
        return Err(ClipboardBackendError::transient());
    };
    operation(&GeneralPasteboard { raw })
}

/// Synthesizes a ⌘V key-down/key-up pair at the HID tap.
///
/// A key-down without its matching key-up leaves ⌘ logically held in the target
/// application, so both events are posted even if the caller only cares about
/// the first.
pub(super) fn post_command_v() -> Result<(), ClipboardBackendError> {
    let source = unsafe { cg_event_source_create(EVENT_SOURCE_STATE_PRIVATE) };
    if source.is_null() {
        return Err(ClipboardBackendError::transient());
    }

    let result = post_pair(source);

    unsafe { cf_release(source.cast()) };
    result
}

fn post_pair(source: CGEventSourceRef) -> Result<(), ClipboardBackendError> {
    for key_down in [true, false] {
        let event = unsafe { cg_event_create_keyboard_event(source, KEYCODE_V, key_down) };
        if event.is_null() {
            return Err(ClipboardBackendError::transient());
        }
        unsafe {
            cg_event_set_flags(event, EVENT_FLAG_COMMAND);
            cg_event_post(HID_EVENT_TAP, event);
            cf_release(event.cast());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controlled_contents() -> PasteboardContents {
        let items = vec![
            PasteboardItemContents {
                representations: vec![
                    PasteboardRepresentation {
                        kind: "public.utf8-plain-text".to_owned(),
                        bytes: b"clipboard sentinel".to_vec(),
                    },
                    PasteboardRepresentation {
                        kind: "public.html".to_owned(),
                        bytes: b"<b>clipboard sentinel</b>".to_vec(),
                    },
                ],
            },
            PasteboardItemContents {
                representations: vec![PasteboardRepresentation {
                    kind: "public.file-url".to_owned(),
                    bytes: b"file:///tmp/remtene-clipboard-sentinel".to_vec(),
                }],
            },
        ];
        let representation_count = items.iter().map(|item| item.representations.len()).sum();
        let total_bytes = items
            .iter()
            .flat_map(|item| &item.representations)
            .map(|representation| representation.bytes.len())
            .sum();
        PasteboardContents {
            items,
            representation_count,
            total_bytes,
        }
    }

    #[test]
    fn materialized_items_preserve_boundaries_types_and_bytes() {
        let expected = controlled_contents();
        let items = materialize_items(&expected).expect("materialize complete snapshot");

        assert_eq!(
            snapshot_items(items).expect("read materialized items"),
            expected
        );
    }

    #[test]
    #[ignore = "requires a live macOS pasteboard service; uses an isolated pasteboard"]
    fn isolated_pasteboard_round_trip_preserves_items_types_and_bytes() {
        let pasteboard = GeneralPasteboard {
            raw: NSPasteboard::pasteboardWithUniqueName(),
        };
        let expected = controlled_contents();

        pasteboard
            .restore_contents(&expected)
            .expect("seed isolated pasteboard");
        assert_eq!(
            pasteboard.snapshot_contents().expect("snapshot seed"),
            expected
        );

        pasteboard
            .write_string("temporary staged text")
            .expect("stage temporary text");
        pasteboard
            .restore_contents(&expected)
            .expect("restore complete snapshot");

        assert_eq!(
            pasteboard.snapshot_contents().expect("snapshot restore"),
            expected
        );
    }
}
