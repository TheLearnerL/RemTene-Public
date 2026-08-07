//! Session-scoped, content-free recovery feedback.
//!
//! The Recording HUD reports activity only. Once a workflow has failed or
//! completed through a safe fallback, Application selects one of four approved
//! semantic notifications and this adapter owns the independent 420×120
//! feedback window. No transcript, target, provider response, path, or debug
//! string crosses this boundary.

use std::sync::{Arc, Mutex, MutexGuard};

use remtene_application::ports::{
    PortError, PortFuture, UserNotificationKind, UserNotificationPort,
};
use remtene_contracts::{
    AppError, CONTRACT_VERSION, ControlPanelNavigationEvent, ControlPanelNavigationTarget,
    ErrorCategory, ErrorSeverity, UserNotification, UserNotificationCode,
};
use remtene_domain::SessionId;
use tauri::{
    AppHandle, Emitter, EventTarget, Manager, State, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};

use crate::temporary_text::TemporaryTextBoxController;
use crate::{
    CONTROL_PANEL_LABEL, SESSION_FEEDBACK_LABEL, WindowCommandClass, authorize_window,
    commands::permissions::open_macos_privacy_pane, ensure_minimum_inner_size,
};

const NOTIFICATION_RAISED_EVENT: &str = "notification:raised";
const CONTROL_PANEL_NAVIGATE_EVENT: &str = "control-panel:navigate";
const FEEDBACK_WIDTH_LOGICAL: f64 = 420.0;
const FEEDBACK_HEIGHT_LOGICAL: f64 = 120.0;

pub struct SessionFeedbackController {
    app: AppHandle,
    /// Serializes replacement, user actions, and window reconstruction.
    operation_lock: Mutex<()>,
    /// Current action authority. The renderer must echo this exact, validated
    /// payload; stale or cross-session commands fail closed.
    pending: Mutex<Option<UserNotification>>,
}

impl SessionFeedbackController {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            operation_lock: Mutex::new(()),
            pending: Mutex::new(None),
        }
    }

    fn pending(&self) -> MutexGuard<'_, Option<UserNotification>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn operation(&self) -> MutexGuard<'_, ()> {
        self.operation_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn window(&self) -> Result<WebviewWindow, PortError> {
        if let Some(existing) = self.app.get_webview_window(SESSION_FEEDBACK_LABEL) {
            ensure_minimum_inner_size(&existing, FEEDBACK_WIDTH_LOGICAL, FEEDBACK_HEIGHT_LOGICAL);
            return Ok(existing);
        }
        build_session_feedback(&self.app)
            .map_err(|_| feedback_port_error("notification.window_unavailable", true))
    }

    fn raise(&self, session_id: SessionId, kind: UserNotificationKind) -> Result<(), PortError> {
        let _operation = self.operation();
        let payload = user_notification(session_id, kind);
        *self.pending() = Some(payload);

        let window = self.window()?;
        // A newly created webview may mount after this emit. The active payload
        // also remains available through `notification_get_pending`.
        let _ = self.app.emit_to(
            EventTarget::webview_window(SESSION_FEEDBACK_LABEL),
            NOTIFICATION_RAISED_EVENT,
            payload,
        );
        window
            .show()
            .map_err(|_| feedback_port_error("notification.show_failed", true))?;
        window
            .set_focus()
            .map_err(|_| feedback_port_error("notification.focus_failed", true))
    }

    fn get_pending(&self) -> Option<UserNotification> {
        *self.pending()
    }

    /// Native window close is a dismissal, not a pause. Clearing the pending
    /// authority prevents a later reconstructed webview from reviving an old
    /// recovery action.
    pub(crate) fn clear_pending_after_user_close(&self) {
        let _operation = self.operation();
        *self.pending() = None;
    }

    fn apply_action(
        &self,
        requested: UserNotification,
        temporary_text: &TemporaryTextBoxController,
    ) -> Result<(), AppError> {
        let _operation = self.operation();
        if requested.contract_version != CONTRACT_VERSION {
            return Err(feedback_error(
                "ipc.contract_version_mismatch",
                ErrorCategory::Security,
                false,
            ));
        }
        let current = self.pending().as_ref().copied();
        if current != Some(requested) {
            return Err(feedback_error(
                "notification.stale",
                ErrorCategory::Lifecycle,
                false,
            ));
        }

        match requested.code {
            UserNotificationCode::MicrophonePermission => {
                open_macos_privacy_pane("Privacy_Microphone")?;
            }
            UserNotificationCode::Asr => {
                self.show_control_panel(ControlPanelNavigationTarget::ModelAsr)?;
            }
            UserNotificationCode::Llm => {
                self.show_control_panel(ControlPanelNavigationTarget::ModelTextService)?;
            }
            UserNotificationCode::Delivery => {
                temporary_text.show_pending().map_err(port_to_app_error)?;
            }
        }

        // Invalidate the action authority before removing the surface. If
        // destruction fails after the action committed, a repeated click must
        // still fail rather than dispatching the recovery action twice.
        *self.pending() = None;
        if let Some(window) = self.app.get_webview_window(SESSION_FEEDBACK_LABEL)
            && window.destroy().is_err()
        {
            let _ = window.hide();
        }
        Ok(())
    }

    fn show_control_panel(&self, target: ControlPanelNavigationTarget) -> Result<(), AppError> {
        let Some(window) = self.app.get_webview_window(CONTROL_PANEL_LABEL) else {
            return Err(feedback_error(
                "notification.control_panel_unavailable",
                ErrorCategory::Lifecycle,
                true,
            ));
        };
        window.unminimize().map_err(|_| {
            feedback_error(
                "notification.control_panel_unavailable",
                ErrorCategory::Lifecycle,
                true,
            )
        })?;
        window.show().map_err(|_| {
            feedback_error(
                "notification.control_panel_unavailable",
                ErrorCategory::Lifecycle,
                true,
            )
        })?;
        self.app
            .emit_to(
                EventTarget::webview_window(CONTROL_PANEL_LABEL),
                CONTROL_PANEL_NAVIGATE_EVENT,
                ControlPanelNavigationEvent {
                    contract_version: CONTRACT_VERSION,
                    target,
                },
            )
            .map_err(|_| {
                feedback_error(
                    "notification.navigation_failed",
                    ErrorCategory::Lifecycle,
                    true,
                )
            })?;
        window.set_focus().map_err(|_| {
            feedback_error(
                "notification.control_panel_unavailable",
                ErrorCategory::Lifecycle,
                true,
            )
        })
    }
}

