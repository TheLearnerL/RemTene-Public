//! Local ASR Worker runtime.
//!
//! This crate deliberately depends only on the versioned Worker contract and
//! model-runtime implementations. It has no access to the desktop UI,
//! microphone, history, secrets, selected text, LLM providers, or target input.

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use remtene_contracts::{
    CONTRACT_VERSION, CancelledResult, CoreToWorkerEnvelope, CoreToWorkerMessage,
    HealthCheckRequest, HealthResult, HealthStatus, ShutdownComplete, TranscribeRequest,
    TranscriptResult, WorkerCapability, WorkerEngineId, WorkerError, WorkerErrorCode,
    WorkerProtocolError, WorkerProtocolPhase, WorkerProtocolState, WorkerReady,
    WorkerToCoreEnvelope, WorkerToCoreMessage,
};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

mod memory_pressure;
#[cfg(target_os = "macos")]
mod qwen_engine;
#[cfg(all(target_os = "macos", feature = "whisper-runtime"))]
mod whisper_engine;

pub use memory_pressure::{MemoryPressureLevel, MemoryPressureListener};
#[cfg(target_os = "macos")]
pub use qwen_engine::{QwenEngineBackend, QwenEngineConfig};
#[cfg(all(target_os = "macos", feature = "whisper-runtime"))]
pub use whisper_engine::{WhisperEngineBackend, WhisperEngineConfig};

pub const MAX_PROTOCOL_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const WORKER_VERSION: &str = env!("CARGO_PKG_VERSION");
const AUDIO_FILE_SUFFIX: &str = ".wav";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerRuntimeConfig {
    artifact_root: PathBuf,
    runtime_id: String,
    build_signature_id: String,
}

impl WorkerRuntimeConfig {
    pub fn new(
        artifact_root: impl Into<PathBuf>,
        runtime_id: impl Into<String>,
        build_signature_id: impl Into<String>,
    ) -> Result<Self, WorkerRuntimeError> {
        let artifact_root = artifact_root.into();
        let runtime_id = runtime_id.into();
        let build_signature_id = build_signature_id.into();
        if runtime_id.trim().is_empty() {
            return Err(WorkerRuntimeError::InvalidConfiguration("runtime_id"));
        }
        if build_signature_id.trim().is_empty() {
            return Err(WorkerRuntimeError::InvalidConfiguration(
                "build_signature_id",
            ));
        }
        validate_artifact_root(&artifact_root)?;
        Ok(Self {
            artifact_root,
            runtime_id,
            build_signature_id,
        })
    }

    #[must_use]
    pub fn artifact_root(&self) -> &Path {
        &self.artifact_root
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineTranscript {
    pub final_text: String,
    pub detected_language: Option<String>,
    pub audio_duration_ms: u64,
    pub inference_duration_ms: u64,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{safe_message_key}")]
pub struct EngineError {
    pub code: WorkerErrorCode,
    pub retryable: bool,
    pub safe_message_key: String,
}

pub trait EngineBackend: Send + Sync + 'static {
    fn supported_engines(&self) -> Vec<WorkerEngineId>;

    fn health(&self, request: &HealthCheckRequest) -> HealthResult;

    fn transcribe(
        &self,
        request: &TranscribeRequest,
        audio_path: &Path,
        cancelled: &AtomicBool,
    ) -> Result<EngineTranscript, EngineError>;

    fn unload(&self) {}
}

pub struct CompositeEngineBackend {
    qwen: Option<Arc<dyn EngineBackend>>,
    whisper: Option<Arc<dyn EngineBackend>>,
    active_engine: std::sync::Mutex<Option<WorkerEngineId>>,
}

impl CompositeEngineBackend {
    #[must_use]
    pub fn start(
        qwen: Option<Arc<dyn EngineBackend>>,
        whisper: Option<Arc<dyn EngineBackend>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            qwen,
            whisper,
            active_engine: std::sync::Mutex::new(None),
        })
    }

    fn backend(&self, engine: WorkerEngineId) -> Option<&Arc<dyn EngineBackend>> {
        match engine {
            WorkerEngineId::Qwen => self.qwen.as_ref(),
            WorkerEngineId::Whisper => self.whisper.as_ref(),
        }
    }

