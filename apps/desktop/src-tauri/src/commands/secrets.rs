//! ControlPanel-only LLM secret management and connection test commands.
//!
//! No command accepts a logical secret ID, path, endpoint, model, prompt, or
//! arbitrary user text. Endpoint fingerprinting and encrypted-store access stay
//! inside Rust Application/Adapter layers.

use remtene_application::ports::SecretValue;
use remtene_application::{
    LlmApiKeyStatus, LlmConfigurationError, LlmConnectionFailure, LlmConnectionTestOutcome,
};
use remtene_contracts::{
    AppError, CONTRACT_VERSION, DeleteLlmApiKeyCommand, ErrorCategory, ErrorSeverity,
    LlmApiKeyMutationResult, LlmApiKeyState, LlmApiKeyStatusView, LlmConnectionTestErrorCode,
    LlmConnectionTestResult, LlmConnectionTestStatus, LlmTestConnectionCommand,
    LlmUpstreamErrorView, ResetUnrecoverableLlmSecretsCommand, RevealLlmApiKeyCommand,
    RevealLlmApiKeyResult, SecretStorageKind, SetLlmApiKeyCommand,
};
use tauri::{State, WebviewWindow};

use crate::composition_root::CompositionRoot;
use crate::{WindowCommandClass, authorize_window};

#[tauri::command]
pub async fn secret_get_llm_api_key_status(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
) -> Result<LlmApiKeyStatusView, AppError> {
    authorize_window(window.label(), WindowCommandClass::Secret)?;
    Ok(status_view(
        composition.llm_configuration.api_key_status().await,
    ))
}

#[tauri::command]
pub async fn secret_set_llm_api_key(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
    mut command: SetLlmApiKeyCommand,
) -> Result<LlmApiKeyMutationResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::Secret)?;
    validate_contract(command.contract_version)?;
    let request_id = command.request_id;
    // Move the String directly into the zeroizing Application wrapper. Never
    // format, trim, log, or duplicate it in the Handler.
    let status = composition
        .llm_configuration
        .set_api_key(SecretValue::new(command.take_secret_value()))
        .await
        .map_err(configuration_error)?;
    Ok(mutation_result(request_id, status))
}

#[tauri::command]
pub async fn secret_reveal_llm_api_key(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
    command: RevealLlmApiKeyCommand,
) -> Result<RevealLlmApiKeyResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::Secret)?;
    validate_contract(command.contract_version)?;
    let secret = composition
        .llm_configuration
        .reveal_api_key()
        .await
        .map_err(configuration_error)?;
    Ok(RevealLlmApiKeyResult {
        contract_version: CONTRACT_VERSION,
        request_id: command.request_id,
        secret_value: secret.expose().to_owned(),
    })
}

#[tauri::command]
pub async fn secret_delete_llm_api_key(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
    command: DeleteLlmApiKeyCommand,
) -> Result<LlmApiKeyMutationResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::Secret)?;
    validate_contract(command.contract_version)?;
    let status = composition
        .llm_configuration
        .delete_api_key()
        .await
        .map_err(configuration_error)?;
    Ok(mutation_result(command.request_id, status))
}

#[tauri::command]
pub async fn secret_reset_unrecoverable_llm_secrets(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
    command: ResetUnrecoverableLlmSecretsCommand,
) -> Result<LlmApiKeyMutationResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::Secret)?;
    validate_contract(command.contract_version)?;
    if !command.acknowledge_data_loss {
        return Err(AppError::new(
            "secret.reset_confirmation_required",
            ErrorCategory::Security,
            ErrorSeverity::Warning,
            false,
            "errors.secret.reset_confirmation_required",
        ));
    }
    let status = composition
        .llm_configuration
        .reset_unrecoverable_secrets()
        .await
        .map_err(configuration_error)?;
    Ok(mutation_result(command.request_id, status))
}

