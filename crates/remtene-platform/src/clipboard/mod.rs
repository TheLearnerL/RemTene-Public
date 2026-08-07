//! Explicitly authorized, transactional clipboard bridge core.
//!
//! The clipboard is not a normal output path.  An operational adapter can only
//! be constructed with [`ClipboardBridgeAuthorization`], and the only public
//! way to obtain that proof is to pass an enabled user setting explicitly.
//! Platform-specific pasteboard and key-event code is injected through
//! [`ClipboardTransactionBackend`]; this module owns the safety policy and can
//! therefore test it without touching a real clipboard.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
};

#[allow(unsafe_code)]
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub use macos::{MacClipboardBackend, PasteboardSnapshot};

use remtene_application::ports::{
    ClipboardBridge, ClipboardTextWriter, InsertOutcome, LifecycleFence, PortError, PortFuture,
    SelectionSnapshot, TargetSnapshotRef, UserDirectedPasteOutcome, ValidatedTargetRef,
};
use remtene_domain::DeliveryId;
use thiserror::Error;

/// Proof that the user explicitly enabled the clipboard bridge setting.
///
/// The field is private so callers cannot synthesize authorization by directly
/// constructing the value.  This is a composition-time capability, not a
/// runtime fallback that the adapter may enable for itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipboardBridgeAuthorization {
    _private: (),
}

impl ClipboardBridgeAuthorization {
    /// Returns authorization only when the caller explicitly supplies an
    /// enabled user setting.
    #[must_use]
    pub const fn from_enabled_user_setting(enabled: bool) -> Option<Self> {
        if enabled {
            Some(Self { _private: () })
        } else {
            None
        }
    }
}

/// Result of checking that a captured target still denotes the same safe
/// control and insertion anchor at the real clipboard operation boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardTargetStatus {
    Valid,
    Invalid,
    Indeterminate,
}

/// The strongest conclusion a native paste backend can prove after dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardPasteOutcome {
    Inserted,
    NotInserted,
    Indeterminate,
}

/// The strongest conclusion a native copy-selection backend can prove.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardSelectionCopyOutcome {
    Copied,
    NotCopied,
    Indeterminate,
}

/// Structurally content-free native backend failure.
///
/// The type intentionally carries no message or arbitrary code so clipboard
/// text, selected text, target labels, and paths cannot leak into a
/// [`PortError`]. The policy layer supplies a fixed operation-stage code.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("clipboard backend operation failed")]
pub struct ClipboardBackendError {
    retryable: bool,
}

impl ClipboardBackendError {
    #[must_use]
    pub const fn transient() -> Self {
        Self { retryable: true }
    }

    #[must_use]
    pub const fn permanent() -> Self {
        Self { retryable: false }
    }

    #[must_use]
    pub const fn is_retryable(self) -> bool {
        self.retryable
    }
}

/// Injected native clipboard and input-event operations.
///
/// Implementations must treat `snapshot` as read-only. `stage_text`,
/// `copy_selection`, and `paste` may mutate external state. A call returning an
/// error after one of those methods begins does not prove that no mutation took
/// place; the policy layer consequently restores the snapshot and reports an
/// indeterminate outcome.
pub trait ClipboardTransactionBackend: Send + Sync + 'static {
    type Snapshot: Send + 'static;

    fn snapshot(&self) -> Result<Self::Snapshot, ClipboardBackendError>;

    fn validate_selection_target(&self, target: &TargetSnapshotRef) -> ClipboardTargetStatus;

    fn copy_selection(
        &self,
        target: &TargetSnapshotRef,
    ) -> Result<ClipboardSelectionCopyOutcome, ClipboardBackendError>;

    fn read_text(&self) -> Result<Option<String>, ClipboardBackendError>;

    fn validate_insert_target(&self, target: &ValidatedTargetRef) -> ClipboardTargetStatus;

    fn stage_text(&self, text: &str) -> Result<(), ClipboardBackendError>;

    fn paste(
        &self,
        target: &ValidatedTargetRef,
        text: &str,
    ) -> Result<ClipboardPasteOutcome, ClipboardBackendError>;

    /// Posts one paste shortcut to whatever keyboard focus the user selected
    /// at this exact boundary. No target identity or content verification is
    /// available on this compatibility path.
    fn dispatch_user_directed_paste(&self) -> Result<(), ClipboardBackendError>;

    fn restore(&self, snapshot: Self::Snapshot) -> Result<(), ClipboardBackendError>;
}

