//! Application use cases for output history.
//!
//! Presentation must not reach a concrete history adapter or its backing file.
//! This controller owns availability, ordering, row lookup, explicit copy, and
//! destructive-clear serialization.

use std::{collections::HashSet, sync::Arc};

use futures::lock::Mutex as AsyncMutex;
use remtene_domain::DeliveryId;
use thiserror::Error;

use crate::{
    OrchestratorError, TranscriptionOrchestrator,
    ports::{
        ClipboardTextWriter, CommitGuard, HistoryRecord, HistoryStore, LifecycleFence, PortError,
        SettingsStore,
    },
};

#[derive(Debug, Error, Eq, PartialEq)]
pub enum HistoryError {
    #[error("history is unavailable on this platform")]
    Unavailable,
    #[error("an input Session is active")]
    Busy,
    #[error("the application is quitting")]
    Quitting,
    #[error("the selected history record is no longer available")]
    RecordStale,
    #[error("history contains an invalid record")]
    InvalidRecord,
    #[error("orchestrator state is unavailable")]
    RuntimeUnavailable,
    #[error("history store failed: {0}")]
    Port(#[from] PortError),
}

enum HistorySource {
    Available {
        history: Arc<dyn HistoryStore>,
        clipboard: Arc<dyn ClipboardTextWriter>,
    },
    Unavailable,
}

type ActiveWorkProbe = dyn Fn() -> Result<bool, OrchestratorError> + Send + Sync + 'static;
type ApplicationOperationEntry = dyn Fn() -> Option<CommitGuard> + Send + Sync + 'static;
type AvailableHistorySource<'a> = (&'a Arc<dyn HistoryStore>, &'a Arc<dyn ClipboardTextWriter>);

/// The only Application entry point for output-history operations.
pub struct HistoryController {
    source: HistorySource,
    settings: Option<Arc<dyn SettingsStore>>,
    configuration_gate: Arc<AsyncMutex<()>>,
    active_work: Arc<ActiveWorkProbe>,
    application_operation: Arc<ApplicationOperationEntry>,
    operations: AsyncMutex<()>,
}

impl HistoryController {
    #[must_use]
    pub fn new(
        orchestrator: Arc<TranscriptionOrchestrator>,
        history: Arc<dyn HistoryStore>,
        clipboard: Arc<dyn ClipboardTextWriter>,
    ) -> Self {
        let configuration_gate = orchestrator.configuration_gate();
        let active_orchestrator = Arc::clone(&orchestrator);
        let operation_orchestrator = Arc::clone(&orchestrator);
        Self {
            source: HistorySource::Available { history, clipboard },
            settings: None,
            configuration_gate,
            active_work: Arc::new(move || active_orchestrator.has_active_work()),
            application_operation: Arc::new(move || {
                operation_orchestrator.enter_external_operation()
            }),
            operations: AsyncMutex::new(()),
        }
    }

    /// Production constructor that applies the current enabled retention
    /// policy before exposing rows. This also covers an app that remains open
    /// across the expiry boundary without creating a new transcription.
    #[must_use]
    pub fn new_with_settings(
        orchestrator: Arc<TranscriptionOrchestrator>,
        history: Arc<dyn HistoryStore>,
        clipboard: Arc<dyn ClipboardTextWriter>,
        settings: Arc<dyn SettingsStore>,
    ) -> Self {
        let mut controller = Self::new(orchestrator, history, clipboard);
        controller.settings = Some(settings);
        controller
    }