#[tauri::command]
pub async fn llm_test_connection(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
    command: LlmTestConnectionCommand,
) -> Result<LlmConnectionTestResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::Secret)?;
    validate_contract(command.contract_version)?;
    let outcome = composition.llm_configuration.test_connection().await;
    let (status, error_code, upstream_error) = match outcome {
        LlmConnectionTestOutcome::Succeeded => (LlmConnectionTestStatus::Succeeded, None, None),
        LlmConnectionTestOutcome::Failed(failure) => (
            LlmConnectionTestStatus::Failed,
            Some(connection_error_code(failure)),
            None,
        ),
        LlmConnectionTestOutcome::FailedWithUpstream { failure, upstream } => (
            LlmConnectionTestStatus::Failed,
            Some(connection_error_code(failure)),
            Some(LlmUpstreamErrorView {
                http_status: upstream.http_status(),
                response_body: upstream.response_body().to_owned(),
                truncated: upstream.truncated(),
            }),
        ),
    };
    Ok(LlmConnectionTestResult {
        contract_version: CONTRACT_VERSION,
        request_id: command.request_id,
        status,
        error_code,
        upstream_error,
    })
}

fn validate_contract(version: u16) -> Result<(), AppError> {
    if version == CONTRACT_VERSION {
        Ok(())
    } else {
        Err(AppError::new(
            "ipc.contract_mismatch",
            ErrorCategory::Security,
            ErrorSeverity::Error,
            false,
            "errors.ipc.contract_mismatch",
        ))
    }
}

fn mutation_result(request_id: uuid::Uuid, status: LlmApiKeyStatus) -> LlmApiKeyMutationResult {
    LlmApiKeyMutationResult {
        contract_version: CONTRACT_VERSION,
        request_id,
        status: status_view(status),
    }
}

fn status_view(status: LlmApiKeyStatus) -> LlmApiKeyStatusView {
    LlmApiKeyStatusView {
        contract_version: CONTRACT_VERSION,
        state: match status {
            LlmApiKeyStatus::NotConfigured => LlmApiKeyState::NotConfigured,
            LlmApiKeyStatus::Configured => LlmApiKeyState::Configured,
            LlmApiKeyStatus::RecoveryRequired => LlmApiKeyState::RecoveryRequired,
            LlmApiKeyStatus::Unavailable => LlmApiKeyState::Unavailable,
        },
        storage: SecretStorageKind::EncryptedLocal,
    }
}

pub(super) fn configuration_error(error: LlmConfigurationError) -> AppError {
    match error {
        LlmConfigurationError::Busy => AppError::new(
            "llm.configuration_busy",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Warning,
            true,
            "errors.llm.configuration_busy",
        ),
        LlmConfigurationError::NotConfigured => AppError::new(
            "llm.not_configured",
            ErrorCategory::Llm,
            ErrorSeverity::Warning,
            false,
            "errors.llm.not_configured",
        ),
        LlmConfigurationError::RecoveryRequired => AppError::new(
            "secret.recovery_required",
            ErrorCategory::Security,
            ErrorSeverity::Blocking,
            false,
            "errors.secret.recovery_required",
        ),
        LlmConfigurationError::InvalidSecret => AppError::new(
            "secret.value_invalid",
            ErrorCategory::Security,
            ErrorSeverity::Error,
            false,
            "errors.secret.value_invalid",
        ),
        LlmConfigurationError::InvalidConfiguration(error) => AppError::new(
            error.code,
            ErrorCategory::Llm,
            ErrorSeverity::Error,
            error.retryable,
            error.safe_message_key,
        ),
        LlmConfigurationError::SecretVerificationFailed => AppError::new(
            "secret.verification_failed",
            ErrorCategory::Security,
            ErrorSeverity::Error,
            true,
            "errors.secret.verification_failed",
        ),
        LlmConfigurationError::RuntimeUnavailable => AppError::new(
            "llm.runtime_unavailable",
            ErrorCategory::Lifecycle,
            ErrorSeverity::Error,
            true,
            "errors.llm.runtime_unavailable",
        ),
        LlmConfigurationError::Port(error) => {
            let category =
                if error.code.starts_with("secret.") || error.code.starts_with("settings.") {
                    ErrorCategory::Storage
                } else {
                    ErrorCategory::Llm
                };
            AppError::new(
                error.code,
                category,
                ErrorSeverity::Error,
                error.retryable,
                error.safe_message_key,
            )
        }
    }
}

