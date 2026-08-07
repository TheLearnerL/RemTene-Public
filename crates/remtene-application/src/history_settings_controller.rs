//! 历史保存策略写入用例。
//!
//! 输出页允许切换“是否保存后续最终文字”，并调整条数上限与
//! 保存天数。关闭保存不会删除已有历史；缩短保存期限前由后端根据真实时间戳
//! 再次确认影响条数。写入与 Session 启动共享配置门闩，避免活动任务中途
//! 改变策略；立即淘汰还进入应用级退出屏障。

use std::sync::Arc;

use futures::lock::Mutex as AsyncMutex;
use remtene_domain::SettingsSnapshot;
use thiserror::Error;

use crate::ports::{Clock, CommitGuard, HistoryStore, LifecycleFence, PortError, SettingsStore};
use crate::{OrchestratorError, TranscriptionOrchestrator};

type ActiveWorkProbe = dyn Fn() -> Result<bool, OrchestratorError> + Send + Sync + 'static;
type ApplicationOperationEntry = dyn Fn() -> Option<CommitGuard> + Send + Sync + 'static;

#[derive(Debug, Error)]
pub enum HistorySettingsError {
    #[error("an input Session is active")]
    Busy,
    #[error("orchestrator state is unavailable")]
    RuntimeUnavailable,
    #[error("the application is quitting")]
    Quitting,
    #[error("lowering the history limit requires data-loss confirmation")]
    ConfirmationRequired,
    #[error("shortening history retention requires data-loss confirmation")]
    RetentionConfirmationRequired,
    #[error("history policy is invalid")]
    InvalidPolicy,
    #[error(transparent)]
    Port(#[from] PortError),
}

pub struct HistorySettingsController {
    settings: Arc<dyn SettingsStore>,
    history: Arc<dyn HistoryStore>,
    clock: Arc<dyn Clock>,
    configuration_gate: Arc<AsyncMutex<()>>,
    active_work: Arc<ActiveWorkProbe>,
    application_operation: Arc<ApplicationOperationEntry>,
    operations: AsyncMutex<()>,
}

impl HistorySettingsController {
    #[must_use]
    pub fn new(
        orchestrator: Arc<TranscriptionOrchestrator>,
        settings: Arc<dyn SettingsStore>,
        history: Arc<dyn HistoryStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let configuration_gate = orchestrator.configuration_gate();
        let active_orchestrator = Arc::clone(&orchestrator);
        let operation_orchestrator = Arc::clone(&orchestrator);
        Self {
            settings,
            history,
            clock,
            configuration_gate,
            active_work: Arc::new(move || active_orchestrator.has_active_work()),
            application_operation: Arc::new(move || {
                operation_orchestrator.enter_external_operation()
            }),
            operations: AsyncMutex::new(()),
        }
    }

    pub async fn set_enabled(
        &self,
        expected_version: u64,
        enabled: bool,
    ) -> Result<SettingsSnapshot, HistorySettingsError> {
        let _operation = self.operations.lock().await;
        let Some(_application_operation) = (self.application_operation)() else {
            return Err(HistorySettingsError::Quitting);
        };
        let _configuration = self.configuration_gate.lock().await;
        self.ensure_idle()?;

        let current = self.settings.load().await?;
        if current.version() != expected_version {
            return Err(PortError {
                code: "settings.version_conflict".to_owned(),
                safe_message_key: "errors.settings.version_conflict".to_owned(),
                retryable: false,
            }
            .into());
        }
        if current.history_policy().enabled == enabled {
            return Ok(current);
        }

        let mut input = current.into_input();
        input.version = expected_version;
        input.history_policy.enabled = enabled;
        let candidate =
            SettingsSnapshot::new(input).map_err(|_| HistorySettingsError::InvalidPolicy)?;
        self.settings
            .replace(expected_version, candidate)
            .await
            .map_err(Into::into)
    }

    pub async fn set_limit(
        &self,
        expected_version: u64,
        limit: u16,
        acknowledge_data_loss: bool,
    ) -> Result<SettingsSnapshot, HistorySettingsError> {
        let _operation = self.operations.lock().await;
        let Some(_application_operation) = (self.application_operation)() else {
            return Err(HistorySettingsError::Quitting);
        };
        let _configuration = self.configuration_gate.lock().await;
        self.ensure_idle()?;

        let current = self.settings.load().await?;
        if current.version() != expected_version {
            return Err(PortError {
                code: "settings.version_conflict".to_owned(),
                safe_message_key: "errors.settings.version_conflict".to_owned(),
                retryable: false,
            }
            .into());
        }
        if current.history_policy().limit == limit {
            return Ok(current);
        }

        let policy_enabled = current.history_policy().enabled;
        let requires_trim = if policy_enabled {
            self.history.list().await?.len() > usize::from(limit)
        } else {
            false
        };
        if requires_trim && !acknowledge_data_loss {
            return Err(HistorySettingsError::ConfirmationRequired);
        }

        let mut input = current.into_input();
        input.version = expected_version;
        input.history_policy.limit = limit;
        let candidate =
            SettingsSnapshot::new(input).map_err(|_| HistorySettingsError::InvalidPolicy)?;
        let stored = self.settings.replace(expected_version, candidate).await?;

        if requires_trim {
            self.history
                .enforce_policy(&stored, LifecycleFence::new())
                .await?;
        }
        Ok(stored)
    }

