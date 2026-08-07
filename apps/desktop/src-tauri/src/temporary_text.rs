//! 临时文本框交付面（UI-020）
//!
//! 当锚点不可验证、写入结果不确定或 LLM 结果不可信时，最终文本只能进入这个一次性
//! 回退窗口，不得写进任何外部应用。事件只投给 `temporary-text-box` 窗口，其他窗口既
//! 无 Capability 也收不到内容。

use std::sync::{Arc, Mutex, MutexGuard};

use remtene_application::ports::{
    ClipboardTextWriter, LifecycleFence, PortError, PortFuture, TemporaryTextOutput,
    TemporaryTextStatus,
};
use remtene_contracts::{AppError, CONTRACT_VERSION, ErrorCategory, ErrorSeverity};
use remtene_domain::{DeliveryId, SessionId};
use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, Emitter, EventTarget, Manager, State, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};
use uuid::Uuid;

use crate::composition_root::CompositionRoot;
use crate::{
    TEMPORARY_TEXT_BOX_LABEL, WindowCommandClass, authorize_window, ensure_minimum_inner_size,
};

const TEMPORARY_TEXT_DELIVERED_EVENT: &str = "temporary-text:delivered";
const BOX_WIDTH_LOGICAL: f64 = 420.0;
const BOX_HEIGHT_LOGICAL: f64 = 240.0;

/// 一次性回退交付载荷。
///
/// 只包含用户已经看到的最终文本与回退原因；音频路径、目标身份、选区和请求细节不进入
/// 这个边界。
#[derive(Clone, Serialize)]
pub(crate) struct TemporaryTextDelivery {
    contract_version: u16,
    delivery_id: String,
    status_code: &'static str,
    final_text: String,
}

/// Explicit user intent to copy the currently visible fallback text.
///
/// The Renderer supplies only a version and the delivery identity it is
/// displaying. The text itself remains owned by Rust pending state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TemporaryTextCopyAllCommand {
    contract_version: u16,
    delivery_id: String,
}

/// Content-free acknowledgement of one clipboard write.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TemporaryTextCopyAllResult {
    contract_version: u16,
    delivery_id: String,
}

pub struct TemporaryTextBoxController {
    app: AppHandle,
    /// 序列化窗口的创建与销毁，避免两次交付并发建同一个 label 的窗口。
    build_lock: Mutex<()>,
    /// 当前待显示的交付内容。
    ///
    /// 窗口是按需新建的，它的 webview 可能在 emit 之后才挂上监听，光靠事件会丢内容。
    /// 因此内容同时留在这里，供窗口挂载后主动拉取；销毁窗口时一并清除。
    pending: Mutex<Option<TemporaryTextDelivery>>,
}

