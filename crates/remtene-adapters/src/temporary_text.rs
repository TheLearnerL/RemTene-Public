//! Temporary text output implementation for displaying partial transcripts.

use remtene_application::ports::{
    LifecycleFence, PortError, PortFuture, TemporaryTextOutput, TemporaryTextStatus,
};
use remtene_domain::{DeliveryId, SessionId};

/// Stub temporary text output that does nothing.
///
/// Production implementation would show/update/hide a floating overlay.
pub struct StubTemporaryTextOutput;

impl StubTemporaryTextOutput {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for StubTemporaryTextOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl TemporaryTextOutput for StubTemporaryTextOutput {
    fn show(
        &self,
        _session_id: SessionId,
        _delivery_id: DeliveryId,
        _final_text: String,
        _status: TemporaryTextStatus,
        _lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            // Stub: do nothing
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_accepts_show() {
        let output = StubTemporaryTextOutput::new();
        let session_id = SessionId::new();
        let delivery_id = DeliveryId::new();
        let status = TemporaryTextStatus::Indeterminate;
        let lifecycle = LifecycleFence::new();

        let result = futures::executor::block_on(output.show(
            session_id,
            delivery_id,
            "test".to_owned(),
            status,
            lifecycle,
        ));
        assert!(result.is_ok());
    }
}
