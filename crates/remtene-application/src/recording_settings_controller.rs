//! 录音设置写入用例。
//!
//! 模式、时长和全局快捷键共享 Orchestrator 的配置门闩。活动 Session 冻结设置后，
//! 这里不会在中途改变其语义；快捷键的系统注册与设置持久化也在一个可回滚事务中完成。

use std::{sync::Arc, time::Duration};

use futures::lock::Mutex as AsyncMutex;
use remtene_domain::{RecordingMode, RecordingShortcut, SettingsSnapshot};
use thiserror::Error;

use crate::ports::{PortError, RecordingShortcutPort, SettingsStore};
use crate::{OrchestratorError, TranscriptionOrchestrator};

pub const RECORDING_DURATION_OPTIONS_SECONDS: [u64; 4] = [180, 300, 600, 1_200];

type ActiveWorkProbe = dyn Fn() -> Result<bool, OrchestratorError> + Send + Sync + 'static;

#[derive(Debug, Error)]
pub enum RecordingSettingsError {
    #[error("an input Session is active")]
    Busy,
    #[error("orchestrator state is unavailable")]
    RuntimeUnavailable,
    #[error("recording duration is not an approved option")]
    InvalidDuration,
    #[error("recording shortcut is invalid")]
    InvalidShortcut,
    #[error("shortcut rollback failed after settings persistence failed")]
    ShortcutRollbackFailed {
        store: PortError,
        rollback: PortError,
    },
    #[error(transparent)]
    Port(#[from] PortError),
}

pub struct RecordingSettingsController {
    settings: Arc<dyn SettingsStore>,
    shortcuts: Arc<dyn RecordingShortcutPort>,
    configuration_gate: Arc<AsyncMutex<()>>,
    active_work: Arc<ActiveWorkProbe>,
    operations: AsyncMutex<()>,
}

impl RecordingSettingsController {
    #[must_use]
    pub fn new(
        orchestrator: Arc<TranscriptionOrchestrator>,
        settings: Arc<dyn SettingsStore>,
        shortcuts: Arc<dyn RecordingShortcutPort>,
    ) -> Self {
        let configuration_gate = orchestrator.configuration_gate();
        let active_orchestrator = Arc::clone(&orchestrator);
        Self {
            settings,
            shortcuts,
            configuration_gate,
            active_work: Arc::new(move || active_orchestrator.has_active_work()),
            operations: AsyncMutex::new(()),
        }
    }

    pub async fn set_recording_preferences(
        &self,
        expected_version: u64,
        recording_mode: RecordingMode,
        max_recording_duration_seconds: u64,
    ) -> Result<SettingsSnapshot, RecordingSettingsError> {
        if !RECORDING_DURATION_OPTIONS_SECONDS.contains(&max_recording_duration_seconds) {
            return Err(RecordingSettingsError::InvalidDuration);
        }

        let _operation = self.operations.lock().await;
        let _configuration = self.configuration_gate.lock().await;
        self.ensure_idle()?;

        let current = self.settings.load().await?;
        self.ensure_version(&current, expected_version)?;
        let duration = Duration::from_secs(max_recording_duration_seconds);
        if current.recording_mode() == recording_mode
            && current.max_recording_duration() == duration
        {
            return Ok(current);
        }

        let mut input = current.into_input();
        input.version = expected_version;
        input.recording_mode = recording_mode;
        input.max_recording_duration = duration;
        let candidate =
            SettingsSnapshot::new(input).map_err(|_| RecordingSettingsError::InvalidDuration)?;
        self.settings
            .replace(expected_version, candidate)
            .await
            .map_err(Into::into)
    }