    /// Creates an explicitly unavailable controller.
    ///
    /// This is intentionally different from an available store returning an
    /// empty list: the latter means “no records”, while this means the current
    /// platform has no production history implementation.
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            source: HistorySource::Unavailable,
            settings: None,
            configuration_gate: Arc::new(AsyncMutex::new(())),
            active_work: Arc::new(|| Ok(false)),
            application_operation: Arc::new(|| LifecycleFence::new().begin_commit()),
            operations: AsyncMutex::new(()),
        }
    }

    /// Returns complete V1 history in deterministic newest-first order.
    ///
    /// Equal timestamps use the opaque record identity as a stable tiebreaker.
    /// The identity has no user-visible meaning and does not expose delivery
    /// state to Presentation.
    pub async fn list(&self) -> Result<Vec<HistoryRecord>, HistoryError> {
        let _operation = self.operations.lock().await;
        let (history, _) = self.available_source()?;
        // Production listing can apply retention and therefore is no longer a
        // purely read-only operation. Join the app-wide quiescence barrier so
        // expiry cleanup cannot start after shutdown has begun.
        let Some(_application_operation) = (self.application_operation)() else {
            return Err(HistoryError::Quitting);
        };
        if let Some(settings) = &self.settings {
            let settings = settings.load().await?;
            if settings.history_policy().enabled {
                history
                    .enforce_policy(&settings, LifecycleFence::new())
                    .await?;
            }
        }
        Self::validated_records(history).await
    }

    /// Copies one current record by opaque identity.
    ///
    /// The text is re-resolved from Rust-owned storage at click time. Renderer
    /// cache content never crosses this command boundary.
    pub async fn copy(&self, record_id: DeliveryId) -> Result<(), HistoryError> {
        let _operation = self.operations.lock().await;
        let (history, clipboard) = self.available_source()?;
        let Some(_application_operation) = (self.application_operation)() else {
            return Err(HistoryError::Quitting);
        };
        let _configuration = self.configuration_gate.lock().await;
        self.ensure_idle()?;
        let records = Self::validated_records(history).await?;
        let record = records
            .into_iter()
            .find(|record| record.delivery_id == record_id)
            .ok_or(HistoryError::RecordStale)?;
        clipboard.write_text(record.final_text).await?;
        Ok(())
    }

    /// Clears all current records only while no Session can be writing history.
    ///
    /// Holding the shared configuration gate orders this operation against
    /// Session start. A running Session returns Busy; a new Session cannot
    /// begin until the clear has committed.
    pub async fn clear_all(&self) -> Result<usize, HistoryError> {
        let _operation = self.operations.lock().await;
        let (history, _) = self.available_source()?;
        let Some(_application_operation) = (self.application_operation)() else {
            return Err(HistoryError::Quitting);
        };
        let _configuration = self.configuration_gate.lock().await;
        self.ensure_idle()?;
        let record_count = Self::validated_records(history).await?.len();
        history.clear_all().await?;
        Ok(record_count)
    }

    fn available_source(&self) -> Result<AvailableHistorySource<'_>, HistoryError> {
        let HistorySource::Available { history, clipboard } = &self.source else {
            return Err(HistoryError::Unavailable);
        };
        Ok((history, clipboard))
    }

    async fn validated_records(
        history: &Arc<dyn HistoryStore>,
    ) -> Result<Vec<HistoryRecord>, HistoryError> {
        let mut records = history.list().await?;
        let mut record_ids = HashSet::with_capacity(records.len());
        for record in &records {
            if record.final_text.trim().is_empty() || !record_ids.insert(record.delivery_id) {
                return Err(HistoryError::InvalidRecord);
            }
        }
        records.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.delivery_id.as_uuid().cmp(&left.delivery_id.as_uuid()))
        });
        Ok(records)
    }

    fn ensure_idle(&self) -> Result<(), HistoryError> {
        match (self.active_work)() {
            Ok(true) => Err(HistoryError::Busy),
            Ok(false) => Ok(()),
            Err(_) => Err(HistoryError::RuntimeUnavailable),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    use futures::lock::Mutex as AsyncMutex;
    use remtene_domain::{DeliveryId, SettingsSnapshot, TimestampMs};

    use super::*;
    use crate::ports::{LifecycleFence, PortFuture};

    struct ListHistoryStore {
        records: Mutex<Result<Vec<HistoryRecord>, PortError>>,
        list_calls: AtomicUsize,
        clear_calls: AtomicUsize,
    }

    impl ListHistoryStore {
        fn records(records: Vec<HistoryRecord>) -> Self {
            Self {
                records: Mutex::new(Ok(records)),
                list_calls: AtomicUsize::new(0),
                clear_calls: AtomicUsize::new(0),
            }
        }

        fn failing(error: PortError) -> Self {
            Self {
                records: Mutex::new(Err(error)),
                list_calls: AtomicUsize::new(0),
                clear_calls: AtomicUsize::new(0),
            }
        }
    }

    impl HistoryStore for ListHistoryStore {
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
                self.list_calls.fetch_add(1, Ordering::Relaxed);
                self.records.lock().expect("history records lock").clone()
            })
        }

        fn clear_all(&self) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async move {
                self.clear_calls.fetch_add(1, Ordering::Relaxed);
                *self.records.lock().expect("history records lock") = Ok(Vec::new());
                Ok(())
            })
        }

        fn enforce_policy(
            &self,
            _settings: &SettingsSnapshot,
            _lifecycle: LifecycleFence,
        ) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[derive(Default)]
    struct RecordingClipboard {
        writes: Mutex<Vec<String>>,
    }

    impl ClipboardTextWriter for RecordingClipboard {
        fn write_text(&self, text: String) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async move {
                self.writes
                    .lock()
                    .expect("clipboard writes lock")
                    .push(text);
                Ok(())
            })
        }
    }

    struct BlockingClipboard {
        entered: Mutex<Option<mpsc::Sender<()>>>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl BlockingClipboard {
        fn new() -> (Self, mpsc::Receiver<()>, mpsc::Sender<()>) {
            let (entered_sender, entered_receiver) = mpsc::channel();
            let (release_sender, release_receiver) = mpsc::channel();
            (
                Self {
                    entered: Mutex::new(Some(entered_sender)),
                    release: Mutex::new(release_receiver),
                },
                entered_receiver,
                release_sender,
            )
        }
    }

    impl ClipboardTextWriter for BlockingClipboard {
        fn write_text(&self, _text: String) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async move {
                if let Some(entered) = self.entered.lock().expect("entered lock").take() {
                    entered.send(()).expect("copy observer should remain alive");
                }
                self.release
                    .lock()
                    .expect("release lock")
                    .recv()
                    .expect("copy release should arrive");
                Ok(())
            })
        }
    }

    fn controller(
        history: Arc<ListHistoryStore>,
        clipboard: Arc<RecordingClipboard>,
        active: bool,
    ) -> HistoryController {
        controller_with_operation(history, clipboard, active, true)
    }

    fn controller_with_operation(
        history: Arc<ListHistoryStore>,
        clipboard: Arc<RecordingClipboard>,
        active: bool,
        operation_open: bool,
    ) -> HistoryController {
        HistoryController {
            source: HistorySource::Available { history, clipboard },
            settings: None,
            configuration_gate: Arc::new(AsyncMutex::new(())),
            active_work: Arc::new(move || Ok(active)),
            application_operation: Arc::new(move || {
                operation_open
                    .then(LifecycleFence::new)
                    .and_then(|fence| fence.begin_commit())
            }),
            operations: AsyncMutex::new(()),
        }
    }

    fn record(id: DeliveryId, text: &str, created_at_ms: u64) -> HistoryRecord {
        HistoryRecord {
            delivery_id: id,
            final_text: text.to_owned(),
            created_at: TimestampMs::new(created_at_ms),
        }
    }

    #[test]
    fn list_sorts_newest_first_with_stable_opaque_id_tiebreaker() {
        let older = DeliveryId::new();
        let tied_a = DeliveryId::new();
        let tied_b = DeliveryId::new();
        let history = Arc::new(ListHistoryStore::records(vec![
            record(tied_a, "同一时间 A", 2_000),
            record(older, "较早", 1_000),
            record(tied_b, "同一时间 B", 2_000),
        ]));
        let controller = controller(
            Arc::clone(&history),
            Arc::new(RecordingClipboard::default()),
            false,
        );

        let records = futures::executor::block_on(controller.list()).expect("history list");

        assert_eq!(records.len(), 3);
        assert_eq!(records[2].delivery_id, older);
        assert_eq!(records[0].created_at, TimestampMs::new(2_000));
        assert_eq!(records[1].created_at, TimestampMs::new(2_000));
        assert!(
            records[0].delivery_id.as_uuid() > records[1].delivery_id.as_uuid(),
            "equal timestamps must sort by opaque ID descending"
        );
        assert_eq!(history.list_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn unavailable_is_not_reported_as_an_empty_history() {
        let error = futures::executor::block_on(HistoryController::unavailable().list())
            .expect_err("unsupported history must remain explicit");

        assert_eq!(error, HistoryError::Unavailable);
    }

    #[test]
    fn store_failure_and_invalid_content_fail_closed() {
        let port_error = PortError {
            code: "history.read_failed".to_owned(),
            safe_message_key: "errors.history.io".to_owned(),
            retryable: true,
        };
        let failing = controller(
            Arc::new(ListHistoryStore::failing(port_error.clone())),
            Arc::new(RecordingClipboard::default()),
            false,
        );
        assert_eq!(
            futures::executor::block_on(failing.list()).expect_err("store failure"),
            HistoryError::Port(port_error)
        );

        for invalid_records in [vec![record(DeliveryId::new(), "   ", 1)], {
            let duplicate = DeliveryId::new();
            vec![
                record(duplicate, "第一条", 1),
                record(duplicate, "重复标识", 2),
            ]
        }] {
            let invalid = controller(
                Arc::new(ListHistoryStore::records(invalid_records)),
                Arc::new(RecordingClipboard::default()),
                false,
            );
            assert_eq!(
                futures::executor::block_on(invalid.list()).expect_err("invalid history record"),
                HistoryError::InvalidRecord
            );
        }
    }

    #[test]
    fn copy_resolves_exact_current_record_and_stale_id_never_writes() {
        let first_id = DeliveryId::new();
        let selected_id = DeliveryId::new();
        let history = Arc::new(ListHistoryStore::records(vec![
            record(first_id, "第一条", 2_000),
            record(selected_id, "需要复制的完整文字", 2_000),
        ]));
        let clipboard = Arc::new(RecordingClipboard::default());
        let controller = controller(Arc::clone(&history), Arc::clone(&clipboard), false);

        futures::executor::block_on(controller.copy(selected_id)).expect("copy current record");
        assert_eq!(
            clipboard
                .writes
                .lock()
                .expect("clipboard writes lock")
                .as_slice(),
            ["需要复制的完整文字"]
        );

        let stale_error = futures::executor::block_on(controller.copy(DeliveryId::new()))
            .expect_err("stale row must fail closed");
        assert_eq!(stale_error, HistoryError::RecordStale);
        assert_eq!(
            clipboard
                .writes
                .lock()
                .expect("clipboard writes lock")
                .len(),
            1
        );
    }

    #[test]
    fn copy_is_busy_during_active_work_and_never_reads_or_writes() {
        let selected_id = DeliveryId::new();
        let history = Arc::new(ListHistoryStore::records(vec![record(
            selected_id,
            "不得复制",
            1,
        )]));
        let clipboard = Arc::new(RecordingClipboard::default());
        let controller = controller(Arc::clone(&history), Arc::clone(&clipboard), true);

        assert_eq!(
            futures::executor::block_on(controller.copy(selected_id))
                .expect_err("active Session must serialize against history copy"),
            HistoryError::Busy
        );
        assert_eq!(history.list_calls.load(Ordering::Relaxed), 0);
        assert!(
            clipboard
                .writes
                .lock()
                .expect("clipboard writes lock")
                .is_empty()
        );
    }

    #[test]
    fn quitting_rejects_new_copy_and_clear_without_mutation() {
        let selected_id = DeliveryId::new();
        let history = Arc::new(ListHistoryStore::records(vec![record(
            selected_id,
            "保留",
            1,
        )]));
        let clipboard = Arc::new(RecordingClipboard::default());
        let controller =
            controller_with_operation(Arc::clone(&history), Arc::clone(&clipboard), false, false);

        assert_eq!(
            futures::executor::block_on(controller.list())
                .expect_err("retention-aware list cannot start after quiescing"),
            HistoryError::Quitting
        );
        assert_eq!(
            futures::executor::block_on(controller.copy(selected_id))
                .expect_err("copy cannot start after quiescing"),
            HistoryError::Quitting
        );
        assert_eq!(
            futures::executor::block_on(controller.clear_all())
                .expect_err("clear cannot start after quiescing"),
            HistoryError::Quitting
        );
        assert_eq!(history.list_calls.load(Ordering::Relaxed), 0);
        assert_eq!(history.clear_calls.load(Ordering::Relaxed), 0);
        assert!(
            clipboard
                .writes
                .lock()
                .expect("clipboard writes lock")
                .is_empty()
        );
    }

    #[test]
    fn copy_holds_session_gate_and_exit_guard_through_clipboard_commit() {
        let selected_id = DeliveryId::new();
        let history = Arc::new(ListHistoryStore::records(vec![record(
            selected_id,
            "完整正文",
            1,
        )]));
        let (clipboard, copy_entered, release_copy) = BlockingClipboard::new();
        let configuration_gate = Arc::new(AsyncMutex::new(()));
        let application_fence = LifecycleFence::new();
        let entry_fence = application_fence.clone();
        let controller = Arc::new(HistoryController {
            source: HistorySource::Available {
                history,
                clipboard: Arc::new(clipboard),
            },
            settings: None,
            configuration_gate: Arc::clone(&configuration_gate),
            active_work: Arc::new(|| Ok(false)),
            application_operation: Arc::new(move || entry_fence.begin_commit()),
            operations: AsyncMutex::new(()),
        });

        let copy_controller = Arc::clone(&controller);
        let copy_worker =
            thread::spawn(move || futures::executor::block_on(copy_controller.copy(selected_id)));
        copy_entered
            .recv_timeout(Duration::from_secs(2))
            .expect("copy should reach clipboard commit");

        let (exit_done_sender, exit_done_receiver) = mpsc::channel();
        let exit_wait = application_fence.invalidate_and_wait();
        let exit_worker = thread::spawn(move || {
            futures::executor::block_on(exit_wait);
            exit_done_sender
                .send(())
                .expect("exit observer should remain alive");
        });

        let (gate_done_sender, gate_done_receiver) = mpsc::channel();
        let waiting_gate = Arc::clone(&configuration_gate);
        let gate_worker = thread::spawn(move || {
            let _gate = futures::executor::block_on(waiting_gate.lock());
            gate_done_sender
                .send(())
                .expect("gate observer should remain alive");
        });

        assert!(
            exit_done_receiver
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "exit must wait while clipboard mutation is in progress"
        );
        assert!(
            gate_done_receiver
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "a new Session cannot cross the configuration gate during copy"
        );

        release_copy
            .send(())
            .expect("clipboard copy should still be waiting");
        copy_worker
            .join()
            .expect("copy worker should not panic")
            .expect("copy should succeed");
        exit_done_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("exit guard should drain after copy");
        gate_done_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("configuration gate should release after copy");
        exit_worker.join().expect("exit worker should not panic");
        gate_worker.join().expect("gate worker should not panic");
    }

    #[test]
    fn clear_is_busy_during_active_work_and_reports_the_removed_count_when_idle() {
        let busy_history = Arc::new(ListHistoryStore::records(vec![record(
            DeliveryId::new(),
            "保留",
            1,
        )]));
        let busy = controller(
            Arc::clone(&busy_history),
            Arc::new(RecordingClipboard::default()),
            true,
        );
        assert_eq!(
            futures::executor::block_on(busy.clear_all()).expect_err("active Session"),
            HistoryError::Busy
        );
        assert_eq!(busy_history.list_calls.load(Ordering::Relaxed), 0);
        assert_eq!(busy_history.clear_calls.load(Ordering::Relaxed), 0);

        let idle_history = Arc::new(ListHistoryStore::records(vec![
            record(DeliveryId::new(), "一", 1),
            record(DeliveryId::new(), "二", 2),
        ]));
        let idle = controller(
            Arc::clone(&idle_history),
            Arc::new(RecordingClipboard::default()),
            false,
        );
        assert_eq!(
            futures::executor::block_on(idle.clear_all()).expect("clear succeeds"),
            2
        );
        assert_eq!(idle_history.clear_calls.load(Ordering::Relaxed), 1);
        assert!(
            futures::executor::block_on(idle.list())
                .expect("empty after clear")
                .is_empty()
        );
        assert_eq!(
            futures::executor::block_on(idle.clear_all()).expect("idempotent clear"),
            0
        );
    }
}
