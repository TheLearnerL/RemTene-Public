use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::CONTRACT_VERSION;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    Permission,
    Security,
    Audio,
    Asr,
    Llm,
    Delivery,
    Storage,
    Lifecycle,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorSeverity {
    Info,
    Warning,
    Error,
    Blocking,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AppError {
    pub contract_version: u16,
    pub code: String,
    pub category: ErrorCategory,
    pub severity: ErrorSeverity,
    pub retryable: bool,
    pub user_message_key: String,
    pub correlation_id: Uuid,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub safe_details: BTreeMap<String, String>,
}

impl AppError {
    #[must_use]
    pub fn new(
        code: impl Into<String>,
        category: ErrorCategory,
        severity: ErrorSeverity,
        retryable: bool,
        user_message_key: impl Into<String>,
    ) -> Self {
        Self {
            contract_version: CONTRACT_VERSION,
            code: code.into(),
            category,
            severity,
            retryable,
            user_message_key: user_message_key.into(),
            correlation_id: Uuid::new_v4(),
            safe_details: BTreeMap::new(),
        }
    }
}
