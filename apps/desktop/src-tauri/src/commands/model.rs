//! Local ASR model actions for startup and the Control Panel.

use std::sync::Arc;

use remtene_application::{AsrHealthCheckError, AsrModelSwitchError};
use remtene_contracts::{AppError, AppSnapshot, ErrorCategory, ErrorSeverity, LocalAsrModel};
use remtene_domain::AsrEngine;
use tauri::{AppHandle, Emitter, EventTarget, Manager, State, WebviewWindow};

use crate::{
    AppRuntime, WindowCommandClass, authorize_window, composition_root::CompositionRoot,
    recording_hud::RecordingHudController, refresh_public_snapshot_with_asr_preference,
};

pub(crate) const APP_SNAPSHOT_CHANGED_EVENT: &str = "app:snapshot-changed";

#[tauri::command]
pub async fn model_check_health(
    window: WebviewWindow,
    runtime: State<'_, AppRuntime>,
    composition: State<'_, CompositionRoot>,
    recording_hud: State<'_, Arc<RecordingHudController>>,
) -> Result<AppSnapshot, AppError> {
    authorize_window(window.label(), WindowCommandClass::Model)?;
    check_and_refresh(runtime.inner(), composition.inner(), recording_hud.inner())
        .await
        .map_err(health_error)
}

/// Switch the explicit local ASR selection for future Sessions.
///
/// Renderer supplies only a closed enum. Core resolves the fixed package, verifies hashes,
/// prewarms it and persists the new selection under the shared Session configuration gate.
#[tauri::command]
pub async fn model_switch_engine(
    window: WebviewWindow,
    runtime: State<'_, AppRuntime>,
    composition: State<'_, CompositionRoot>,
    recording_hud: State<'_, Arc<RecordingHudController>>,
    engine: LocalAsrModel,
) -> Result<AppSnapshot, AppError> {
    authorize_window(window.label(), WindowCommandClass::Model)?;
    let engine = match engine {
        LocalAsrModel::Qwen => AsrEngine::Qwen,
        LocalAsrModel::Whisper => AsrEngine::Whisper,
    };
    let outcome = composition
        .asr_health
        .switch_to(engine)
        .await
        .map_err(switch_error)?;
    record_outcome(composition.inner(), outcome);
    let snapshot = refresh_public_snapshot_with_asr_preference(
        runtime.inner(),
        composition.inner(),
        recording_hud.inner(),
        outcome.preference,
    )
    .await;
    let _ = window.emit(APP_SNAPSHOT_CHANGED_EVENT, snapshot.clone());
    Ok(snapshot)
}

/// Run the same model verification once after the desktop composition is ready.
///
/// The task is asynchronous so model loading and warmup do not block creation of the Control
/// Panel. If the Renderer mounts before completion it first sees `discovering`, then receives the
/// refreshed public snapshot. If it mounts later, its ordinary snapshot read observes the stored
/// result, so there is no event-order dependency.
pub(crate) fn spawn_startup_health_check(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let result = {
            let runtime = app.state::<AppRuntime>();
            let composition = app.state::<CompositionRoot>();
            let recording_hud = app.state::<Arc<RecordingHudController>>();
            check_and_refresh(runtime.inner(), composition.inner(), recording_hud.inner()).await
        };

        match result {
            Ok(snapshot) => {
                if let Err(error) = app.emit_to(
                    EventTarget::webview_window(crate::CONTROL_PANEL_LABEL),
                    APP_SNAPSHOT_CHANGED_EVENT,
                    snapshot,
                ) {
                    eprintln!("startup model health event failed: {error}");
                }
            }
            Err(error) => {
                eprintln!("startup model health check did not complete: {error}");
            }
        }
    });
}

async fn check_and_refresh(
    runtime: &AppRuntime,
    composition: &CompositionRoot,
    recording_hud: &Arc<RecordingHudController>,
) -> Result<AppSnapshot, AsrHealthCheckError> {
    let outcome = composition.asr_health.check().await?;
    record_outcome(composition, outcome);
    Ok(refresh_public_snapshot_with_asr_preference(
        runtime,
        composition,
        recording_hud,
        outcome.preference,
    )
    .await)
}

fn record_outcome(
    composition: &CompositionRoot,
    outcome: remtene_application::AsrHealthCheckOutcome,
) {
    if let Some(health) = outcome.qwen {
        composition.asr_status.record(AsrEngine::Qwen, health);
    }
    if let Some(health) = outcome.whisper {
        composition.asr_status.record(AsrEngine::Whisper, health);
    }
}

/// Open the active local-model directory owned by this exact application build.
///
/// The Renderer deliberately supplies neither an engine nor a filesystem path.
/// Qwen and Whisper share the same verified `models/active` root, resolved from
/// the App Group identity compiled into the running desktop application.
#[tauri::command]
pub fn model_open_directory(window: WebviewWindow) -> Result<(), AppError> {
    authorize_window(window.label(), WindowCommandClass::Model)?;
    open_active_models_directory()
}

