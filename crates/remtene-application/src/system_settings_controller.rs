//! 系统页通用设置写入用例。
//!
//! 自动复制属于 Session 启动时冻结的交付设置；本地诊断开关同时
//! 写入持久化设置与当前进程的 Sink。两者都经过同一个版本 CAS 与
//! Orchestrator 配置门闩，不允许 Renderer 盲写覆盖新设置。

use std::sync::Arc;

use futures::lock::Mutex as AsyncMutex;
use remtene_domain::SettingsSnapshot;
use thiserror::Error;

use crate::{
    OrchestratorError, TranscriptionOrchestrator,
    ports::{DiagnosticsControl, PortError, SettingsStore},
};

type ActiveWorkProbe = dyn Fn() -> Result<bool, OrchestratorError> + Send + Sync + 'static;

#[derive(Debug, Error)]
pub enum SystemSettingsError {
    #[error("an input Session is active")]
    Busy,
    #[error("orchestrator state is unavailable")]
    RuntimeUnavailable,
    #[error(transparent)]
    Port(#[from] PortError),
}

pub struct SystemSettingsController {
    settings: Arc<dyn SettingsStore>,
    diagnostics: Arc<dyn DiagnosticsControl>,
    configuration_gate: Arc<AsyncMutex<()>>,
    active_work: Arc<ActiveWorkProbe>,
    operations: AsyncMutex<()>,
}

impl SystemSettingsController {
    #[must_use]
    pub fn new(
        orchestrator: Arc<TranscriptionOrchestrator>,
        settings: Arc<dyn SettingsStore>,
        diagnostics: Arc<dyn DiagnosticsControl>,
    ) -> Self {
        let configuration_gate = orchestrator.configuration_gate();
        let active_orchestrator = Arc::clone(&orchestrator);
        Self {
            settings,
            diagnostics,
            configuration_gate,
            active_work: Arc::new(move || active_orchestrator.has_active_work()),
            operations: AsyncMutex::new(()),
        }
    }

    pub async fn set_auto_copy_result(
        &self,
        expected_version: u64,
        enabled: bool,
    ) -> Result<SettingsSnapshot, SystemSettingsError> {
        let _operation = self.operations.lock().await;
        let _configuration = self.configuration_gate.lock().await;
        self.ensure_idle()?;

        let current = self.settings.load().await?;
        ensure_version(&current, expected_version)?;
        if current.auto_copy_result() == enabled {
            return Ok(current);
        }

        let mut input = current.into_input();
        input.version = expected_version;
        input.auto_copy_result = enabled;
        let candidate = SettingsSnapshot::new(input).map_err(|_| invalid_settings_error())?;
        self.settings
            .replace(expected_version, candidate)
            .await
            .map_err(Into::into)
    }

    pub async fn set_local_diagnostics_enabled(
        &self,
        expected_version: u64,
        enabled: bool,
    ) -> Result<SettingsSnapshot, SystemSettingsError> {
        let _operation = self.operations.lock().await;
        let _configuration = self.configuration_gate.lock().await;
        self.ensure_idle()?;

        let current = self.settings.load().await?;
        ensure_version(&current, expected_version)?;
        if current.local_diagnostics_enabled() == enabled {
            self.diagnostics.set_enabled(enabled);
            return Ok(current);
        }

        let mut input = current.into_input();
        input.version = expected_version;
        input.local_diagnostics_enabled = enabled;
        let candidate = SettingsSnapshot::new(input).map_err(|_| invalid_settings_error())?;
        let stored = self.settings.replace(expected_version, candidate).await?;
        self.diagnostics
            .set_enabled(stored.local_diagnostics_enabled());
        Ok(stored)
    }

    fn ensure_idle(&self) -> Result<(), SystemSettingsError> {
        match (self.active_work)() {
            Ok(true) => Err(SystemSettingsError::Busy),
            Ok(false) => Ok(()),
            Err(_) => Err(SystemSettingsError::RuntimeUnavailable),
        }
    }
}

fn ensure_version(
    current: &SettingsSnapshot,
    expected_version: u64,
) -> Result<(), SystemSettingsError> {
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

fn invalid_settings_error() -> PortError {
    PortError {
        code: "settings.invalid".to_owned(),
        safe_message_key: "errors.settings.invalid".to_owned(),
        retryable: false,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use futures::executor::block_on;
    use remtene_domain::{
        AsrPreference, HistoryPolicy, ProcessingMode, RecordingMode, SettingsSnapshotInput,
    };

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

    struct TestDiagnosticsControl {
        enabled: AtomicBool,
    }

    impl DiagnosticsControl for TestDiagnosticsControl {
        fn enabled(&self) -> bool {
            self.enabled.load(Ordering::Acquire)
        }

        fn set_enabled(&self, enabled: bool) {
            self.enabled.store(enabled, Ordering::Release);
        }
    }

    fn snapshot() -> SettingsSnapshot {
        SettingsSnapshot::new(SettingsSnapshotInput {
            version: 4,
            recording_mode: RecordingMode::Toggle,
            max_recording_duration: Duration::from_secs(600),
            recording_shortcut: None,
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
        diagnostics: Arc<TestDiagnosticsControl>,
    ) -> SystemSettingsController {
        SystemSettingsController {
            settings: store,
            diagnostics,
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
    fn auto_copy_is_versioned_and_disabled_until_explicitly_enabled() {
        let store = Arc::new(TestSettingsStore::new(snapshot()));
        let diagnostics = Arc::new(TestDiagnosticsControl {
            enabled: AtomicBool::new(true),
        });
        let controller = controller(Arc::clone(&store), diagnostics);

        assert!(!block_on(store.load()).unwrap().auto_copy_result());
        let saved = block_on(controller.set_auto_copy_result(4, true)).unwrap();
        assert_eq!(saved.version(), 5);
        assert!(saved.auto_copy_result());
        assert!(matches!(
            block_on(controller.set_auto_copy_result(4, false)),
            Err(SystemSettingsError::Port(_))
        ));
    }

    #[test]
    fn diagnostics_switch_changes_runtime_only_after_persistence_succeeds() {
        let store = Arc::new(TestSettingsStore::new(snapshot()));
        let diagnostics = Arc::new(TestDiagnosticsControl {
            enabled: AtomicBool::new(true),
        });
        let controller = controller(Arc::clone(&store), Arc::clone(&diagnostics));

        store.fail_replace.store(true, Ordering::SeqCst);
        assert!(matches!(
            block_on(controller.set_local_diagnostics_enabled(4, false)),
            Err(SystemSettingsError::Port(_))
        ));
        assert!(diagnostics.enabled());
        assert!(block_on(store.load()).unwrap().local_diagnostics_enabled());

        let saved = block_on(controller.set_local_diagnostics_enabled(4, false)).unwrap();
        assert_eq!(saved.version(), 5);
        assert!(!saved.local_diagnostics_enabled());
        assert!(!diagnostics.enabled());
    }
}