impl TemporaryTextBoxController {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            build_lock: Mutex::new(()),
            pending: Mutex::new(None),
        }
    }

    fn pending(&self) -> MutexGuard<'_, Option<TemporaryTextDelivery>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 取得可用的临时文本框窗口，必要时重建。
    ///
    /// 关闭该窗口即销毁它（用户语义：这个东西没了，文字留在历史里），所以每次交付都不能
    /// 假设窗口还在——必须按需重建，否则一次关闭就会让后续所有回退交付失败。
    fn window(&self) -> Result<WebviewWindow, PortError> {
        let _serialized = self
            .build_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = self.app.get_webview_window(TEMPORARY_TEXT_BOX_LABEL) {
            ensure_minimum_inner_size(&existing, BOX_WIDTH_LOGICAL, BOX_HEIGHT_LOGICAL);
            return Ok(existing);
        }
        build_temporary_text_box(&self.app)
            .map_err(|_| port_error("temporary_text.window_unavailable", true))
    }

    /// 用户关闭时销毁窗口，并丢掉这一次的文本。
    fn dismiss(&self) -> Result<(), PortError> {
        let _serialized = self
            .build_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 先清内容再销毁窗口：即使销毁失败，这一次的文本也不会被下一个窗口读到。
        *self.pending() = None;
        if let Some(window) = self.app.get_webview_window(TEMPORARY_TEXT_BOX_LABEL) {
            window
                .destroy()
                .map_err(|_| port_error("temporary_text.dismiss_failed", true))?;
        }
        Ok(())
    }

    /// 窗口挂载后主动拉取待显示内容，弥补「新建窗口晚于 emit」的竞态。
    fn take_pending(&self) -> Option<TemporaryTextDelivery> {
        self.pending().clone()
    }

    /// Reopens the current one-time fallback only after an explicit user
    /// recovery action. Automatic delivery still uses the non-focusing path;
    /// here focus is intentional because the user clicked “查看临时文字”.
    pub(crate) fn show_pending(&self) -> Result<(), PortError> {
        if self.pending().is_none() {
            return Err(port_error("temporary_text.pending_missing", false));
        }
        let window = self.window()?;
        window
            .show()
            .map_err(|_| port_error("temporary_text.show_failed", true))?;
        window
            .set_focus()
            .map_err(|_| port_error("temporary_text.focus_failed", true))
    }

    fn deliver(
        &self,
        delivery_id: DeliveryId,
        final_text: String,
        status: TemporaryTextStatus,
        lifecycle: LifecycleFence,
    ) -> Result<(), PortError> {
        // 退出过程中不再制造新的可见交付面，否则关闭流程会与一次性交付竞争。
        let Some(_commit) = lifecycle.begin_commit() else {
            return Err(port_error("temporary_text.lifecycle_closed", false));
        };

        let payload = TemporaryTextDelivery {
            contract_version: CONTRACT_VERSION,
            delivery_id: delivery_id.as_uuid().to_string(),
            status_code: status_code(status),
            final_text,
        };
        // 先备好内容再建窗口：新窗口挂载后会主动拉取，不依赖 emit 的到达时机。
        *self.pending() = Some(payload.clone());

        let window = self.window()?;
        // 事件仍然发送，覆盖窗口已存在（监听已就绪）的场景，让内容立即刷新。
        let _ = self.app.emit_to(
            EventTarget::webview_window(TEMPORARY_TEXT_BOX_LABEL),
            TEMPORARY_TEXT_DELIVERED_EVENT,
            payload,
        );

        window
            .show()
            .map_err(|_| port_error("temporary_text.show_failed", true))
    }
}

impl TemporaryTextOutput for TemporaryTextBoxController {
    fn show(
        &self,
        _session_id: SessionId,
        delivery_id: DeliveryId,
        final_text: String,
        status: TemporaryTextStatus,
        lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move { self.deliver(delivery_id, final_text, status, lifecycle) })
    }
}

/// 按需创建临时文本框窗口，交付时才显示。
///
/// 窗口不抢前台焦点，避免回退本身把用户从原应用里拉出来。窗口不使用 macOS
/// 原生标题栏：所有状态和关闭动作都在受控 Renderer 内，避免再显示一层空的标题框。
/// 关闭即销毁是有意的产品语义：这一次的回退文本随窗口一起消失，只有历史记录保留内容。
/// 因此调用方必须每次交付都走 `window()` 按需重建，不能缓存窗口句柄。
fn build_temporary_text_box(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let window = WebviewWindowBuilder::new(
        app,
        TEMPORARY_TEXT_BOX_LABEL,
        WebviewUrl::App("index.html?surface=temporary-text-box".into()),
    )
    .title("辑语 · 临时文本")
    .inner_size(BOX_WIDTH_LOGICAL, BOX_HEIGHT_LOGICAL)
    .min_inner_size(BOX_WIDTH_LOGICAL, BOX_HEIGHT_LOGICAL)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .decorations(false)
    .transparent(true)
    .shadow(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .focused(false)
    .visible(false)
    .build()?;
    ensure_minimum_inner_size(&window, BOX_WIDTH_LOGICAL, BOX_HEIGHT_LOGICAL);
    Ok(window)
}

/// 窗口挂载后拉取本次待显示的交付内容。
///
/// 窗口按需新建，其 webview 可能在 emit 之后才挂上监听，因此必须提供拉取路径。
#[tauri::command]
pub(crate) fn temporary_text_get_pending(
    window: WebviewWindow,
    controller: State<'_, Arc<TemporaryTextBoxController>>,
) -> Result<Option<TemporaryTextDelivery>, AppError> {
    authorize_window(window.label(), WindowCommandClass::TemporaryTextControl)?;
    Ok(controller.take_pending())
}

/// 用户在临时文本框里点「关闭」。窗口连同这一次的文本一起销毁。
#[tauri::command]
pub(crate) fn temporary_text_dismiss(
    window: WebviewWindow,
    controller: State<'_, Arc<TemporaryTextBoxController>>,
) -> Result<(), AppError> {
    authorize_window(window.label(), WindowCommandClass::TemporaryTextControl)?;
    controller.dismiss().map_err(|error| {
        AppError::new(
            &error.code,
            ErrorCategory::Lifecycle,
            ErrorSeverity::Error,
            error.retryable,
            &error.safe_message_key,
        )
    })
}

/// 用户明确点击“复制全部”。
///
/// 调用方只能声明自己正在查看哪一次 Delivery；正文从 Rust 当前 pending 克隆。
/// 克隆完成后立即释放 pending 锁，再执行可能阻塞的原生剪贴板写入。成功不会清除
/// pending，用户仍可继续查看、再次复制或自行关闭临时文字框。
#[tauri::command]
pub(crate) async fn temporary_text_copy_all(
    command: TemporaryTextCopyAllCommand,
    window: WebviewWindow,
    controller: State<'_, Arc<TemporaryTextBoxController>>,
    root: State<'_, CompositionRoot>,
) -> Result<TemporaryTextCopyAllResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::TemporaryTextControl)?;
    copy_pending_text(
        &controller.pending,
        root.clipboard_text_writer.as_ref(),
        command,
    )
    .await
}

