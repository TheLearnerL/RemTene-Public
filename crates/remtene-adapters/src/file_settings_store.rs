//! File-backed settings store.
//!
//! Persists the user settings snapshot as a JSON file. Uses optimistic
//! concurrency: `replace` only succeeds when the caller's `expected_version`
//! matches the currently stored version, then writes back with an incremented
//! version. The file is rewritten atomically (temp file + rename) so a crash
//! mid-write cannot corrupt existing settings.
//!
//! The domain `SettingsSnapshot` intentionally does not derive `Serialize`, so
//! this adapter owns a private DTO and converts explicitly. This keeps the
//! on-disk format an adapter concern rather than a domain contract.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use remtene_application::ports::{PortError, PortFuture, SettingsStore};
use remtene_domain::{
    AsrPreference, HistoryPolicy, LlmNonSecretSettings, ProcessingMode, RecordingMode,
    RecordingShortcut, SettingsSnapshot, SettingsSnapshotInput,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const SETTINGS_SCHEMA_VERSION: u16 = 4;
const MAX_SETTINGS_FILE_BYTES: usize = 256 * 1024;

/// File-backed settings store with optimistic-concurrency writes.
pub struct FileSettingsStore {
    path: PathBuf,
    // Serializes read-modify-write cycles so concurrent replaces cannot race.
    guard: Mutex<()>,
    // Fallback defaults used when the file does not exist yet.
    defaults: SettingsSnapshotInput,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecordingModeDto {
    Toggle,
    PushToTalk,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProcessingModeDto {
    Raw,
    Faithful,
    Structured,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AsrPreferenceDto {
    QwenFirst,
    WhisperOnly,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryPolicyDto {
    enabled: bool,
    limit: u16,
    retention_days: Option<u16>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LlmNonSecretSettingsDto {
    base_url: String,
    model: String,
}

/// Current on-disk settings representation. Owned by this adapter, not a domain contract.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsDtoCurrent {
    schema_version: u16,
    version: u64,
    recording_mode: RecordingModeDto,
    max_recording_duration_ms: u64,
    recording_shortcut: Option<String>,
    processing_mode: ProcessingModeDto,
    asr_preference: AsrPreferenceDto,
    llm: Option<LlmNonSecretSettingsDto>,
    read_selected_text: bool,
    clipboard_bridge_allowed: bool,
    auto_copy_result: bool,
    local_diagnostics_enabled: bool,
    history_policy: HistoryPolicyDto,
}

/// V3 已有快捷键，但还没有本地诊断开关；迁移时采用当前产品默认值。
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsDtoV3 {
    #[serde(rename = "schema_version")]
    _schema_version: u16,
    version: u64,
    recording_mode: RecordingModeDto,
    max_recording_duration_ms: u64,
    recording_shortcut: Option<String>,
    processing_mode: ProcessingModeDto,
    asr_preference: AsrPreferenceDto,
    llm: Option<LlmNonSecretSettingsDto>,
    read_selected_text: bool,
    clipboard_bridge_allowed: bool,
    auto_copy_result: bool,
    history_policy: HistoryPolicyDto,
}

/// V2 没有全局快捷键字段；读取后会立即迁移到当前 Schema，保持首次安装未绑定。
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsDtoV2 {
    #[serde(rename = "schema_version")]
    _schema_version: u16,
    version: u64,
    recording_mode: RecordingModeDto,
    max_recording_duration_ms: u64,
    processing_mode: ProcessingModeDto,
    asr_preference: AsrPreferenceDto,
    llm: Option<LlmNonSecretSettingsDto>,
    read_selected_text: bool,
    clipboard_bridge_allowed: bool,
    auto_copy_result: bool,
    history_policy: HistoryPolicyDto,
}

/// Legacy format written before `schema_version` and Base URL existed.
///
/// Its LLM fields are intentionally read only to recognize the exact old
/// format. They are never promoted into a usable route because the old file
/// does not contain the Base URL needed to prove where a request would go.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySettingsDtoV1 {
    version: u64,
    recording_mode: RecordingModeDto,
    max_recording_duration_ms: u64,
    processing_mode: ProcessingModeDto,
    asr_preference: AsrPreferenceDto,
    #[serde(rename = "llm_provider_ref")]
    _llm_provider_ref: Option<String>,
    #[serde(rename = "llm_model")]
    _llm_model: Option<String>,
    #[serde(rename = "llm_configured")]
    _llm_configured: bool,
    read_selected_text: bool,
    clipboard_bridge_allowed: bool,
    auto_copy_result: bool,
    history_policy: HistoryPolicyDto,
}

impl SettingsDtoCurrent {
    /// Convert a domain snapshot into the on-disk DTO.
    fn from_snapshot(s: &SettingsSnapshot) -> Self {
        let recording_mode = match s.recording_mode() {
            RecordingMode::Toggle => RecordingModeDto::Toggle,
            RecordingMode::PushToTalk => RecordingModeDto::PushToTalk,
        };
        let processing_mode = match s.processing_mode() {
            ProcessingMode::Raw => ProcessingModeDto::Raw,
            ProcessingMode::Faithful => ProcessingModeDto::Faithful,
            ProcessingMode::Structured => ProcessingModeDto::Structured,
        };
        let asr_preference = match s.asr_preference() {
            AsrPreference::Qwen => AsrPreferenceDto::QwenFirst,
            AsrPreference::Whisper => AsrPreferenceDto::WhisperOnly,
        };
        let policy = s.history_policy();
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            version: s.version(),
            recording_mode,
            max_recording_duration_ms: s.max_recording_duration().as_millis() as u64,
            recording_shortcut: s
                .recording_shortcut()
                .map(|shortcut| shortcut.as_str().to_owned()),
            processing_mode,
            asr_preference,
            llm: s.llm().map(|llm| LlmNonSecretSettingsDto {
                base_url: llm.base_url().to_owned(),
                model: llm.model().to_owned(),
            }),
            read_selected_text: s.read_selected_text(),
            clipboard_bridge_allowed: s.clipboard_bridge_allowed(),
            auto_copy_result: s.auto_copy_result(),
            local_diagnostics_enabled: s.local_diagnostics_enabled(),
            history_policy: HistoryPolicyDto {
                enabled: policy.enabled,
                limit: policy.limit,
                retention_days: policy.retention_days,
            },
        }
    }

    /// Convert the DTO into a validated domain snapshot input.
    fn into_input(self) -> Result<SettingsSnapshotInput, remtene_domain::SettingsValidationError> {
        let recording_mode = match self.recording_mode {
            RecordingModeDto::Toggle => RecordingMode::Toggle,
            RecordingModeDto::PushToTalk => RecordingMode::PushToTalk,
        };
        let processing_mode = match self.processing_mode {
            ProcessingModeDto::Raw => ProcessingMode::Raw,
            ProcessingModeDto::Faithful => ProcessingMode::Faithful,
            ProcessingModeDto::Structured => ProcessingMode::Structured,
        };
        let asr_preference = match self.asr_preference {
            AsrPreferenceDto::QwenFirst => AsrPreference::Qwen,
            AsrPreferenceDto::WhisperOnly => AsrPreference::Whisper,
        };
        let llm = self
            .llm
            .map(|llm| LlmNonSecretSettings::new(llm.base_url, llm.model))
            .transpose()?;
        let recording_shortcut = self
            .recording_shortcut
            .map(RecordingShortcut::new)
            .transpose()?;
        Ok(SettingsSnapshotInput {
            version: self.version,
            recording_mode,
            max_recording_duration: Duration::from_millis(self.max_recording_duration_ms),
            recording_shortcut,
            processing_mode,
            asr_preference,
            llm,
            read_selected_text: self.read_selected_text,
            clipboard_bridge_allowed: self.clipboard_bridge_allowed,
            auto_copy_result: self.auto_copy_result,
            local_diagnostics_enabled: self.local_diagnostics_enabled,
            history_policy: HistoryPolicy {
                enabled: self.history_policy.enabled,
                limit: self.history_policy.limit,
                retention_days: self.history_policy.retention_days,
            },
        })
    }
}

impl SettingsDtoV3 {
    fn into_input(self) -> Result<SettingsSnapshotInput, remtene_domain::SettingsValidationError> {
        SettingsDtoCurrent {
            schema_version: SETTINGS_SCHEMA_VERSION,
            version: self.version,
            recording_mode: self.recording_mode,
            max_recording_duration_ms: self.max_recording_duration_ms,
            recording_shortcut: self.recording_shortcut,
            processing_mode: self.processing_mode,
            asr_preference: self.asr_preference,
            llm: self.llm,
            read_selected_text: self.read_selected_text,
            clipboard_bridge_allowed: self.clipboard_bridge_allowed,
            auto_copy_result: self.auto_copy_result,
            local_diagnostics_enabled: true,
            history_policy: self.history_policy,
        }
        .into_input()
    }
}

impl SettingsDtoV2 {
    fn into_input(self) -> Result<SettingsSnapshotInput, remtene_domain::SettingsValidationError> {
        let recording_mode = match self.recording_mode {
            RecordingModeDto::Toggle => RecordingMode::Toggle,
            RecordingModeDto::PushToTalk => RecordingMode::PushToTalk,
        };
        let processing_mode = match self.processing_mode {
            ProcessingModeDto::Raw => ProcessingMode::Raw,
            ProcessingModeDto::Faithful => ProcessingMode::Faithful,
            ProcessingModeDto::Structured => ProcessingMode::Structured,
        };
        let asr_preference = match self.asr_preference {
            AsrPreferenceDto::QwenFirst => AsrPreference::Qwen,
            AsrPreferenceDto::WhisperOnly => AsrPreference::Whisper,
        };
        let llm = self
            .llm
            .map(|llm| LlmNonSecretSettings::new(llm.base_url, llm.model))
            .transpose()?;
        Ok(SettingsSnapshotInput {
            version: self.version,
            recording_mode,
            max_recording_duration: Duration::from_millis(self.max_recording_duration_ms),
            recording_shortcut: None,
            processing_mode,
            asr_preference,
            llm,
            read_selected_text: self.read_selected_text,
            clipboard_bridge_allowed: self.clipboard_bridge_allowed,
            auto_copy_result: self.auto_copy_result,
            local_diagnostics_enabled: true,
            history_policy: HistoryPolicy {
                enabled: self.history_policy.enabled,
                limit: self.history_policy.limit,
                retention_days: self.history_policy.retention_days,
            },
        })
    }
}

impl LegacySettingsDtoV1 {
    fn into_input(self) -> SettingsSnapshotInput {
        let recording_mode = match self.recording_mode {
            RecordingModeDto::Toggle => RecordingMode::Toggle,
            RecordingModeDto::PushToTalk => RecordingMode::PushToTalk,
        };
        let processing_mode = match self.processing_mode {
            ProcessingModeDto::Raw => ProcessingMode::Raw,
            ProcessingModeDto::Faithful => ProcessingMode::Faithful,
            ProcessingModeDto::Structured => ProcessingMode::Structured,
        };
        let asr_preference = match self.asr_preference {
            AsrPreferenceDto::QwenFirst => AsrPreference::Qwen,
            AsrPreferenceDto::WhisperOnly => AsrPreference::Whisper,
        };

        SettingsSnapshotInput {
            version: self.version,
            recording_mode,
            max_recording_duration: Duration::from_millis(self.max_recording_duration_ms),
            recording_shortcut: None,
            processing_mode,
            asr_preference,
            llm: None,
            read_selected_text: self.read_selected_text,
            clipboard_bridge_allowed: self.clipboard_bridge_allowed,
            auto_copy_result: self.auto_copy_result,
            local_diagnostics_enabled: true,
            history_policy: HistoryPolicy {
                enabled: self.history_policy.enabled,
                limit: self.history_policy.limit,
                retention_days: self.history_policy.retention_days,
            },
        }
    }
}

struct DecodedSnapshot {
    snapshot: SettingsSnapshot,
    requires_migration: bool,
}

impl FileSettingsStore {
    /// Create a store backed by `path`, seeding reads with `defaults` when the
    /// file does not exist yet. `defaults` must itself be a valid snapshot input.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, defaults: SettingsSnapshotInput) -> Self {
        Self {
            path: path.into(),
            guard: Mutex::new(()),
            defaults,
        }
    }

    /// Read and validate the current snapshot, or the seeded defaults when the
    /// file is absent.
    fn read_snapshot(&self) -> Result<DecodedSnapshot, PortError> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let snapshot = SettingsSnapshot::new(self.defaults.clone())
                    .map_err(|e| invalid_error(&format!("default settings invalid: {e}")))?;
                return Ok(DecodedSnapshot {
                    snapshot,
                    requires_migration: false,
                });
            }
            Err(e) => return Err(io_error(&format!("read settings: {e}"))),
        };
        let mut bytes = Vec::with_capacity(MAX_SETTINGS_FILE_BYTES.min(8 * 1024));
        file.take((MAX_SETTINGS_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| io_error(&format!("read settings: {e}")))?;
        if bytes.len() > MAX_SETTINGS_FILE_BYTES {
            return Err(file_too_large_error());
        }

        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| decode_error(&format!("parse settings: {e}")))?;
        match value.get("schema_version") {
            None => {
                let legacy: LegacySettingsDtoV1 = serde_json::from_value(value)
                    .map_err(|e| decode_error(&format!("parse legacy settings: {e}")))?;
                let snapshot = SettingsSnapshot::new(legacy.into_input())
                    .map_err(|e| decode_error(&format!("legacy settings invalid: {e}")))?;
                Ok(DecodedSnapshot {
                    snapshot,
                    requires_migration: true,
                })
            }
            Some(Value::Number(version))
                if version.as_u64() == Some(u64::from(SETTINGS_SCHEMA_VERSION)) =>
            {
                let dto: SettingsDtoCurrent = serde_json::from_value(value)
                    .map_err(|e| decode_error(&format!("parse current settings: {e}")))?;
                let input = dto
                    .into_input()
                    .map_err(|e| decode_error(&format!("stored settings invalid: {e}")))?;
                let snapshot = SettingsSnapshot::new(input)
                    .map_err(|e| decode_error(&format!("stored settings invalid: {e}")))?;
                Ok(DecodedSnapshot {
                    snapshot,
                    requires_migration: false,
                })
            }
            Some(Value::Number(version)) if version.as_u64() == Some(3) => {
                let dto: SettingsDtoV3 = serde_json::from_value(value)
                    .map_err(|e| decode_error(&format!("parse settings v3: {e}")))?;
                let input = dto
                    .into_input()
                    .map_err(|e| decode_error(&format!("stored v3 settings invalid: {e}")))?;
                let snapshot = SettingsSnapshot::new(input)
                    .map_err(|e| decode_error(&format!("stored v3 settings invalid: {e}")))?;
                Ok(DecodedSnapshot {
                    snapshot,
                    requires_migration: true,
                })
            }
            Some(Value::Number(version)) if version.as_u64() == Some(2) => {
                let dto: SettingsDtoV2 = serde_json::from_value(value)
                    .map_err(|e| decode_error(&format!("parse settings v2: {e}")))?;
                let input = dto
                    .into_input()
                    .map_err(|e| decode_error(&format!("stored v2 settings invalid: {e}")))?;
                let snapshot = SettingsSnapshot::new(input)
                    .map_err(|e| decode_error(&format!("stored v2 settings invalid: {e}")))?;
                Ok(DecodedSnapshot {
                    snapshot,
                    requires_migration: true,
                })
            }
            Some(_) => Err(unsupported_schema_error()),
        }
    }

    /// Atomically write the snapshot: serialize to a temp file in the same
    /// directory, then rename over the target.
    fn write_snapshot(&self, snapshot: &SettingsSnapshot) -> Result<(), PortError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| io_error(&format!("create settings dir: {e}")))?;
        }
        let dto = SettingsDtoCurrent::from_snapshot(snapshot);
        let bytes = serde_json::to_vec_pretty(&dto)
            .map_err(|e| io_error(&format!("encode settings: {e}")))?;

        let tmp = temp_sibling(&self.path);
        let mut file =
            File::create(&tmp).map_err(|e| io_error(&format!("create temp settings: {e}")))?;
        file.write_all(&bytes)
            .map_err(|e| io_error(&format!("write temp settings: {e}")))?;
        file.sync_all()
            .map_err(|e| io_error(&format!("sync temp settings: {e}")))?;
        drop(file);
        fs::rename(&tmp, &self.path).map_err(|e| io_error(&format!("commit settings: {e}")))?;
        sync_parent_directory(&self.path)?;
        Ok(())
    }

    /// Read while holding `guard`, eagerly committing older schemas to the current
    /// migration before exposing the snapshot to callers.
    fn read_and_migrate_locked(&self) -> Result<SettingsSnapshot, PortError> {
        let decoded = self.read_snapshot()?;
        if decoded.requires_migration {
            self.write_snapshot(&decoded.snapshot)?;
        }
        Ok(decoded.snapshot)
    }

    /// Acquires a cross-process advisory lock for the complete read/CAS/write
    /// cycle. The in-memory mutex only coordinates callers sharing this exact
    /// instance; macOS can still launch a second application process.
    fn acquire_process_lock(&self) -> Result<File, PortError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| io_error(&format!("create settings dir: {error}")))?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_sibling(&self.path))
            .map_err(|error| io_error(&format!("open settings lock: {error}")))?;
        lock.lock()
            .map_err(|error| io_error(&format!("lock settings: {error}")))?;
        Ok(lock)
    }
}

