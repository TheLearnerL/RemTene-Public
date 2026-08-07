//! 会话控制命令 - Tauri IPC 入口点
//!
//! 提供前端调用的会话生命周期管理命令：
//! - 开始录音会话
//! - 取消会话
//! - 完成会话

use remtene_contracts::{AppError, CONTRACT_VERSION, ErrorCategory, ErrorSeverity};
use remtene_domain::SessionId;
use serde::Serialize;
use tauri::{State, WebviewWindow};

use crate::composition_root::CompositionRoot;
use crate::{WindowCommandClass, authorize_window};

/// 结束录音的结果投影。只含状态标签，不含转录文本。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SessionFinishView {
    /// IPC（程序间通信）契约版本。
    pub contract_version: u16,
    /// `delivered` / `failed` / `discarded` / `not_recording`
    pub status: &'static str,
    /// 交付通道：`inserted` / `clipboard` / `temporary_text`
    pub delivery: Option<&'static str>,
    /// 不含内容的安全提示标签；用于说明 AI 未配置／不可用时已直接交付本地 ASR。
    pub notice: Option<&'static str>,
    /// 失败类别，仅在 `status == "failed"` 时出现
    pub failure: Option<String>,
}

fn finish_view(outcome: remtene_application::FinishOutcome) -> SessionFinishView {
    match outcome {
        remtene_application::FinishOutcome::Completed(completion) => SessionFinishView {
            contract_version: CONTRACT_VERSION,
            status: "delivered",
            delivery: Some(match completion.delivery {
                remtene_application::DeliveryKind::Inserted => "inserted",
                remtene_application::DeliveryKind::ClipboardBridge => "clipboard",
                remtene_application::DeliveryKind::TemporaryText => "temporary_text",
            }),
            notice: match completion.direct_delivery_reason {
                Some(remtene_application::DirectDeliveryReason::LlmNotConfigured) => {
                    Some("llm_not_configured")
                }
                Some(remtene_application::DirectDeliveryReason::LlmUnavailable) => {
                    Some("llm_unavailable")
                }
                Some(remtene_application::DirectDeliveryReason::RawMode) | None => None,
            },
            failure: None,
        },
        remtene_application::FinishOutcome::Failed(category) => SessionFinishView {
            contract_version: CONTRACT_VERSION,
            status: "failed",
            delivery: None,
            notice: None,
            failure: Some(crate::session_projection::failure_error_code(category).to_owned()),
        },
        remtene_application::FinishOutcome::NoSpeech
        | remtene_application::FinishOutcome::Discarded => SessionFinishView {
            contract_version: CONTRACT_VERSION,
            status: "discarded",
            delivery: None,
            notice: None,
            failure: None,
        },
        remtene_application::FinishOutcome::NotRecording => SessionFinishView {
            contract_version: CONTRACT_VERSION,
            status: "not_recording",
            delivery: None,
            notice: None,
            failure: None,
        },
    }
}

/// 从字符串解析 SessionId
fn parse_session_id(s: &str) -> Result<SessionId, AppError> {
    s.parse().map(SessionId::from_uuid).map_err(|_| {
        AppError::new(
            "session.invalid_id",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Error,
            false,
            "Invalid session ID format",
        )
    })
}

/// 开始新的录音会话
///
/// # 返回
/// - 成功：SessionId（UUID 字符串）
/// - 失败：AppError
#[tauri::command]
pub async fn session_start(
    window: WebviewWindow,
    root: State<'_, CompositionRoot>,
) -> Result<String, AppError> {
    authorize_window(window.label(), WindowCommandClass::RecordingControl)?;

    let outcome = root.orchestrator.start().await.map_err(|e| {
        AppError::new(
            "session.start_failed",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Error,
            true, // retryable
            format!("Failed to start session: {:?}", e),
        )
    })?;

    match outcome {
        remtene_application::StartOutcome::Started { session_id } => {
            Ok(session_id.as_uuid().to_string())
        }
        remtene_application::StartOutcome::Rejected(reason) => Err(AppError::new(
            "session.rejected",
            ErrorCategory::Permission,
            ErrorSeverity::Error,
            false,
            format!("Session rejected: {:?}", reason),
        )),
        remtene_application::StartOutcome::Busy { active_session_id } => Err(AppError::new(
            "session.busy",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Warning,
            false,
            format!("Session already active: {:?}", active_session_id),
        )),
        remtene_application::StartOutcome::Quitting => Err(AppError::new(
            "session.quitting",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Info,
            false,
            "Application is quitting",
        )),
        remtene_application::StartOutcome::Failed(category) => Err(AppError::new(
            "session.failed",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Error,
            true,
            format!("Session failed: {:?}", category),
        )),
    }
}