    fn with_backend<T>(
        &self,
        engine: WorkerEngineId,
        operation: impl FnOnce(&dyn EngineBackend) -> T,
    ) -> Option<T> {
        let backend = self.backend(engine)?;
        let mut active = self
            .active_engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.as_ref() != Some(&engine) {
            if let Some(previous) = active.and_then(|previous| self.backend(previous)) {
                previous.unload();
            }
            *active = Some(engine);
        }
        Some(operation(backend.as_ref()))
    }
}

impl EngineBackend for CompositeEngineBackend {
    fn supported_engines(&self) -> Vec<WorkerEngineId> {
        vec![WorkerEngineId::Qwen, WorkerEngineId::Whisper]
    }

    fn health(&self, request: &HealthCheckRequest) -> HealthResult {
        self.with_backend(request.engine_id, |backend| backend.health(request))
            .unwrap_or_else(|| HealthResult {
                engine_id: request.engine_id,
                model_id: request.model_id.clone(),
                model_version: "unavailable".to_owned(),
                status: HealthStatus::Missing,
                device_class: "local".to_owned(),
                safe_error_code: Some(match request.engine_id {
                    WorkerEngineId::Qwen => "worker.qwen.runtime_not_linked".to_owned(),
                    WorkerEngineId::Whisper => "worker.whisper.runtime_not_linked".to_owned(),
                }),
            })
    }

    fn transcribe(
        &self,
        request: &TranscribeRequest,
        audio_path: &Path,
        cancelled: &AtomicBool,
    ) -> Result<EngineTranscript, EngineError> {
        self.with_backend(request.engine_id, |backend| {
            backend.transcribe(request, audio_path, cancelled)
        })
        .unwrap_or_else(|| {
            Err(EngineError {
                code: WorkerErrorCode::EngineUnavailable,
                retryable: false,
                safe_message_key: match request.engine_id {
                    WorkerEngineId::Qwen => "worker.qwen.runtime_not_linked".to_owned(),
                    WorkerEngineId::Whisper => "worker.whisper.runtime_not_linked".to_owned(),
                },
            })
        })
    }

    fn unload(&self) {
        let mut active = self
            .active_engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(engine) = active.take()
            && let Some(backend) = self.backend(engine)
        {
            backend.unload();
        }
    }
}

#[derive(Default)]
pub struct UnavailableEngineBackend;

impl EngineBackend for UnavailableEngineBackend {
    fn supported_engines(&self) -> Vec<WorkerEngineId> {
        vec![WorkerEngineId::Qwen, WorkerEngineId::Whisper]
    }

    fn health(&self, request: &HealthCheckRequest) -> HealthResult {
        HealthResult {
            engine_id: request.engine_id,
            model_id: request.model_id.clone(),
            model_version: "unavailable".to_owned(),
            status: HealthStatus::Missing,
            device_class: "local".to_owned(),
            safe_error_code: Some("worker.engine.not_linked".to_owned()),
        }
    }

    fn transcribe(
        &self,
        _request: &TranscribeRequest,
        _audio_path: &Path,
        _cancelled: &AtomicBool,
    ) -> Result<EngineTranscript, EngineError> {
        Err(EngineError {
            code: WorkerErrorCode::EngineUnavailable,
            retryable: false,
            safe_message_key: "worker.error.engine_unavailable".to_owned(),
        })
    }
}

#[cfg(debug_assertions)]
#[derive(Default)]
pub struct DeterministicTestBackend;

#[cfg(debug_assertions)]
impl EngineBackend for DeterministicTestBackend {
    fn supported_engines(&self) -> Vec<WorkerEngineId> {
        vec![WorkerEngineId::Qwen, WorkerEngineId::Whisper]
    }

    fn health(&self, request: &HealthCheckRequest) -> HealthResult {
        HealthResult {
            engine_id: request.engine_id,
            model_id: request.model_id.clone(),
            model_version: "deterministic-test".to_owned(),
            status: HealthStatus::Healthy,
            device_class: "deterministic-test".to_owned(),
            safe_error_code: None,
        }
    }