impl SettingsStore for FileSettingsStore {
    fn load(&self) -> PortFuture<'_, Result<SettingsSnapshot, PortError>> {
        Box::pin(async move {
            let _lock = self.guard.lock().map_err(|_| lock_error())?;
            let _process_lock = self.acquire_process_lock()?;
            self.read_and_migrate_locked()
        })
    }

    fn replace(
        &self,
        expected_version: u64,
        settings: SettingsSnapshot,
    ) -> PortFuture<'_, Result<SettingsSnapshot, PortError>> {
        Box::pin(async move {
            let _lock = self.guard.lock().map_err(|_| lock_error())?;
            let _process_lock = self.acquire_process_lock()?;
            let current = self.read_and_migrate_locked()?;
            if current.version() != expected_version {
                return Err(version_conflict_error(current.version(), expected_version));
            }

            // Rebuild the incoming settings with an incremented version.
            let mut input = settings.into_input();
            input.version = expected_version
                .checked_add(1)
                .ok_or_else(version_overflow_error)?;
            let next = SettingsSnapshot::new(input)
                .map_err(|e| invalid_error(&format!("incoming settings invalid: {e}")))?;

            self.write_snapshot(&next)?;
            Ok(next)
        })
    }
}

fn temp_sibling(path: &Path) -> PathBuf {
    sibling_with_suffix(path, ".tmp")
}