impl UserNotificationPort for SessionFeedbackController {
    fn raise(
        &self,
        session_id: SessionId,
        kind: UserNotificationKind,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move { self.raise(session_id, kind) })
    }
}

fn build_session_feedback(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let window = WebviewWindowBuilder::new(
        app,
        SESSION_FEEDBACK_LABEL,
        WebviewUrl::App("index.html?surface=session-feedback".into()),
    )
    .title("辑语 · 错误反馈")
    .inner_size(FEEDBACK_WIDTH_LOGICAL, FEEDBACK_HEIGHT_LOGICAL)
    .min_inner_size(FEEDBACK_WIDTH_LOGICAL, FEEDBACK_HEIGHT_LOGICAL)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .decorations(false)
    .transparent(true)
    .shadow(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .visible(false)
    .build()?;
    ensure_minimum_inner_size(&window, FEEDBACK_WIDTH_LOGICAL, FEEDBACK_HEIGHT_LOGICAL);
    Ok(window)
}

#[tauri::command]
pub(crate) fn notification_get_pending(
    window: WebviewWindow,
    controller: State<'_, Arc<SessionFeedbackController>>,
) -> Result<Option<UserNotification>, AppError> {
    authorize_window(window.label(), WindowCommandClass::NotificationControl)?;
    Ok(controller.get_pending())
}

#[tauri::command]
pub(crate) fn notification_apply_action(
    window: WebviewWindow,
    notification: UserNotification,
    controller: State<'_, Arc<SessionFeedbackController>>,
    temporary_text: State<'_, Arc<TemporaryTextBoxController>>,
) -> Result<(), AppError> {
    authorize_window(window.label(), WindowCommandClass::NotificationControl)?;
    controller.apply_action(notification, &temporary_text)
}

fn user_notification(session_id: SessionId, kind: UserNotificationKind) -> UserNotification {
    let code = match kind {
        UserNotificationKind::MicrophonePermission => UserNotificationCode::MicrophonePermission,
        UserNotificationKind::Asr => UserNotificationCode::Asr,
        UserNotificationKind::Llm => UserNotificationCode::Llm,
        UserNotificationKind::Delivery => UserNotificationCode::Delivery,
    };
    UserNotification {
        contract_version: CONTRACT_VERSION,
        session_id: session_id.as_uuid(),
        code,
    }
}

fn port_to_app_error(error: PortError) -> AppError {
    AppError::new(
        &error.code,
        ErrorCategory::Delivery,
        ErrorSeverity::Error,
        error.retryable,
        &error.safe_message_key,
    )
}

fn feedback_port_error(code: &str, retryable: bool) -> PortError {
    PortError {
        code: code.to_owned(),
        safe_message_key: format!("errors.{code}"),
        retryable,
    }
}

fn feedback_error(code: &str, category: ErrorCategory, retryable: bool) -> AppError {
    AppError::new(
        code,
        category,
        ErrorSeverity::Error,
        retryable,
        format!("errors.{code}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_event_name_is_valid_for_the_tauri_runtime() {
        assert!(!NOTIFICATION_RAISED_EVENT.is_empty());
        assert!(NOTIFICATION_RAISED_EVENT.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '/' | ':' | '_')
        }));
    }

    #[test]
    fn notification_projection_is_content_free_and_uses_approved_codes() {
        let session_id = SessionId::new();
        let expected = [
            (
                UserNotificationKind::MicrophonePermission,
                "\"notification.permission_microphone\"",
            ),
            (UserNotificationKind::Asr, "\"notification.asr\""),
            (UserNotificationKind::Llm, "\"notification.llm\""),
            (UserNotificationKind::Delivery, "\"notification.delivery\""),
        ];

        for (kind, code) in expected {
            let serialized =
                serde_json::to_string(&user_notification(session_id, kind)).expect("serialize");
            assert!(serialized.contains(code));
            for forbidden in [
                "final_text",
                "transcript",
                "audio",
                "target",
                "selection",
                "provider",
                "path",
            ] {
                assert!(!serialized.contains(forbidden));
            }
        }
    }

    #[test]
    fn navigation_targets_select_the_exact_model_tab() {
        let asr = serde_json::to_string(&ControlPanelNavigationEvent {
            contract_version: CONTRACT_VERSION,
            target: ControlPanelNavigationTarget::ModelAsr,
        })
        .expect("serialize ASR route");
        let llm = serde_json::to_string(&ControlPanelNavigationEvent {
            contract_version: CONTRACT_VERSION,
            target: ControlPanelNavigationTarget::ModelTextService,
        })
        .expect("serialize LLM route");
        assert!(asr.contains("\"model.asr\""));
        assert!(llm.contains("\"model.text_service\""));
    }
}
