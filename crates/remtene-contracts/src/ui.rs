use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroize;

use crate::CONTRACT_VERSION;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Starting,
    Ready,
    Quitting,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MicrophonePermission {
    Unknown,
    NotDetermined,
    Granted,
    Denied,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemPermission {
    Unknown,
    NotDetermined,
    Granted,
    Denied,
    NotRequired,
    /// Trust is inherited from the launching terminal or IDE rather than owned
    /// by this app. The OS reports authorized, but the app is absent from System
    /// Settings and cross-process Accessibility writes fail. A development-time
    /// state: it means the app was started as a bare executable, not as a bundle.
    InheritedFromLauncher,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AsrReadiness {
    Discovering,
    QwenReady,
    WhisperReady,
    Unavailable,
}

/// Renderer 可选择的固定本地 ASR 模型；不允许传任意模型 ID 或文件路径。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalAsrModel {
    Qwen,
    Whisper,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelSummary {
    pub selected_model: LocalAsrModel,
    pub active_model_id: Option<String>,
    pub qwen_ready: bool,
    pub whisper_ready: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionUserState {
    Preparing,
    Recording,
    Processing,
    Completed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhaseView {
    Preparing,
    Recording,
    Recognizing,
    Processing,
    Delivering,
    Finalizing,
    Terminated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionPublicSnapshot {
    pub contract_version: u16,
    pub session_id: Uuid,
    pub user_state: SessionUserState,
    pub phase: SessionPhaseView,
    pub recording_elapsed_ms: Option<u64>,
    pub recording_limit_ms: Option<u64>,
    pub can_finish: bool,
    pub can_cancel: bool,
    pub status_code: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTerminalOutcomeView {
    Completed,
    Cancelled,
    Rejected,
    Failed,
}

/// Versioned, content-free terminal projection for one Session.
///
/// `error_code` is a stable Presentation code such as
/// `session.failed.asr`; it never contains a debug message, transcript,
/// target identity, provider response, or file path.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionTerminalEvent {
    pub contract_version: u16,
    pub session_id: Uuid,
    pub outcome: SessionTerminalOutcomeView,
    pub error_code: Option<String>,
}

/// Stable, content-free recovery surface selected by Application.
///
/// The enum values are the only cross-boundary error information. Exact
/// user-facing copy stays in the approved Presentation component.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum UserNotificationCode {
    #[serde(rename = "notification.permission_microphone")]
    MicrophonePermission,
    #[serde(rename = "notification.asr")]
    Asr,
    #[serde(rename = "notification.llm")]
    Llm,
    #[serde(rename = "notification.delivery")]
    Delivery,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserNotification {
    pub contract_version: u16,
    pub session_id: Uuid,
    pub code: UserNotificationCode,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ControlPanelNavigationTarget {
    #[serde(rename = "model.asr")]
    ModelAsr,
    #[serde(rename = "model.text_service")]
    ModelTextService,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlPanelNavigationEvent {
    pub contract_version: u16,
    pub target: ControlPanelNavigationTarget,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AppSnapshot {
    pub contract_version: u16,
    pub lifecycle_state: LifecycleState,
    pub active_session: Option<SessionPublicSnapshot>,
    pub microphone_permission: MicrophonePermission,
    pub accessibility_permission: SystemPermission,
    pub asr_readiness: AsrReadiness,
    pub llm_configured: bool,
    pub model_summary: ModelSummary,
    pub shortcut_configured: bool,
    pub autostart_enabled: bool,
}

/// Current operating-system autostart state.
///
/// Read failures are returned as command errors instead of being represented
/// as `enabled: false`, so Presentation never mistakes unavailable state for a
/// confirmed disabled login item.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AutostartStatusView {
    pub contract_version: u16,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetAutostartCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetAutostartResult {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub status: AutostartStatusView,
}

/// Versioned, read-only request for the complete V1 output history.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryQuery {
    pub contract_version: u16,
}

/// One Renderer-safe history row.
///
/// `record_id` is an opaque technical reference. It deliberately does not
/// expose the internal delivery field name or any delivery state.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryRecordView {
    pub record_id: Uuid,
    pub final_text: String,
    pub created_at: String,
}

impl fmt::Debug for HistoryRecordView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HistoryRecordView")
            .field("record_id", &self.record_id)
            .field("final_text", &"[REDACTED]")
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// Point-in-time read projection of output history.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryPage {
    pub contract_version: u16,
    pub records: Vec<HistoryRecordView>,
}

impl fmt::Debug for HistoryPage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HistoryPage")
            .field("contract_version", &self.contract_version)
            .field("records", &self.records)
            .finish()
    }
}

/// Explicit request to copy one current history row.
///
/// The command carries only an opaque identity. Final text is always resolved
/// inside Application from the current HistoryStore.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryCopyCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub record_id: Uuid,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryCopyResult {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub record_id: Uuid,
}

/// Explicit, confirmed request to irreversibly clear local final-text history.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryClearAllCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub acknowledge_data_loss: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryClearAllResult {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub cleared_count: u64,
}

impl AppSnapshot {
    #[must_use]
    pub const fn bootstrap() -> Self {
        Self {
            contract_version: CONTRACT_VERSION,
            lifecycle_state: LifecycleState::Starting,
            active_session: None,
            microphone_permission: MicrophonePermission::Unknown,
            accessibility_permission: SystemPermission::Unknown,
            asr_readiness: AsrReadiness::Discovering,
            llm_configured: false,
            model_summary: ModelSummary {
                selected_model: LocalAsrModel::Qwen,
                active_model_id: None,
                qwen_ready: false,
                whisper_ready: false,
            },
            shortcut_configured: false,
            autostart_enabled: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StartRecordingCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub session_id: Uuid,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionAccepted {
    pub contract_version: u16,
    pub session_id: Uuid,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandAccepted {
    pub contract_version: u16,
    pub request_id: Uuid,
}

/// Renderer-safe projection of the single global OpenAI-compatible route.
///
/// API keys are deliberately absent. A missing value means that LLM-assisted
/// processing is not configured and does not prevent local ASR from working.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LlmSettingsView {
    pub base_url: String,
    pub model: String,
}

impl fmt::Debug for LlmSettingsView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmSettingsView")
            .field("base_url", &"[REDACTED]")
            .field("model", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingModeView {
    Toggle,
    PushToTalk,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingModeView {
    Raw,
    Faithful,
    Structured,
}

/// The currently persisted settings exposed to the Control Panel.
///
/// This is a read projection, so it has no request correlation. Mutations wrap
/// the resulting view in a correlated result instead.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsView {
    pub contract_version: u16,
    pub version: u64,
    pub recording_mode: RecordingModeView,
    pub max_recording_duration_seconds: u64,
    pub recording_shortcut: Option<String>,
    pub processing_mode: ProcessingModeView,
    pub read_selected_text: bool,
    pub clipboard_bridge_allowed: bool,
    pub auto_copy_result: bool,
    pub local_diagnostics_enabled: bool,
    pub history_policy: HistoryPolicyView,
    pub llm: Option<LlmSettingsView>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryPolicyView {
    pub enabled: bool,
    pub limit: u16,
    pub retention_days: Option<u16>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetRecordingPreferencesCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub expected_version: u64,
    pub recording_mode: RecordingModeView,
    pub max_recording_duration_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetRecordingPreferencesResult {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub settings: SettingsView,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetRecordingShortcutCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub expected_version: u64,
    pub recording_shortcut: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetRecordingShortcutResult {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub settings: SettingsView,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetHistoryEnabledCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub expected_version: u64,
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetHistoryEnabledResult {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub settings: SettingsView,
}

/// Updates the bounded local history count.
///
/// `acknowledge_data_loss` is required only when the currently enabled policy
/// would immediately remove older rows. The Application layer rechecks the
/// actual store count; Renderer confirmation alone is not trusted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetHistoryLimitCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub expected_version: u64,
    pub limit: u16,
    pub acknowledge_data_loss: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetHistoryLimitResult {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub settings: SettingsView,
}

/// Updates the optional age bound for local final-text history.
///
/// Renderer confirmation is advisory only. Application rechecks persisted
/// timestamps and requires acknowledgement when rows would be removed now.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetHistoryRetentionCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub expected_version: u64,
    pub retention_days: Option<u16>,
    pub acknowledge_data_loss: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetHistoryRetentionResult {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub settings: SettingsView,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetAutoCopyResultCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub expected_version: u64,
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetAutoCopyResultResult {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub settings: SettingsView,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetLocalDiagnosticsCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub expected_version: u64,
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetLocalDiagnosticsResult {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub settings: SettingsView,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetTextProcessingSettingsCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub expected_version: u64,
    pub processing_mode: ProcessingModeView,
    pub read_selected_text: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetTextProcessingSettingsResult {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub settings: SettingsView,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetLlmSettingsCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub expected_version: u64,
    pub llm: Option<LlmSettingsView>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetLlmSettingsResult {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub settings: SettingsView,
}

/// Authenticated state of the API key record.
///
/// `Configured` means the current record was authenticated and decrypted, not
/// merely that ciphertext exists. `RecoveryRequired` is reserved for an
/// explicit destructive recovery flow; infrastructure failures remain
/// `Unavailable`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmApiKeyState {
    NotConfigured,
    Configured,
    RecoveryRequired,
    Unavailable,
}

/// V1 uses one cross-platform encrypted local store.
///
/// Availability is expressed by [`LlmApiKeyState`], so this field never
/// pretends that an unavailable store silently changed storage backends.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretStorageKind {
    EncryptedLocal,
}

/// Content-free read projection. It contains no length, prefix, suffix,
/// fingerprint, secret identifier, timestamp, or other API-key metadata.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LlmApiKeyStatusView {
    pub contract_version: u16,
    pub state: LlmApiKeyState,
    pub storage: SecretStorageKind,
}

#[derive(Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetLlmApiKeyCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub secret_value: String,
}

impl SetLlmApiKeyCommand {
    /// Moves the plaintext into the Application secret wrapper while leaving
    /// an empty String for this DTO's zeroizing destructor.
    pub fn take_secret_value(&mut self) -> String {
        std::mem::take(&mut self.secret_value)
    }
}

impl fmt::Debug for SetLlmApiKeyCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SetLlmApiKeyCommand")
            .field("contract_version", &self.contract_version)
            .field("request_id", &self.request_id)
            .field("secret_value", &"[REDACTED]")
            .finish()
    }
}

impl Drop for SetLlmApiKeyCommand {
    fn drop(&mut self) {
        self.secret_value.zeroize();
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RevealLlmApiKeyCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
}

/// A mutation result keeps correlation outside the reusable status projection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LlmApiKeyMutationResult {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub status: LlmApiKeyStatusView,
}

/// The only response DTO allowed to return the API key in plaintext.
///
/// Do not derive `Debug`: Tauri errors and diagnostics must never render the
/// plaintext value.
#[derive(Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RevealLlmApiKeyResult {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub secret_value: String,
}

impl fmt::Debug for RevealLlmApiKeyResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RevealLlmApiKeyResult")
            .field("contract_version", &self.contract_version)
            .field("request_id", &self.request_id)
            .field("secret_value", &"[REDACTED]")
            .finish()
    }
}

impl Drop for RevealLlmApiKeyResult {
    fn drop(&mut self) {
        self.secret_value.zeroize();
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteLlmApiKeyCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResetUnrecoverableLlmSecretsCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub acknowledge_data_loss: bool,
}

/// A connection test is generated entirely in Rust. The Renderer supplies
/// neither user text nor an API key.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LlmTestConnectionCommand {
    pub contract_version: u16,
    pub request_id: Uuid,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmConnectionTestStatus {
    Succeeded,
    Failed,
}

/// Stable, content-free error categories for a user-triggered connection test.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmConnectionTestErrorCode {
    Busy,
    RuntimeUnavailable,
    SettingsUnavailable,
    NotConfigured,
    RecoveryRequired,
    SecretUnavailable,
    InvalidConfiguration,
    AuthenticationFailed,
    PermissionDenied,
    RateLimited,
    Timeout,
    Network,
    ProviderUnavailable,
    RequestRejected,
    InvalidResponse,
    ResponseTooLarge,
    Cancelled,
    Internal,
}

/// Transient, bounded response from the upstream Provider. The body has
/// already been credential-redacted by the network adapter and is available
/// only to the ControlPanel connection-test result.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LlmUpstreamErrorView {
    pub http_status: u16,
    pub response_body: String,
    pub truncated: bool,
}

impl fmt::Debug for LlmUpstreamErrorView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmUpstreamErrorView")
            .field("http_status", &self.http_status)
            .field("response_body", &"[REDACTED]")
            .field("truncated", &self.truncated)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LlmConnectionTestResult {
    pub contract_version: u16,
    pub request_id: Uuid,
    pub status: LlmConnectionTestStatus,
    pub error_code: Option<LlmConnectionTestErrorCode>,
    pub upstream_error: Option<LlmUpstreamErrorView>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_snapshot_is_versioned_and_content_free() {
        let json = serde_json::to_value(AppSnapshot::bootstrap()).expect("snapshot must serialize");

        assert_eq!(json["contract_version"], CONTRACT_VERSION);
        assert_eq!(json["asr_readiness"], "discovering");
        assert_eq!(json["model_summary"]["selected_model"], "qwen");
        let rendered = json.to_string();
        for forbidden in ["api_key", "final_text", "selected_text", "audio_ref"] {
            assert!(!rendered.contains(forbidden));
        }
    }

    #[test]
    fn local_asr_model_is_a_closed_wire_enum() {
        assert_eq!(
            serde_json::to_value(LocalAsrModel::Whisper).expect("model must serialize"),
            "whisper"
        );
        assert!(serde_json::from_value::<LocalAsrModel>(serde_json::json!("other")).is_err());
    }

    #[test]
    fn settings_and_secret_status_projections_do_not_serialize_secret_metadata() {
        let settings = SettingsView {
            contract_version: CONTRACT_VERSION,
            version: 7,
            recording_mode: RecordingModeView::PushToTalk,
            max_recording_duration_seconds: 600,
            recording_shortcut: Some("command+shift+KeyR".to_owned()),
            processing_mode: ProcessingModeView::Faithful,
            read_selected_text: true,
            clipboard_bridge_allowed: true,
            auto_copy_result: false,
            local_diagnostics_enabled: true,
            history_policy: HistoryPolicyView {
                enabled: true,
                limit: 10,
                retention_days: None,
            },
            llm: Some(LlmSettingsView {
                base_url: "https://provider.invalid/v1".to_owned(),
                model: "test-model".to_owned(),
            }),
        };
        let status = LlmApiKeyStatusView {
            contract_version: CONTRACT_VERSION,
            state: LlmApiKeyState::Configured,
            storage: SecretStorageKind::EncryptedLocal,
        };

        let settings_json = serde_json::to_value(settings).expect("settings must serialize");
        assert_eq!(settings_json["llm"]["model"], "test-model");
        assert_eq!(settings_json["recording_mode"], "push_to_talk");
        assert_eq!(settings_json["max_recording_duration_seconds"], 600);
        assert_eq!(settings_json["recording_shortcut"], "command+shift+KeyR");
        assert_eq!(settings_json["processing_mode"], "faithful");
        assert_eq!(settings_json["read_selected_text"], true);
        assert_eq!(settings_json["clipboard_bridge_allowed"], true);
        assert_eq!(settings_json["auto_copy_result"], false);
        assert_eq!(settings_json["local_diagnostics_enabled"], true);
        let status_json = serde_json::to_value(status).expect("status must serialize");
        assert_eq!(status_json["state"], "configured");
        assert_eq!(status_json["storage"], "encrypted_local");

        for json in [settings_json, status_json] {
            let rendered = json.to_string();
            for forbidden in [
                "secret_value",
                "api_key",
                "secret_id",
                "fingerprint",
                "prefix",
                "suffix",
                "length",
                "updated_at",
            ] {
                assert!(!rendered.contains(forbidden), "{forbidden} leaked");
            }
        }
    }

    #[test]
    fn llm_settings_debug_redacts_provider_routing_metadata() {
        let view = LlmSettingsView {
            base_url: "https://private.example/v1".to_owned(),
            model: "private-model".to_owned(),
        };
        let rendered = format!("{view:?}");

        assert!(!rendered.contains("private.example"));
        assert!(!rendered.contains("private-model"));
        assert!(rendered.contains("[REDACTED]"));
    }

    #[test]
    fn secret_plaintext_dtos_have_redacted_debug_output() {
        let marker = "sk-plain-text-must-not-appear";
        let request_id = Uuid::new_v4();
        let set = SetLlmApiKeyCommand {
            contract_version: CONTRACT_VERSION,
            request_id,
            secret_value: marker.to_owned(),
        };
        let reveal = RevealLlmApiKeyResult {
            contract_version: CONTRACT_VERSION,
            request_id,
            secret_value: marker.to_owned(),
        };

        for rendered in [format!("{set:?}"), format!("{reveal:?}")] {
            assert!(!rendered.contains(marker));
            assert!(rendered.contains("[REDACTED]"));
        }

        assert_eq!(
            serde_json::to_value(&set).expect("set command must serialize")["secret_value"],
            marker
        );
        assert_eq!(
            serde_json::to_value(&reveal).expect("reveal result must serialize")["secret_value"],
            marker
        );
    }

    #[test]
    fn connection_test_command_cannot_carry_key_or_user_content() {
        let json = serde_json::to_value(LlmTestConnectionCommand {
            contract_version: CONTRACT_VERSION,
            request_id: Uuid::new_v4(),
        })
        .expect("connection test command must serialize");
        let object = json.as_object().expect("connection test is an object");

        assert_eq!(object.len(), 2);
        assert!(object.contains_key("contract_version"));
        assert!(object.contains_key("request_id"));
        for forbidden in [
            "secret_value",
            "api_key",
            "prompt",
            "text",
            "transcript",
            "selected_text",
            "base_url",
            "model",
        ] {
            assert!(!object.contains_key(forbidden));
        }
    }

    #[test]
    fn connection_test_result_serializes_transient_upstream_body_but_redacts_debug() {
        let marker = "upstream-auth-policy-marker";
        let result = LlmConnectionTestResult {
            contract_version: CONTRACT_VERSION,
            request_id: Uuid::new_v4(),
            status: LlmConnectionTestStatus::Failed,
            error_code: Some(LlmConnectionTestErrorCode::AuthenticationFailed),
            upstream_error: Some(LlmUpstreamErrorView {
                http_status: 401,
                response_body: marker.to_owned(),
                truncated: false,
            }),
        };

        let json = serde_json::to_value(&result).expect("connection result must serialize");
        assert_eq!(json["upstream_error"]["http_status"], 401);
        assert_eq!(json["upstream_error"]["response_body"], marker);
        assert!(!format!("{result:?}").contains(marker));
    }

    #[test]
    fn user_notifications_are_closed_content_free_projections() {
        let session_id = Uuid::new_v4();
        let expected = [
            (
                UserNotificationCode::MicrophonePermission,
                "notification.permission_microphone",
            ),
            (UserNotificationCode::Asr, "notification.asr"),
            (UserNotificationCode::Llm, "notification.llm"),
            (UserNotificationCode::Delivery, "notification.delivery"),
        ];

        for (code, serialized_code) in expected {
            let value = serde_json::to_value(UserNotification {
                contract_version: CONTRACT_VERSION,
                session_id,
                code,
            })
            .expect("notification must serialize");
            let object = value.as_object().expect("notification is an object");
            assert_eq!(object.len(), 3);
            assert_eq!(object["code"], serialized_code);
            assert!(object.contains_key("contract_version"));
            assert!(object.contains_key("session_id"));
        }

        let error = serde_json::from_value::<UserNotification>(serde_json::json!({
            "contract_version": CONTRACT_VERSION,
            "session_id": session_id,
            "code": "notification.asr",
            "final_text": "must not enter this boundary"
        }))
        .expect_err("notification must reject content fields");
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn history_query_and_page_keep_the_read_boundary_minimal() {
        let query = serde_json::to_value(HistoryQuery {
            contract_version: CONTRACT_VERSION,
        })
        .expect("history query must serialize");
        assert_eq!(query.as_object().expect("history query object").len(), 1);

        let private_text = "只允许进入历史列表结果的最终文字";
        let record_id = Uuid::new_v4();
        let page = HistoryPage {
            contract_version: CONTRACT_VERSION,
            records: vec![HistoryRecordView {
                record_id,
                final_text: private_text.to_owned(),
                created_at: "2026-07-31T06:05:22.123Z".to_owned(),
            }],
        };
        let value = serde_json::to_value(&page).expect("history page must serialize");
        let row = value["records"][0]
            .as_object()
            .expect("history row must be an object");
        assert_eq!(row.len(), 3);
        assert_eq!(row["record_id"], record_id.to_string());
        assert_eq!(row["final_text"], private_text);
        assert_eq!(row["created_at"], "2026-07-31T06:05:22.123Z");
        for forbidden in [
            "delivery_id",
            "source_app",
            "processing_mode",
            "delivery_status",
            "selected_text",
            "provider",
            "path",
            "api_key",
        ] {
            assert!(!value.to_string().contains(forbidden));
        }

        let rendered = format!("{page:?}");
        assert!(!rendered.contains(private_text));
        assert!(rendered.contains("[REDACTED]"));

        let error = serde_json::from_value::<HistoryQuery>(serde_json::json!({
            "contract_version": CONTRACT_VERSION,
            "page": 1
        }))
        .expect_err("history query must reject pagination fields not in V1");
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn history_action_dtos_are_correlated_and_content_free() {
        let request_id = Uuid::new_v4();
        let record_id = Uuid::new_v4();
        for value in [
            serde_json::to_value(HistoryCopyCommand {
                contract_version: CONTRACT_VERSION,
                request_id,
                record_id,
            })
            .expect("copy command"),
            serde_json::to_value(HistoryCopyResult {
                contract_version: CONTRACT_VERSION,
                request_id,
                record_id,
            })
            .expect("copy result"),
        ] {
            let object = value.as_object().expect("history copy object");
            assert_eq!(object.len(), 3);
            assert!(object.contains_key("contract_version"));
            assert!(object.contains_key("request_id"));
            assert!(object.contains_key("record_id"));
            for forbidden in ["final_text", "delivery_id", "path", "api_key"] {
                assert!(!object.contains_key(forbidden));
            }
        }

        let clear = serde_json::to_value(HistoryClearAllCommand {
            contract_version: CONTRACT_VERSION,
            request_id,
            acknowledge_data_loss: true,
        })
        .expect("clear command");
        assert_eq!(clear["acknowledge_data_loss"], true);
        assert_eq!(clear.as_object().expect("clear object").len(), 3);

        let limit = serde_json::to_value(SetHistoryLimitCommand {
            contract_version: CONTRACT_VERSION,
            request_id,
            expected_version: 7,
            limit: 25,
            acknowledge_data_loss: true,
        })
        .expect("history limit command");
        let object = limit.as_object().expect("history limit object");
        assert_eq!(object.len(), 5);
        assert_eq!(object["limit"], 25);
        assert_eq!(object["acknowledge_data_loss"], true);
        assert!(!object.contains_key("final_text"));

        let unknown = serde_json::from_value::<HistoryCopyCommand>(serde_json::json!({
            "contract_version": CONTRACT_VERSION,
            "request_id": request_id,
            "record_id": record_id,
            "final_text": "must not cross this boundary"
        }))
        .expect_err("copy command rejects Renderer text");
        assert!(unknown.to_string().contains("unknown field"));

        let unknown = serde_json::from_value::<SetHistoryLimitCommand>(serde_json::json!({
            "contract_version": CONTRACT_VERSION,
            "request_id": request_id,
            "expected_version": 7,
            "limit": 25,
            "acknowledge_data_loss": true,
            "records": []
        }))
        .expect_err("history limit rejects record content");
        assert!(unknown.to_string().contains("unknown field"));
    }

    #[test]
    fn new_commands_reject_unknown_fields() {
        let request_id = Uuid::new_v4();
        let error = serde_json::from_value::<LlmTestConnectionCommand>(serde_json::json!({
            "contract_version": CONTRACT_VERSION,
            "request_id": request_id,
            "secret_value": "must-not-be-accepted"
        }))
        .expect_err("unknown connection-test fields must fail closed");
        assert!(error.to_string().contains("unknown field"));

        let autostart = serde_json::to_value(SetAutostartCommand {
            contract_version: CONTRACT_VERSION,
            request_id,
            enabled: true,
        })
        .expect("autostart command");
        assert_eq!(autostart.as_object().expect("autostart object").len(), 3);

        let error = serde_json::from_value::<SetAutostartCommand>(serde_json::json!({
            "contract_version": CONTRACT_VERSION,
            "request_id": request_id,
            "enabled": true,
            "launch_path": "/tmp/must-not-cross"
        }))
        .expect_err("autostart must not accept a Renderer-provided path");
        assert!(error.to_string().contains("unknown field"));

        let error =
            serde_json::from_value::<ResetUnrecoverableLlmSecretsCommand>(serde_json::json!({
                "contract_version": CONTRACT_VERSION,
                "request_id": request_id,
                "acknowledge_data_loss": true,
                "force": true
            }))
            .expect_err("unknown reset fields must fail closed");
        assert!(error.to_string().contains("unknown field"));
    }
}
