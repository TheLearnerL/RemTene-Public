//! File-backed history store.
//!
//! Persists transcription delivery records as a JSON file. Per DATA-020,
//! history stores only the final delivered text and its creation time —
//! never audio, selections, target handles, or intermediate transcripts.
//!
//! ## Storage format
//!
//! A single JSON file holding an array of records. Each record carries the
//! delivery id (UUID string), the final text, and the creation timestamp in
//! milliseconds. The file is rewritten atomically (temp file + rename) on
//! every mutation so a crash mid-write cannot corrupt existing history.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use remtene_application::ports::{
    HistoryRecord, HistoryStore, LifecycleFence, PortError, PortFuture,
};
use remtene_domain::{DeliveryId, SettingsSnapshot, TimestampMs};
use serde::{Deserialize, Serialize};

/// Serializable mirror of `HistoryRecord`.
///
/// The domain type intentionally does not derive `Serialize`, so this adapter
/// owns its own on-disk representation and converts explicitly.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct HistoryRecordDto {
    /// Delivery id as a UUID string.
    delivery_id: String,
    /// The final delivered text (the only content persisted).
    final_text: String,
    /// Creation time in milliseconds since the Unix epoch.
    created_at_ms: u64,
}

impl HistoryRecordDto {
    fn from_record(record: &HistoryRecord) -> Self {
        Self {
            delivery_id: record.delivery_id.as_uuid().to_string(),
            final_text: record.final_text.clone(),
            created_at_ms: record.created_at.get(),
        }
    }

    fn into_record(self) -> Result<HistoryRecord, PortError> {
        let uuid = self
            .delivery_id
            .parse()
            .map_err(|_| decode_error("history.record_id_invalid", "invalid record UUID"))?;
        Ok(HistoryRecord {
            delivery_id: DeliveryId::from_uuid(uuid),
            final_text: self.final_text,
            created_at: TimestampMs::new(self.created_at_ms),
        })
    }
}

/// History store that persists records to a JSON file on disk.
pub struct FileHistoryStore {
    path: PathBuf,
    // Serializes read-modify-write cycles so concurrent saves cannot race on
    // the file. Held only for the synchronous file work inside each future.
    guard: Mutex<()>,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl FileHistoryStore {
    /// Creates a store backed by `path`. The parent directory is created if it
    /// does not yet exist. A missing history file is treated as empty history.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, PortError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| io_error("history.create_dir_failed", &e))?;
        }
        Ok(Self {
            path,
            guard: Mutex::new(()),
            now_ms: Arc::new(system_now_ms),
        })
    }

    #[cfg(test)]
    fn new_with_now(path: impl Into<PathBuf>, now_ms: u64) -> Result<Self, PortError> {
        let mut store = Self::new(path)?;
        store.now_ms = Arc::new(move || now_ms);
        Ok(store)
    }

    fn read_all(path: &Path) -> Result<Vec<HistoryRecordDto>, PortError> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| decode_error("history.decode_failed", &e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(io_error("history.read_failed", &e)),
        }
    }

    fn write_all(path: &Path, records: &[HistoryRecordDto]) -> Result<(), PortError> {
        let bytes = serde_json::to_vec_pretty(records)
            .map_err(|e| decode_error("history.encode_failed", &e.to_string()))?;

        // Atomic replace: write to a temp file in the same directory, then rename.
        let tmp = path.with_extension("json.tmp");
        {
            let mut file =
                fs::File::create(&tmp).map_err(|e| io_error("history.temp_create_failed", &e))?;
            file.write_all(&bytes)
                .map_err(|e| io_error("history.temp_write_failed", &e))?;
            file.sync_all()
                .map_err(|e| io_error("history.temp_sync_failed", &e))?;
        }
        fs::rename(&tmp, path).map_err(|e| io_error("history.rename_failed", &e))?;
        Ok(())
    }
}

impl HistoryStore for FileHistoryStore {
    fn save_with_policy(
        &self,
        record: HistoryRecord,
        settings: &SettingsSnapshot,
        lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<(), PortError>> {
        let policy = settings.history_policy();
        let now_ms = (self.now_ms)();
        Box::pin(async move {
            if !policy.enabled {
                return Ok(());
            }
            let _lock = self.guard.lock().map_err(|_| lock_error())?;
            let mut records = Self::read_all(&self.path)?;
            let record_id = record.delivery_id.as_uuid().to_string();
            if let Some(existing) = records
                .iter()
                .find(|existing| existing.delivery_id == record_id)
            {
                if existing.final_text == record.final_text
                    && existing.created_at_ms == record.created_at.get()
                {
                    apply_policy(&mut records, policy.limit, policy.retention_days, now_ms);
                } else {
                    return Err(decode_error(
                        "history.record_id_conflict",
                        "record identity already stores different content",
                    ));
                }
            } else {
                records.push(HistoryRecordDto::from_record(&record));
                apply_policy(&mut records, policy.limit, policy.retention_days, now_ms);
            }
            let Some(_commit) = lifecycle.begin_commit() else {
                return Err(lifecycle_error());
            };
            Self::write_all(&self.path, &records)
        })
    }

    fn list(&self) -> PortFuture<'_, Result<Vec<HistoryRecord>, PortError>> {
        Box::pin(async move {
            let _lock = self.guard.lock().map_err(|_| lock_error())?;
            let records = Self::read_all(&self.path)?;
            records
                .into_iter()
                .map(HistoryRecordDto::into_record)
                .collect()
        })
    }