    fn transcribe(
        &self,
        _request: &TranscribeRequest,
        _audio_path: &Path,
        cancelled: &AtomicBool,
    ) -> Result<EngineTranscript, EngineError> {
        for _ in 0..100 {
            if cancelled.load(Ordering::Acquire) {
                return Err(EngineError {
                    code: WorkerErrorCode::InferenceFailed,
                    retryable: false,
                    safe_message_key: "worker.test.cancelled".to_owned(),
                });
            }
            thread::sleep(Duration::from_millis(2));
        }
        Ok(EngineTranscript {
            final_text: "deterministic worker transcript".to_owned(),
            detected_language: Some("en".to_owned()),
            audio_duration_ms: 500,
            inference_duration_ms: 200,
        })
    }
}

#[cfg(debug_assertions)]
#[derive(Default)]
pub struct CrashOnTranscribeTestBackend;

#[cfg(debug_assertions)]
impl EngineBackend for CrashOnTranscribeTestBackend {
    fn supported_engines(&self) -> Vec<WorkerEngineId> {
        vec![WorkerEngineId::Qwen, WorkerEngineId::Whisper]
    }

    fn health(&self, request: &HealthCheckRequest) -> HealthResult {
        HealthResult {
            engine_id: request.engine_id,
            model_id: request.model_id.clone(),
            model_version: "crash-test".to_owned(),
            status: HealthStatus::Healthy,
            device_class: "crash-test".to_owned(),
            safe_error_code: None,
        }
    }

    fn transcribe(
        &self,
        _request: &TranscribeRequest,
        _audio_path: &Path,
        _cancelled: &AtomicBool,
    ) -> Result<EngineTranscript, EngineError> {
        std::process::exit(86)
    }
}

#[derive(Debug, Error)]
pub enum WorkerRuntimeError {
    #[error("invalid Worker configuration: {0}")]
    InvalidConfiguration(&'static str),
    #[error("Worker artifact root is unavailable")]
    ArtifactRootUnavailable,
    #[error("Worker protocol input failed")]
    Input,
    #[error("Worker protocol output failed")]
    Output,
    #[error("Worker protocol was rejected")]
    Protocol,
    #[error("Worker input closed before graceful shutdown")]
    UnexpectedInputClosure,
    #[error("Worker shutdown grace period elapsed")]
    ShutdownTimeout,
}

enum RuntimeEvent {
    Input(CoreToWorkerEnvelope),
    InputFailure,
    InputClosed,
    InferenceFinished {
        session_id: Uuid,
        request_id: Uuid,
        outcome: Result<EngineTranscript, EngineError>,
    },
    RequestDeadline {
        session_id: Uuid,
        request_id: Uuid,
    },
    ShutdownDeadline,
}

struct ActiveRequest {
    request: TranscribeRequest,
    cancelled: Arc<AtomicBool>,
    cancel_requested: bool,
    timed_out: bool,
}

struct BackendUnloadGuard {
    backend: Arc<dyn EngineBackend>,
}

impl BackendUnloadGuard {
    fn new(backend: Arc<dyn EngineBackend>) -> Self {
        Self { backend }
    }

    fn as_arc(&self) -> &Arc<dyn EngineBackend> {
        &self.backend
    }
}

impl std::ops::Deref for BackendUnloadGuard {
    type Target = dyn EngineBackend;

