//! 应用层交付决策的无正文诊断。
//!
//! 该模块不选择文件路径，也不直接写 `stderr`。Composition Root 把它连接到
//! 与 ASR、LLM 和平台适配层共用的 `DiagnosticsSink`，从而统一开关、时间戳
//! 和轮转策略。`cause` 只允许结构性事实，不得包含转录正文或选区。

use std::sync::{Arc, OnceLock, RwLock, Weak};

use crate::ports::{DiagnosticEvent, DiagnosticsSink};

fn configured_sink() -> &'static RwLock<Option<Weak<dyn DiagnosticsSink>>> {
    static SINK: OnceLock<RwLock<Option<Weak<dyn DiagnosticsSink>>>> = OnceLock::new();
    SINK.get_or_init(|| RwLock::new(None))
}

pub(crate) fn configure(sink: &Arc<dyn DiagnosticsSink>) {
    *configured_sink()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(sink));
}

fn emit(phase: String, state: String, detail: Option<String>) {
    let sink = configured_sink()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(Weak::upgrade);
    if let Some(sink) = sink {
        sink.record(DiagnosticEvent {
            session_id: None,
            phase: Some(phase),
            state: Some(state),
            duration_ms: None,
            error_code: None,
            detail,
        });
    }
}

pub fn decision(stage: &str, decision: &str, cause: &str) {
    emit(
        format!("application.delivery.{stage}"),
        decision.to_owned(),
        (!cause.is_empty()).then(|| cause.to_owned()),
    );
}

pub fn session_mark(label: &str) {
    emit("application.session".to_owned(), label.to_owned(), None);
}