    pub async fn set_recording_shortcut(
        &self,
        expected_version: u64,
        recording_shortcut: Option<RecordingShortcut>,
    ) -> Result<SettingsSnapshot, RecordingSettingsError> {
        let _operation = self.operations.lock().await;
        let _configuration = self.configuration_gate.lock().await;
        self.ensure_idle()?;

        let current = self.settings.load().await?;
        self.ensure_version(&current, expected_version)?;
        let previous = current.recording_shortcut().cloned();
        if previous == recording_shortcut {
            self.shortcuts
                .replace_binding(previous, recording_shortcut)
                .await?;
            return Ok(current);
        }

        let mut input = current.into_input();
        input.version = expected_version;
        input.recording_shortcut = recording_shortcut.clone();
        let candidate =
            SettingsSnapshot::new(input).map_err(|_| RecordingSettingsError::InvalidShortcut)?;

        self.shortcuts
            .replace_binding(previous.clone(), recording_shortcut.clone())
            .await?;

        match self.settings.replace(expected_version, candidate).await {
            Ok(stored) => Ok(stored),
            Err(store) => {
                if let Err(rollback) = self
                    .shortcuts
                    .replace_binding(recording_shortcut, previous)
                    .await
                {
                    return Err(RecordingSettingsError::ShortcutRollbackFailed { store, rollback });
                }
                Err(store.into())
            }
        }
    }

    fn ensure_idle(&self) -> Result<(), RecordingSettingsError> {
        match (self.active_work)() {
            Ok(true) => Err(RecordingSettingsError::Busy),
            Ok(false) => Ok(()),
            Err(_) => Err(RecordingSettingsError::RuntimeUnavailable),
        }
    }

    fn ensure_version(
        &self,
        current: &SettingsSnapshot,
        expected_version: u64,
    ) -> Result<(), RecordingSettingsError> {
        if current.version() == expected_version {
            Ok(())
        } else {
            Err(PortError {
                code: "settings.version_conflict".to_owned(),
                safe_message_key: "errors.settings.version_conflict".to_owned(),
                retryable: false,
            }
            .into())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use futures::executor::block_on;
    use remtene_domain::{AsrPreference, HistoryPolicy, ProcessingMode, SettingsSnapshotInput};

    use super::*;
    use crate::ports::PortFuture;

    struct TestSettingsStore {
        snapshot: Mutex<SettingsSnapshot>,
        fail_replace: AtomicBool,
    }

    impl TestSettingsStore {
        fn new(snapshot: SettingsSnapshot) -> Self {
            Self {
                snapshot: Mutex::new(snapshot),
                fail_replace: AtomicBool::new(false),
            }
        }
    }

    impl SettingsStore for TestSettingsStore {
        fn load(&self) -> PortFuture<'_, Result<SettingsSnapshot, PortError>> {
            let snapshot = self.snapshot.lock().unwrap().clone();
            Box::pin(async move { Ok(snapshot) })
        }

        fn replace(
            &self,
            expected_version: u64,
            settings: SettingsSnapshot,
        ) -> PortFuture<'_, Result<SettingsSnapshot, PortError>> {
            Box::pin(async move {
                if self.fail_replace.swap(false, Ordering::SeqCst) {
                    return Err(port_error("settings.write_failed"));
                }
                let mut stored = self.snapshot.lock().unwrap();
                if stored.version() != expected_version {
                    return Err(port_error("settings.version_conflict"));
                }
                let mut input = settings.into_input();
                input.version = expected_version + 1;
                let next = SettingsSnapshot::new(input).unwrap();
                *stored = next.clone();
                Ok(next)
            })
        }
    }

    #[derive(Default)]
    struct TestShortcutPort {
        calls: Mutex<Vec<(Option<String>, Option<String>)>>,
        fail_next: AtomicBool,
    }

    impl RecordingShortcutPort for TestShortcutPort {
        fn replace_binding(
            &self,
            current: Option<RecordingShortcut>,
            next: Option<RecordingShortcut>,
        ) -> PortFuture<'_, Result<(), PortError>> {
            let call = (
                current.as_ref().map(|value| value.as_str().to_owned()),
                next.as_ref().map(|value| value.as_str().to_owned()),
            );
            Box::pin(async move {
                self.calls.lock().unwrap().push(call);
                if self.fail_next.swap(false, Ordering::SeqCst) {
                    Err(port_error("shortcut.register_failed"))
                } else {
                    Ok(())
                }
            })
        }
    }

