use std::{fmt, time::Duration};

use thiserror::Error;

const MAX_LLM_BASE_URL_BYTES: usize = 8 * 1024;
const MAX_LLM_MODEL_BYTES: usize = 512;
const MAX_RECORDING_SHORTCUT_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct TimestampMs(u64);

impl TimestampMs {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RecordingMode {
    #[default]
    Toggle,
    PushToTalk,
}

/// 用户选择的全局录音快捷键。
///
/// 领域层只保存跨平台、规范化后的绑定文本；具体键码解析与系统注册仍属于
/// Platform Adapter。`None` 表示未绑定，因此值对象自身不接受空字符串。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingShortcut(String);

impl RecordingShortcut {
    pub fn new(value: impl Into<String>) -> Result<Self, SettingsValidationError> {
        let value = value.into().trim().to_owned();
        if value.is_empty() {
            return Err(SettingsValidationError::EmptyRecordingShortcut);
        }
        if value.len() > MAX_RECORDING_SHORTCUT_BYTES {
            return Err(SettingsValidationError::RecordingShortcutTooLong);
        }
        if value.chars().any(char::is_control) {
            return Err(SettingsValidationError::RecordingShortcutContainsControl);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProcessingMode {
    Raw,
    #[default]
    Faithful,
    Structured,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntentDecision {
    Dictation,
    TextCommand,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AsrPreference {
    #[default]
    Qwen,
    Whisper,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AsrEngine {
    Qwen,
    Whisper,
}

impl AsrPreference {
    /// 返回用户明确选择、且不得自动回退的 ASR 引擎。
    #[must_use]
    pub const fn engine(self) -> AsrEngine {
        match self {
            Self::Qwen => AsrEngine::Qwen,
            Self::Whisper => AsrEngine::Whisper,
        }
    }
}

impl From<AsrEngine> for AsrPreference {
    fn from(engine: AsrEngine) -> Self {
        match engine {
            AsrEngine::Qwen => Self::Qwen,
            AsrEngine::Whisper => Self::Whisper,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryPolicy {
    pub enabled: bool,
    pub limit: u16,
    pub retention_days: Option<u16>,
}

impl Default for HistoryPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            limit: 10,
            retention_days: None,
        }
    }
}

/// V1 的单一全局 OpenAI-Compatible Provider 非秘密配置。
///
/// API Key 不属于普通设置；它只能由 `SecretStore` 管理。将 Base URL 与模型
/// 收进同一个值对象，保证设置快照不存在“只有 URL”或“只有模型”的部分状态。
#[derive(Clone, Eq, PartialEq)]
pub struct LlmNonSecretSettings {
    base_url: String,
    model: String,
}

impl fmt::Debug for LlmNonSecretSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmNonSecretSettings")
            .field("base_url", &"[REDACTED]")
            .field("model", &"[REDACTED]")
            .finish()
    }
}

impl LlmNonSecretSettings {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, SettingsValidationError> {
        let base_url = base_url.into().trim().to_owned();
        let model = model.into().trim().to_owned();
        if base_url.is_empty() {
            return Err(SettingsValidationError::EmptyLlmBaseUrl);
        }
        if base_url.len() > MAX_LLM_BASE_URL_BYTES {
            return Err(SettingsValidationError::LlmBaseUrlTooLong);
        }
        if base_url.chars().any(char::is_control) {
            return Err(SettingsValidationError::LlmBaseUrlContainsControl);
        }
        if model.is_empty() {
            return Err(SettingsValidationError::EmptyLlmModel);
        }
        if model.len() > MAX_LLM_MODEL_BYTES {
            return Err(SettingsValidationError::LlmModelTooLong);
        }
        if model.chars().any(char::is_control) {
            return Err(SettingsValidationError::LlmModelContainsControl);
        }

        Ok(Self { base_url, model })
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettingsSnapshotInput {
    pub version: u64,
    pub recording_mode: RecordingMode,
    pub max_recording_duration: Duration,
    pub recording_shortcut: Option<RecordingShortcut>,
    pub processing_mode: ProcessingMode,
    pub asr_preference: AsrPreference,
    pub llm: Option<LlmNonSecretSettings>,
    pub read_selected_text: bool,
    pub clipboard_bridge_allowed: bool,
    pub auto_copy_result: bool,
    pub local_diagnostics_enabled: bool,
    pub history_policy: HistoryPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettingsSnapshot(SettingsSnapshotInput);

impl SettingsSnapshot {
    pub fn new(input: SettingsSnapshotInput) -> Result<Self, SettingsValidationError> {
        if input.max_recording_duration.is_zero() {
            return Err(SettingsValidationError::ZeroRecordingDuration);
        }
        if input.history_policy.limit == 0 {
            return Err(SettingsValidationError::ZeroHistoryLimit);
        }
        if input.history_policy.retention_days == Some(0) {
            return Err(SettingsValidationError::ZeroHistoryRetention);
        }
        Ok(Self(input))
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.0.version
    }

    #[must_use]
    pub const fn recording_mode(&self) -> RecordingMode {
        self.0.recording_mode
    }

    #[must_use]
    pub const fn max_recording_duration(&self) -> Duration {
        self.0.max_recording_duration
    }

    #[must_use]
    pub fn recording_shortcut(&self) -> Option<&RecordingShortcut> {
        self.0.recording_shortcut.as_ref()
    }

    #[must_use]
    pub const fn processing_mode(&self) -> ProcessingMode {
        self.0.processing_mode
    }

    #[must_use]
    pub const fn asr_preference(&self) -> AsrPreference {
        self.0.asr_preference
    }

    #[must_use]
    pub fn llm(&self) -> Option<&LlmNonSecretSettings> {
        self.0.llm.as_ref()
    }

    #[must_use]
    pub const fn read_selected_text(&self) -> bool {
        self.0.read_selected_text
    }

    #[must_use]
    pub const fn clipboard_bridge_allowed(&self) -> bool {
        self.0.clipboard_bridge_allowed
    }

    #[must_use]
    pub const fn auto_copy_result(&self) -> bool {
        self.0.auto_copy_result
    }

    #[must_use]
    pub const fn local_diagnostics_enabled(&self) -> bool {
        self.0.local_diagnostics_enabled
    }

    #[must_use]
    pub const fn history_policy(&self) -> HistoryPolicy {
        self.0.history_policy
    }

    /// 拆回可修改的输入，用于「读—改一个字段—写」。
    ///
    /// 不变量不会因此松动：返回值必须重新经 [`SettingsSnapshot::new`] 验证
    /// 才能变回快照，所以任何改动都会再过一次校验。
    #[must_use]
    pub fn into_input(self) -> SettingsSnapshotInput {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SettingsValidationError {
    #[error("recording duration must be greater than zero")]
    ZeroRecordingDuration,
    #[error("recording shortcut must not be empty")]
    EmptyRecordingShortcut,
    #[error("recording shortcut is too long")]
    RecordingShortcutTooLong,
    #[error("recording shortcut must not contain control characters")]
    RecordingShortcutContainsControl,
    #[error("history limit must be greater than zero")]
    ZeroHistoryLimit,
    #[error("history retention must be absent or greater than zero")]
    ZeroHistoryRetention,
    #[error("LLM base URL must not be empty")]
    EmptyLlmBaseUrl,
    #[error("LLM base URL is too long")]
    LlmBaseUrlTooLong,
    #[error("LLM base URL must not contain control characters")]
    LlmBaseUrlContainsControl,
    #[error("LLM model must not be empty")]
    EmptyLlmModel,
    #[error("LLM model is too long")]
    LlmModelTooLong,
    #[error("LLM model must not contain control characters")]
    LlmModelContainsControl,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llm_non_secret_settings_are_trimmed_and_kept_as_one_value() {
        let settings =
            LlmNonSecretSettings::new(" https://example.test/v1/ ", " user-model ").unwrap();

        assert_eq!(settings.base_url(), "https://example.test/v1/");
        assert_eq!(settings.model(), "user-model");
    }

    #[test]
    fn llm_non_secret_settings_reject_missing_halves() {
        assert_eq!(
            LlmNonSecretSettings::new("   ", "model").unwrap_err(),
            SettingsValidationError::EmptyLlmBaseUrl
        );
        assert_eq!(
            LlmNonSecretSettings::new("https://example.test/v1", "\n").unwrap_err(),
            SettingsValidationError::EmptyLlmModel
        );
    }

    #[test]
    fn llm_non_secret_settings_reject_oversized_and_control_character_inputs() {
        assert_eq!(
            LlmNonSecretSettings::new(
                format!(
                    "https://example.test/{}",
                    "a".repeat(MAX_LLM_BASE_URL_BYTES)
                ),
                "model"
            )
            .unwrap_err(),
            SettingsValidationError::LlmBaseUrlTooLong
        );
        assert_eq!(
            LlmNonSecretSettings::new("https://example.test/\nv1", "model").unwrap_err(),
            SettingsValidationError::LlmBaseUrlContainsControl
        );
        assert_eq!(
            LlmNonSecretSettings::new(
                "https://example.test/v1",
                "m".repeat(MAX_LLM_MODEL_BYTES + 1)
            )
            .unwrap_err(),
            SettingsValidationError::LlmModelTooLong
        );
        assert_eq!(
            LlmNonSecretSettings::new("https://example.test/v1", "model\u{7f}").unwrap_err(),
            SettingsValidationError::LlmModelContainsControl
        );
    }

    #[test]
    fn llm_non_secret_settings_debug_redacts_routing_metadata() {
        let settings =
            LlmNonSecretSettings::new("https://private.example/v1", "private-model").unwrap();
        let debug = format!("{settings:?}");

        assert!(!debug.contains("private.example"));
        assert!(!debug.contains("private-model"));
        assert_eq!(
            debug,
            "LlmNonSecretSettings { base_url: \"[REDACTED]\", model: \"[REDACTED]\" }"
        );
    }

    #[test]
    fn recording_shortcut_is_trimmed_and_rejects_invalid_storage_values() {
        let shortcut = RecordingShortcut::new("  Command+Shift+KeyR  ").unwrap();
        assert_eq!(shortcut.as_str(), "Command+Shift+KeyR");
        assert_eq!(
            RecordingShortcut::new("   ").unwrap_err(),
            SettingsValidationError::EmptyRecordingShortcut
        );
        assert_eq!(
            RecordingShortcut::new("Command+\nKeyR").unwrap_err(),
            SettingsValidationError::RecordingShortcutContainsControl
        );
        assert_eq!(
            RecordingShortcut::new("x".repeat(MAX_RECORDING_SHORTCUT_BYTES + 1)).unwrap_err(),
            SettingsValidationError::RecordingShortcutTooLong
        );
    }

    #[test]
    fn history_policy_keeps_valid_values_while_disabled() {
        let mut input = SettingsSnapshotInput {
            version: 0,
            recording_mode: RecordingMode::Toggle,
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

        SettingsSnapshot::new(input.clone()).expect("disabled policy keeps its valid values");
        input.history_policy.limit = 0;
        assert_eq!(
            SettingsSnapshot::new(input.clone()).unwrap_err(),
            SettingsValidationError::ZeroHistoryLimit
        );
        input.history_policy.limit = 10;
        input.history_policy.retention_days = Some(0);
        assert_eq!(
            SettingsSnapshot::new(input).unwrap_err(),
            SettingsValidationError::ZeroHistoryRetention
        );
    }
}
