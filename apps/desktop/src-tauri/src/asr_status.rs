//! ASR Runtime（运行时）健康状态的只读观测。
//!
//! `AppSnapshot` 不能为了渲染控制面板而启动 Worker（工作程序）、加载或切换模型。
//! 本装饰器只记录 Application（应用层）工作流原本就会执行的 Health（健康检查），
//! 再向公开快照提供该观测结果。

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use remtene_application::ports::{
    ASR_NO_SPEECH_CODE, AsrEnginePort, AsrRequest, AsrResult, DiagnosticEvent, DiagnosticsSink,
    EngineHealth, PortError, PortFuture,
};
use remtene_domain::{AsrEngine, RequestId};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AsrHealthSnapshot {
    pub qwen: Option<EngineHealth>,
    pub whisper: Option<EngineHealth>,
}

#[derive(Default)]
pub(crate) struct AsrRuntimeStatus {
    health: Mutex<AsrHealthSnapshot>,
    model_access: Mutex<ModelAccessSnapshot>,
}

#[derive(Default)]
struct ModelAccessSnapshot {
    qwen: ModelAccess,
    whisper: ModelAccess,
}

#[derive(Default)]
struct ModelAccess {
    ever_ready: bool,
    last_access: Option<Instant>,
}

const MODEL_KEEP_ALIVE: Duration = Duration::from_secs(5 * 60);

impl AsrRuntimeStatus {
    pub(crate) fn snapshot(&self) -> AsrHealthSnapshot {
        *self
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn record(&self, engine: AsrEngine, health: EngineHealth) {
        let mut snapshot = self
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match engine {
            AsrEngine::Qwen => snapshot.qwen = Some(health),
            AsrEngine::Whisper => snapshot.whisper = Some(health),
        }
    }

    fn expected_load_path(&self, engine: AsrEngine) -> &'static str {
        let access = self
            .model_access
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let access = match engine {
            AsrEngine::Qwen => &access.qwen,
            AsrEngine::Whisper => &access.whisper,
        };
        if !access.ever_ready {
            "cold"
        } else if access
            .last_access
            .is_some_and(|last_access| last_access.elapsed() < MODEL_KEEP_ALIVE)
        {
            "warm"
        } else {
            "restored"
        }
    }

    fn record_model_access(&self, engine: AsrEngine) {
        let mut access = self
            .model_access
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let access = match engine {
            AsrEngine::Qwen => &mut access.qwen,
            AsrEngine::Whisper => &mut access.whisper,
        };
        access.ever_ready = true;
        access.last_access = Some(Instant::now());
    }
}

pub(crate) struct ObservedAsrEngine {
    inner: Arc<dyn AsrEnginePort>,
    status: Arc<AsrRuntimeStatus>,
    diagnostics: Arc<dyn DiagnosticsSink>,
}

impl ObservedAsrEngine {
    pub(crate) fn new(
        inner: Arc<dyn AsrEnginePort>,
        status: Arc<AsrRuntimeStatus>,
        diagnostics: Arc<dyn DiagnosticsSink>,
    ) -> Self {
        Self {
            inner,
            status,
            diagnostics,
        }
    }
}

