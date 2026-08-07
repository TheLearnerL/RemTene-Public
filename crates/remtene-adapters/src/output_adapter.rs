//! Output adapter implementation for delivering transcribed text.

use remtene_application::ports::{
    InsertOutcome, LifecycleFence, OutputAdapter, PortError, PortFuture, ValidatedTargetRef,
};
use remtene_domain::DeliveryId;

/// Console output adapter that prints text to stdout.
///
/// Production implementation would use IMK to insert text into the target application.
pub struct ConsoleOutputAdapter;

impl ConsoleOutputAdapter {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for ConsoleOutputAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputAdapter for ConsoleOutputAdapter {
    fn insert(
        &self,
        _target: ValidatedTargetRef,
        text: String,
        _delivery_id: DeliveryId,
        _lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<InsertOutcome, PortError>> {
        Box::pin(async move {
            // Print to stdout with a clear marker
            println!("📝 [TRANSCRIPTION OUTPUT] {}", text);

            Ok(InsertOutcome::Inserted)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_inserts_text() {
        let adapter = ConsoleOutputAdapter::new();
        let target = ValidatedTargetRef::new("test-target");
        let delivery_id = DeliveryId::new();
        let lifecycle = LifecycleFence::new();

        let result = futures::executor::block_on(adapter.insert(
            target,
            "test".to_owned(),
            delivery_id,
            lifecycle,
        ));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), InsertOutcome::Inserted);
    }

    #[test]
    fn adapter_counts_unicode_chars() {
        let adapter = ConsoleOutputAdapter::new();
        let target = ValidatedTargetRef::new("test-target");
        let delivery_id = DeliveryId::new();
        let lifecycle = LifecycleFence::new();

        let result = futures::executor::block_on(adapter.insert(
            target,
            "你好".to_owned(),
            delivery_id,
            lifecycle,
        ));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), InsertOutcome::Inserted);
    }
}