    fn clear_all(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            let _lock = self.guard.lock().map_err(|_| lock_error())?;
            Self::write_all(&self.path, &[])
        })
    }

    fn enforce_policy(
        &self,
        settings: &SettingsSnapshot,
        lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<(), PortError>> {
        let policy = settings.history_policy();
        let now_ms = (self.now_ms)();
        Box::pin(async move {
            let _lock = self.guard.lock().map_err(|_| lock_error())?;

            // Disabling history only prevents future Session writes. Existing
            // records remain until an explicit clear or an enabled retention
            // policy removes them.
            if !policy.enabled {
                return Ok(());
            }

            let mut records = Self::read_all(&self.path)?;
            apply_policy(&mut records, policy.limit, policy.retention_days, now_ms);

            let Some(_commit) = lifecycle.begin_commit() else {
                return Err(lifecycle_error());
            };
            Self::write_all(&self.path, &records)
        })
    }
}

const HISTORY_DAY_MS: u64 = 24 * 60 * 60 * 1_000;

fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn apply_policy(
    records: &mut Vec<HistoryRecordDto>,
    limit: u16,
    retention_days: Option<u16>,
    now_ms: u64,
) {
    if let Some(retention_days) = retention_days {
        let retention_ms = u64::from(retention_days).saturating_mul(HISTORY_DAY_MS);
        let cutoff_ms = now_ms.saturating_sub(retention_ms);
        records.retain(|record| record.created_at_ms >= cutoff_ms);
    }
    apply_limit(records, limit);
}

fn apply_limit(records: &mut Vec<HistoryRecordDto>, limit: u16) {
    // FIFO is defined by creation time, not JSON insertion order. Equal
    // timestamps use the opaque identity for deterministic ordering without
    // inventing user-visible meaning.
    records.sort_by(|left, right| {
        left.created_at_ms
            .cmp(&right.created_at_ms)
            .then_with(|| left.delivery_id.cmp(&right.delivery_id))
    });

    // Domain validation guarantees a non-zero enabled limit.
    let limit = limit as usize;
    if records.len() > limit {
        let excess = records.len() - limit;
        records.drain(0..excess);
    }
}

fn io_error(code: &str, err: &std::io::Error) -> PortError {
    PortError {
        code: code.to_string(),
        safe_message_key: "errors.history.io".to_string(),
        retryable: matches!(err.kind(), std::io::ErrorKind::Interrupted),
    }
}

fn decode_error(code: &str, _detail: &str) -> PortError {
    PortError {
        code: code.to_string(),
        safe_message_key: "errors.history.decode".to_string(),
        retryable: false,
    }
}

fn lock_error() -> PortError {
    PortError {
        code: "history.lock_poisoned".to_string(),
        safe_message_key: "errors.history.lock".to_string(),
        retryable: false,
    }
}

