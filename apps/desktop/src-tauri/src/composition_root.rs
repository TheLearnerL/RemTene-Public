//! Composition Root - 依赖注入与组件组装
//!
//! 负责创建和组装所有应用组件：
//! - Port 实现（Adapters）
//! - 领域服务（TranscriptionOrchestrator）
//! - 平台集成（ASR Worker、录音、目标检测等）

use std::{path::PathBuf, sync::Arc};

use crate::{
    asr_status::{AsrRuntimeStatus, ObservedAsrEngine},
    recording_deadline::TauriRecordingDeadline,
};
use remtene_adapters::{
    clock::SystemClock,
    diagnostics::{ConsoleDiagnosticsSink, FileDiagnosticsSink},
    id_generator::UuidV7Generator,
    llm::{OpenAiCompatibleLlmProvider, OpenAiCompatiblePolicy},
    llm_provider::UnavailableLlmProvider,
    local_encrypted_secret_store::{LocalEncryptedSecretStore, UnavailableSecretStore},
};
use remtene_application::{
    AsrHealthController, HistoryController, HistorySettingsController, LlmConfigurationController,
    OrchestratorPorts, SystemSettingsController, TranscriptionOrchestrator,
    ports::{
        AsrEnginePort, AsrModelControlPort, AudioCapture, ClipboardBridge, ClipboardTextWriter,
        Clock, DiagnosticsControl, DiagnosticsSink, HistoryStore, IdGenerator, LlmProvider,
        MicrophonePermissionPort, OutputAdapter, RecordingDeadlinePort, RecordingHudPort,
        SecretStore, SettingsStore, TargetContextPort, TemporaryTextOutput, UserNotificationPort,
    },
};
use remtene_contracts::AppError;

#[cfg(target_os = "macos")]
use remtene_platform::{
    MacOsMicrophonePermission, MacTargetContext,
    clipboard::{AuthorizedClipboardBridge, ClipboardBridgeAuthorization, MacClipboardBackend},
};

#[cfg(target_os = "windows")]
use remtene_adapters::{
    clipboard_bridge::UnsupportedClipboardBridge,
    clipboard_text_writer::UnsupportedClipboardTextWriter,
    history_store::StubHistoryStore,
    output_adapter::ConsoleOutputAdapter,
    settings_store::InMemorySettingsStore,
    stub_ports::{StubAudioCapture, StubMicrophonePermission, StubTargetContext},
};

/// 应用程序的依赖注入容器
///
/// 组装所有 Port 实现并创建核心业务逻辑组件。
/// 每个 Tauri 应用实例应该只有一个 CompositionRoot。
pub struct CompositionRoot {
    /// 转录编排器 - 核心业务逻辑
    pub orchestrator: Arc<TranscriptionOrchestrator>,
    /// 设置读取入口；触发层需要据此决定 Toggle 与 Push-to-Talk 行为
    pub settings: Arc<dyn SettingsStore>,
    /// 正式 ASR 工作流产生的只读健康观测；读取不会启动 Worker 或切换模型。
    pub(crate) asr_status: Arc<AsrRuntimeStatus>,
    /// 应用启动一次、用户明确点击时再次运行的本地 ASR Health 用例。
    pub(crate) asr_health: Arc<AsrHealthController>,
    /// 唯一的 LLM 设置与秘密管理入口。Tauri 不接触通用 SecretStore。
    pub llm_configuration: Arc<LlmConfigurationController>,
    /// 历史操作入口；与 Orchestrator 共用同一个 HistoryStore。
    pub(crate) history: Arc<HistoryController>,
    /// 历史保存策略入口；只改普通设置，不直接删除历史记录。
    pub(crate) history_settings: Arc<HistorySettingsController>,
    /// 系统页自动复制与本地诊断设置入口。
    pub(crate) system_settings: Arc<SystemSettingsController>,
    /// 所有结构化运行时事件的唯一 Sink。
    pub(crate) diagnostics: Arc<dyn DiagnosticsSink>,
    /// 用户明确点击“复制全部”时使用的纯剪贴板文字写入能力。
    ///
    /// 它不读取、恢复或派发粘贴，也不属于自动交付路径。
    pub(crate) clipboard_text_writer: Arc<dyn ClipboardTextWriter>,
}