    fn deref(&self) -> &Self::Target {
        self.backend.as_ref()
    }
}

impl Drop for BackendUnloadGuard {
    fn drop(&mut self) {
        self.backend.unload();
    }
}

pub fn run_worker<R, W>(
    input: R,
    mut output: W,
    config: WorkerRuntimeConfig,
    backend: Arc<dyn EngineBackend>,
) -> Result<(), WorkerRuntimeError>
where
    R: Read + Send + 'static,
    W: Write,
{
    let backend = BackendUnloadGuard::new(backend);
    let (events_tx, events_rx) = mpsc::channel();
    spawn_input_reader(input, events_tx.clone());

    let mut protocol = WorkerProtocolState::new();
    let mut active: Option<ActiveRequest> = None;
    let mut shutdown_requested = false;

    while let Ok(event) = events_rx.recv() {
        match event {
            RuntimeEvent::Input(envelope) => {
                if let Err(error) = protocol.observe_core(&envelope) {
                    emit_fatal_protocol_error(&mut output, &mut protocol, &error)?;
                    return Err(WorkerRuntimeError::Protocol);
                }

                match envelope.message {
                    CoreToWorkerMessage::Hello(_) => {
                        let ready = WorkerReady {
                            protocol_version: CONTRACT_VERSION,
                            worker_version: WORKER_VERSION.to_owned(),
                            supported_engines: backend.supported_engines(),
                            runtime_id: config.runtime_id.clone(),
                            capabilities: required_capabilities(),
                            build_signature_id: config.build_signature_id.clone(),
                        };
                        emit(
                            &mut output,
                            &mut protocol,
                            None,
                            None,
                            WorkerToCoreMessage::Ready(ready),
                        )?;
                    }
                    CoreToWorkerMessage::HealthCheck(request) => {
                        let request_id = envelope
                            .request_id
                            .expect("validated health request has request_id");
                        emit(
                            &mut output,
                            &mut protocol,
                            None,
                            Some(request_id),
                            WorkerToCoreMessage::HealthResult(backend.health(&request)),
                        )?;
                    }
                    CoreToWorkerMessage::Transcribe(request) => {
                        if active.is_some() {
                            emit(
                                &mut output,
                                &mut protocol,
                                Some(request.session_id),
                                Some(request.request_id),
                                WorkerToCoreMessage::Error(WorkerError {
                                    code: WorkerErrorCode::InvalidRequest,
                                    retryable: true,
                                    fatal: false,
                                    safe_message_key: "worker.error.busy".to_owned(),
                                }),
                            )?;
                            continue;
                        }

                        let audio_path = match resolve_audio_artifact(&config, &request) {
                            Ok(path) => path,
                            Err(error) => {
                                emit(
                                    &mut output,
                                    &mut protocol,
                                    Some(request.session_id),
                                    Some(request.request_id),
                                    WorkerToCoreMessage::Error(error),
                                )?;
                                continue;
                            }
                        };
                        let cancelled = Arc::new(AtomicBool::new(false));
                        spawn_inference(
                            Arc::clone(backend.as_arc()),
                            request.clone(),
                            audio_path,
                            Arc::clone(&cancelled),
                            events_tx.clone(),
                        );
                        spawn_request_deadline(
                            request.session_id,
                            request.request_id,
                            request.deadline_ms,
                            events_tx.clone(),
                        );
                        active = Some(ActiveRequest {
                            request,
                            cancelled,
                            cancel_requested: false,
                            timed_out: false,
                        });
                    }
                    CoreToWorkerMessage::Cancel(request) => {
                        let active_request = active.as_mut().expect(
                            "protocol state only accepts cancellation for an active request",
                        );
                        debug_assert_eq!(active_request.request.session_id, request.session_id);
                        debug_assert_eq!(active_request.request.request_id, request.request_id);
                        active_request.cancel_requested = true;
                        active_request.cancelled.store(true, Ordering::Release);
                    }
                    CoreToWorkerMessage::Shutdown(request) => {
                        shutdown_requested = true;
                        if let Some(active_request) = active.as_mut() {
                            active_request.cancel_requested = true;
                            active_request.cancelled.store(true, Ordering::Release);
                            spawn_shutdown_deadline(request.grace_period_ms, events_tx.clone());
                        } else {
                            emit_shutdown_complete(&mut output, &mut protocol)?;
                            return Ok(());
                        }
                    }
                }
            }
            RuntimeEvent::InferenceFinished {
                session_id,
                request_id,
                outcome,
            } => {
                let Some(completed) = active.take() else {
                    continue;
                };
                if completed.request.session_id != session_id
                    || completed.request.request_id != request_id
                {
                    active = Some(completed);
                    continue;
                }

                let message = if completed.cancel_requested {
                    WorkerToCoreMessage::Cancelled(CancelledResult {
                        session_id,
                        request_id,
                    })
                } else if completed.timed_out {
                    WorkerToCoreMessage::Error(WorkerError {
                        code: WorkerErrorCode::InferenceFailed,
                        retryable: true,
                        fatal: false,
                        safe_message_key: "worker.error.deadline_exceeded".to_owned(),
                    })
                } else {
                    match outcome {
                        Ok(result) => WorkerToCoreMessage::Transcript(TranscriptResult {
                            session_id,
                            request_id,
                            engine_id: completed.request.engine_id,
                            model_id: completed.request.model_id,
                            final_text: result.final_text,
                            detected_language: result.detected_language,
                            audio_duration_ms: result.audio_duration_ms,
                            inference_duration_ms: result.inference_duration_ms,
                        }),
                        Err(error) => WorkerToCoreMessage::Error(WorkerError {
                            code: error.code,
                            retryable: error.retryable,
                            fatal: false,
                            safe_message_key: error.safe_message_key,
                        }),
                    }
                };
                emit(
                    &mut output,
                    &mut protocol,
                    Some(session_id),
                    Some(request_id),
                    message,
                )?;

                if shutdown_requested {
                    emit_shutdown_complete(&mut output, &mut protocol)?;
                    return Ok(());
                }
            }
            RuntimeEvent::RequestDeadline {
                session_id,
                request_id,
            } => {
                if let Some(active_request) = active.as_mut()
                    && active_request.request.session_id == session_id
                    && active_request.request.request_id == request_id
                {
                    active_request.timed_out = true;
                    active_request.cancelled.store(true, Ordering::Release);
                }
            }
            RuntimeEvent::ShutdownDeadline => {
                if let Some(active_request) = active.take() {
                    emit(
                        &mut output,
                        &mut protocol,
                        Some(active_request.request.session_id),
                        Some(active_request.request.request_id),
                        WorkerToCoreMessage::Error(WorkerError {
                            code: WorkerErrorCode::CancellationFailed,
                            retryable: true,
                            fatal: true,
                            safe_message_key: "worker.error.shutdown_timeout".to_owned(),
                        }),
                    )?;
                    return Err(WorkerRuntimeError::ShutdownTimeout);
                }
            }
            RuntimeEvent::InputFailure => {
                emit_fatal_input_error(&mut output, &mut protocol)?;
                return Err(WorkerRuntimeError::Input);
            }
            RuntimeEvent::InputClosed => {
                if protocol.phase() == WorkerProtocolPhase::Closed {
                    return Ok(());
                }
                if let Some(active_request) = active.as_mut() {
                    active_request.cancelled.store(true, Ordering::Release);
                }
                return Err(WorkerRuntimeError::UnexpectedInputClosure);
            }
        }
    }

    Err(WorkerRuntimeError::UnexpectedInputClosure)
}

fn required_capabilities() -> Vec<WorkerCapability> {
    vec![
        WorkerCapability::HealthCheck,
        WorkerCapability::FinalTranscript,
        WorkerCapability::Cancellation,
        WorkerCapability::GracefulShutdown,
    ]
}

fn validate_artifact_root(path: &Path) -> Result<(), WorkerRuntimeError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| WorkerRuntimeError::ArtifactRootUnavailable)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorkerRuntimeError::ArtifactRootUnavailable);
    }
    Ok(())
}

