//! Fallback clipboard bridge for platforms without a pasteboard backend.
//!
//! The previous stub answered `insert_and_restore` with `InsertOutcome::Inserted`
//! without touching the clipboard, so the desktop reported "已经由剪贴板插入" for a
//! delivery that never happened. A bridge that cannot paste must say so: the
//! orchestrator can then fall back to the temporary text box, which keeps the
//! transcript reachable instead of dropping it silently.

use remtene_application::ports::{
    ClipboardBridge, InsertOutcome, LifecycleFence, PortError, PortFuture, SelectionSnapshot,
    TargetSnapshotRef, UserDirectedPasteOutcome, ValidatedTargetRef,
};
use remtene_domain::DeliveryId;

/// Clipboard bridge for platforms whose pasteboard backend is not implemented.
pub struct UnsupportedClipboardBridge;

impl UnsupportedClipboardBridge {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for UnsupportedClipboardBridge {
    fn default() -> Self {
        Self::new()
    }
}

fn unsupported_error(operation: &str) -> PortError {
    PortError {
        code: format!("clipboard.unsupported_platform.{operation}"),
        safe_message_key: "errors.clipboard.unsupported_platform".to_owned(),
        retryable: false,
    }
}

impl ClipboardBridge for UnsupportedClipboardBridge {
    /// Reports no selection. Reading nothing is honest here: the caller treats an
    /// empty selection as "insert at the caret", which stays correct.
    fn read_selected_text(
        &self,
        _target: &TargetSnapshotRef,
    ) -> PortFuture<'_, Result<SelectionSnapshot, PortError>> {
        Box::pin(async move {
            Ok(SelectionSnapshot {
                text: None,
                anchor_normalized_to_end: false,
                exceeded_limit: false,
            })
        })
    }

    /// Fails instead of claiming an insertion. Nothing was written to the
    /// clipboard and no key event was synthesised, so the document is untouched
    /// and the caller may safely fall back.
    fn insert_and_restore(
        &self,
        _target: ValidatedTargetRef,
        _text: String,
        _delivery_id: DeliveryId,
        _lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<InsertOutcome, PortError>> {
        Box::pin(async move { Err(unsupported_error("insert_and_restore")) })
    }

    fn insert_at_current_focus_and_restore(
        &self,
        _text: String,
        _delivery_id: DeliveryId,
        _lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<UserDirectedPasteOutcome, PortError>> {
        Box::pin(async move { Err(unsupported_error("insert_at_current_focus_and_restore")) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_returns_empty_selection() {
        let bridge = UnsupportedClipboardBridge::new();
        let target = TargetSnapshotRef::new("test-target");
        let result = futures::executor::block_on(bridge.read_selected_text(&target)).unwrap();
        assert_eq!(result.text, None);
    }

    /// The regression this file exists for: an unavailable clipboard must not
    /// report a delivery.
    #[test]
    fn bridge_refuses_to_claim_an_insertion() {
        let bridge = UnsupportedClipboardBridge::new();
        let error = futures::executor::block_on(bridge.insert_and_restore(
            ValidatedTargetRef::new("test-target"),
            "test".to_owned(),
            DeliveryId::new(),
            LifecycleFence::new(),
        ))
        .expect_err("a bridge without a pasteboard backend must not report Inserted");

        assert_eq!(
            error.code,
            "clipboard.unsupported_platform.insert_and_restore"
        );
        assert!(!error.retryable);
    }

    #[test]
    fn bridge_refuses_to_claim_a_user_directed_dispatch() {
        let bridge = UnsupportedClipboardBridge::new();
        let error = futures::executor::block_on(bridge.insert_at_current_focus_and_restore(
            "test".to_owned(),
            DeliveryId::new(),
            LifecycleFence::new(),
        ))
        .expect_err("an unsupported bridge must not claim a shortcut dispatch");

        assert_eq!(
            error.code,
            "clipboard.unsupported_platform.insert_at_current_focus_and_restore"
        );
    }
}