impl CompositionRoot {
    /// 创建完整的应用程序组件树
    ///
    /// # 参数
    /// - `targets`: 目标上下文 Port（平台特定）
    /// - `microphone_permission`: 麦克风权限 Port（平台特定）
    /// - `audio`: 录音捕获 Port（平台特定）
    /// - `recording_hud`: 录音 HUD Port
    /// - `asr`: ASR 引擎 Port
    /// - `output`: 输出适配器 Port（平台特定）
    /// - `history`: 历史存储 Port（平台特定；桌面用文件持久化，测试可用 Stub）
    /// - `settings`: 设置存储 Port（平台特定；桌面用文件持久化，测试可用内存）
    /// - `secrets`: 秘密存储 Port（桌面用本地加密存储，初始化失败时用不可用降级实现）
    /// - `temporary_text`: 一次性回退交付面（桌面用 Tauri 浮动窗口，测试可用 Stub）
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        targets: Arc<dyn TargetContextPort>,
        microphone_permission: Arc<dyn MicrophonePermissionPort>,
        audio: Arc<dyn AudioCapture>,
        recording_hud: Arc<dyn RecordingHudPort>,
        asr: Arc<dyn AsrEnginePort>,
        asr_model_control: Arc<dyn AsrModelControlPort>,
        output: Arc<dyn OutputAdapter>,
        history: Arc<dyn HistoryStore>,
        settings: Arc<dyn SettingsStore>,
        secrets: Arc<dyn SecretStore>,
        temporary_text: Arc<dyn TemporaryTextOutput>,
        user_notifications: Arc<dyn UserNotificationPort>,
        clipboard: Arc<dyn ClipboardBridge>,
        clipboard_text_writer: Arc<dyn ClipboardTextWriter>,
    ) -> Self {
        let diagnostics_impl = Arc::new(ConsoleDiagnosticsSink::new());
        let diagnostics: Arc<dyn DiagnosticsSink> = diagnostics_impl.clone();
        let diagnostics_control: Arc<dyn DiagnosticsControl> = diagnostics_impl;
        Self::new_with_llm_policy(
            targets,
            microphone_permission,
            audio,
            recording_hud,
            asr,
            asr_model_control,
            output,
            history,
            settings,
            secrets,
            temporary_text,
            user_notifications,
            clipboard,
            clipboard_text_writer,
            OpenAiCompatiblePolicy::default(),
            diagnostics,
            diagnostics_control,
        )
    }

    /// 与 [`Self::new`] 相同，但允许组合测试缩短真实 HTTP Deadline。
    ///
    /// 生产调用始终使用默认策略；此入口不替换 Provider，也不绕过 SecretStore。
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_llm_policy(
        targets: Arc<dyn TargetContextPort>,
        microphone_permission: Arc<dyn MicrophonePermissionPort>,
        audio: Arc<dyn AudioCapture>,
        recording_hud: Arc<dyn RecordingHudPort>,
        asr: Arc<dyn AsrEnginePort>,
        asr_model_control: Arc<dyn AsrModelControlPort>,
        output: Arc<dyn OutputAdapter>,
        history: Arc<dyn HistoryStore>,
        settings: Arc<dyn SettingsStore>,
        secrets: Arc<dyn SecretStore>,
        temporary_text: Arc<dyn TemporaryTextOutput>,
        user_notifications: Arc<dyn UserNotificationPort>,
        clipboard: Arc<dyn ClipboardBridge>,
        clipboard_text_writer: Arc<dyn ClipboardTextWriter>,
        llm_policy: OpenAiCompatiblePolicy,
        diagnostics: Arc<dyn DiagnosticsSink>,
        diagnostics_control: Arc<dyn DiagnosticsControl>,
    ) -> Self {
        remtene_application::configure_diagnostics_trace(&diagnostics);
        remtene_platform::configure_diagnostics_sink(&diagnostics);
        // 真实 Provider、Controller 与 Orchestrator 共用同一个 SecretStore。
        // HTTP Client 初始化失败时只关闭 LLM 路径，本地 ASR 与录音仍可启动。
        let llm: Arc<dyn LlmProvider> =
            match OpenAiCompatibleLlmProvider::with_policy(Arc::clone(&secrets), llm_policy) {
                Ok(provider) => Arc::new(provider),
                Err(error) => Arc::new(UnavailableLlmProvider::from_error(error)),
            };
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let ids: Arc<dyn IdGenerator> = Arc::new(UuidV7Generator::new());
        let recording_deadline: Arc<dyn RecordingDeadlinePort> = Arc::new(TauriRecordingDeadline);
        let recording_cue = remtene_platform::create_default_recording_cue();
        let asr_status = Arc::new(AsrRuntimeStatus::default());
        let observed_asr: Arc<dyn AsrEnginePort> = Arc::new(ObservedAsrEngine::new(
            asr,
            Arc::clone(&asr_status),
            Arc::clone(&diagnostics),
        ));
        let health_asr = Arc::clone(&observed_asr);
        let history_controller_store = Arc::clone(&history);
        let history_settings_store = Arc::clone(&history);

        // 组装 Orchestrator Ports
        let ports = OrchestratorPorts {
            settings: Arc::clone(&settings),
            targets,
            microphone_permission,
            audio,
            recording_cue,
            recording_deadline,
            recording_hud,
            asr: observed_asr,
            llm: Arc::clone(&llm),
            output,
            clipboard,
            clipboard_text_writer: Arc::clone(&clipboard_text_writer),
            temporary_text,
            user_notifications,
            history,
            diagnostics: Arc::clone(&diagnostics),
            clock: Arc::clone(&clock),
            ids,
        };

        // 创建 Orchestrator
        let orchestrator = Arc::new(TranscriptionOrchestrator::new(ports));
        let asr_health = Arc::new(AsrHealthController::new(
            Arc::clone(&orchestrator),
            Arc::clone(&settings),
            health_asr,
            asr_model_control,
        ));
        let llm_configuration = Arc::new(LlmConfigurationController::new(
            Arc::clone(&orchestrator),
            Arc::clone(&settings),
            secrets,
            llm,
        ));
        let history = Arc::new(HistoryController::new_with_settings(
            Arc::clone(&orchestrator),
            history_controller_store,
            Arc::clone(&clipboard_text_writer),
            Arc::clone(&settings),
        ));
        let history_settings = Arc::new(HistorySettingsController::new(
            Arc::clone(&orchestrator),
            Arc::clone(&settings),
            history_settings_store,
            clock,
        ));
        let system_settings = Arc::new(SystemSettingsController::new(
            Arc::clone(&orchestrator),
            Arc::clone(&settings),
            diagnostics_control,
        ));

        Self {
            orchestrator,
            settings,
            asr_status,
            asr_health,
            llm_configuration,
            history,
            history_settings,
            system_settings,
            diagnostics,
            clipboard_text_writer,
        }
    }

    /// 创建 macOS 平台的 CompositionRoot
    ///
    /// 使用真实的 macOS 平台实现：
    /// - MacTargetContext (AX + 安全输入检测)
    /// - MacOsMicrophonePermission (CoreAudio 权限)
    /// - SafeAudioCapture (CoreAudio 录音)
    ///
    /// # 参数
    /// - `audio`: 已建立的 macOS 录音适配器；ASR Worker 的音频解析器共用同一实例
    /// - `history_path`: 历史记录 JSON 文件路径（DATA-020：仅存最终文本与时间）
    /// - `settings_path`: 设置 JSON 文件路径（DATA-020：磁盘持久化）
    /// - `diagnostics_root`: 应用缓存内的本地日志目录
    /// - `secret_root`: API Key 本地加密存储目录
    /// - `recording_hud`: 录音 HUD Port
    /// - `asr`: ASR 引擎 Port
    /// - `temporary_text`: 一次性回退交付面
    /// - `user_notifications`: 独立、无正文的错误恢复反馈面
    #[cfg(target_os = "macos")]
    #[allow(clippy::too_many_arguments)]
    pub fn new_macos(
        audio: Arc<dyn AudioCapture>,
        history_path: PathBuf,
        settings_path: PathBuf,
        diagnostics_root: PathBuf,
        secret_root: PathBuf,
        recording_hud: Arc<dyn RecordingHudPort>,
        asr: Arc<dyn AsrEnginePort>,
        asr_model_control: Arc<dyn AsrModelControlPort>,
        temporary_text: Arc<dyn TemporaryTextOutput>,
        user_notifications: Arc<dyn UserNotificationPort>,
    ) -> Result<Self, AppError> {
        // 创建 macOS 平台 Ports
        let targets = Arc::new(MacTargetContext::new());
        let microphone_permission = Arc::new(MacOsMicrophonePermission);

        // MacTargetContext 同时实现 TargetContextPort 和 OutputAdapter
        let output: Arc<dyn OutputAdapter> = targets.clone();

        // 剪贴板兜底必须与 AX 直写共用同一张凭证表：粘贴前要核对的「目标是否仍是
        // 捕获时那个控件」只有 MacTargetContext 知道。换成新实例就等于放弃校验，
        // 可能把文字粘到用户此刻切过去的另一个窗口里。
        let clipboard_backend = Arc::new(MacClipboardBackend::new(Arc::clone(&targets)));
        let clipboard_bridge = Arc::new(AuthorizedClipboardBridge::new(
            clipboard_backend,
            ClipboardBridgeAuthorization::from_enabled_user_setting(true)
                .expect("剪贴板兜底在桌面端默认启用；从 true 请求授权不可能返回 None"),
        ));
        let clipboard_text_writer: Arc<dyn ClipboardTextWriter> =
            Arc::clone(&clipboard_bridge) as Arc<dyn ClipboardTextWriter>;
        let clipboard: Arc<dyn ClipboardBridge> = clipboard_bridge;

        // 创建文件持久化历史存储（DATA-020）
        let history: Arc<dyn HistoryStore> = Arc::new(
            remtene_adapters::file_history_store::FileHistoryStore::new(history_path).map_err(
                |e| {
                    AppError::new(
                        "history.initialization_failed",
                        remtene_contracts::ErrorCategory::Storage,
                        remtene_contracts::ErrorSeverity::Error,
                        false,
                        format!("Failed to initialize history store: {}", e),
                    )
                },
            )?,
        );

        // 创建文件持久化设置存储（DATA-020）
        let settings_store = Arc::new(
            remtene_adapters::file_settings_store::FileSettingsStore::new(
                settings_path,
                default_settings_input(),
            ),
        );
        let initial_settings =
            futures::executor::block_on(settings_store.load()).map_err(|error| {
                AppError::new(
                    "settings.initialization_failed",
                    remtene_contracts::ErrorCategory::Storage,
                    remtene_contracts::ErrorSeverity::Error,
                    error.retryable,
                    error.safe_message_key,
                )
            })?;
        let diagnostics_impl = Arc::new(FileDiagnosticsSink::new(
            diagnostics_root,
            initial_settings.local_diagnostics_enabled(),
        ));
        let diagnostics: Arc<dyn DiagnosticsSink> = diagnostics_impl.clone();
        let diagnostics_control: Arc<dyn DiagnosticsControl> = diagnostics_impl;
        let settings: Arc<dyn SettingsStore> = settings_store;
        let secrets = initialize_secret_store(secret_root);

        Ok(Self::new_with_llm_policy(
            targets,
            microphone_permission,
            audio,
            recording_hud,
            asr,
            asr_model_control,
            output,
            history,
            settings,
            secrets,
            temporary_text,
            user_notifications,
            clipboard,
            clipboard_text_writer,
            OpenAiCompatiblePolicy::default(),
            diagnostics,
            diagnostics_control,
        ))
    }

    /// 创建 Windows 平台的 CompositionRoot（使用 Stub）
    ///
    /// Windows 真实实现待 ASR-WIN-001 完成。
    #[cfg(target_os = "windows")]
    pub fn new_windows(
        secret_root: Option<PathBuf>,
        recording_hud: Arc<dyn RecordingHudPort>,
        asr: Arc<dyn AsrEnginePort>,
        asr_model_control: Arc<dyn AsrModelControlPort>,
        temporary_text: Arc<dyn TemporaryTextOutput>,
        user_notifications: Arc<dyn UserNotificationPort>,
    ) -> Result<Self, AppError> {
        let targets = Arc::new(StubTargetContext::new());
        let microphone_permission = Arc::new(StubMicrophonePermission::new());
        let audio = Arc::new(StubAudioCapture::new());
        let output: Arc<dyn OutputAdapter> = Arc::new(ConsoleOutputAdapter::new());
        let history: Arc<dyn HistoryStore> = Arc::new(StubHistoryStore::new());
        let settings: Arc<dyn SettingsStore> = Arc::new(InMemorySettingsStore::with_defaults());
        let secrets = secret_root.map_or_else(
            || Arc::new(UnavailableSecretStore::new()) as Arc<dyn SecretStore>,
            initialize_secret_store,
        );
        // Windows 尚无剪贴板后端（ASR-WIN-001）。这里必须如实失败，让编排器走临时文本框，
        // 而不是报告一次没有发生的插入。
        let clipboard: Arc<dyn ClipboardBridge> = Arc::new(UnsupportedClipboardBridge::new());
        let clipboard_text_writer: Arc<dyn ClipboardTextWriter> =
            Arc::new(UnsupportedClipboardTextWriter::new());

        let mut root = Self::new(
            targets,
            microphone_permission,
            audio,
            recording_hud,
            asr,
            asr_model_control,
            output,
            history,
            settings,
            secrets,
            temporary_text,
            user_notifications,
            clipboard,
            clipboard_text_writer,
        );
        // Windows 当前仍是丢弃数据的 StubHistoryStore。读取必须明确不可用，
        // 不能把 Stub 的空数组投影成用户真的“没有历史”。
        root.history = Arc::new(HistoryController::unavailable());
        Ok(root)
    }
}