fn resolve_audio_artifact(
    config: &WorkerRuntimeConfig,
    request: &TranscribeRequest,
) -> Result<PathBuf, WorkerError> {
    let path = config
        .artifact_root
        .join(format!("{}{AUDIO_FILE_SUFFIX}", request.audio_artifact_id));
    let metadata = fs::symlink_metadata(&path).map_err(|_| unavailable_audio_error())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(unavailable_audio_error());
    }
    Ok(path)
}

fn unavailable_audio_error() -> WorkerError {
    WorkerError {
        code: WorkerErrorCode::InvalidRequest,
        retryable: false,
        fatal: false,
        safe_message_key: "worker.error.audio_artifact_unavailable".to_owned(),
    }
}

fn spawn_input_reader<R: Read + Send + 'static>(input: R, sender: mpsc::Sender<RuntimeEvent>) {
    thread::spawn(move || {
        let mut reader = BufReader::new(input);
        loop {
            match read_protocol_frame(&mut reader) {
                Ok(Some(frame)) => match serde_json::from_slice::<CoreToWorkerEnvelope>(&frame) {
                    Ok(envelope) => {
                        if sender.send(RuntimeEvent::Input(envelope)).is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = sender.send(RuntimeEvent::InputFailure);
                        return;
                    }
                },
                Ok(None) => {
                    let _ = sender.send(RuntimeEvent::InputClosed);
                    return;
                }
                Err(()) => {
                    let _ = sender.send(RuntimeEvent::InputFailure);
                    return;
                }
            }
        }
    });
}