async fn copy_pending_text(
    pending: &Mutex<Option<TemporaryTextDelivery>>,
    writer: &dyn ClipboardTextWriter,
    command: TemporaryTextCopyAllCommand,
) -> Result<TemporaryTextCopyAllResult, AppError> {
    if command.contract_version != CONTRACT_VERSION {
        return Err(copy_command_error(
            "ipc.contract_version_mismatch",
            ErrorCategory::Security,
            false,
        ));
    }

    let requested_delivery_id = Uuid::parse_str(&command.delivery_id).map_err(|_| {
        copy_command_error(
            "temporary_text.delivery_id_invalid",
            ErrorCategory::Security,
            false,
        )
    })?;

    // Keep the mutex scope deliberately narrow. Clipboard writes can block on
    // the native pasteboard and must never hold pending state hostage.
    let (delivery_id, final_text) = {
        let current = pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = current.as_ref().ok_or_else(|| {
            copy_command_error(
                "temporary_text.pending_missing",
                ErrorCategory::Lifecycle,
                false,
            )
        })?;
        let current_delivery_id = Uuid::parse_str(&current.delivery_id).map_err(|_| {
            copy_command_error(
                "temporary_text.pending_invalid",
                ErrorCategory::Lifecycle,
                false,
            )
        })?;
        if current_delivery_id != requested_delivery_id {
            return Err(copy_command_error(
                "temporary_text.delivery_stale",
                ErrorCategory::Lifecycle,
                false,
            ));
        }
        (current.delivery_id.clone(), current.final_text.clone())
    };

    writer.write_text(final_text).await.map_err(|error| {
        // Do not forward adapter strings into the IPC error. The Port contract
        // requires content-free errors, but this fixed projection keeps the UI
        // boundary safe even if a future implementation violates that rule.
        copy_command_error(
            "temporary_text.copy_failed",
            ErrorCategory::Delivery,
            error.retryable,
        )
    })?;

    Ok(TemporaryTextCopyAllResult {
        contract_version: CONTRACT_VERSION,
        delivery_id,
    })
}

fn copy_command_error(code: &'static str, category: ErrorCategory, retryable: bool) -> AppError {
    AppError::new(
        code,
        category,
        ErrorSeverity::Error,
        retryable,
        format!("errors.{code}"),
    )
}

const fn status_code(status: TemporaryTextStatus) -> &'static str {
    match status {
        TemporaryTextStatus::NotInserted => "temporary_text.not_inserted",
        TemporaryTextStatus::Indeterminate => "temporary_text.indeterminate",
        TemporaryTextStatus::LlmFallback => "temporary_text.llm_fallback",
    }
}