fn initialize_secret_store(secret_root: PathBuf) -> Arc<dyn SecretStore> {
    match LocalEncryptedSecretStore::new(secret_root) {
        Ok(store) => Arc::new(store),
        Err(error) => {
            // 这里只记录稳定错误码，避免路径、密钥或底层数据库细节进入诊断日志。
            eprintln!("⚠ SecretStore initialization failed: {}", error.code);
            Arc::new(UnavailableSecretStore::from_error(error))
        }
    }
}

/// 默认设置输入（用于设置文件首次创建时的种子值）
///
/// 取值以产品手册的预设值为准：Toggle 触发、单次录音上限 10 分钟、历史默认开启且
/// 上限 10 条、不启用独立保存期限；选区读取、剪贴板桥接与自动复制默认关闭。
#[cfg(target_os = "macos")]
fn default_settings_input() -> remtene_domain::SettingsSnapshotInput {
    use remtene_domain::{
        AsrPreference, HistoryPolicy, ProcessingMode, RecordingMode, SettingsSnapshotInput,
    };
    use std::time::Duration;
    SettingsSnapshotInput {
        version: 0,
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_store_initialization_failure_degrades_without_blocking_composition() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let invalid_root =
            std::env::temp_dir().join(format!("remtene-secret-root-file-{nanos}.tmp"));
        std::fs::write(&invalid_root, b"not a directory").unwrap();

        let secrets = initialize_secret_store(invalid_root.clone());
        let error =
            futures::executor::block_on(secrets.is_configured("provider.test")).unwrap_err();

        assert_eq!(error.code, "secret.directory_invalid");
        std::fs::remove_file(invalid_root).unwrap();
    }
}