fn read_protocol_frame<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>, ()> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(|_| ())?;
        if available.is_empty() {
            return if frame.is_empty() { Ok(None) } else { Err(()) };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        if frame.len().saturating_add(consumed) > MAX_PROTOCOL_FRAME_BYTES {
            return Err(());
        }
        frame.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if newline.is_some() {
            frame.pop();
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return if frame.is_empty() {
                Err(())
            } else {
                Ok(Some(frame))
            };
        }
    }
}

fn spawn_inference(
    backend: Arc<dyn EngineBackend>,
    request: TranscribeRequest,
    audio_path: PathBuf,
    cancelled: Arc<AtomicBool>,
    sender: mpsc::Sender<RuntimeEvent>,
) {
    thread::spawn(move || {
        let session_id = request.session_id;
        let request_id = request.request_id;
        let outcome = backend.transcribe(&request, &audio_path, &cancelled);
        let _ = sender.send(RuntimeEvent::InferenceFinished {
            session_id,
            request_id,
            outcome,
        });
    });
}

fn spawn_request_deadline(
    session_id: Uuid,
    request_id: Uuid,
    deadline_ms: u64,
    sender: mpsc::Sender<RuntimeEvent>,
) {
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(deadline_ms));
        let _ = sender.send(RuntimeEvent::RequestDeadline {
            session_id,
            request_id,
        });
    });
}

fn spawn_shutdown_deadline(grace_period_ms: u64, sender: mpsc::Sender<RuntimeEvent>) {
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(grace_period_ms));
        let _ = sender.send(RuntimeEvent::ShutdownDeadline);
    });
}

fn emit_shutdown_complete<W: Write>(
    output: &mut W,
    protocol: &mut WorkerProtocolState,
) -> Result<(), WorkerRuntimeError> {
    emit(
        output,
        protocol,
        None,
        None,
        WorkerToCoreMessage::ShutdownComplete(ShutdownComplete {
            worker_version: WORKER_VERSION.to_owned(),
        }),
    )
}

fn emit<W: Write>(
    output: &mut W,
    protocol: &mut WorkerProtocolState,
    session_id: Option<Uuid>,
    request_id: Option<Uuid>,
    message: WorkerToCoreMessage,
) -> Result<(), WorkerRuntimeError> {
    let envelope = worker_envelope(session_id, request_id, message)?;
    protocol
        .observe_worker(&envelope)
        .map_err(|_| WorkerRuntimeError::Protocol)?;
    write_envelope(output, &envelope)
}

fn emit_fatal_protocol_error<W: Write>(
    output: &mut W,
    protocol: &mut WorkerProtocolState,
    error: &WorkerProtocolError,
) -> Result<(), WorkerRuntimeError> {
    let code = if matches!(
        error,
        WorkerProtocolError::UnsupportedContractVersion { .. }
            | WorkerProtocolError::NegotiationFailed(_)
            | WorkerProtocolError::MissingCapability(_)
    ) {
        WorkerErrorCode::ProtocolIncompatible
    } else {
        WorkerErrorCode::InvalidRequest
    };
    let envelope = worker_envelope(
        None,
        None,
        WorkerToCoreMessage::Error(WorkerError {
            code,
            retryable: false,
            fatal: true,
            safe_message_key: "worker.error.protocol_rejected".to_owned(),
        }),
    )?;
    if protocol.phase() != WorkerProtocolPhase::AwaitingHello {
        protocol
            .observe_worker(&envelope)
            .map_err(|_| WorkerRuntimeError::Protocol)?;
    }
    write_envelope(output, &envelope)
}

