//! History store implementation for persisting transcription records.

use remtene_application::ports::{
    HistoryRecord, HistoryStore, LifecycleFence, PortError, PortFuture,
};
use remtene_domain::SettingsSnapshot;

/// Stub history store that discards all records.
///
/// Production implementation would persist to SQLite or similar.
pub struct StubHistoryStore;

impl StubHistoryStore {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for StubHistoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl HistoryStore for StubHistoryStore {
    fn save_with_policy(
        &self,
        _record: HistoryRecord,
        _settings: &SettingsSnapshot,
        _lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            // Stub: silently discard
            Ok(())
        })
    }

    fn list(&self) -> PortFuture<'_, Result<Vec<HistoryRecord>, PortError>> {
        Box::pin(async move {
            // Stub: always return empty
            Ok(Vec::new())
        })
    }

    fn clear_all(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            // Stub: nothing to clear
            Ok(())
        })
    }

    fn enforce_policy(
        &self,
        _settings: &SettingsSnapshot,
        _lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            // Stub: no policy to enforce
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remtene_domain::{
        AsrPreference, DeliveryId, HistoryPolicy, ProcessingMode, RecordingMode,
        SettingsSnapshotInput, TimestampMs,
    };
    use std::time::Duration;

    #[test]
    fn store_accepts_records() {
        let store = StubHistoryStore::new();
        let record = HistoryRecord {
            delivery_id: DeliveryId::new(),
            final_text: "test".to_owned(),
            created_at: TimestampMs::new(1234567890),
        };
        let lifecycle = LifecycleFence::new();

        let settings = SettingsSnapshot::new(SettingsSnapshotInput {
            version: 0,
            recording_mode: RecordingMode::Toggle,
            max_recording_duration: Duration::from_secs(60),
            recording_shortcut: None,
            processing_mode: ProcessingMode::Raw,
            asr_preference: AsrPreference::Qwen,
            llm: None,
            read_selected_text: false,
            clipboard_bridge_allowed: false,
            auto_copy_result: false,
            local_diagnostics_enabled: true,
            history_policy: HistoryPolicy::default(),
        })
        .expect("valid history settings");
        let result =
            futures::executor::block_on(store.save_with_policy(record, &settings, lifecycle));
        assert!(result.is_ok());
    }

    #[test]
    fn store_returns_empty_for_list() {
        let store = StubHistoryStore::new();
        let result = futures::executor::block_on(store.list());
        assert_eq!(result.unwrap().len(), 0);
    }

    #[test]
    fn store_clear_all_succeeds() {
        let store = StubHistoryStore::new();
        let result = futures::executor::block_on(store.clear_all());
        assert!(result.is_ok());
    }
}
