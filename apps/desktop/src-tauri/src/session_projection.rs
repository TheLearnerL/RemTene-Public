//! Content-free Session labels shared by desktop Presentation entry points.

use remtene_domain::{FailureCategory, RejectReason};

pub(crate) const fn failure_error_code(category: FailureCategory) -> &'static str {
    match category {
        FailureCategory::Audio => "session.failed.audio",
        FailureCategory::Asr => "session.failed.asr",
        FailureCategory::Llm => "session.failed.llm",
        FailureCategory::Delivery => "session.failed.delivery",
        FailureCategory::Storage => "session.failed.storage",
        FailureCategory::Lifecycle => "session.failed.lifecycle",
    }
}

pub(crate) const fn reject_error_code(reason: RejectReason) -> &'static str {
    match reason {
        RejectReason::SecureInput => "session.rejected.secure_input",
        RejectReason::SelectionTooLong => "session.rejected.selection_too_long",
        RejectReason::PermissionUnavailable => "session.rejected.permission_unavailable",
        RejectReason::AsrUnavailable => "session.rejected.asr_unavailable",
        RejectReason::RecordingHudUnavailable => "session.rejected.recording_hud_unavailable",
    }
}