impl AsrEnginePort for ObservedAsrEngine {
    fn health(&self, engine: AsrEngine) -> PortFuture<'_, Result<EngineHealth, PortError>> {
        let started = Instant::now();
        let load_path = self.status.expected_load_path(engine);
        self.diagnostics.record(DiagnosticEvent {
            session_id: None,
            phase: Some("asr.model.load".to_owned()),
            state: Some("started".to_owned()),
            duration_ms: None,
            error_code: None,
            detail: Some(format!(
                "engine={} load_path={load_path}",
                engine_label(engine)
            )),
        });
        let pending = self.inner.health(engine);
        let status = Arc::clone(&self.status);
        let diagnostics = Arc::clone(&self.diagnostics);
        Box::pin(async move {
            let result = pending.await;
            let health = result.as_ref().copied().unwrap_or(EngineHealth::Unhealthy);
            status.record(engine, health);
            if health == EngineHealth::Healthy {
                status.record_model_access(engine);
            }
            diagnostics.record(DiagnosticEvent {
                session_id: None,
                phase: Some("asr.model.load".to_owned()),
                state: Some(
                    if health == EngineHealth::Healthy {
                        "loaded"
                    } else {
                        "unavailable"
                    }
                    .to_owned(),
                ),
                duration_ms: Some(elapsed_ms(started)),
                error_code: result.as_ref().err().map(|error| error.code.clone()),
                detail: Some(format!(
                    "engine={} load_path={load_path} health={}",
                    engine_label(engine),
                    health_label(health)
                )),
            });
            result
        })
    }

    fn transcribe(&self, request: AsrRequest) -> PortFuture<'_, Result<AsrResult, PortError>> {
        let session_id = request.session_id;
        let engine = request.engine;
        let started = Instant::now();
        self.diagnostics.record(DiagnosticEvent {
            session_id: Some(session_id),
            phase: Some("asr.transcribe".to_owned()),
            state: Some("started".to_owned()),
            duration_ms: None,
            error_code: None,
            detail: Some(format!("engine={}", engine_label(engine))),
        });
        let pending = self.inner.transcribe(request);
        let status = Arc::clone(&self.status);
        let diagnostics = Arc::clone(&self.diagnostics);
        Box::pin(async move {
            let result = pending.await;
            match &result {
                Ok(transcript) => {
                    status.record_model_access(engine);
                    diagnostics.record(DiagnosticEvent {
                        session_id: Some(session_id),
                        phase: Some("asr.transcribe".to_owned()),
                        state: Some("completed".to_owned()),
                        duration_ms: Some(transcript.inference_duration_ms),
                        error_code: None,
                        detail: Some(format!("engine={}", engine_label(engine))),
                    });
                }
                Err(error) => {
                    let (state, error_code) = transcription_error_diagnostic(error);
                    if state == "no_speech" {
                        status.record_model_access(engine);
                    }
                    diagnostics.record(DiagnosticEvent {
                        session_id: Some(session_id),
                        phase: Some("asr.transcribe".to_owned()),
                        state: Some(state.to_owned()),
                        duration_ms: Some(elapsed_ms(started)),
                        error_code,
                        detail: Some(format!("engine={}", engine_label(engine))),
                    });
                }
            }
            result
        })
    }

    fn cancel(&self, request_id: RequestId) -> PortFuture<'_, Result<(), PortError>> {
        self.inner.cancel(request_id)
    }
}

fn transcription_error_diagnostic(error: &PortError) -> (&'static str, Option<String>) {
    if error.code == ASR_NO_SPEECH_CODE {
        ("no_speech", None)
    } else {
        ("failed", Some(error.code.clone()))
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

const fn engine_label(engine: AsrEngine) -> &'static str {
    match engine {
        AsrEngine::Qwen => "qwen",
        AsrEngine::Whisper => "whisper",
    }
}

const fn health_label(health: EngineHealth) -> &'static str {
    match health {
        EngineHealth::Healthy => "healthy",
        EngineHealth::Unhealthy => "unhealthy",
        EngineHealth::Missing => "missing",
        EngineHealth::Incompatible => "incompatible",
    }
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;

    use super::*;

    #[test]
    fn records_real_health_failures_without_exposing_the_port_error() {
        let status = Arc::new(AsrRuntimeStatus::default());
        let observed = ObservedAsrEngine::new(
            Arc::new(remtene_adapters::stub_ports::StubAsrEngine::new()),
            Arc::clone(&status),
            Arc::new(remtene_adapters::diagnostics::ConsoleDiagnosticsSink::new()),
        );

        assert_eq!(status.snapshot(), AsrHealthSnapshot::default());
        assert!(block_on(observed.health(AsrEngine::Qwen)).is_err());
        assert_eq!(status.snapshot().qwen, Some(EngineHealth::Unhealthy));
        assert_eq!(status.snapshot().whisper, None);
    }

    #[test]
    fn model_load_path_distinguishes_cold_warm_and_restored_access() {
        let status = AsrRuntimeStatus::default();

        assert_eq!(status.expected_load_path(AsrEngine::Qwen), "cold");
        assert_eq!(status.expected_load_path(AsrEngine::Whisper), "cold");

        status.record_model_access(AsrEngine::Qwen);
        assert_eq!(status.expected_load_path(AsrEngine::Qwen), "warm");
        assert_eq!(status.expected_load_path(AsrEngine::Whisper), "cold");

        status.model_access.lock().unwrap().qwen.last_access =
            Some(Instant::now() - MODEL_KEEP_ALIVE - Duration::from_secs(1));
        assert_eq!(status.expected_load_path(AsrEngine::Qwen), "restored");
    }

    #[test]
    fn no_speech_diagnostics_do_not_report_a_model_failure() {
        let no_speech = PortError {
            code: ASR_NO_SPEECH_CODE.to_owned(),
            safe_message_key: "worker.qwen.empty_transcript".to_owned(),
            retryable: false,
        };
        assert_eq!(
            transcription_error_diagnostic(&no_speech),
            ("no_speech", None)
        );

        let failure = PortError {
            code: "asr.transcription_failed".to_owned(),
            safe_message_key: "worker.qwen.inference_failed".to_owned(),
            retryable: true,
        };
        assert_eq!(
            transcription_error_diagnostic(&failure),
            ("failed", Some("asr.transcription_failed".to_owned()))
        );
    }
}