    fn snapshot(shortcut: Option<&str>) -> SettingsSnapshot {
        SettingsSnapshot::new(SettingsSnapshotInput {
            version: 4,
            recording_mode: RecordingMode::Toggle,
            max_recording_duration: Duration::from_secs(600),
            recording_shortcut: shortcut.map(|value| RecordingShortcut::new(value).unwrap()),
            processing_mode: ProcessingMode::Faithful,
            asr_preference: AsrPreference::Qwen,
            llm: None,
            read_selected_text: false,
            clipboard_bridge_allowed: false,
            auto_copy_result: false,
            local_diagnostics_enabled: true,
            history_policy: HistoryPolicy {
                enabled: true,
                limit: 10,
                retention_days: None,
            },
        })
        .unwrap()
    }

    fn controller(
        store: Arc<TestSettingsStore>,
        shortcuts: Arc<TestShortcutPort>,
    ) -> RecordingSettingsController {
        RecordingSettingsController {
            settings: store,
            shortcuts,
            configuration_gate: Arc::new(AsyncMutex::new(())),
            active_work: Arc::new(|| Ok(false)),
            operations: AsyncMutex::new(()),
        }
    }

    fn port_error(code: &str) -> PortError {
        PortError {
            code: code.to_owned(),
            safe_message_key: format!("errors.{code}"),
            retryable: false,
        }
    }

    #[test]
    fn recording_preferences_accept_only_the_four_product_options() {
        let store = Arc::new(TestSettingsStore::new(snapshot(None)));
        let shortcuts = Arc::new(TestShortcutPort::default());
        let controller = controller(Arc::clone(&store), shortcuts);

        let saved =
            block_on(controller.set_recording_preferences(4, RecordingMode::PushToTalk, 1_200))
                .unwrap();
        assert_eq!(saved.version(), 5);
        assert_eq!(saved.recording_mode(), RecordingMode::PushToTalk);
        assert_eq!(saved.max_recording_duration(), Duration::from_secs(1_200));

        assert!(matches!(
            block_on(controller.set_recording_preferences(5, RecordingMode::Toggle, 61)),
            Err(RecordingSettingsError::InvalidDuration)
        ));
    }

    #[test]
    fn shortcut_registration_failure_does_not_change_persisted_settings() {
        let store = Arc::new(TestSettingsStore::new(snapshot(None)));
        let shortcuts = Arc::new(TestShortcutPort::default());
        shortcuts.fail_next.store(true, Ordering::SeqCst);
        let controller = controller(Arc::clone(&store), Arc::clone(&shortcuts));

        let error = block_on(controller.set_recording_shortcut(
            4,
            Some(RecordingShortcut::new("Command+Shift+KeyR").unwrap()),
        ))
        .unwrap_err();

        assert!(matches!(error, RecordingSettingsError::Port(_)));
        assert!(
            block_on(store.load())
                .unwrap()
                .recording_shortcut()
                .is_none()
        );
        assert_eq!(shortcuts.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn unchanged_persisted_shortcut_can_be_re_registered_without_version_bump() {
        let store = Arc::new(TestSettingsStore::new(snapshot(Some("Command+Shift+KeyR"))));
        let shortcuts = Arc::new(TestShortcutPort::default());
        let controller = controller(Arc::clone(&store), Arc::clone(&shortcuts));

        let saved = block_on(controller.set_recording_shortcut(
            4,
            Some(RecordingShortcut::new("Command+Shift+KeyR").unwrap()),
        ))
        .unwrap();

        assert_eq!(saved.version(), 4);
        assert_eq!(shortcuts.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn persistence_failure_rolls_the_system_binding_back() {
        let store = Arc::new(TestSettingsStore::new(snapshot(Some("Command+Shift+KeyR"))));
        store.fail_replace.store(true, Ordering::SeqCst);
        let shortcuts = Arc::new(TestShortcutPort::default());
        let controller = controller(Arc::clone(&store), Arc::clone(&shortcuts));

        let error = block_on(controller.set_recording_shortcut(
            4,
            Some(RecordingShortcut::new("Command+Shift+KeyT").unwrap()),
        ))
        .unwrap_err();

        assert!(matches!(error, RecordingSettingsError::Port(_)));
        let calls = shortcuts.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1.as_deref(), Some("Command+Shift+KeyT"));
        assert_eq!(calls[1].1.as_deref(), Some("Command+Shift+KeyR"));
        assert_eq!(
            block_on(store.load())
                .unwrap()
                .recording_shortcut()
                .map(RecordingShortcut::as_str),
            Some("Command+Shift+KeyR")
        );
    }
}