fn emit_fatal_input_error<W: Write>(
    output: &mut W,
    protocol: &mut WorkerProtocolState,
) -> Result<(), WorkerRuntimeError> {
    let envelope = worker_envelope(
        None,
        None,
        WorkerToCoreMessage::Error(WorkerError {
            code: WorkerErrorCode::InvalidRequest,
            retryable: false,
            fatal: true,
            safe_message_key: "worker.error.invalid_frame".to_owned(),
        }),
    )?;
    if protocol.phase() != WorkerProtocolPhase::AwaitingHello {
        protocol
            .observe_worker(&envelope)
            .map_err(|_| WorkerRuntimeError::Protocol)?;
    }
    write_envelope(output, &envelope)
}

fn worker_envelope(
    session_id: Option<Uuid>,
    request_id: Option<Uuid>,
    message: WorkerToCoreMessage,
) -> Result<WorkerToCoreEnvelope, WorkerRuntimeError> {
    let sent_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| WorkerRuntimeError::Output)?;
    let envelope = WorkerToCoreEnvelope {
        contract_version: CONTRACT_VERSION,
        message_id: Uuid::new_v4(),
        session_id,
        request_id,
        sent_at,
        message,
    };
    envelope
        .validate()
        .map_err(|_| WorkerRuntimeError::Protocol)?;
    Ok(envelope)
}