    pub async fn set_retention_days(
        &self,
        expected_version: u64,
        retention_days: Option<u16>,
        acknowledge_data_loss: bool,
    ) -> Result<SettingsSnapshot, HistorySettingsError> {
        let _operation = self.operations.lock().await;
        let Some(_application_operation) = (self.application_operation)() else {
            return Err(HistorySettingsError::Quitting);
        };
        let _configuration = self.configuration_gate.lock().await;
        self.ensure_idle()?;

        let current = self.settings.load().await?;
        if current.version() != expected_version {
            return Err(PortError {
                code: "settings.version_conflict".to_owned(),
                safe_message_key: "errors.settings.version_conflict".to_owned(),
                retryable: false,
            }
            .into());
        }
        if current.history_policy().retention_days == retention_days {
            return Ok(current);
        }

        let mut input = current.clone().into_input();
        input.version = expected_version;
        input.history_policy.retention_days = retention_days;
        let candidate =
            SettingsSnapshot::new(input).map_err(|_| HistorySettingsError::InvalidPolicy)?;

        let requires_trim = if candidate.history_policy().enabled {
            if let Some(days) = retention_days {
                let now_ms = self.clock.now().get();
                let retention_ms = u64::from(days).saturating_mul(HISTORY_DAY_MS);
                let cutoff_ms = now_ms.saturating_sub(retention_ms);
                self.history
                    .list()
                    .await?
                    .iter()
                    .any(|record| record.created_at.get() < cutoff_ms)
            } else {
                false
            }
        } else {
            false
        };
        if requires_trim && !acknowledge_data_loss {
            return Err(HistorySettingsError::RetentionConfirmationRequired);
        }

        let stored = self.settings.replace(expected_version, candidate).await?;
        if stored.history_policy().enabled && stored.history_policy().retention_days.is_some() {
            self.history
                .enforce_policy(&stored, LifecycleFence::new())
                .await?;
        }
        Ok(stored)
    }

    fn ensure_idle(&self) -> Result<(), HistorySettingsError> {
        match (self.active_work)() {
            Ok(true) => Err(HistorySettingsError::Busy),
            Ok(false) => Ok(()),
            Err(_) => Err(HistorySettingsError::RuntimeUnavailable),
        }
    }
}

const HISTORY_DAY_MS: u64 = 24 * 60 * 60 * 1_000;

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use futures::executor::block_on;
    use remtene_domain::{
        AsrPreference, DeliveryId, HistoryPolicy, ProcessingMode, RecordingMode,
        SettingsSnapshotInput, TimestampMs,
    };

    use super::*;
    use crate::ports::{Clock, HistoryRecord, HistoryStore, LifecycleFence, PortFuture};

    struct TestClock(u64);

    impl Clock for TestClock {
        fn now(&self) -> TimestampMs {
            TimestampMs::new(self.0)
        }
    }

    struct TestSettingsStore {
        snapshot: Mutex<SettingsSnapshot>,
    }

    impl SettingsStore for TestSettingsStore {
        fn load(&self) -> PortFuture<'_, Result<SettingsSnapshot, PortError>> {
            let snapshot = self.snapshot.lock().expect("settings lock").clone();
            Box::pin(async move { Ok(snapshot) })
        }