/// Operational bridge available only after explicit user authorization.
///
/// Transactions are serialized so one invocation cannot snapshot or restore a
/// clipboard that another invocation currently owns. This type deliberately
/// has no `Default` implementation and no constructor that omits
/// [`ClipboardBridgeAuthorization`].
pub struct AuthorizedClipboardBridge<B>
where
    B: ClipboardTransactionBackend,
{
    backend: Arc<B>,
    transaction: Mutex<ClipboardTransactionState>,
}

const DELIVERY_LEDGER_CAPACITY: usize = 256;

#[derive(Default)]
struct ClipboardTransactionState {
    delivery_outcomes: HashMap<DeliveryId, ClipboardDeliveryOutcome>,
    delivery_order: VecDeque<DeliveryId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClipboardDeliveryOutcome {
    TargetBound(InsertOutcome),
    UserDirected(UserDirectedPasteOutcome),
}

impl<B> AuthorizedClipboardBridge<B>
where
    B: ClipboardTransactionBackend,
{
    #[must_use]
    pub fn new(backend: Arc<B>, _authorization: ClipboardBridgeAuthorization) -> Self {
        Self {
            backend,
            transaction: Mutex::new(ClipboardTransactionState::default()),
        }
    }

    fn read_selected_text_sync(
        &self,
        target: &TargetSnapshotRef,
    ) -> Result<SelectionSnapshot, PortError> {
        let _transaction = self.lock_transaction();
        let snapshot = self
            .backend
            .snapshot()
            .map_err(|error| pre_mutation_error("snapshot", &error))?;

        match self.backend.validate_selection_target(target) {
            ClipboardTargetStatus::Valid => {}
            ClipboardTargetStatus::Invalid => {
                return Err(port_error(
                    "clipboard.selection_target_invalid",
                    "errors.clipboard.selection_target_invalid",
                    false,
                ));
            }
            ClipboardTargetStatus::Indeterminate => {
                return Err(port_error(
                    "clipboard.selection_target_indeterminate",
                    "errors.clipboard.selection_target_indeterminate",
                    false,
                ));
            }
        }

        // From this point onward a native copy attempt may already have
        // replaced the clipboard, even when it returns an error. Restoration is
        // therefore mandatory on every exit path.
        let selection_result = match self.backend.copy_selection(target) {
            Ok(ClipboardSelectionCopyOutcome::Copied) => self
                .backend
                .read_text()
                .map_err(|error| pre_mutation_error("selection_read", &error)),
            Ok(ClipboardSelectionCopyOutcome::NotCopied) => Err(port_error(
                "clipboard.selection_not_copied",
                "errors.clipboard.selection_not_copied",
                true,
            )),
            Ok(ClipboardSelectionCopyOutcome::Indeterminate) => Err(port_error(
                "clipboard.selection_copy_indeterminate",
                "errors.clipboard.selection_copy_indeterminate",
                false,
            )),
            Err(error) => Err(pre_mutation_error("selection_copy", &error)),
        };

        let restore_result = self.backend.restore(snapshot);
        if let Err(error) = restore_result {
            crate::trace::delivery(
                "clipboard.restore",
                "失败",
                "reason=backend_restore_failed operation=selection_read",
            );
            return Err(pre_mutation_error("restore", &error));
        }

        let text = selection_result?;
        Ok(SelectionSnapshot {
            text,
            anchor_normalized_to_end: true,
            exceeded_limit: false,
        })
    }

    fn insert_and_restore_sync(
        &self,
        target: &ValidatedTargetRef,
        text: &str,
        delivery_id: DeliveryId,
        lifecycle: &LifecycleFence,
    ) -> Result<InsertOutcome, PortError> {
        if text.is_empty() {
            return Ok(InsertOutcome::NotInserted);
        }

        let mut transaction = self.lock_transaction();
        if let Some(outcome) = transaction.delivery_outcomes.get(&delivery_id) {
            return Ok(match outcome {
                ClipboardDeliveryOutcome::TargetBound(outcome) => *outcome,
                ClipboardDeliveryOutcome::UserDirected(_) => InsertOutcome::Indeterminate,
            });
        }

        let snapshot = self
            .backend
            .snapshot()
            .map_err(|error| pre_mutation_error("snapshot", &error))?;

        match self.backend.validate_insert_target(target) {
            ClipboardTargetStatus::Valid => {}
            ClipboardTargetStatus::Invalid => {
                record_delivery(
                    &mut transaction,
                    delivery_id,
                    ClipboardDeliveryOutcome::TargetBound(InsertOutcome::NotInserted),
                );
                return Ok(InsertOutcome::NotInserted);
            }
            ClipboardTargetStatus::Indeterminate => {
                record_delivery(
                    &mut transaction,
                    delivery_id,
                    ClipboardDeliveryOutcome::TargetBound(InsertOutcome::Indeterminate),
                );
                return Ok(InsertOutcome::Indeterminate);
            }
        };

        let Some(_commit_guard) = lifecycle.begin_commit() else {
            return Err(port_error(
                "clipboard.lifecycle_invalidated",
                "errors.clipboard.lifecycle_invalidated",
                false,
            ));
        };

        // Calling stage_text is the first irreversible boundary. After it
        // begins, no backend error is allowed to escape as `Err` or
        // `NotInserted`, because either could invite a second write upstream.
        let outcome = match self.backend.stage_text(text) {
            Ok(()) => match self.backend.paste(target, text) {
                Ok(ClipboardPasteOutcome::Inserted) => InsertOutcome::Inserted,
                Ok(ClipboardPasteOutcome::NotInserted) => InsertOutcome::NotInserted,
                Ok(ClipboardPasteOutcome::Indeterminate) | Err(_) => InsertOutcome::Indeterminate,
            },
            Err(_) => InsertOutcome::Indeterminate,
        };

        // The commit guard intentionally remains alive through restoration.
        // A restoration failure makes the whole external transaction
        // indeterminate even when the paste itself reported a stronger result.
        if self.backend.restore(snapshot).is_err() {
            crate::trace::delivery(
                "clipboard.restore",
                "失败",
                "reason=backend_restore_failed operation=target_bound_paste",
            );
            record_delivery(
                &mut transaction,
                delivery_id,
                ClipboardDeliveryOutcome::TargetBound(InsertOutcome::Indeterminate),
            );
            return Ok(InsertOutcome::Indeterminate);
        }

        record_delivery(
            &mut transaction,
            delivery_id,
            ClipboardDeliveryOutcome::TargetBound(outcome),
        );
        Ok(outcome)
    }

    fn insert_at_current_focus_and_restore_sync(
        &self,
        text: &str,
        delivery_id: DeliveryId,
        lifecycle: &LifecycleFence,
    ) -> Result<UserDirectedPasteOutcome, PortError> {
        if text.is_empty() {
            return Ok(UserDirectedPasteOutcome::NotDispatched);
        }

        let mut transaction = self.lock_transaction();
        if let Some(outcome) = transaction.delivery_outcomes.get(&delivery_id) {
            return Ok(match outcome {
                ClipboardDeliveryOutcome::UserDirected(outcome) => *outcome,
                ClipboardDeliveryOutcome::TargetBound(_) => UserDirectedPasteOutcome::Indeterminate,
            });
        }

        let snapshot = self
            .backend
            .snapshot()
            .map_err(|error| pre_mutation_error("snapshot", &error))?;

        let Some(_commit_guard) = lifecycle.begin_commit() else {
            return Err(port_error(
                "clipboard.lifecycle_invalidated",
                "errors.clipboard.lifecycle_invalidated",
                false,
            ));
        };

        // Staging is the first irreversible boundary. Once it begins, an
        // error cannot prove the clipboard or target stayed unchanged.
        let outcome = match self.backend.stage_text(text) {
            Ok(()) => match self.backend.dispatch_user_directed_paste() {
                Ok(()) => UserDirectedPasteOutcome::Dispatched,
                Err(_) => UserDirectedPasteOutcome::Indeterminate,
            },
            Err(_) => UserDirectedPasteOutcome::Indeterminate,
        };

        // A successful shortcut dispatch remains a completed user-directed
        // delivery even if restoring the prior clipboard fails. Showing a
        // fallback surface here would invite a duplicate manual insertion.
        let restore_failed = self.backend.restore(snapshot).is_err();
        let outcome = if restore_failed && outcome == UserDirectedPasteOutcome::Dispatched {
            crate::trace::delivery(
                "userpaste.restore",
                "失败",
                "按键已派发；剪贴板可能保留本次文本",
            );
            UserDirectedPasteOutcome::Dispatched
        } else if restore_failed {
            UserDirectedPasteOutcome::Indeterminate
        } else {
            outcome
        };

        record_delivery(
            &mut transaction,
            delivery_id,
            ClipboardDeliveryOutcome::UserDirected(outcome),
        );
        Ok(outcome)
    }

    fn lock_transaction(&self) -> MutexGuard<'_, ClipboardTransactionState> {
        self.transaction
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<B> ClipboardBridge for AuthorizedClipboardBridge<B>
where
    B: ClipboardTransactionBackend,
{
    fn read_selected_text(
        &self,
        target: &TargetSnapshotRef,
    ) -> PortFuture<'_, Result<SelectionSnapshot, PortError>> {
        let target = target.clone();
        Box::pin(async move { self.read_selected_text_sync(&target) })
    }

    fn insert_and_restore(
        &self,
        target: ValidatedTargetRef,
        text: String,
        delivery_id: DeliveryId,
        lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<InsertOutcome, PortError>> {
        Box::pin(
            async move { self.insert_and_restore_sync(&target, &text, delivery_id, &lifecycle) },
        )
    }

    fn insert_at_current_focus_and_restore(
        &self,
        text: String,
        delivery_id: DeliveryId,
        lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<UserDirectedPasteOutcome, PortError>> {
        Box::pin(async move {
            self.insert_at_current_focus_and_restore_sync(&text, delivery_id, &lifecycle)
        })
    }
}

impl<B> ClipboardTextWriter for AuthorizedClipboardBridge<B>
where
    B: ClipboardTransactionBackend,
{
    fn write_text(&self, text: String) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            // Explicit copy and compatibility paste are different product
            // actions, but both mutate the same system pasteboard. Reusing the
            // transaction lock prevents a manual copy from landing between a
            // delivery snapshot and restoration.
            let _transaction = self.lock_transaction();
            self.backend.stage_text(&text).map_err(|error| {
                port_error(
                    "clipboard_text.write_failed",
                    "errors.clipboard_text.write_failed",
                    error.is_retryable(),
                )
            })
        })
    }
}