fn write_envelope<W: Write>(
    output: &mut W,
    envelope: &WorkerToCoreEnvelope,
) -> Result<(), WorkerRuntimeError> {
    serde_json::to_writer(&mut *output, envelope).map_err(|_| WorkerRuntimeError::Output)?;
    output
        .write_all(b"\n")
        .and_then(|()| output.flush())
        .map_err(|_| WorkerRuntimeError::Output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn frame_reader_rejects_empty_truncated_and_oversized_frames() {
        assert_eq!(
            read_protocol_frame(&mut std::io::Cursor::new(Vec::new())),
            Ok(None)
        );
        assert_eq!(
            read_protocol_frame(&mut std::io::Cursor::new(b"\n")),
            Err(())
        );
        assert_eq!(
            read_protocol_frame(&mut std::io::Cursor::new(b"{}")),
            Err(())
        );

        let oversized = vec![b'x'; MAX_PROTOCOL_FRAME_BYTES + 1];
        assert_eq!(
            read_protocol_frame(&mut std::io::Cursor::new(oversized)),
            Err(())
        );
    }

    #[test]
    fn artifact_resolution_rejects_missing_files_and_symlinks() {
        let root = test_root("artifact-resolution");
        fs::create_dir_all(&root).expect("create test root");
        let config = WorkerRuntimeConfig::new(&root, "test", "test-build").expect("config");
        let request = test_request();
        assert!(resolve_audio_artifact(&config, &request).is_err());

        let path = root.join(format!("{}{AUDIO_FILE_SUFFIX}", request.audio_artifact_id));
        fs::write(&path, b"test").expect("write artifact");
        assert_eq!(resolve_audio_artifact(&config, &request), Ok(path.clone()));

        fs::remove_file(&path).expect("remove artifact");
        fs::create_dir(&path).expect("replace artifact with directory");
        assert!(resolve_audio_artifact(&config, &request).is_err());
        fs::remove_dir_all(&root).expect("remove test root");
    }

    #[test]
    fn composite_backend_unloads_the_previous_engine_before_switching() {
        let qwen_unloads = Arc::new(AtomicUsize::new(0));
        let whisper_unloads = Arc::new(AtomicUsize::new(0));
        let composite = CompositeEngineBackend::start(
            Some(Arc::new(CountingBackend {
                engine: WorkerEngineId::Qwen,
                unloads: Arc::clone(&qwen_unloads),
            })),
            Some(Arc::new(CountingBackend {
                engine: WorkerEngineId::Whisper,
                unloads: Arc::clone(&whisper_unloads),
            })),
        );

        let qwen_request = test_request();
        let whisper_health = HealthCheckRequest {
            engine_id: WorkerEngineId::Whisper,
            model_id: "whisper".to_owned(),
        };
        assert_eq!(
            composite
                .health(&HealthCheckRequest {
                    engine_id: WorkerEngineId::Qwen,
                    model_id: "qwen".to_owned(),
                })
                .status,
            HealthStatus::Healthy
        );
        assert_eq!(
            composite.health(&whisper_health).status,
            HealthStatus::Healthy
        );
        assert_eq!(qwen_unloads.load(Ordering::Acquire), 1);
        assert_eq!(whisper_unloads.load(Ordering::Acquire), 0);

        let cancelled = AtomicBool::new(false);
        composite
            .transcribe(&qwen_request, Path::new("unused"), &cancelled)
            .expect("counting Qwen backend transcribes");
        assert_eq!(whisper_unloads.load(Ordering::Acquire), 1);
    }

    #[test]
    fn worker_unloads_the_active_backend_on_graceful_shutdown() {
        let root = test_root("graceful-unload");
        fs::create_dir_all(&root).expect("create test root");
        let unloads = Arc::new(AtomicUsize::new(0));
        let backend: Arc<dyn EngineBackend> = Arc::new(CountingBackend {
            engine: WorkerEngineId::Qwen,
            unloads: Arc::clone(&unloads),
        });
        let config = WorkerRuntimeConfig::new(&root, "test", "test-build").expect("config");
        let mut input = Vec::new();
        for envelope in [
            test_core_envelope(
                None,
                None,
                CoreToWorkerMessage::Hello(remtene_contracts::CoreHello {
                    supported_protocol_versions: vec![CONTRACT_VERSION],
                    core_version: "test-core".to_owned(),
                    required_capabilities: required_capabilities(),
                }),
            ),
            test_core_envelope(
                None,
                None,
                CoreToWorkerMessage::Shutdown(remtene_contracts::ShutdownRequest {
                    grace_period_ms: 100,
                }),
            ),
        ] {
            serde_json::to_writer(&mut input, &envelope).expect("serialize test envelope");
            input.push(b'\n');
        }

        let mut output = Vec::new();
        run_worker(std::io::Cursor::new(input), &mut output, config, backend)
            .expect("Worker shuts down cleanly");

        assert_eq!(unloads.load(Ordering::Acquire), 1);
        assert!(
            String::from_utf8(output)
                .expect("Worker output is UTF-8 JSONL")
                .contains("shutdown_complete")
        );
        fs::remove_dir_all(root).expect("remove test root");
    }

    struct CountingBackend {
        engine: WorkerEngineId,
        unloads: Arc<AtomicUsize>,
    }

    impl EngineBackend for CountingBackend {
        fn supported_engines(&self) -> Vec<WorkerEngineId> {
            vec![self.engine]
        }

        fn health(&self, request: &HealthCheckRequest) -> HealthResult {
            HealthResult {
                engine_id: request.engine_id,
                model_id: request.model_id.clone(),
                model_version: "test".to_owned(),
                status: HealthStatus::Healthy,
                device_class: "test".to_owned(),
                safe_error_code: None,
            }
        }

        fn transcribe(
            &self,
            _request: &TranscribeRequest,
            _audio_path: &Path,
            _cancelled: &AtomicBool,
        ) -> Result<EngineTranscript, EngineError> {
            Ok(EngineTranscript {
                final_text: "test".to_owned(),
                detected_language: Some("en".to_owned()),
                audio_duration_ms: 1,
                inference_duration_ms: 1,
            })
        }

        fn unload(&self) {
            self.unloads.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn test_request() -> TranscribeRequest {
        let session_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();
        TranscribeRequest {
            session_id,
            request_id,
            engine_id: WorkerEngineId::Qwen,
            model_id: "test-model".to_owned(),
            audio_artifact_id: remtene_contracts::AudioArtifactId::random(),
            audio_format: remtene_contracts::AudioFormatDto {
                sample_rate_hz: 16_000,
                channels: 1,
                bits_per_sample: 16,
            },
            language_hint: None,
            deadline_ms: 1_000,
        }
    }

    fn test_core_envelope(
        session_id: Option<Uuid>,
        request_id: Option<Uuid>,
        message: CoreToWorkerMessage,
    ) -> CoreToWorkerEnvelope {
        CoreToWorkerEnvelope {
            contract_version: CONTRACT_VERSION,
            message_id: Uuid::new_v4(),
            session_id,
            request_id,
            sent_at: "2026-07-22T00:00:00Z".to_owned(),
            message,
        }
    }

    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("remtene-worker-{label}-{}", Uuid::new_v4()))
    }
}