fn lifecycle_error() -> PortError {
    PortError {
        code: "history.lifecycle_invalidated".to_string(),
        safe_message_key: "errors.history.lifecycle_invalidated".to_string(),
        retryable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("remtene-history-{name}-{nanos}/history.json"))
    }

    fn record(text: &str, ms: u64) -> HistoryRecord {
        HistoryRecord {
            delivery_id: DeliveryId::new(),
            final_text: text.to_owned(),
            created_at: TimestampMs::new(ms),
        }
    }

    fn save(store: &FileHistoryStore, record: HistoryRecord) -> Result<(), PortError> {
        futures::executor::block_on(store.save_with_policy(
            record,
            &settings_with_history(true, u16::MAX),
            LifecycleFence::new(),
        ))
    }

    #[test]
    fn missing_file_lists_empty() {
        let store = FileHistoryStore::new(temp_path("missing")).unwrap();
        let listed = futures::executor::block_on(store.list()).unwrap();
        assert!(listed.is_empty());
    }

    #[test]
    fn save_then_list_roundtrips_text_and_time() {
        let store = FileHistoryStore::new(temp_path("roundtrip")).unwrap();
        let rec = record("你好世界", 1_700_000_000_000);
        save(&store, rec.clone()).unwrap();

        let listed = futures::executor::block_on(store.list()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].final_text, "你好世界");
        assert_eq!(listed[0].created_at.get(), 1_700_000_000_000);
        assert_eq!(listed[0].delivery_id, rec.delivery_id);
    }

    #[test]
    fn saved_records_persist_across_instances() {
        let path = temp_path("persist");
        {
            let store = FileHistoryStore::new(&path).unwrap();
            save(&store, record("first", 1)).unwrap();
        }
        let reopened = FileHistoryStore::new(&path).unwrap();
        let listed = futures::executor::block_on(reopened.list()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].final_text, "first");
    }

    #[test]
    fn invalid_record_identity_fails_the_whole_read_instead_of_looking_empty() {
        let path = temp_path("invalid-record-id");
        let store = FileHistoryStore::new(&path).unwrap();
        std::fs::write(
            &path,
            r#"[{"delivery_id":"not-a-uuid","final_text":"不能静默丢弃","created_at_ms":1}]"#
                .as_bytes(),
        )
        .unwrap();

        let error = futures::executor::block_on(store.list())
            .expect_err("corrupt record identity must fail closed");

        assert_eq!(error.code, "history.record_id_invalid");
        assert_eq!(error.safe_message_key, "errors.history.decode");
    }

    #[test]
    fn clear_all_empties_history() {
        let store = FileHistoryStore::new(temp_path("clear")).unwrap();
        save(&store, record("x", 1)).unwrap();
        futures::executor::block_on(store.clear_all()).unwrap();
        let listed = futures::executor::block_on(store.list()).unwrap();
        assert!(listed.is_empty());
    }

    fn settings_with_history(enabled: bool, limit: u16) -> SettingsSnapshot {
        use remtene_domain::{
            AsrPreference, HistoryPolicy, ProcessingMode, RecordingMode, SettingsSnapshotInput,
        };
        use std::time::Duration;
        let input = SettingsSnapshotInput {
            version: 0,
            recording_mode: RecordingMode::PushToTalk,
            max_recording_duration: Duration::from_secs(60),
            recording_shortcut: None,
            processing_mode: ProcessingMode::Raw,
            asr_preference: AsrPreference::Qwen,
            llm: None,
            read_selected_text: false,
            clipboard_bridge_allowed: false,
            auto_copy_result: false,
            local_diagnostics_enabled: true,
            history_policy: HistoryPolicy {
                enabled,
                limit,
                retention_days: None,
            },
        };
        SettingsSnapshot::new(input).expect("valid settings")
    }

    fn settings_with_retention(days: u16) -> SettingsSnapshot {
        let mut input = settings_with_history(true, u16::MAX).into_input();
        input.history_policy.retention_days = Some(days);
        SettingsSnapshot::new(input).expect("valid retention")
    }

    #[test]
    fn enforce_policy_disabled_preserves_existing_history() {
        let store = FileHistoryStore::new(temp_path("policy-off")).unwrap();
        save(&store, record("x", 1)).unwrap();

        let settings = settings_with_history(false, 10);
        futures::executor::block_on(store.enforce_policy(&settings, LifecycleFence::new()))
            .unwrap();
        let listed = futures::executor::block_on(store.list()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].final_text, "x");
    }

    #[test]
    fn enforce_policy_trims_to_limit() {
        let store = FileHistoryStore::new(temp_path("policy-limit")).unwrap();
        for i in 0..5u64 {
            save(&store, record("r", i)).unwrap();
        }
        let settings = settings_with_history(true, 2);
        futures::executor::block_on(store.enforce_policy(&settings, LifecycleFence::new()))
            .unwrap();
        let listed = futures::executor::block_on(store.list()).unwrap();
        // Keeps the most recent 2 (created_at 3 and 4).
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].created_at.get(), 3);
        assert_eq!(listed[1].created_at.get(), 4);
    }

    #[test]
    fn enforce_policy_uses_creation_time_instead_of_insertion_order() {
        let store = FileHistoryStore::new(temp_path("policy-time-order")).unwrap();
        for timestamp in [30, 10, 20] {
            save(&store, record("r", timestamp)).unwrap();
        }

        let settings = settings_with_history(true, 2);
        futures::executor::block_on(store.enforce_policy(&settings, LifecycleFence::new()))
            .unwrap();
        let listed = futures::executor::block_on(store.list()).unwrap();

        assert_eq!(
            listed
                .iter()
                .map(|record| record.created_at.get())
                .collect::<Vec<_>>(),
            vec![20, 30]
        );
    }

    #[test]
    fn retention_removes_expired_rows_before_applying_the_count_limit() {
        let day = HISTORY_DAY_MS;
        let now = 10 * day;
        let store = FileHistoryStore::new_with_now(temp_path("retention"), now).unwrap();
        for (text, timestamp) in [
            ("expired", now - 4 * day),
            ("boundary", now - 3 * day),
            ("recent", now - day),
        ] {
            futures::executor::block_on(store.save_with_policy(
                record(text, timestamp),
                &settings_with_history(true, u16::MAX),
                LifecycleFence::new(),
            ))
            .unwrap();
        }

        futures::executor::block_on(
            store.enforce_policy(&settings_with_retention(3), LifecycleFence::new()),
        )
        .unwrap();
        let listed = futures::executor::block_on(store.list()).unwrap();

        assert_eq!(
            listed
                .iter()
                .map(|record| record.final_text.as_str())
                .collect::<Vec<_>>(),
            vec!["boundary", "recent"]
        );
    }

    #[test]
    fn save_with_policy_evicts_expired_rows_in_the_same_commit() {
        let day = HISTORY_DAY_MS;
        let now = 20 * day;
        let store = FileHistoryStore::new_with_now(temp_path("retention-save"), now).unwrap();
        save(&store, record("old", now - 5 * day)).unwrap();

        futures::executor::block_on(store.save_with_policy(
            record("new", now),
            &settings_with_retention(3),
            LifecycleFence::new(),
        ))
        .unwrap();
        let listed = futures::executor::block_on(store.list()).unwrap();

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].final_text, "new");
    }

    #[test]
    fn save_with_policy_commits_the_new_record_and_limit_atomically() {
        let store = FileHistoryStore::new(temp_path("save-policy-atomic")).unwrap();
        save(&store, record("oldest", 10)).unwrap();
        save(&store, record("middle", 20)).unwrap();

        futures::executor::block_on(store.save_with_policy(
            record("newest", 30),
            &settings_with_history(true, 2),
            LifecycleFence::new(),
        ))
        .unwrap();

        let listed = futures::executor::block_on(store.list()).unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|record| (record.final_text.as_str(), record.created_at.get()))
                .collect::<Vec<_>>(),
            vec![("middle", 20), ("newest", 30)]
        );
    }

    #[test]
    fn save_is_idempotent_for_the_same_record_and_rejects_identity_conflicts() {
        let store = FileHistoryStore::new(temp_path("record-idempotency")).unwrap();
        let original = record("原文", 1);

        save(&store, original.clone()).unwrap();
        save(&store, original.clone()).unwrap();
        let conflict = HistoryRecord {
            delivery_id: original.delivery_id,
            final_text: "不同正文".to_owned(),
            created_at: original.created_at,
        };
        let error = save(&store, conflict).expect_err("same identity cannot be rebound");

        assert_eq!(error.code, "history.record_id_conflict");
        assert_eq!(futures::executor::block_on(store.list()).unwrap().len(), 1);
    }

    #[test]
    fn invalidated_lifecycle_prevents_save_and_policy_commits() {
        let store = FileHistoryStore::new(temp_path("lifecycle")).unwrap();
        let invalid_save = LifecycleFence::new();
        invalid_save.invalidate();
        let save_error = futures::executor::block_on(store.save_with_policy(
            record("x", 1),
            &settings_with_history(true, 10),
            invalid_save,
        ))
        .expect_err("invalidated Session cannot save history");
        assert_eq!(save_error.code, "history.lifecycle_invalidated");
        assert!(
            futures::executor::block_on(store.list())
                .unwrap()
                .is_empty()
        );

        save(&store, record("old", 1)).unwrap();
        save(&store, record("new", 2)).unwrap();
        let invalid_policy = LifecycleFence::new();
        invalid_policy.invalidate();
        let policy_error = futures::executor::block_on(
            store.enforce_policy(&settings_with_history(true, 1), invalid_policy),
        )
        .expect_err("invalidated Session cannot commit policy cleanup");
        assert_eq!(policy_error.code, "history.lifecycle_invalidated");
        assert_eq!(futures::executor::block_on(store.list()).unwrap().len(), 2);
    }

    #[test]
    fn invalidated_atomic_save_preserves_existing_records_without_partial_trim() {
        let store = FileHistoryStore::new(temp_path("lifecycle-atomic-save")).unwrap();
        save(&store, record("old", 1)).unwrap();
        save(&store, record("current", 2)).unwrap();

        let invalid = LifecycleFence::new();
        invalid.invalidate();
        let error = futures::executor::block_on(store.save_with_policy(
            record("must-not-commit", 3),
            &settings_with_history(true, 1),
            invalid,
        ))
        .expect_err("invalidated atomic save cannot append or trim");

        assert_eq!(error.code, "history.lifecycle_invalidated");
        let listed = futures::executor::block_on(store.list()).unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|record| record.final_text.as_str())
                .collect::<Vec<_>>(),
            vec!["old", "current"]
        );
    }
}
