//! Native macOS backend for the transactional clipboard bridge.
//!
//! The policy layer in [`crate::clipboard`] owns ordering, serialization, and
//! the restore guarantee. This module only performs the four native operations
//! it cannot express portably: reading and rewriting `NSPasteboard`, proving the
//! focused target still matches the captured one, and synthesizing ⌘V.
//!
//! Target-bound paste keeps two strict invariants:
//!
//! - The pasteboard `changeCount` taken at `snapshot` is the transaction's
//!   identity. `restore` refuses to write back when something else has claimed
//!   the pasteboard in the meantime, because overwriting another app's newer
//!   contents is worse than leaving the staged text behind.
//! - `paste` fails closed when Secure Event Input is active. A synthesized ⌘V is
//!   silently swallowed in that state, so reporting `Inserted` would repeat the
//!   exact lie this backend exists to remove.
//!
//! The separately authorized user-directed operation intentionally omits those
//! target guarantees: it posts once to the keyboard focus the user selected
//! and reports `Dispatched`, never `Inserted`.

use std::sync::Arc;

use remtene_application::ports::{TargetSnapshotRef, ValidatedTargetRef};

use super::{
    ClipboardBackendError, ClipboardPasteOutcome, ClipboardSelectionCopyOutcome,
    ClipboardTargetStatus, ClipboardTransactionBackend,
};
use crate::target_context::MacTargetContext;

mod ffi;

/// How long to wait for the target application to consume the synthesized ⌘V.
///
/// Successful applications return on the first few polls. The two-second
/// ceiling applies only to a slow or swallowed event and covers the delayed AX
/// metadata observed in Chromium/Electron without blocking the normal path.
const PASTE_VERIFY_ATTEMPTS: usize = 100;
const PASTE_VERIFY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

/// Gives an asynchronously consuming target time to read the staged text
/// before the transaction restores the previous pasteboard.
///
/// This is deliberately a settle delay, not insertion verification: targets
/// on the compatibility path expose no readable text element or caret.
const USER_DIRECTED_PASTE_SETTLE_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// Pasteboard contents captured before the transaction mutated anything.
///
/// Every item boundary, declared type, and materialized byte representation is
/// kept. If any representation cannot be captured safely, snapshot creation
/// fails before `stage_text` can clear the user's clipboard.
pub struct PasteboardSnapshot {
    change_count: isize,
    contents: ffi::PasteboardContents,
}

/// Native clipboard backend bound to the target registry that issued the tokens.
///
/// The shared [`MacTargetContext`] is not an optimization. An opaque target
/// token only means something to the registry that minted it, so a backend with
/// its own registry could not tell "the captured field still has focus" from
/// "some field has focus" — and would paste into whatever the user switched to.
pub struct MacClipboardBackend {
    targets: Arc<MacTargetContext>,
}

impl MacClipboardBackend {
    #[must_use]
    pub fn new(targets: Arc<MacTargetContext>) -> Self {
        Self { targets }
    }
}

impl ClipboardTransactionBackend for MacClipboardBackend {
    type Snapshot = PasteboardSnapshot;

    fn snapshot(&self) -> Result<Self::Snapshot, ClipboardBackendError> {
        ffi::with_general_pasteboard(|pasteboard| {
            let change_count = pasteboard.change_count();
            let contents = pasteboard.snapshot_contents()?;
            crate::trace::delivery(
                "clipboard.snapshot",
                "已保存",
                &format!(
                    "items={} representations={} bytes={}",
                    contents.item_count(),
                    contents.representation_count(),
                    contents.total_bytes()
                ),
            );
            Ok(PasteboardSnapshot {
                change_count,
                contents,
            })
        })
    }

    fn validate_selection_target(&self, target: &TargetSnapshotRef) -> ClipboardTargetStatus {
        self.targets.clipboard_selection_status(target)
    }

    fn copy_selection(
        &self,
        target: &TargetSnapshotRef,
    ) -> Result<ClipboardSelectionCopyOutcome, ClipboardBackendError> {
        // Reading a selection never needs the clipboard on macOS: the AX layer
        // returns `AXSelectedText` directly. Synthesizing ⌘C here would mutate
        // the pasteboard for no gain, so this path reports that it copied
        // nothing and leaves the read to `TargetContextPort::read_selected_text`.
        let _ = target;
        Ok(ClipboardSelectionCopyOutcome::NotCopied)
    }

