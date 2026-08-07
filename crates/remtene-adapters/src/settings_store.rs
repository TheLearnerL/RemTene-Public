//! Settings store implementation.

use std::sync::RwLock;
use std::time::Duration;

use remtene_application::ports::{PortError, PortFuture, SettingsStore};
use remtene_domain::{
    AsrPreference, HistoryPolicy, ProcessingMode, RecordingMode, SettingsSnapshot,
    SettingsSnapshotInput,
};

/// In-memory settings store.
///
/// This is a stub implementation that holds settings in memory.
/// Production implementation would persist to disk/database.
pub struct InMemorySettingsStore {
    settings: RwLock<SettingsSnapshot>,
}

impl InMemorySettingsStore {
    #[must_use]
    pub fn new(initial: SettingsSnapshot) -> Self {
        Self {
            settings: RwLock::new(initial),
        }
    }

    /// Create with default settings.
    #[must_use]
    pub fn with_defaults() -> Self {
        let input = SettingsSnapshotInput {
            version: 0,
            recording_mode: RecordingMode::PushToTalk,
            max_recording_duration: Duration::from_secs(60),
            recording_shortcut: None,
            processing_mode: ProcessingMode::Faithful,
            asr_preference: AsrPreference::Qwen,
            llm: None,
            read_selected_text: false,
            clipboard_bridge_allowed: false,
            auto_copy_result: false,
            local_diagnostics_enabled: true,
            history_policy: HistoryPolicy {
                enabled: false,
                limit: 10,
                retention_days: None,
            },
        };
        Self::new(SettingsSnapshot::new(input).expect("default settings should be valid"))
    }
}

impl SettingsStore for InMemorySettingsStore {
    fn load(&self) -> PortFuture<'_, Result<SettingsSnapshot, PortError>> {
        Box::pin(async move {
            let settings = self
                .settings
                .read()
                .map_err(|_| PortError {
                    code: "settings.lock_poisoned".to_owned(),
                    safe_message_key: "Settings lock poisoned".to_owned(),
                    retryable: false,
                })?
                .clone();
            Ok(settings)
        })
    }

    fn replace(
        &self,
        expected_version: u64,
        settings: SettingsSnapshot,
    ) -> PortFuture<'_, Result<SettingsSnapshot, PortError>> {
        Box::pin(async move {
            let mut store = self.settings.write().map_err(|_| PortError {
                code: "settings.lock_poisoned".to_owned(),
                safe_message_key: "Settings lock poisoned".to_owned(),
                retryable: false,
            })?;
            if store.version() != expected_version {
                return Err(version_conflict_error());
            }

            let mut input = settings.into_input();
            input.version = expected_version
                .checked_add(1)
                .ok_or_else(version_overflow_error)?;
            let next = SettingsSnapshot::new(input).map_err(|_| invalid_settings_error())?;
            *store = next.clone();
            Ok(next)
        })
    }
}

fn version_conflict_error() -> PortError {
    PortError {
        code: "settings.version_conflict".to_owned(),
        safe_message_key: "errors.settings.version_conflict".to_owned(),
        retryable: false,
    }
}

fn version_overflow_error() -> PortError {
    PortError {
        code: "settings.version_overflow".to_owned(),
        safe_message_key: "errors.settings.version_overflow".to_owned(),
        retryable: false,
    }
}

fn invalid_settings_error() -> PortError {
    PortError {
        code: "settings.invalid".to_owned(),
        safe_message_key: "errors.settings.invalid".to_owned(),
        retryable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_loads_initial_settings() {
        let store = InMemorySettingsStore::with_defaults();
        let result = futures::executor::block_on(store.load());
        assert!(result.is_ok());
    }

    #[test]
    fn store_saves_and_loads_settings() {
        let store = InMemorySettingsStore::with_defaults();
        let loaded = futures::executor::block_on(store.load()).unwrap();

        let input = SettingsSnapshotInput {
            version: loaded.version(),
            recording_mode: loaded.recording_mode(),
            max_recording_duration: loaded.max_recording_duration(),
            recording_shortcut: loaded.recording_shortcut().cloned(),
            processing_mode: loaded.processing_mode(),
            asr_preference: loaded.asr_preference(),
            llm: loaded.llm().cloned(),
            read_selected_text: loaded.read_selected_text(),
            clipboard_bridge_allowed: true, // Change this
            auto_copy_result: loaded.auto_copy_result(),
            local_diagnostics_enabled: loaded.local_diagnostics_enabled(),
            history_policy: loaded.history_policy(),
        };
        let settings = SettingsSnapshot::new(input).unwrap();

        let saved = futures::executor::block_on(store.replace(0, settings.clone())).unwrap();
        let reloaded = futures::executor::block_on(store.load()).unwrap();

        assert_eq!(saved.version(), 1);
        assert_eq!(saved, reloaded);
        assert!(reloaded.clipboard_bridge_allowed());
    }

    #[test]
    fn stale_replace_is_rejected_without_changing_settings() {
        let store = InMemorySettingsStore::with_defaults();
        let initial = futures::executor::block_on(store.load()).unwrap();
        let mut input = initial.clone().into_input();
        input.clipboard_bridge_allowed = true;
        let candidate = SettingsSnapshot::new(input).unwrap();

        let error = futures::executor::block_on(store.replace(9, candidate)).unwrap_err();
        let reloaded = futures::executor::block_on(store.load()).unwrap();

        assert_eq!(error.code, "settings.version_conflict");
        assert_eq!(reloaded, initial);
    }
}
