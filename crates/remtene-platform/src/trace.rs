//! 平台适配层交付 trace。
//!
//! 所有事件都经 Application 定义的统一 Sink 进入 App 缓存；该模块不再
//! 写桌面文件。`detail` 只允许环节、布尔值和稳定 reason code，严禁用户正文。

use std::sync::{Arc, OnceLock, RwLock, Weak};

use remtene_application::ports::{DiagnosticEvent, DiagnosticsSink};
use remtene_domain::SessionId;

fn configured_sink() -> &'static RwLock<Option<Weak<dyn DiagnosticsSink>>> {
    static SINK: OnceLock<RwLock<Option<Weak<dyn DiagnosticsSink>>>> = OnceLock::new();
    SINK.get_or_init(|| RwLock::new(None))
}

pub(crate) fn configure(sink: &Arc<dyn DiagnosticsSink>) {
    *configured_sink()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(sink));
}

fn emit(kind: &str, stage: &str, outcome: &str, detail: &str) {
    let sink = configured_sink()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(Weak::upgrade);
    if let Some(sink) = sink {
        sink.record(DiagnosticEvent {
            session_id: None,
            phase: Some(format!("platform.{kind}.{stage}")),
            state: Some(outcome.to_owned()),
            duration_ms: None,
            error_code: None,
            detail: (!detail.is_empty()).then(|| detail.to_owned()),
        });
    }
}

pub fn delivery(stage: &str, outcome: &str, detail: &str) {
    emit("delivery", stage, outcome, detail);
}

pub fn checkpoint(stage: &str, detail: &str) {
    emit("checkpoint", stage, "passed", detail);
}

pub(crate) fn audio_normalization(
    session_id: SessionId,
    state: &str,
    duration_ms: Option<u64>,
    error_code: Option<&str>,
    detail: &str,
) {
    let sink = configured_sink()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(Weak::upgrade);
    if let Some(sink) = sink {
        sink.record(DiagnosticEvent {
            session_id: Some(session_id),
            phase: Some("platform.audio.normalization".to_owned()),
            state: Some(state.to_owned()),
            duration_ms,
            error_code: error_code.map(str::to_owned),
            detail: (!detail.is_empty()).then(|| detail.to_owned()),
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct MemoryDiagnostics(Mutex<Vec<DiagnosticEvent>>);

    impl DiagnosticsSink for MemoryDiagnostics {
        fn record(&self, event: DiagnosticEvent) {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }
    }

    #[test]
    fn normalization_events_are_session_correlated_and_have_distinct_terminals() {
        let sink = Arc::new(MemoryDiagnostics::default());
        let sink_port: Arc<dyn DiagnosticsSink> = sink.clone();
        configure(&sink_port);
        let session_id = SessionId::new();

        audio_normalization(
            session_id,
            "started",
            None,
            None,
            "mode=resample source_sample_rate=48000 target_sample_rate=16000",
        );
        audio_normalization(
            session_id,
            "completed",
            Some(7),
            None,
            "source_frames=48000 target_frames=16000",
        );
        audio_normalization(
            session_id,
            "cancelled",
            Some(3),
            None,
            "source_frames=12000 target_frames=4000",
        );

        let events = sink
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let events = events
            .iter()
            .filter(|event| {
                event.session_id == Some(session_id)
                    && event.phase.as_deref() == Some("platform.audio.normalization")
            })
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].state.as_deref(), Some("started"));
        assert_eq!(events[0].duration_ms, None);
        assert_eq!(events[1].state.as_deref(), Some("completed"));
        assert_eq!(events[1].duration_ms, Some(7));
        assert_eq!(events[2].state.as_deref(), Some("cancelled"));
        assert_eq!(events[2].duration_ms, Some(3));
    }
}