fn lock_sibling(path: &Path) -> PathBuf {
    sibling_with_suffix(path, ".lock")
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    match path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<(), PortError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io_error(&format!("sync settings directory: {error}")))
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> Result<(), PortError> {
    Ok(())
}

fn io_error(detail: &str) -> PortError {
    PortError {
        code: "settings.io".to_string(),
        safe_message_key: "errors.settings.io".to_string(),
        retryable: true,
    }
    .tap_detail(detail)
}

fn decode_error(detail: &str) -> PortError {
    PortError {
        code: "settings.decode".to_string(),
        safe_message_key: "errors.settings.decode".to_string(),
        retryable: false,
    }
    .tap_detail(detail)
}

fn invalid_error(detail: &str) -> PortError {
    PortError {
        code: "settings.invalid".to_string(),
        safe_message_key: "errors.settings.invalid".to_string(),
        retryable: false,
    }
    .tap_detail(detail)
}

fn version_conflict_error(current: u64, expected: u64) -> PortError {
    let _ = (current, expected);
    PortError {
        code: "settings.version_conflict".to_string(),
        safe_message_key: "errors.settings.version_conflict".to_string(),
        retryable: false,
    }
}

fn version_overflow_error() -> PortError {
    PortError {
        code: "settings.version_overflow".to_string(),
        safe_message_key: "errors.settings.version_overflow".to_string(),
        retryable: false,
    }
}