    fn read_text(&self) -> Result<Option<String>, ClipboardBackendError> {
        ffi::with_general_pasteboard(|pasteboard| Ok(pasteboard.read_string()))
    }

    fn validate_insert_target(&self, target: &ValidatedTargetRef) -> ClipboardTargetStatus {
        self.targets.clipboard_insert_status(target)
    }

    fn stage_text(&self, text: &str) -> Result<(), ClipboardBackendError> {
        ffi::with_general_pasteboard(|pasteboard| pasteboard.write_string(text))
    }

    fn paste(
        &self,
        target: &ValidatedTargetRef,
        text: &str,
    ) -> Result<ClipboardPasteOutcome, ClipboardBackendError> {
        // Re-check immediately before the keystroke rather than trusting the
        // policy layer's earlier check: focus can move between the two, and a
        // ⌘V aimed at the wrong window is an unrecoverable edit to a document
        // this app was never authorized to touch.
        match self.targets.clipboard_insert_status(target) {
            ClipboardTargetStatus::Valid => {}
            ClipboardTargetStatus::Invalid => return Ok(ClipboardPasteOutcome::NotInserted),
            ClipboardTargetStatus::Indeterminate => {
                return Ok(ClipboardPasteOutcome::Indeterminate);
            }
        }

        if ffi::secure_event_input_active() {
            // Synthetic key events cannot reach a secure input session. Fail
            // closed so the orchestrator degrades to the temporary text box.
            return Ok(ClipboardPasteOutcome::NotInserted);
        }

        let outcome = self.targets.dispatch_and_verify_clipboard_insert(
            target,
            text,
            PASTE_VERIFY_ATTEMPTS,
            PASTE_VERIFY_INTERVAL,
            ffi::post_command_v,
        )?;
        match outcome {
            ClipboardPasteOutcome::Inserted => {
                crate::trace::delivery("clipboard.verify", "已验证", "原锚点精确范围与本次文本一致")
            }
            ClipboardPasteOutcome::NotInserted => {
                crate::trace::delivery("clipboard.verify", "确定未派发", "真实按键边界前目标已失效")
            }
            ClipboardPasteOutcome::Indeterminate => crate::trace::delivery(
                "clipboard.verify",
                "不确定",
                "轮询期内未取得足以证明插入的范围与状态组合",
            ),
        }
        Ok(outcome)
    }

    fn dispatch_user_directed_paste(&self) -> Result<(), ClipboardBackendError> {
        // The user explicitly owns the current keyboard focus on this
        // compatibility path. Do not reject Secure Event Input or attempt an
        // AX lookup: either would recreate the no_focused_control dead end
        // this path exists to bypass.
        ffi::post_command_v()?;
        crate::trace::delivery(
            "userpaste.dispatch",
            "已派发",
            "⌘V 已发送到用户当前选择的输入位置",
        );
        std::thread::sleep(USER_DIRECTED_PASTE_SETTLE_DELAY);
        Ok(())
    }

    fn restore(&self, snapshot: Self::Snapshot) -> Result<(), ClipboardBackendError> {
        ffi::with_general_pasteboard(|pasteboard| {
            let staged_count = pasteboard.change_count();
            let ours = snapshot
                .change_count
                .checked_add(1)
                .is_some_and(|expected| staged_count == expected);
            if !ours {
                // Another process wrote to the pasteboard after we staged. Its
                // content is newer than ours and restoring would destroy it.
                crate::trace::delivery(
                    "clipboard.restore",
                    "已保留较新内容",
                    "reason=pasteboard_changed_externally",
                );
                return Ok(());
            }
            pasteboard.restore_contents(&snapshot.contents)?;
            crate::trace::delivery(
                "clipboard.restore",
                "已恢复",
                &format!(
                    "items={} representations={} bytes={}",
                    snapshot.contents.item_count(),
                    snapshot.contents.representation_count(),
                    snapshot.contents.total_bytes()
                ),
            );
            Ok(())
        })
    }
}