fn connection_error_code(failure: LlmConnectionFailure) -> LlmConnectionTestErrorCode {
    match failure {
        LlmConnectionFailure::Busy => LlmConnectionTestErrorCode::Busy,
        LlmConnectionFailure::RuntimeUnavailable => LlmConnectionTestErrorCode::RuntimeUnavailable,
        LlmConnectionFailure::SettingsUnavailable => {
            LlmConnectionTestErrorCode::SettingsUnavailable
        }
        LlmConnectionFailure::NotConfigured => LlmConnectionTestErrorCode::NotConfigured,
        LlmConnectionFailure::RecoveryRequired => LlmConnectionTestErrorCode::RecoveryRequired,
        LlmConnectionFailure::SecretUnavailable => LlmConnectionTestErrorCode::SecretUnavailable,
        LlmConnectionFailure::InvalidConfiguration => {
            LlmConnectionTestErrorCode::InvalidConfiguration
        }
        LlmConnectionFailure::AuthenticationFailed => {
            LlmConnectionTestErrorCode::AuthenticationFailed
        }
        LlmConnectionFailure::PermissionDenied => LlmConnectionTestErrorCode::PermissionDenied,
        LlmConnectionFailure::RateLimited => LlmConnectionTestErrorCode::RateLimited,
        LlmConnectionFailure::Timeout => LlmConnectionTestErrorCode::Timeout,
        LlmConnectionFailure::Network => LlmConnectionTestErrorCode::Network,
        LlmConnectionFailure::ProviderUnavailable => {
            LlmConnectionTestErrorCode::ProviderUnavailable
        }
        LlmConnectionFailure::RequestRejected => LlmConnectionTestErrorCode::RequestRejected,
        LlmConnectionFailure::InvalidResponse => LlmConnectionTestErrorCode::InvalidResponse,
        LlmConnectionFailure::ResponseTooLarge => LlmConnectionTestErrorCode::ResponseTooLarge,
        LlmConnectionFailure::Cancelled => LlmConnectionTestErrorCode::Cancelled,
        LlmConnectionFailure::Internal => LlmConnectionTestErrorCode::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_status_projection_contains_no_secret_metadata() {
        let json = serde_json::to_value(status_view(LlmApiKeyStatus::Configured))
            .expect("status serializes");
        assert_eq!(json["state"], "configured");
        assert_eq!(json["storage"], "encrypted_local");
        let rendered = json.to_string();
        for forbidden in ["secret_id", "fingerprint", "length", "prefix", "suffix"] {
            assert!(!rendered.contains(forbidden));
        }
    }

    #[test]
    fn connection_failures_have_stable_content_free_codes() {
        assert_eq!(
            connection_error_code(LlmConnectionFailure::AuthenticationFailed),
            LlmConnectionTestErrorCode::AuthenticationFailed
        );
        assert_eq!(
            connection_error_code(LlmConnectionFailure::PermissionDenied),
            LlmConnectionTestErrorCode::PermissionDenied
        );
        assert_eq!(
            connection_error_code(LlmConnectionFailure::InvalidResponse),
            LlmConnectionTestErrorCode::InvalidResponse
        );
        assert_eq!(
            connection_error_code(LlmConnectionFailure::Busy),
            LlmConnectionTestErrorCode::Busy
        );
        assert_eq!(
            connection_error_code(LlmConnectionFailure::SettingsUnavailable),
            LlmConnectionTestErrorCode::SettingsUnavailable
        );
    }
}