/// 取消正在进行的会话
///
/// # 参数
/// - `session_id`: 要取消的会话 ID（UUID 字符串）
///
/// # 返回
/// - 成功：()
/// - 失败：AppError
#[tauri::command]
pub async fn session_cancel(
    session_id: String,
    window: WebviewWindow,
    root: State<'_, CompositionRoot>,
) -> Result<(), AppError> {
    authorize_window(window.label(), WindowCommandClass::RecordingControl)?;
    let session_id = parse_session_id(&session_id)?;

    root.orchestrator
        .cancel_recording(session_id)
        .await
        .map_err(|e| {
            AppError::new(
                "session.cancel_failed",
                ErrorCategory::Lifecycle,
                ErrorSeverity::Error,
                true,
                format!("Failed to cancel session: {:?}", e),
            )
        })?;

    Ok(())
}

/// 完成并交付转录结果
///
/// # 参数
/// - `session_id`: 要完成的会话 ID（UUID 字符串）
///
/// # 返回
/// - 成功：交付结果（`delivered` / `no_text` / `discarded` / `not_recording`）
/// - 失败：AppError
///
/// 结果必须回传给调用方：`Discarded` 与 `Failed` 都不产生用户可见输出，
/// 若一律返回 `Ok(())`，界面就无法区分「已交付」和「什么都没发生」。
#[tauri::command]
pub async fn session_finish(
    session_id: String,
    window: WebviewWindow,
    root: State<'_, CompositionRoot>,
) -> Result<SessionFinishView, AppError> {
    authorize_window(window.label(), WindowCommandClass::RecordingControl)?;
    let session_id = parse_session_id(&session_id)?;

    let outcome = root
        .orchestrator
        .finish_recording(session_id)
        .await
        .map_err(|e| {
            AppError::new(
                "session.finish_failed",
                ErrorCategory::Lifecycle,
                ErrorSeverity::Error,
                false, // not retryable
                format!("Failed to finish session: {:?}", e),
            )
        })?;

    Ok(finish_view(outcome))
}

#[cfg(test)]
mod tests {
    use remtene_application::{
        Completion, DeliveryKind, DirectDeliveryReason, FinalizationWarning, FinishOutcome,
    };

    use super::*;

    #[test]
    fn missing_llm_configuration_is_exposed_only_as_a_fixed_notice() {
        let view = finish_view(FinishOutcome::Completed(Completion {
            final_text: "private transcript".to_owned(),
            delivery: DeliveryKind::Inserted,
            direct_delivery_reason: Some(DirectDeliveryReason::LlmNotConfigured),
            warnings: Vec::<FinalizationWarning>::new(),
        }));

        assert_eq!(view.status, "delivered");
        assert_eq!(view.contract_version, CONTRACT_VERSION);
        assert_eq!(view.delivery, Some("inserted"));
        assert_eq!(view.notice, Some("llm_not_configured"));
        let serialized = serde_json::to_string(&view).expect("serialize finish view");
        assert!(!serialized.contains("private transcript"));
    }

    #[test]
    fn no_speech_is_exposed_as_a_content_free_discarded_result() {
        let view = finish_view(FinishOutcome::NoSpeech);

        assert_eq!(view.status, "discarded");
        assert_eq!(view.delivery, None);
        assert_eq!(view.notice, None);
        assert_eq!(view.failure, None);
    }
}