fn record_delivery(
    state: &mut ClipboardTransactionState,
    delivery_id: DeliveryId,
    outcome: ClipboardDeliveryOutcome,
) {
    if state.delivery_outcomes.contains_key(&delivery_id) {
        return;
    }
    while state.delivery_order.len() >= DELIVERY_LEDGER_CAPACITY {
        if let Some(oldest) = state.delivery_order.pop_front() {
            state.delivery_outcomes.remove(&oldest);
        }
    }
    state.delivery_order.push_back(delivery_id);
    state.delivery_outcomes.insert(delivery_id, outcome);
}

fn pre_mutation_error(stage: &str, error: &ClipboardBackendError) -> PortError {
    port_error(
        &format!("clipboard.{stage}"),
        &format!("errors.clipboard.{stage}"),
        error.is_retryable(),
    )
}

fn port_error(code: &str, safe_message_key: &str, retryable: bool) -> PortError {
    PortError {
        code: code.to_owned(),
        safe_message_key: safe_message_key.to_owned(),
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::pin,
        sync::{Arc, Mutex},
        task::{Context, Poll, Waker},
    };

    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Event {
        Snapshot,
        ValidateSelection,
        CopySelection,
        ReadText,
        ValidateInsert,
        Stage,
        Paste,
        UserDirectedPaste,
        Restore,
    }

    #[derive(Clone, Copy, Debug)]
    struct FakeSnapshot;

    #[derive(Clone)]
    struct FakeConfig {
        snapshot: Result<FakeSnapshot, ClipboardBackendError>,
        selection_target: ClipboardTargetStatus,
        copy_selection: Result<ClipboardSelectionCopyOutcome, ClipboardBackendError>,
        read_text: Result<Option<String>, ClipboardBackendError>,
        insert_target: ClipboardTargetStatus,
        stage: Result<(), ClipboardBackendError>,
        paste: Result<ClipboardPasteOutcome, ClipboardBackendError>,
        user_directed_paste: Result<(), ClipboardBackendError>,
        restore: Result<(), ClipboardBackendError>,
    }

    impl Default for FakeConfig {
        fn default() -> Self {
            Self {
                snapshot: Ok(FakeSnapshot),
                selection_target: ClipboardTargetStatus::Valid,
                copy_selection: Ok(ClipboardSelectionCopyOutcome::Copied),
                read_text: Ok(Some("selected context".to_owned())),
                insert_target: ClipboardTargetStatus::Valid,
                stage: Ok(()),
                paste: Ok(ClipboardPasteOutcome::Inserted),
                user_directed_paste: Ok(()),
                restore: Ok(()),
            }
        }
    }

    struct FakeBackend {
        config: Mutex<FakeConfig>,
        events: Mutex<Vec<Event>>,
    }

    impl FakeBackend {
        fn new(config: FakeConfig) -> Self {
            Self {
                config: Mutex::new(config),
                events: Mutex::new(Vec::new()),
            }
        }

        fn record(&self, event: Event) {
            self.events.lock().expect("events lock").push(event);
        }

        fn events(&self) -> Vec<Event> {
            self.events.lock().expect("events lock").clone()
        }
    }

    impl ClipboardTransactionBackend for FakeBackend {
        type Snapshot = FakeSnapshot;

        fn snapshot(&self) -> Result<Self::Snapshot, ClipboardBackendError> {
            self.record(Event::Snapshot);
            self.config.lock().expect("config lock").snapshot
        }

        fn validate_selection_target(&self, _target: &TargetSnapshotRef) -> ClipboardTargetStatus {
            self.record(Event::ValidateSelection);
            self.config.lock().expect("config lock").selection_target
        }

        fn copy_selection(
            &self,
            _target: &TargetSnapshotRef,
        ) -> Result<ClipboardSelectionCopyOutcome, ClipboardBackendError> {
            self.record(Event::CopySelection);
            self.config.lock().expect("config lock").copy_selection
        }

        fn read_text(&self) -> Result<Option<String>, ClipboardBackendError> {
            self.record(Event::ReadText);
            self.config.lock().expect("config lock").read_text.clone()
        }

        fn validate_insert_target(&self, _target: &ValidatedTargetRef) -> ClipboardTargetStatus {
            self.record(Event::ValidateInsert);
            self.config.lock().expect("config lock").insert_target
        }

        fn stage_text(&self, _text: &str) -> Result<(), ClipboardBackendError> {
            self.record(Event::Stage);
            self.config.lock().expect("config lock").stage
        }

        fn paste(
            &self,
            _target: &ValidatedTargetRef,
            _text: &str,
        ) -> Result<ClipboardPasteOutcome, ClipboardBackendError> {
            self.record(Event::Paste);
            self.config.lock().expect("config lock").paste
        }

        fn dispatch_user_directed_paste(&self) -> Result<(), ClipboardBackendError> {
            self.record(Event::UserDirectedPaste);
            self.config.lock().expect("config lock").user_directed_paste
        }

        fn restore(&self, _snapshot: Self::Snapshot) -> Result<(), ClipboardBackendError> {
            self.record(Event::Restore);
            self.config.lock().expect("config lock").restore
        }
    }

    fn backend_error() -> ClipboardBackendError {
        ClipboardBackendError::transient()
    }

    fn authorized_bridge(
        config: FakeConfig,
    ) -> (AuthorizedClipboardBridge<FakeBackend>, Arc<FakeBackend>) {
        let authorization = ClipboardBridgeAuthorization::from_enabled_user_setting(true)
            .expect("enabled setting grants authorization");
        let backend = Arc::new(FakeBackend::new(config));
        let bridge = AuthorizedClipboardBridge::new(Arc::clone(&backend), authorization);
        (bridge, backend)
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = pin!(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[test]
    fn authorization_requires_enabled_setting_and_construction_has_no_side_effects() {
        assert!(ClipboardBridgeAuthorization::from_enabled_user_setting(false).is_none());
        let (bridge, backend) = authorized_bridge(FakeConfig::default());
        fn assert_clipboard_port<T: ClipboardBridge>(_value: &T) {}
        fn assert_text_writer_port<T: ClipboardTextWriter>(_value: &T) {}
        assert_clipboard_port(&bridge);
        assert_text_writer_port(&bridge);
        assert!(backend.events().is_empty());
    }

    #[test]
    fn explicit_text_write_uses_the_same_transaction_gate_as_paste() {
        use std::{sync::mpsc, thread, time::Duration};

        let (bridge, backend) = authorized_bridge(FakeConfig::default());
        let bridge = Arc::new(bridge);
        let transaction = bridge.lock_transaction();
        let writer = Arc::clone(&bridge);
        let (completed_tx, completed_rx) = mpsc::channel();
        let task = thread::spawn(move || {
            let result = block_on(writer.write_text("明确复制的文字".to_owned()));
            completed_tx.send(result).expect("report copy result");
        });

        assert!(
            completed_rx
                .recv_timeout(Duration::from_millis(20))
                .is_err(),
            "manual copy must wait while a clipboard transaction owns the gate"
        );
        assert!(backend.events().is_empty());

        drop(transaction);
        completed_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("copy resumes after transaction")
            .expect("copy succeeds");
        task.join().expect("copy task joins");
        assert_eq!(backend.events(), vec![Event::Stage]);
    }

    #[test]
    fn explicit_text_write_failure_is_content_free() {
        let config = FakeConfig {
            stage: Err(ClipboardBackendError::transient()),
            ..FakeConfig::default()
        };
        let (bridge, _) = authorized_bridge(config);
        let private_text = "不应进入错误的正文";

        let error = block_on(bridge.write_text(private_text.to_owned()))
            .expect_err("native writer failure remains explicit");

        assert_eq!(error.code, "clipboard_text.write_failed");
        assert_eq!(error.safe_message_key, "errors.clipboard_text.write_failed");
        assert!(error.retryable);
        assert!(!error.code.contains(private_text));
        assert!(!error.safe_message_key.contains(private_text));
    }

    #[test]
    fn insert_runs_snapshot_stage_paste_restore_in_order() {
        let (bridge, backend) = authorized_bridge(FakeConfig::default());

        let outcome = block_on(bridge.insert_and_restore(
            ValidatedTargetRef::new("target"),
            "hello".to_owned(),
            DeliveryId::new(),
            LifecycleFence::new(),
        ))
        .expect("insert result");

        assert_eq!(outcome, InsertOutcome::Inserted);
        assert_eq!(
            backend.events(),
            vec![
                Event::Snapshot,
                Event::ValidateInsert,
                Event::Stage,
                Event::Paste,
                Event::Restore,
            ]
        );
    }

    #[test]
    fn delivery_id_is_single_use_and_never_pastes_twice() {
        let (bridge, backend) = authorized_bridge(FakeConfig::default());
        let delivery_id = DeliveryId::new();

        let first = block_on(bridge.insert_and_restore(
            ValidatedTargetRef::new("target"),
            "hello".to_owned(),
            delivery_id,
            LifecycleFence::new(),
        ))
        .expect("first result");
        let events_after_first = backend.events();
        let repeated = block_on(bridge.insert_and_restore(
            ValidatedTargetRef::new("different-target"),
            "different text".to_owned(),
            delivery_id,
            LifecycleFence::new(),
        ))
        .expect("repeated result");

        assert_eq!(first, InsertOutcome::Inserted);
        assert_eq!(repeated, first);
        assert_eq!(backend.events(), events_after_first);
    }

    #[test]
    fn user_directed_paste_runs_one_transaction_without_target_validation() {
        let (bridge, backend) = authorized_bridge(FakeConfig::default());

        let outcome = block_on(bridge.insert_at_current_focus_and_restore(
            "hello".to_owned(),
            DeliveryId::new(),
            LifecycleFence::new(),
        ))
        .expect("user-directed paste result");

        assert_eq!(outcome, UserDirectedPasteOutcome::Dispatched);
        assert_eq!(
            backend.events(),
            vec![
                Event::Snapshot,
                Event::Stage,
                Event::UserDirectedPaste,
                Event::Restore,
            ]
        );
    }

    #[test]
    fn user_directed_delivery_id_is_single_use_across_both_paste_paths() {
        let (bridge, backend) = authorized_bridge(FakeConfig::default());
        let delivery_id = DeliveryId::new();

        let first = block_on(bridge.insert_at_current_focus_and_restore(
            "hello".to_owned(),
            delivery_id,
            LifecycleFence::new(),
        ))
        .expect("first user-directed result");
        let events_after_first = backend.events();
        let target_bound = block_on(bridge.insert_and_restore(
            ValidatedTargetRef::new("target"),
            "duplicate".to_owned(),
            delivery_id,
            LifecycleFence::new(),
        ))
        .expect("second route is typed without dispatch");

        assert_eq!(first, UserDirectedPasteOutcome::Dispatched);
        assert_eq!(target_bound, InsertOutcome::Indeterminate);
        assert_eq!(backend.events(), events_after_first);
    }

    #[test]
    fn target_bound_delivery_id_blocks_later_user_directed_paste() {
        let (bridge, backend) = authorized_bridge(FakeConfig::default());
        let delivery_id = DeliveryId::new();

        let first = block_on(bridge.insert_and_restore(
            ValidatedTargetRef::new("target"),
            "hello".to_owned(),
            delivery_id,
            LifecycleFence::new(),
        ))
        .expect("first target-bound result");
        let events_after_first = backend.events();
        let user_directed = block_on(bridge.insert_at_current_focus_and_restore(
            "duplicate".to_owned(),
            delivery_id,
            LifecycleFence::new(),
        ))
        .expect("second route is typed without dispatch");

        assert_eq!(first, InsertOutcome::Inserted);
        assert_eq!(user_directed, UserDirectedPasteOutcome::Indeterminate);
        assert_eq!(backend.events(), events_after_first);
    }

    #[test]
    fn successful_user_dispatch_is_not_downgraded_by_restore_failure() {
        let config = FakeConfig {
            restore: Err(backend_error()),
            ..FakeConfig::default()
        };
        let (bridge, backend) = authorized_bridge(config);

        let outcome = block_on(bridge.insert_at_current_focus_and_restore(
            "hello".to_owned(),
            DeliveryId::new(),
            LifecycleFence::new(),
        ))
        .expect("posted shortcut remains dispatched");

        assert_eq!(outcome, UserDirectedPasteOutcome::Dispatched);
        assert_eq!(backend.events().last(), Some(&Event::Restore));
    }

    #[test]
    fn user_directed_dispatch_error_is_indeterminate_and_never_retried() {
        let config = FakeConfig {
            user_directed_paste: Err(backend_error()),
            ..FakeConfig::default()
        };
        let (bridge, backend) = authorized_bridge(config);
        let delivery_id = DeliveryId::new();

        let first = block_on(bridge.insert_at_current_focus_and_restore(
            "hello".to_owned(),
            delivery_id,
            LifecycleFence::new(),
        ))
        .expect("dispatch uncertainty is typed");
        let events_after_first = backend.events();
        let repeated = block_on(bridge.insert_at_current_focus_and_restore(
            "retry".to_owned(),
            delivery_id,
            LifecycleFence::new(),
        ))
        .expect("repeat returns the recorded outcome");

        assert_eq!(first, UserDirectedPasteOutcome::Indeterminate);
        assert_eq!(repeated, first);
        assert_eq!(backend.events(), events_after_first);
    }

    #[test]
    fn invalidated_target_never_stages_or_pastes() {
        let config = FakeConfig {
            insert_target: ClipboardTargetStatus::Invalid,
            ..FakeConfig::default()
        };
        let (bridge, backend) = authorized_bridge(config);

        let outcome = block_on(bridge.insert_and_restore(
            ValidatedTargetRef::new("stale-target"),
            "hello".to_owned(),
            DeliveryId::new(),
            LifecycleFence::new(),
        ))
        .expect("invalid target is a proven non-insert");

        assert_eq!(outcome, InsertOutcome::NotInserted);
        assert_eq!(
            backend.events(),
            vec![Event::Snapshot, Event::ValidateInsert]
        );
    }

    #[test]
    fn invalidated_lifecycle_prevents_first_mutation() {
        let (bridge, backend) = authorized_bridge(FakeConfig::default());
        let lifecycle = LifecycleFence::new();
        lifecycle.invalidate();

        let error = block_on(bridge.insert_and_restore(
            ValidatedTargetRef::new("target"),
            "hello".to_owned(),
            DeliveryId::new(),
            lifecycle,
        ))
        .expect_err("invalid lifecycle must reject commit");

        assert_eq!(error.code, "clipboard.lifecycle_invalidated");
        assert_eq!(
            backend.events(),
            vec![Event::Snapshot, Event::ValidateInsert]
        );
    }

    #[test]
    fn stage_failure_attempts_restore_and_is_indeterminate() {
        let config = FakeConfig {
            stage: Err(backend_error()),
            ..FakeConfig::default()
        };
        let (bridge, backend) = authorized_bridge(config);

        let outcome = block_on(bridge.insert_and_restore(
            ValidatedTargetRef::new("target"),
            "hello".to_owned(),
            DeliveryId::new(),
            LifecycleFence::new(),
        ))
        .expect("post-stage failures are outcome uncertainty, not port errors");

        assert_eq!(outcome, InsertOutcome::Indeterminate);
        assert_eq!(
            backend.events(),
            vec![
                Event::Snapshot,
                Event::ValidateInsert,
                Event::Stage,
                Event::Restore,
            ]
        );
    }

    #[test]
    fn paste_uncertainty_attempts_restore_and_stays_indeterminate() {
        let config = FakeConfig {
            paste: Ok(ClipboardPasteOutcome::Indeterminate),
            ..FakeConfig::default()
        };
        let (bridge, backend) = authorized_bridge(config);

        let outcome = block_on(bridge.insert_and_restore(
            ValidatedTargetRef::new("target"),
            "hello".to_owned(),
            DeliveryId::new(),
            LifecycleFence::new(),
        ))
        .expect("paste uncertainty is represented as an outcome");

        assert_eq!(outcome, InsertOutcome::Indeterminate);
        assert_eq!(backend.events().last(), Some(&Event::Restore));
    }

    #[test]
    fn paste_error_attempts_restore_and_is_indeterminate() {
        let config = FakeConfig {
            paste: Err(backend_error()),
            ..FakeConfig::default()
        };
        let (bridge, backend) = authorized_bridge(config);

        let outcome = block_on(bridge.insert_and_restore(
            ValidatedTargetRef::new("target"),
            "hello".to_owned(),
            DeliveryId::new(),
            LifecycleFence::new(),
        ))
        .expect("post-paste errors cannot invite a retry");

        assert_eq!(outcome, InsertOutcome::Indeterminate);
        assert_eq!(backend.events().last(), Some(&Event::Restore));
    }

    #[test]
    fn restore_fault_overrides_confirmed_insert_with_indeterminate() {
        let config = FakeConfig {
            restore: Err(backend_error()),
            ..FakeConfig::default()
        };
        let (bridge, backend) = authorized_bridge(config);

        let outcome = block_on(bridge.insert_and_restore(
            ValidatedTargetRef::new("target"),
            "hello".to_owned(),
            DeliveryId::new(),
            LifecycleFence::new(),
        ))
        .expect("restore uncertainty must not become a retryable error");

        assert_eq!(outcome, InsertOutcome::Indeterminate);
        assert_eq!(backend.events().last(), Some(&Event::Restore));
    }

    #[test]
    fn proven_non_insert_is_returned_only_after_successful_restore() {
        let config = FakeConfig {
            paste: Ok(ClipboardPasteOutcome::NotInserted),
            ..FakeConfig::default()
        };
        let (bridge, backend) = authorized_bridge(config);

        let outcome = block_on(bridge.insert_and_restore(
            ValidatedTargetRef::new("target"),
            "hello".to_owned(),
            DeliveryId::new(),
            LifecycleFence::new(),
        ))
        .expect("proven non-insert");

        assert_eq!(outcome, InsertOutcome::NotInserted);
        assert_eq!(backend.events().last(), Some(&Event::Restore));
    }

    #[test]
    fn selection_read_restores_original_clipboard() {
        let (bridge, backend) = authorized_bridge(FakeConfig::default());

        let selection = block_on(bridge.read_selected_text(&TargetSnapshotRef::new("target")))
            .expect("selection result");

        assert_eq!(selection.text.as_deref(), Some("selected context"));
        assert!(selection.anchor_normalized_to_end);
        assert!(!selection.exceeded_limit);
        assert_eq!(
            backend.events(),
            vec![
                Event::Snapshot,
                Event::ValidateSelection,
                Event::CopySelection,
                Event::ReadText,
                Event::Restore,
            ]
        );
    }

    #[test]
    fn selection_copy_failure_still_attempts_restore() {
        let config = FakeConfig {
            copy_selection: Err(backend_error()),
            ..FakeConfig::default()
        };
        let (bridge, backend) = authorized_bridge(config);

        let error = block_on(bridge.read_selected_text(&TargetSnapshotRef::new("target")))
            .expect_err("copy error");

        assert!(error.code.contains("selection_copy"));
        assert_eq!(backend.events().last(), Some(&Event::Restore));
    }
}