fn port_error(code: &str, retryable: bool) -> PortError {
    PortError {
        code: code.to_owned(),
        safe_message_key: format!("errors.{code}"),
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct RecordingClipboardTextWriter {
        writes: Mutex<Vec<String>>,
        result: Result<(), PortError>,
    }

    impl RecordingClipboardTextWriter {
        fn succeeding() -> Self {
            Self {
                writes: Mutex::new(Vec::new()),
                result: Ok(()),
            }
        }

        fn failing(error: PortError) -> Self {
            Self {
                writes: Mutex::new(Vec::new()),
                result: Err(error),
            }
        }
    }

    impl ClipboardTextWriter for RecordingClipboardTextWriter {
        fn write_text(&self, text: String) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async move {
                self.writes
                    .lock()
                    .expect("clipboard writer lock")
                    .push(text);
                self.result.clone()
            })
        }
    }

    struct PendingLockCheckingWriter<'a> {
        pending: &'a Mutex<Option<TemporaryTextDelivery>>,
        observed_text: Mutex<Option<String>>,
    }

    impl ClipboardTextWriter for PendingLockCheckingWriter<'_> {
        fn write_text(&self, text: String) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async move {
                let _pending_guard = self
                    .pending
                    .try_lock()
                    .expect("pending lock must be released before clipboard I/O");
                *self.observed_text.lock().expect("observed text lock") = Some(text);
                Ok(())
            })
        }
    }

    fn pending_delivery(delivery_id: Uuid, final_text: &str) -> TemporaryTextDelivery {
        TemporaryTextDelivery {
            contract_version: CONTRACT_VERSION,
            delivery_id: delivery_id.to_string(),
            status_code: status_code(TemporaryTextStatus::NotInserted),
            final_text: final_text.to_owned(),
        }
    }

    fn copy_command(delivery_id: impl Into<String>) -> TemporaryTextCopyAllCommand {
        TemporaryTextCopyAllCommand {
            contract_version: CONTRACT_VERSION,
            delivery_id: delivery_id.into(),
        }
    }

    #[test]
    fn status_codes_stay_distinct_and_content_free() {
        let codes = [
            status_code(TemporaryTextStatus::NotInserted),
            status_code(TemporaryTextStatus::Indeterminate),
            status_code(TemporaryTextStatus::LlmFallback),
        ];
        assert_eq!(
            codes.len(),
            codes
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        );
        assert!(codes.iter().all(|code| code.starts_with("temporary_text.")));
    }

    #[test]
    fn delivery_payload_carries_only_the_final_text_and_reason() {
        let payload = TemporaryTextDelivery {
            contract_version: CONTRACT_VERSION,
            delivery_id: DeliveryId::new().as_uuid().to_string(),
            status_code: status_code(TemporaryTextStatus::NotInserted),
            final_text: "回退文本".to_owned(),
        };
        let serialized = serde_json::to_string(&payload).expect("payload must serialize");
        for forbidden in ["audio", "target", "selected", "api_key", "session_id"] {
            assert!(
                !serialized.contains(forbidden),
                "payload must not expose {forbidden}"
            );
        }
        assert!(serialized.contains("回退文本"));
    }

    #[test]
    fn copy_rejects_contract_version_before_touching_pending() {
        let pending = Mutex::new(None);
        let writer = RecordingClipboardTextWriter::succeeding();
        let command = TemporaryTextCopyAllCommand {
            contract_version: CONTRACT_VERSION + 1,
            delivery_id: Uuid::new_v4().to_string(),
        };

        let error = futures::executor::block_on(copy_pending_text(&pending, &writer, command))
            .expect_err("wrong contract version must fail");

        assert_eq!(error.code, "ipc.contract_version_mismatch");
        assert!(writer.writes.lock().expect("writer lock").is_empty());
    }

    #[test]
    fn copy_rejects_invalid_and_stale_delivery_ids() {
        let current_id = Uuid::new_v4();
        let pending = Mutex::new(Some(pending_delivery(current_id, "正文")));
        let writer = RecordingClipboardTextWriter::succeeding();

        let invalid = futures::executor::block_on(copy_pending_text(
            &pending,
            &writer,
            copy_command("not-a-uuid"),
        ))
        .expect_err("invalid UUID must fail");
        assert_eq!(invalid.code, "temporary_text.delivery_id_invalid");

        let stale = futures::executor::block_on(copy_pending_text(
            &pending,
            &writer,
            copy_command(Uuid::new_v4().to_string()),
        ))
        .expect_err("stale delivery must fail");
        assert_eq!(stale.code, "temporary_text.delivery_stale");
        assert!(writer.writes.lock().expect("writer lock").is_empty());
    }

    #[test]
    fn copy_rejects_when_no_pending_delivery_exists() {
        let pending = Mutex::new(None);
        let writer = RecordingClipboardTextWriter::succeeding();

        let error = futures::executor::block_on(copy_pending_text(
            &pending,
            &writer,
            copy_command(Uuid::new_v4().to_string()),
        ))
        .expect_err("missing pending delivery must fail");

        assert_eq!(error.code, "temporary_text.pending_missing");
        assert!(writer.writes.lock().expect("writer lock").is_empty());
    }

    #[test]
    fn successful_copy_writes_rust_owned_text_and_keeps_pending() {
        let delivery_id = Uuid::new_v4();
        let private_text = "只从 Rust pending 取得的正文";
        let pending = Mutex::new(Some(pending_delivery(delivery_id, private_text)));
        let writer = RecordingClipboardTextWriter::succeeding();

        let result = futures::executor::block_on(copy_pending_text(
            &pending,
            &writer,
            copy_command(delivery_id.to_string()),
        ))
        .expect("copy succeeds");

        assert_eq!(result.contract_version, CONTRACT_VERSION);
        assert_eq!(result.delivery_id, delivery_id.to_string());
        assert_eq!(
            writer.writes.lock().expect("writer lock").as_slice(),
            &[private_text.to_owned()]
        );
        assert_eq!(
            pending
                .lock()
                .expect("pending lock")
                .as_ref()
                .map(|delivery| delivery.final_text.as_str()),
            Some(private_text)
        );

        let result_json = serde_json::to_string(&result).expect("serialize copy result");
        assert!(!result_json.contains(private_text));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&result_json)
                .expect("copy result JSON")
                .as_object()
                .expect("copy result object")
                .len(),
            2
        );
    }

    #[test]
    fn copy_releases_pending_lock_before_clipboard_io() {
        let delivery_id = Uuid::new_v4();
        let private_text = "锁外写入剪贴板";
        let pending = Mutex::new(Some(pending_delivery(delivery_id, private_text)));
        let writer = PendingLockCheckingWriter {
            pending: &pending,
            observed_text: Mutex::new(None),
        };

        futures::executor::block_on(copy_pending_text(
            &pending,
            &writer,
            copy_command(delivery_id.to_string()),
        ))
        .expect("copy succeeds without holding pending lock");

        assert_eq!(
            writer
                .observed_text
                .lock()
                .expect("observed text lock")
                .as_deref(),
            Some(private_text)
        );
    }

    #[test]
    fn writer_failure_cannot_put_text_or_adapter_details_in_app_error() {
        let delivery_id = Uuid::new_v4();
        let private_text = "不得进入错误 DTO 的正文";
        let pending = Mutex::new(Some(pending_delivery(delivery_id, private_text)));
        let writer = RecordingClipboardTextWriter::failing(PortError {
            code: format!("malicious.{private_text}"),
            safe_message_key: format!("malicious.{private_text}"),
            retryable: true,
        });

        let error = futures::executor::block_on(copy_pending_text(
            &pending,
            &writer,
            copy_command(delivery_id.to_string()),
        ))
        .expect_err("writer failure must reach the caller");
        let serialized = serde_json::to_string(&error).expect("serialize AppError");

        assert_eq!(error.code, "temporary_text.copy_failed");
        assert!(error.retryable);
        assert!(!serialized.contains(private_text));
        assert!(!serialized.contains("malicious"));
        assert!(pending.lock().expect("pending lock").is_some());
    }

    #[test]
    fn copy_command_and_result_dtos_never_carry_text() {
        let delivery_id = Uuid::new_v4().to_string();
        let command = copy_command(delivery_id.clone());
        let result = TemporaryTextCopyAllResult {
            contract_version: CONTRACT_VERSION,
            delivery_id,
        };

        let command_value = serde_json::to_value(command).expect("serialize command");
        let result_value = serde_json::to_value(result).expect("serialize result");
        assert_eq!(
            command_value
                .as_object()
                .expect("command object")
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            ["contract_version".to_owned(), "delivery_id".to_owned()]
                .into_iter()
                .collect()
        );
        assert_eq!(
            result_value
                .as_object()
                .expect("result object")
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            ["contract_version".to_owned(), "delivery_id".to_owned()]
                .into_iter()
                .collect()
        );

        let command_with_injected_text = serde_json::json!({
            "contract_version": CONTRACT_VERSION,
            "delivery_id": Uuid::new_v4().to_string(),
            "final_text": "Renderer 不得提交正文",
        });
        assert!(
            serde_json::from_value::<TemporaryTextCopyAllCommand>(command_with_injected_text)
                .is_err(),
            "unknown text fields must be rejected rather than ignored"
        );
    }
}