        fn replace(
            &self,
            expected_version: u64,
            settings: SettingsSnapshot,
        ) -> PortFuture<'_, Result<SettingsSnapshot, PortError>> {
            Box::pin(async move {
                let mut stored = self.snapshot.lock().expect("settings lock");
                if stored.version() != expected_version {
                    return Err(port_error("settings.version_conflict"));
                }
                let mut input = settings.into_input();
                input.version = expected_version + 1;
                let next = SettingsSnapshot::new(input).expect("valid settings");
                *stored = next.clone();
                Ok(next)
            })
        }
    }

    struct TestHistoryStore {
        record_count: usize,
        enforce_calls: AtomicUsize,
    }

    impl HistoryStore for TestHistoryStore {
        fn save_with_policy(
            &self,
            _record: HistoryRecord,
            _settings: &SettingsSnapshot,
            _lifecycle: LifecycleFence,
        ) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async { Ok(()) })
        }

        fn list(&self) -> PortFuture<'_, Result<Vec<HistoryRecord>, PortError>> {
            Box::pin(async move {
                Ok((0..self.record_count)
                    .map(|index| HistoryRecord {
                        delivery_id: DeliveryId::new(),
                        final_text: format!("record-{index}"),
                        created_at: TimestampMs::new(index as u64),
                    })
                    .collect())
            })
        }

        fn clear_all(&self) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async { Ok(()) })
        }

        fn enforce_policy(
            &self,
            _settings: &SettingsSnapshot,
            _lifecycle: LifecycleFence,
        ) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async move {
                self.enforce_calls.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
        }
    }

    fn settings() -> SettingsSnapshot {
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
                limit: 25,
                retention_days: Some(30),
            },
        })
        .expect("valid settings")
    }

    fn controller_with_history(
        active: bool,
        record_count: usize,
        operation_open: bool,
    ) -> (HistorySettingsController, Arc<TestHistoryStore>) {
        let history = Arc::new(TestHistoryStore {
            record_count,
            enforce_calls: AtomicUsize::new(0),
        });
        (
            HistorySettingsController {
                settings: Arc::new(TestSettingsStore {
                    snapshot: Mutex::new(settings()),
                }),
                history: Arc::clone(&history) as Arc<dyn HistoryStore>,
                clock: Arc::new(TestClock(100 * HISTORY_DAY_MS)),
                configuration_gate: Arc::new(AsyncMutex::new(())),
                active_work: Arc::new(move || Ok(active)),
                application_operation: Arc::new(move || {
                    operation_open
                        .then(LifecycleFence::new)
                        .and_then(|fence| fence.begin_commit())
                }),
                operations: AsyncMutex::new(()),
            },
            history,
        )
    }

    fn controller(active: bool) -> HistorySettingsController {
        controller_with_history(active, 0, true).0
    }

    fn quitting_controller() -> HistorySettingsController {
        HistorySettingsController {
            settings: Arc::new(TestSettingsStore {
                snapshot: Mutex::new(settings()),
            }),
            history: Arc::new(TestHistoryStore {
                record_count: 0,
                enforce_calls: AtomicUsize::new(0),
            }),
            clock: Arc::new(TestClock(100 * HISTORY_DAY_MS)),
            configuration_gate: Arc::new(AsyncMutex::new(())),
            active_work: Arc::new(|| Ok(false)),
            application_operation: Arc::new(|| None),
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
    fn toggling_enabled_preserves_limit_and_retention() {
        let controller = controller(false);
        let stored = block_on(controller.set_enabled(4, false)).expect("toggle history");

        assert_eq!(stored.version(), 5);
        assert_eq!(
            stored.history_policy(),
            HistoryPolicy {
                enabled: false,
                limit: 25,
                retention_days: Some(30),
            }
        );
    }

    #[test]
    fn stale_version_and_active_session_fail_closed() {
        let stale = block_on(controller(false).set_enabled(3, false)).expect_err("stale write");
        assert!(matches!(
            stale,
            HistorySettingsError::Port(PortError { code, .. })
                if code == "settings.version_conflict"
        ));

        let busy = block_on(controller(true).set_enabled(4, false)).expect_err("active session");
        assert!(matches!(busy, HistorySettingsError::Busy));

        let quitting = block_on(quitting_controller().set_enabled(4, false)).expect_err("quitting");
        assert!(matches!(quitting, HistorySettingsError::Quitting));
    }

    #[test]
    fn lowering_limit_requires_confirmation_and_enforces_immediately() {
        let (controller, history) = controller_with_history(false, 4, true);

        let confirmation = block_on(controller.set_limit(4, 2, false))
            .expect_err("trimming requires acknowledgement");
        assert!(matches!(
            confirmation,
            HistorySettingsError::ConfirmationRequired
        ));
        assert_eq!(history.enforce_calls.load(Ordering::Relaxed), 0);

        let stored = block_on(controller.set_limit(4, 2, true)).expect("confirmed limit");
        assert_eq!(stored.version(), 5);
        assert_eq!(stored.history_policy().limit, 2);
        assert_eq!(history.enforce_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn disabled_history_updates_limit_without_deleting_existing_records() {
        let (controller, history) = controller_with_history(false, 4, true);
        let disabled = block_on(controller.set_enabled(4, false)).expect("disable history");

        let stored = block_on(controller.set_limit(disabled.version(), 2, false))
            .expect("disabled policy does not trim");
        assert!(!stored.history_policy().enabled);
        assert_eq!(stored.history_policy().limit, 2);
        assert_eq!(history.enforce_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn retention_requires_confirmation_when_real_rows_would_expire() {
        let (controller, history) = controller_with_history(false, 2, true);

        let confirmation = block_on(controller.set_retention_days(4, Some(3), false))
            .expect_err("expired rows require acknowledgement");
        assert!(matches!(
            confirmation,
            HistorySettingsError::RetentionConfirmationRequired
        ));
        assert_eq!(history.enforce_calls.load(Ordering::Relaxed), 0);

        let stored =
            block_on(controller.set_retention_days(4, Some(3), true)).expect("confirmed retention");
        assert_eq!(stored.history_policy().retention_days, Some(3));
        assert_eq!(history.enforce_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn disabling_retention_preserves_rows_without_confirmation() {
        let (controller, history) = controller_with_history(false, 2, true);
        let stored = block_on(controller.set_retention_days(4, None, false))
            .expect("disabling retention is non-destructive");

        assert_eq!(stored.history_policy().retention_days, None);
        assert_eq!(history.enforce_calls.load(Ordering::Relaxed), 0);
    }
}
