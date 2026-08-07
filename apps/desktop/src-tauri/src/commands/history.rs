//! Read-only output-history IPC for the Control Panel.
//!
//! This module never reads the backing JSON file directly. It invokes the
//! Application query controller, then projects internal delivery identities to
//! opaque record references and UTC RFC 3339 timestamps.

use remtene_application::{HistoryError, ports::HistoryRecord};
use remtene_contracts::{
    AppError, CONTRACT_VERSION, ErrorCategory, ErrorSeverity, HistoryClearAllCommand,
    HistoryClearAllResult, HistoryCopyCommand, HistoryCopyResult, HistoryPage, HistoryQuery,
    HistoryRecordView,
};
use remtene_domain::DeliveryId;
use tauri::{State, WebviewWindow};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::composition_root::CompositionRoot;
use crate::{WindowCommandClass, authorize_window};

#[tauri::command]
pub async fn history_list(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
    query: HistoryQuery,
) -> Result<HistoryPage, AppError> {
    authorize_window(window.label(), WindowCommandClass::History)?;
    if query.contract_version != CONTRACT_VERSION {
        return Err(AppError::new(
            "ipc.contract_version_mismatch",
            ErrorCategory::Security,
            ErrorSeverity::Error,
            false,
            "errors.ipc.contract_version_mismatch",
        ));
    }

    let records = composition
        .history
        .list()
        .await
        .map_err(history_query_error)?;
    history_page(records)
}

#[tauri::command]
pub async fn history_copy(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
    command: HistoryCopyCommand,
) -> Result<HistoryCopyResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::History)?;
    ensure_contract(command.contract_version)?;
    composition
        .history
        .copy(DeliveryId::from_uuid(command.record_id))
        .await
        .map_err(history_copy_error)?;
    Ok(HistoryCopyResult {
        contract_version: CONTRACT_VERSION,
        request_id: command.request_id,
        record_id: command.record_id,
    })
}

#[tauri::command]
pub async fn history_clear_all(
    window: WebviewWindow,
    composition: State<'_, CompositionRoot>,
    command: HistoryClearAllCommand,
) -> Result<HistoryClearAllResult, AppError> {
    authorize_window(window.label(), WindowCommandClass::History)?;
    ensure_contract(command.contract_version)?;
    if !command.acknowledge_data_loss {
        return Err(AppError::new(
            "history.confirmation_required",
            ErrorCategory::Security,
            ErrorSeverity::Error,
            false,
            "errors.history.confirmation_required",
        ));
    }
    let cleared_count = composition
        .history
        .clear_all()
        .await
        .map_err(history_clear_error)?;
    Ok(HistoryClearAllResult {
        contract_version: CONTRACT_VERSION,
        request_id: command.request_id,
        cleared_count: u64::try_from(cleared_count).map_err(|_| invalid_history())?,
    })
}

fn ensure_contract(contract_version: u16) -> Result<(), AppError> {
    if contract_version != CONTRACT_VERSION {
        return Err(AppError::new(
            "ipc.contract_version_mismatch",
            ErrorCategory::Security,
            ErrorSeverity::Error,
            false,
            "errors.ipc.contract_version_mismatch",
        ));
    }
    Ok(())
}