#[cfg(target_os = "macos")]
fn open_active_models_directory() -> Result<(), AppError> {
    let shared_paths = remtene_platform::asr_shared_data::resolve_macos_app_group(
        crate::asr_runtime::compiled_macos_app_group_id(),
    )
    .map_err(|_| {
        AppError::new(
            "model.directory_unavailable",
            ErrorCategory::Asr,
            ErrorSeverity::Error,
            true,
            "errors.model.directory_unavailable",
        )
    })?;

    let opened = std::process::Command::new("/usr/bin/open")
        .arg(shared_paths.active_models_root())
        .status()
        .ok()
        .is_some_and(|status| status.success());
    if opened {
        Ok(())
    } else {
        Err(AppError::new(
            "model.directory_open_failed",
            ErrorCategory::Asr,
            ErrorSeverity::Error,
            true,
            "errors.model.directory_open_failed",
        ))
    }
}

#[cfg(not(target_os = "macos"))]
fn open_active_models_directory() -> Result<(), AppError> {
    Err(AppError::new(
        "model.directory_unsupported_platform",
        ErrorCategory::Asr,
        ErrorSeverity::Error,
        false,
        "errors.model.directory_unsupported_platform",
    ))
}

fn health_error(error: AsrHealthCheckError) -> AppError {
    match error {
        AsrHealthCheckError::Busy => AppError::new(
            "asr.health_busy",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Warning,
            true,
            "errors.asr.worker_busy",
        ),
        AsrHealthCheckError::Quitting => AppError::new(
            "asr.health_quitting",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Warning,
            false,
            "errors.lifecycle.quitting",
        ),
        AsrHealthCheckError::RuntimeUnavailable => AppError::new(
            "asr.health_runtime_unavailable",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Error,
            true,
            "errors.asr.worker_unavailable",
        ),
        AsrHealthCheckError::Settings(error) => AppError::new(
            error.code,
            ErrorCategory::Storage,
            ErrorSeverity::Error,
            error.retryable,
            error.safe_message_key,
        ),
    }
}

fn switch_error(error: AsrModelSwitchError) -> AppError {
    match error {
        AsrModelSwitchError::Busy => AppError::new(
            "asr.model.switch_busy",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Warning,
            true,
            "errors.asr.model_switch_busy",
        ),
        AsrModelSwitchError::Quitting => AppError::new(
            "asr.model.switch_quitting",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Warning,
            false,
            "errors.lifecycle.quitting",
        ),
        AsrModelSwitchError::RuntimeUnavailable => AppError::new(
            "asr.model.switch_runtime_unavailable",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Error,
            true,
            "errors.asr.worker_unavailable",
        ),
        AsrModelSwitchError::Missing => AppError::new(
            "asr.model.missing",
            ErrorCategory::Asr,
            ErrorSeverity::Warning,
            false,
            "errors.asr.model_missing",
        ),
        AsrModelSwitchError::HashMismatch => AppError::new(
            "asr.model.hash_mismatch",
            ErrorCategory::Asr,
            ErrorSeverity::Error,
            false,
            "errors.asr.model_hash_mismatch",
        ),
        AsrModelSwitchError::Unhealthy => AppError::new(
            "asr.model.switch_failed",
            ErrorCategory::Asr,
            ErrorSeverity::Error,
            true,
            "errors.asr.model_switch_failed",
        ),
        AsrModelSwitchError::Runtime(error) => AppError::new(
            error.code,
            ErrorCategory::Asr,
            ErrorSeverity::Error,
            error.retryable,
            error.safe_message_key,
        ),
        AsrModelSwitchError::Settings(error) => AppError::new(
            error.code,
            ErrorCategory::Storage,
            ErrorSeverity::Error,
            error.retryable,
            error.safe_message_key,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_event_name_is_valid_for_the_tauri_runtime() {
        assert!(!APP_SNAPSHOT_CHANGED_EVENT.is_empty());
        assert!(APP_SNAPSHOT_CHANGED_EVENT.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '/' | ':' | '_')
        }));
    }

    #[test]
    fn open_directory_command_has_no_renderer_supplied_path_or_engine() {
        let _command: fn(WebviewWindow) -> Result<(), AppError> = model_open_directory;
    }

    #[test]
    fn required_model_switch_error_codes_are_stable() {
        assert_eq!(
            switch_error(AsrModelSwitchError::Missing).code,
            "asr.model.missing"
        );
        assert_eq!(
            switch_error(AsrModelSwitchError::HashMismatch).code,
            "asr.model.hash_mismatch"
        );
        assert_eq!(
            switch_error(AsrModelSwitchError::Busy).code,
            "asr.model.switch_busy"
        );
    }
}