fn unsupported_schema_error() -> PortError {
    PortError {
        code: "settings.unsupported_schema".to_string(),
        safe_message_key: "errors.settings.unsupported_schema".to_string(),
        retryable: false,
    }
}

fn file_too_large_error() -> PortError {
    PortError {
        code: "settings.file_too_large".to_string(),
        safe_message_key: "errors.settings.file_too_large".to_string(),
        retryable: false,
    }
}

fn lock_error() -> PortError {
    PortError {
        code: "settings.lock_poisoned".to_string(),
        safe_message_key: "errors.settings.lock".to_string(),
        retryable: false,
    }
}

/// Small helper so error constructors can log detail to stderr without
/// widening `PortError`'s public shape.
trait TapDetail {
    fn tap_detail(self, detail: &str) -> Self;
}

impl TapDetail for PortError {
    fn tap_detail(self, detail: &str) -> Self {
        eprintln!("[settings] {}: {}", self.code, detail);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("remtene-settings-{name}-{nanos}/settings.json"))
    }

    fn default_input() -> SettingsSnapshotInput {
        SettingsSnapshotInput {
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
            history_policy: HistoryPolicy {
                enabled: false,
                limit: 10,
                retention_days: None,
            },
        }
    }

    fn write_fixture(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn v2_fixture(llm: Value) -> Vec<u8> {
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 2,
            "version": 4,
            "recording_mode": "toggle",
            "max_recording_duration_ms": 60_000,
            "processing_mode": "faithful",
            "asr_preference": "qwen_first",
            "llm": llm,
            "read_selected_text": false,
            "clipboard_bridge_allowed": false,
            "auto_copy_result": false,
            "history_policy": {
                "enabled": true,
                "limit": 10,
                "retention_days": null
            }
        }))
        .unwrap()
    }

    fn v3_fixture() -> Vec<u8> {
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 3,
            "version": 8,
            "recording_mode": "push_to_talk",
            "max_recording_duration_ms": 300_000,
            "recording_shortcut": "CommandOrControl+Shift+KeyR",
            "processing_mode": "structured",
            "asr_preference": "qwen_first",
            "llm": null,
            "read_selected_text": true,
            "clipboard_bridge_allowed": false,
            "auto_copy_result": true,
            "history_policy": {
                "enabled": true,
                "limit": 25,
                "retention_days": 10
            }
        }))
        .unwrap()
    }

    #[test]
    fn load_returns_defaults_when_file_absent() {
        let store = FileSettingsStore::new(temp_path("absent"), default_input());
        let loaded = futures::executor::block_on(store.load()).unwrap();
        assert_eq!(loaded.version(), 0);
        assert_eq!(loaded.recording_mode(), RecordingMode::Toggle);
        assert_eq!(loaded.processing_mode(), ProcessingMode::Raw);
    }

    #[test]
    fn replace_increments_version_and_persists() {
        let path = temp_path("replace");
        let store = FileSettingsStore::new(path.clone(), default_input());

        // Build a new snapshot switching to PushToTalk.
        let mut input = default_input();
        input.recording_mode = RecordingMode::PushToTalk;
        let incoming = SettingsSnapshot::new(input).unwrap();

        let saved = futures::executor::block_on(store.replace(0, incoming)).unwrap();
        assert_eq!(saved.version(), 1);
        assert_eq!(saved.recording_mode(), RecordingMode::PushToTalk);

        // A fresh instance reads back the persisted value.
        let reopened = FileSettingsStore::new(path, default_input());
        let loaded = futures::executor::block_on(reopened.load()).unwrap();
        assert_eq!(loaded.version(), 1);
        assert_eq!(loaded.recording_mode(), RecordingMode::PushToTalk);
    }

    #[test]
    fn replace_rejects_stale_version() {
        let store = FileSettingsStore::new(temp_path("stale"), default_input());
        let incoming = SettingsSnapshot::new(default_input()).unwrap();

        // Stored version is 0; expecting 5 must conflict.
        let result = futures::executor::block_on(store.replace(5, incoming));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, "settings.version_conflict");
    }

    #[test]
    fn sequential_replaces_bump_version_each_time() {
        let path = temp_path("seq");
        let store = FileSettingsStore::new(path, default_input());

        let s1 = SettingsSnapshot::new(default_input()).unwrap();
        let r1 = futures::executor::block_on(store.replace(0, s1)).unwrap();
        assert_eq!(r1.version(), 1);

        let s2 = SettingsSnapshot::new(default_input()).unwrap();
        let r2 = futures::executor::block_on(store.replace(1, s2)).unwrap();
        assert_eq!(r2.version(), 2);
    }

    #[test]
    fn independent_instances_serialize_concurrent_compare_and_swap() {
        let path = temp_path("cross-instance-cas");
        let first = FileSettingsStore::new(path.clone(), default_input());
        let second = FileSettingsStore::new(path.clone(), default_input());
        let start = Arc::new(Barrier::new(3));

        let handles = [first, second]
            .into_iter()
            .enumerate()
            .map(|(index, store)| {
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    let mut input = default_input();
                    input.auto_copy_result = index == 0;
                    let incoming = SettingsSnapshot::new(input).unwrap();
                    start.wait();
                    futures::executor::block_on(store.replace(0, incoming))
                })
            })
            .collect::<Vec<_>>();

        start.wait();
        let results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let errors = results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .collect::<Vec<_>>();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, "settings.version_conflict");

        let bytes = fs::read(&path).unwrap();
        let persisted: SettingsDtoCurrent = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(persisted.schema_version, SETTINGS_SCHEMA_VERSION);
        assert_eq!(persisted.version, 1);
        let snapshot = SettingsSnapshot::new(persisted.into_input().unwrap()).unwrap();
        assert_eq!(snapshot.version(), 1);
    }

    #[test]
    fn v4_round_trip_persists_only_non_secret_llm_settings_and_shortcut() {
        let path = temp_path("llm-v4");
        let store = FileSettingsStore::new(path.clone(), default_input());
        let mut input = default_input();
        input.recording_shortcut =
            Some(RecordingShortcut::new("CommandOrControl+Shift+KeyR").unwrap());
        input.llm =
            Some(LlmNonSecretSettings::new("https://gateway.example/v1", "model-name").unwrap());

        let stored =
            futures::executor::block_on(store.replace(0, SettingsSnapshot::new(input).unwrap()))
                .unwrap();
        assert_eq!(
            stored.llm().map(LlmNonSecretSettings::base_url),
            Some("https://gateway.example/v1")
        );

        let json: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(json["schema_version"], SETTINGS_SCHEMA_VERSION);
        assert_eq!(json["version"], 1);
        assert_eq!(json["recording_shortcut"], "CommandOrControl+Shift+KeyR");
        assert_eq!(json["llm"]["model"], "model-name");
        for forbidden in [
            "llm_provider_ref",
            "llm_configured",
            "api_key",
            "secret_value",
        ] {
            assert!(
                json.get(forbidden).is_none(),
                "ordinary settings must not persist {forbidden}"
            );
        }

        let reopened = FileSettingsStore::new(path, default_input());
        let loaded = futures::executor::block_on(reopened.load()).unwrap();
        assert_eq!(loaded, stored);
    }

    #[test]
    fn v2_settings_are_migrated_to_v4_without_inventing_a_shortcut() {
        let path = temp_path("v2-to-v4");
        write_fixture(&path, &v2_fixture(Value::Null));

        let store = FileSettingsStore::new(path.clone(), default_input());
        let loaded = futures::executor::block_on(store.load()).unwrap();

        assert_eq!(loaded.version(), 4);
        assert!(loaded.recording_shortcut().is_none());
        let migrated: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(migrated["schema_version"], SETTINGS_SCHEMA_VERSION);
        assert!(migrated["recording_shortcut"].is_null());
    }

    #[test]
    fn v3_settings_are_migrated_to_v4_with_local_diagnostics_enabled() {
        let path = temp_path("v3-to-v4");
        write_fixture(&path, &v3_fixture());

        let store = FileSettingsStore::new(path.clone(), default_input());
        let loaded = futures::executor::block_on(store.load()).unwrap();

        assert_eq!(loaded.version(), 8);
        assert_eq!(loaded.recording_mode(), RecordingMode::PushToTalk);
        assert_eq!(loaded.history_policy().retention_days, Some(10));
        assert!(loaded.auto_copy_result());
        assert!(loaded.local_diagnostics_enabled());
        let migrated: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(migrated["schema_version"], SETTINGS_SCHEMA_VERSION);
        assert_eq!(migrated["local_diagnostics_enabled"], true);
    }

    #[test]
    fn legacy_settings_are_migrated_and_legacy_llm_is_disabled() {
        let path = temp_path("legacy");
        let legacy = serde_json::to_vec_pretty(&serde_json::json!({
            "version": 7,
            "recording_mode": "push_to_talk",
            "max_recording_duration_ms": 600_000,
            "processing_mode": "structured",
            "asr_preference": "whisper_only",
            "llm_provider_ref": "primary",
            "llm_model": "legacy-model",
            "llm_configured": true,
            "read_selected_text": true,
            "clipboard_bridge_allowed": true,
            "auto_copy_result": true,
            "history_policy": {
                "enabled": true,
                "limit": 20,
                "retention_days": 30
            }
        }))
        .unwrap();
        write_fixture(&path, &legacy);

        let store = FileSettingsStore::new(path.clone(), default_input());
        let loaded = futures::executor::block_on(store.load()).unwrap();

        assert_eq!(loaded.version(), 7);
        assert_eq!(loaded.recording_mode(), RecordingMode::PushToTalk);
        assert_eq!(loaded.processing_mode(), ProcessingMode::Structured);
        assert_eq!(loaded.asr_preference(), AsrPreference::Whisper);
        assert!(loaded.llm().is_none());
        assert!(loaded.clipboard_bridge_allowed());

        let migrated: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(migrated["schema_version"], SETTINGS_SCHEMA_VERSION);
        assert_eq!(migrated["version"], 7);
        assert!(migrated["llm"].is_null());
        assert!(migrated.get("llm_configured").is_none());
        assert!(migrated.get("llm_provider_ref").is_none());
    }

    #[test]
    fn unknown_schema_is_rejected_without_rewriting_the_file() {
        let path = temp_path("future-schema");
        let original = br#"{"schema_version":999,"version":1}"#;
        write_fixture(&path, original);
        let store = FileSettingsStore::new(path.clone(), default_input());

        let error = futures::executor::block_on(store.load()).unwrap_err();

        assert_eq!(error.code, "settings.unsupported_schema");
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn malformed_json_is_rejected_without_rewriting_the_file() {
        let path = temp_path("malformed");
        let original = br#"{"schema_version":2,"version":"#;
        write_fixture(&path, original);
        let store = FileSettingsStore::new(path.clone(), default_input());

        let error = futures::executor::block_on(store.load()).unwrap_err();

        assert_eq!(error.code, "settings.decode");
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn oversized_file_is_rejected_without_reading_or_rewriting_beyond_limit() {
        let path = temp_path("oversized");
        let original = vec![b'x'; MAX_SETTINGS_FILE_BYTES + 1];
        write_fixture(&path, &original);
        let store = FileSettingsStore::new(path.clone(), default_input());

        let error = futures::executor::block_on(store.load()).unwrap_err();

        assert_eq!(error.code, "settings.file_too_large");
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn partial_llm_config_is_rejected_without_rewriting_the_file() {
        let path = temp_path("partial-llm");
        let original = v2_fixture(serde_json::json!({
            "base_url": "https://gateway.example/v1"
        }));
        write_fixture(&path, &original);
        let store = FileSettingsStore::new(path.clone(), default_input());

        let error = futures::executor::block_on(store.load()).unwrap_err();

        assert_eq!(error.code, "settings.decode");
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn blank_llm_config_is_rejected_without_rewriting_the_file() {
        let path = temp_path("blank-llm");
        let original = v2_fixture(serde_json::json!({
            "base_url": " ",
            "model": "model"
        }));
        write_fixture(&path, &original);
        let store = FileSettingsStore::new(path.clone(), default_input());

        let error = futures::executor::block_on(store.load()).unwrap_err();

        assert_eq!(error.code, "settings.decode");
        assert_eq!(fs::read(path).unwrap(), original);
    }
}