fn history_page(records: Vec<HistoryRecord>) -> Result<HistoryPage, AppError> {
    let records = records
        .into_iter()
        .map(|record| {
            Ok(HistoryRecordView {
                record_id: record.delivery_id.as_uuid(),
                final_text: record.final_text,
                created_at: format_timestamp(record.created_at.get())?,
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;

    Ok(HistoryPage {
        contract_version: CONTRACT_VERSION,
        records,
    })
}

fn format_timestamp(timestamp_ms: u64) -> Result<String, AppError> {
    let nanoseconds = i128::from(timestamp_ms) * 1_000_000;
    let timestamp =
        OffsetDateTime::from_unix_timestamp_nanos(nanoseconds).map_err(|_| invalid_history())?;
    timestamp.format(&Rfc3339).map_err(|_| invalid_history())
}

fn invalid_history() -> AppError {
    AppError::new(
        "history.unavailable",
        ErrorCategory::Storage,
        ErrorSeverity::Error,
        false,
        "errors.history.unavailable",
    )
}

fn history_query_error(error: HistoryError) -> AppError {
    let retryable = matches!(
        &error,
        HistoryError::Port(port_error) if port_error.retryable
    );
    AppError::new(
        "history.unavailable",
        ErrorCategory::Storage,
        ErrorSeverity::Error,
        retryable,
        "errors.history.unavailable",
    )
}

fn history_copy_error(error: HistoryError) -> AppError {
    let (code, retryable) = match error {
        HistoryError::RecordStale => ("history.record_stale", false),
        HistoryError::Busy => ("history.busy", true),
        HistoryError::Port(port_error) => ("history.copy_failed", port_error.retryable),
        HistoryError::Unavailable
        | HistoryError::Quitting
        | HistoryError::InvalidRecord
        | HistoryError::RuntimeUnavailable => ("history.copy_failed", false),
    };
    AppError::new(
        code,
        ErrorCategory::Storage,
        ErrorSeverity::Error,
        retryable,
        match code {
            "history.record_stale" => "errors.history.record_stale",
            "history.busy" => "errors.history.busy",
            _ => "errors.history.copy_failed",
        },
    )
}

fn history_clear_error(error: HistoryError) -> AppError {
    let (code, retryable) = match error {
        HistoryError::Busy => ("history.busy", true),
        HistoryError::Port(port_error) => ("history.operation_failed", port_error.retryable),
        HistoryError::Unavailable
        | HistoryError::Quitting
        | HistoryError::RecordStale
        | HistoryError::InvalidRecord
        | HistoryError::RuntimeUnavailable => ("history.operation_failed", false),
    };
    AppError::new(
        code,
        ErrorCategory::Storage,
        ErrorSeverity::Error,
        retryable,
        match code {
            "history.busy" => "errors.history.busy",
            _ => "errors.history.operation_failed",
        },
    )
}

#[cfg(test)]
mod tests {
    use remtene_domain::{DeliveryId, TimestampMs};

    use super::*;

    #[test]
    fn history_page_uses_opaque_record_id_and_utc_rfc3339_time() {
        let record_id = DeliveryId::new();
        let private_text = "最终文字";
        let page = history_page(vec![HistoryRecord {
            delivery_id: record_id,
            final_text: private_text.to_owned(),
            created_at: TimestampMs::new(1_700_000_000_123),
        }])
        .expect("valid history page");

        assert_eq!(page.contract_version, CONTRACT_VERSION);
        assert_eq!(page.records.len(), 1);
        assert_eq!(page.records[0].record_id, record_id.as_uuid());
        assert_eq!(page.records[0].final_text, private_text);
        assert_eq!(page.records[0].created_at, "2023-11-14T22:13:20.123Z");

        let serialized = serde_json::to_string(&page).expect("serialize page");
        assert!(!serialized.contains("delivery_id"));
        for forbidden in [
            "source_app",
            "processing_mode",
            "delivery_status",
            "selected_text",
            "provider",
            "path",
            "api_key",
        ] {
            assert!(!serialized.contains(forbidden));
        }
    }

    #[test]
    fn timestamp_outside_rfc3339_range_fails_closed() {
        let error = format_timestamp(u64::MAX).expect_err("unrepresentable time must fail");
        let serialized = serde_json::to_string(&error).expect("serialize error");

        assert_eq!(error.code, "history.unavailable");
        assert!(!serialized.contains(&u64::MAX.to_string()));
    }

    #[test]
    fn port_details_do_not_cross_the_history_error_boundary() {
        let private_marker = "private-path-or-text";
        let error =
            history_query_error(HistoryError::Port(remtene_application::ports::PortError {
                code: format!("history.{private_marker}"),
                safe_message_key: format!("errors.{private_marker}"),
                retryable: true,
            }));
        let serialized = serde_json::to_string(&error).expect("serialize error");

        assert_eq!(error.code, "history.unavailable");
        assert!(error.retryable);
        assert!(!serialized.contains(private_marker));
    }

    #[test]
    fn copy_and_clear_errors_never_expose_port_details() {
        let private_marker = "private-history-text-or-path";
        let port_error = || {
            HistoryError::Port(remtene_application::ports::PortError {
                code: format!("history.{private_marker}"),
                safe_message_key: format!("errors.{private_marker}"),
                retryable: true,
            })
        };

        for error in [
            history_copy_error(port_error()),
            history_clear_error(port_error()),
        ] {
            let serialized = serde_json::to_string(&error).expect("serialize safe error");
            assert!(error.retryable);
            assert!(!serialized.contains(private_marker));
        }

        assert_eq!(
            history_copy_error(HistoryError::RecordStale).code,
            "history.record_stale"
        );
        assert_eq!(history_copy_error(HistoryError::Busy).code, "history.busy");
        assert_eq!(history_clear_error(HistoryError::Busy).code, "history.busy");
        assert_eq!(
            history_copy_error(HistoryError::Quitting).code,
            "history.copy_failed"
        );
        assert_eq!(
            history_clear_error(HistoryError::Quitting).code,
            "history.operation_failed"
        );
    }
}
