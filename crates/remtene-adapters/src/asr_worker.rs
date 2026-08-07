//! Core-side supervision and protocol mapping for the local ASR Worker.

use std::{
    ffi::OsString,
    fs,
    future::Future,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    pin::Pin,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    task::{Context, Poll, Waker},
    thread,
    time::{Duration, Instant},
};

use remtene_application::ports::{
    ASR_NO_SPEECH_CODE, AsrEnginePort, AsrRequest, AsrResult, AudioRef, EngineHealth, PortError,
    PortFuture,
};
use remtene_contracts::{
    AudioArtifactId, AudioFormatDto, CONTRACT_VERSION, CancelRequest, CoreHello,
    CoreToWorkerEnvelope, CoreToWorkerMessage, HealthCheckRequest, HealthResult, HealthStatus,
    ShutdownRequest, TranscribeRequest, TranscriptResult, WorkerCapability, WorkerEngineId,
    WorkerError, WorkerErrorCode, WorkerProtocolState, WorkerReady, WorkerToCoreEnvelope,
    WorkerToCoreMessage,
};
use remtene_domain::{AsrEngine, RequestId};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

const MAX_PROTOCOL_FRAME_BYTES: usize = 8 * 1024 * 1024;
const ACTOR_POLL_INTERVAL: Duration = Duration::from_millis(10);
const GRANT_FILE_SUFFIX: &str = ".wav";
const GRANTS_DIRECTORY: &str = "grants";
const MODELS_DIRECTORY: &str = "models";
const ACTIVE_MODELS_DIRECTORY: &str = "active";
/// `qwen-asr` writes its INT8 quantization cache next to the weights unless this is `0`.
const QWEN_SIDECAR_CACHE_ENV: &str = "QWEN_ASR_SIDECAR";
#[cfg(test)]
const CANDIDATE_MODELS_DIRECTORY: &str = "candidates";

pub type AudioArtifactResolver =
    Arc<dyn Fn(&AudioRef) -> Result<Option<PathBuf>, PortError> + Send + Sync>;

#[derive(Clone, Debug)]
pub struct WorkerLaunchConfig {
    executable: PathBuf,
    app_group_id: Option<String>,
    shared_root: Option<PathBuf>,
    grant_root: PathBuf,
    grant_directory: Arc<GrantDirectory>,
    core_version: String,
    qwen_model_id: String,
    whisper_model_id: String,
    qwen_model: Option<WorkerModelLaunch>,
    whisper_model: Option<WorkerModelLaunch>,
    handshake_timeout: Duration,
    health_timeout: Duration,
    cancel_timeout: Duration,
    shutdown_timeout: Duration,
    extra_args: Vec<OsString>,
}

impl WorkerLaunchConfig {
    pub fn new(
        executable: impl Into<PathBuf>,
        grant_root: impl Into<PathBuf>,
        core_version: impl Into<String>,
        qwen_model_id: impl Into<String>,
        whisper_model_id: impl Into<String>,
    ) -> Result<Self, PortError> {
        let executable = executable.into();
        let grant_root = grant_root.into();
        let core_version = core_version.into();
        let qwen_model_id = qwen_model_id.into();
        let whisper_model_id = whisper_model_id.into();
        let grant_directory = Arc::new(GrantDirectory::prepare(&grant_root)?);
        let config = Self::from_parts(
            WorkerLaunchIdentity {
                executable,
                core_version,
                qwen_model_id,
                whisper_model_id,
            },
            grant_root,
            grant_directory,
            None,
            None,
        );
        config.validate()?;
        Ok(config)
    }

    pub fn new_sandboxed(
        executable: impl Into<PathBuf>,
        app_group_id: impl Into<String>,
        shared_root: impl Into<PathBuf>,
        core_version: impl Into<String>,
        qwen_model_id: impl Into<String>,
        whisper_model_id: impl Into<String>,
    ) -> Result<Self, PortError> {
        let executable = executable.into();
        let app_group_id = app_group_id.into();
        let shared_root = shared_root.into();
        let core_version = core_version.into();
        let qwen_model_id = qwen_model_id.into();
        let whisper_model_id = whisper_model_id.into();
        let grant_root = shared_root.join(GRANTS_DIRECTORY);
        validate_shared_paths(&shared_root, &grant_root, None, None)?;
        let grant_directory = Arc::new(GrantDirectory::open_existing_strict(&grant_root)?);
        let config = Self::from_parts(
            WorkerLaunchIdentity {
                executable,
                core_version,
                qwen_model_id,
                whisper_model_id,
            },
            grant_root,
            grant_directory,
            Some(app_group_id),
            Some(shared_root),
        );
        config.validate()?;
        Ok(config)
    }

    fn from_parts(
        identity: WorkerLaunchIdentity,
        grant_root: PathBuf,
        grant_directory: Arc<GrantDirectory>,
        app_group_id: Option<String>,
        shared_root: Option<PathBuf>,
    ) -> Self {
        Self {
            executable: identity.executable,
            app_group_id,
            shared_root,
            grant_root,
            grant_directory,
            core_version: identity.core_version,
            qwen_model_id: identity.qwen_model_id,
            whisper_model_id: identity.whisper_model_id,
            qwen_model: None,
            whisper_model: None,
            handshake_timeout: Duration::from_secs(2),
            health_timeout: Duration::from_secs(2),
            cancel_timeout: Duration::from_millis(500),
            shutdown_timeout: Duration::from_secs(1),
            extra_args: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_extra_arg(mut self, argument: impl Into<OsString>) -> Self {
        self.extra_args.push(argument.into());
        self
    }

    #[must_use]
    pub fn with_qwen_model(
        mut self,
        model_dir: impl Into<PathBuf>,
        model_version: impl Into<String>,
    ) -> Self {
        self.qwen_model = Some(WorkerModelLaunch {
            path: model_dir.into(),
            version: model_version.into(),
        });
        self
    }

    #[must_use]
    pub fn with_whisper_model(
        mut self,
        model_file: impl Into<PathBuf>,
        model_version: impl Into<String>,
    ) -> Self {
        self.whisper_model = Some(WorkerModelLaunch {
            path: model_file.into(),
            version: model_version.into(),
        });
        self
    }

    #[must_use]
    pub fn with_timeouts(
        mut self,
        handshake: Duration,
        health: Duration,
        cancel: Duration,
        shutdown: Duration,
    ) -> Self {
        self.handshake_timeout = handshake;
        self.health_timeout = health;
        self.cancel_timeout = cancel;
        self.shutdown_timeout = shutdown;
        self
    }

    fn validate(&self) -> Result<(), PortError> {
        if self.executable.as_os_str().is_empty()
            || self.core_version.trim().is_empty()
            || self.qwen_model_id.trim().is_empty()
            || self.whisper_model_id.trim().is_empty()
            || self.handshake_timeout.is_zero()
            || self.health_timeout.is_zero()
            || self.cancel_timeout.is_zero()
            || self.shutdown_timeout.is_zero()
            || self
                .app_group_id
                .as_deref()
                .is_some_and(|identifier| !valid_app_group_identifier(identifier))
            || self.app_group_id.is_some() != self.shared_root.is_some()
            || self.grant_directory.root.as_os_str() != self.grant_root.as_os_str()
            || self.qwen_model.as_ref().is_some_and(|model| {
                model.path.as_os_str().is_empty() || model.version.trim().is_empty()
            })
            || self.whisper_model.as_ref().is_some_and(|model| {
                model.path.as_os_str().is_empty() || model.version.trim().is_empty()
            })
        {
            return Err(port_error(
                "asr.worker.invalid_configuration",
                "errors.asr.worker_invalid_configuration",
                false,
            ));
        }
        if let Some(shared_root) = &self.shared_root {
            if !self.extra_args.is_empty() {
                return Err(invalid_worker_configuration());
            }
            validate_shared_paths(
                shared_root,
                &self.grant_root,
                self.qwen_model.as_ref(),
                self.whisper_model.as_ref(),
            )?;
        }
        Ok(())
    }

    fn model_id(&self, engine: AsrEngine) -> &str {
        match engine {
            AsrEngine::Qwen => &self.qwen_model_id,
            AsrEngine::Whisper => &self.whisper_model_id,
        }
    }
}

struct WorkerLaunchIdentity {
    executable: PathBuf,
    core_version: String,
    qwen_model_id: String,
    whisper_model_id: String,
}

#[derive(Clone, Debug)]
struct WorkerModelLaunch {
    path: PathBuf,
    version: String,
}

#[derive(Clone)]
pub struct AsrWorkerAdapter {
    commands: mpsc::Sender<SupervisorCommand>,
}

impl AsrWorkerAdapter {
    pub fn start(
        config: WorkerLaunchConfig,
        resolver: AudioArtifactResolver,
    ) -> Result<Self, PortError> {
        config.validate()?;
        let (commands, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("remtene-asr-supervisor".to_owned())
            .spawn(move || SupervisorActor::new(config, resolver).run(receiver))
            .map_err(|_| {
                port_error(
                    "asr.worker.supervisor_unavailable",
                    "errors.asr.worker_unavailable",
                    true,
                )
            })?;
        Ok(Self { commands })
    }

    pub fn shutdown(&self) -> PortFuture<'static, Result<(), PortError>> {
        let (response, future) = response_pair();
        if self
            .commands
            .send(SupervisorCommand::Shutdown {
                close_supervisor: true,
                response,
            })
            .is_err()
        {
            future.complete_if_pending(Err(supervisor_closed_error()));
        }
        Box::pin(future)
    }

    pub fn release_idle_resources(&self) -> PortFuture<'static, Result<(), PortError>> {
        let (response, future) = response_pair();
        if self
            .commands
            .send(SupervisorCommand::Shutdown {
                close_supervisor: false,
                response,
            })
            .is_err()
        {
            future.complete_if_pending(Err(supervisor_closed_error()));
        }
        Box::pin(future)
    }
}

impl AsrEnginePort for AsrWorkerAdapter {
    fn health(&self, engine: AsrEngine) -> PortFuture<'_, Result<EngineHealth, PortError>> {
        let (response, future) = response_pair();
        if self
            .commands
            .send(SupervisorCommand::Health { engine, response })
            .is_err()
        {
            future.complete_if_pending(Err(supervisor_closed_error()));
        }
        Box::pin(future)
    }

    fn transcribe(&self, request: AsrRequest) -> PortFuture<'_, Result<AsrResult, PortError>> {
        let (response, future) = response_pair();
        if self
            .commands
            .send(SupervisorCommand::Transcribe { request, response })
            .is_err()
        {
            future.complete_if_pending(Err(supervisor_closed_error()));
        }
        Box::pin(future)
    }

    fn cancel(&self, request_id: RequestId) -> PortFuture<'_, Result<(), PortError>> {
        let (response, future) = response_pair();
        if self
            .commands
            .send(SupervisorCommand::Cancel {
                request_id,
                response,
            })
            .is_err()
        {
            future.complete_if_pending(Err(supervisor_closed_error()));
        }
        Box::pin(future)
    }
}

enum SupervisorCommand {
    Health {
        engine: AsrEngine,
        response: ResponseSender<Result<EngineHealth, PortError>>,
    },
    Transcribe {
        request: AsrRequest,
        response: ResponseSender<Result<AsrResult, PortError>>,
    },
    Cancel {
        request_id: RequestId,
        response: ResponseSender<Result<(), PortError>>,
    },
    Shutdown {
        close_supervisor: bool,
        response: ResponseSender<Result<(), PortError>>,
    },
}

struct SupervisorActor {
    config: WorkerLaunchConfig,
    resolver: AudioArtifactResolver,
    process: Option<WorkerProcess>,
    pending_health: Option<PendingHealth>,
    pending_transcription: Option<PendingTranscription>,
    pending_shutdown: Option<PendingShutdown>,
    closed: bool,
}

impl SupervisorActor {
    fn new(config: WorkerLaunchConfig, resolver: AudioArtifactResolver) -> Self {
        Self {
            config,
            resolver,
            process: None,
            pending_health: None,
            pending_transcription: None,
            pending_shutdown: None,
            closed: false,
        }
    }

    fn run(mut self, commands: mpsc::Receiver<SupervisorCommand>) {
        let mut stop = false;
        while !stop {
            self.drain_worker_output();
            if self.closed {
                break;
            }
            self.enforce_deadlines();
            match commands.recv_timeout(ACTOR_POLL_INTERVAL) {
                Ok(command) => stop = self.handle_command(command),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.fail_all(supervisor_closed_error());
                    self.terminate_process();
                    return;
                }
            }
        }
        self.terminate_process();
    }

    fn handle_command(&mut self, command: SupervisorCommand) -> bool {
        match command {
            SupervisorCommand::Health { engine, response } => {
                self.start_health(engine, response);
                false
            }
            SupervisorCommand::Transcribe { request, response } => {
                self.start_transcription(request, response);
                false
            }
            SupervisorCommand::Cancel {
                request_id,
                response,
            } => {
                self.cancel_transcription(request_id, response);
                false
            }
            SupervisorCommand::Shutdown {
                close_supervisor,
                response,
            } => self.start_shutdown(close_supervisor, response),
        }
    }

    fn start_health(
        &mut self,
        engine: AsrEngine,
        response: ResponseSender<Result<EngineHealth, PortError>>,
    ) {
        if self.pending_shutdown.is_some() {
            response.complete(Err(supervisor_closed_error()));
            return;
        }
        if self.pending_health.is_some() || self.pending_transcription.is_some() {
            response.complete(Err(worker_busy_error()));
            return;
        }
        if let Err(error) = self.ensure_worker() {
            response.complete(Err(error));
            return;
        }

        let request_id = Uuid::new_v4();
        let model_id = self.config.model_id(engine).to_owned();
        let envelope = core_envelope(
            None,
            Some(request_id),
            CoreToWorkerMessage::HealthCheck(HealthCheckRequest {
                engine_id: worker_engine(engine),
                model_id: model_id.clone(),
            }),
        );
        if let Err(error) = self.send_to_worker(&envelope) {
            response.complete(Err(error.clone()));
            self.fail_process(error);
            return;
        }
        self.pending_health = Some(PendingHealth {
            request_id,
            engine,
            model_id,
            deadline: deadline_after(self.config.health_timeout),
            response,
        });
    }

    fn start_transcription(
        &mut self,
        request: AsrRequest,
        response: ResponseSender<Result<AsrResult, PortError>>,
    ) {
        if self.pending_shutdown.is_some() {
            response.complete(Err(supervisor_closed_error()));
            return;
        }
        if self.pending_transcription.is_some() || self.pending_health.is_some() {
            response.complete(Err(worker_busy_error()));
            return;
        }
        if let Err(error) = self.ensure_worker() {
            response.complete(Err(error));
            return;
        }

        let source = match (self.resolver)(&request.audio.audio_ref) {
            Ok(Some(source)) => source,
            Ok(None) => {
                response.complete(Err(port_error(
                    "asr.audio_artifact_missing",
                    "errors.asr.audio_artifact_missing",
                    false,
                )));
                return;
            }
            Err(error) => {
                response.complete(Err(error));
                return;
            }
        };
        let grant = match ArtifactGrant::create(Arc::clone(&self.config.grant_directory), &source) {
            Ok(grant) => grant,
            Err(error) => {
                response.complete(Err(error));
                return;
            }
        };
        let model_id = self.config.model_id(request.engine).to_owned();
        let worker_request = TranscribeRequest {
            session_id: request.session_id.as_uuid(),
            request_id: request.request_id.as_uuid(),
            engine_id: worker_engine(request.engine),
            model_id: model_id.clone(),
            audio_artifact_id: grant.id,
            audio_format: AudioFormatDto {
                sample_rate_hz: request.audio.format.sample_rate_hz,
                channels: request.audio.format.channels,
                bits_per_sample: request.audio.format.bits_per_sample,
            },
            language_hint: request.language_hint.clone(),
            deadline_ms: request.deadline_ms,
        };
        let envelope = core_envelope(
            Some(worker_request.session_id),
            Some(worker_request.request_id),
            CoreToWorkerMessage::Transcribe(worker_request),
        );
        if let Err(error) = self.send_to_worker(&envelope) {
            let terminal_error = grant.revoke().err().unwrap_or_else(|| error.clone());
            response.complete(Err(terminal_error));
            self.fail_process(error);
            return;
        }
        self.pending_transcription = Some(PendingTranscription {
            request,
            model_id,
            grant,
            deadline: deadline_after(Duration::from_millis(envelope_deadline_ms(&envelope))),
            cancel_deadline: None,
            terminal_error: None,
            cancel_response: None,
            response,
        });
    }

    fn cancel_transcription(
        &mut self,
        request_id: RequestId,
        response: ResponseSender<Result<(), PortError>>,
    ) {
        let Some(pending) = self.pending_transcription.as_mut() else {
            response.complete(Ok(()));
            return;
        };
        if pending.request.request_id != request_id {
            response.complete(Ok(()));
            return;
        }
        if pending.cancel_response.is_some() {
            response.complete(Err(port_error(
                "asr.cancel_already_pending",
                "errors.asr.cancel_pending",
                true,
            )));
            return;
        }
        let envelope = core_envelope(
            Some(pending.request.session_id.as_uuid()),
            Some(pending.request.request_id.as_uuid()),
            CoreToWorkerMessage::Cancel(CancelRequest {
                session_id: pending.request.session_id.as_uuid(),
                request_id: pending.request.request_id.as_uuid(),
            }),
        );
        if let Err(error) = self.send_to_worker(&envelope) {
            response.complete(Err(error.clone()));
            self.fail_process(error);
            return;
        }
        let pending = self
            .pending_transcription
            .as_mut()
            .expect("pending transcription remains after cancel send");
        pending.cancel_deadline = Some(deadline_after(self.config.cancel_timeout));
        pending.terminal_error = Some(cancelled_error());
        pending.cancel_response = Some(response);
    }

    fn start_shutdown(
        &mut self,
        close_supervisor: bool,
        response: ResponseSender<Result<(), PortError>>,
    ) -> bool {
        if self.pending_shutdown.is_some() {
            response.complete(Err(supervisor_closed_error()));
            return false;
        }
        if !close_supervisor
            && (self.pending_health.is_some() || self.pending_transcription.is_some())
        {
            response.complete(Err(worker_busy_error()));
            return false;
        }
        if self.process.is_none() {
            response.complete(Ok(()));
            return close_supervisor;
        }
        if let Some(health) = self.pending_health.take() {
            health.response.complete(Err(supervisor_closed_error()));
        }
        let envelope = core_envelope(
            None,
            None,
            CoreToWorkerMessage::Shutdown(ShutdownRequest {
                grace_period_ms: duration_ms(self.config.shutdown_timeout),
            }),
        );
        if let Err(error) = self.send_to_worker(&envelope) {
            response.complete(Err(error.clone()));
            self.fail_process(error);
            return close_supervisor;
        }
        if let Some(pending) = self.pending_transcription.as_mut() {
            pending.terminal_error = Some(supervisor_closed_error());
            pending.cancel_deadline = Some(deadline_after(self.config.shutdown_timeout));
        }
        self.pending_shutdown = Some(PendingShutdown {
            deadline: deadline_after(self.config.shutdown_timeout),
            close_supervisor,
            response,
        });
        false
    }

    fn ensure_worker(&mut self) -> Result<(), PortError> {
        if self.process.is_some() {
            return Ok(());
        }
        let mut process = WorkerProcess::spawn(&self.config)?;
        let hello = core_envelope(
            None,
            None,
            CoreToWorkerMessage::Hello(CoreHello {
                supported_protocol_versions: vec![CONTRACT_VERSION],
                core_version: self.config.core_version.clone(),
                required_capabilities: required_capabilities(),
            }),
        );
        process.send(&hello)?;
        let response = process
            .responses
            .recv_timeout(self.config.handshake_timeout)
            .map_err(|_| handshake_error())?;
        let response = response.map_err(|()| protocol_error())?;
        process
            .protocol
            .observe_worker(&response)
            .map_err(|_| protocol_error())?;
        let WorkerToCoreMessage::Ready(ready) = response.message else {
            process.terminate();
            return Err(protocol_error());
        };
        validate_ready_engines(&ready).inspect_err(|_| process.terminate())?;
        self.process = Some(process);
        Ok(())
    }

    fn send_to_worker(&mut self, envelope: &CoreToWorkerEnvelope) -> Result<(), PortError> {
        self.process
            .as_mut()
            .ok_or_else(supervisor_closed_error)?
            .send(envelope)
    }

    fn drain_worker_output(&mut self) {
        loop {
            let event = self
                .process
                .as_ref()
                .and_then(|process| process.responses.try_recv().ok());
            let Some(event) = event else {
                return;
            };
            match event {
                Ok(envelope) => self.handle_worker_envelope(envelope),
                Err(()) => {
                    self.fail_process(protocol_error());
                    return;
                }
            }
        }
    }

    fn handle_worker_envelope(&mut self, envelope: WorkerToCoreEnvelope) {
        let Some(process) = self.process.as_mut() else {
            return;
        };
        if process.protocol.observe_worker(&envelope).is_err() {
            self.fail_process(protocol_error());
            return;
        }

        match envelope.message {
            WorkerToCoreMessage::HealthResult(result) => {
                self.complete_health(envelope.request_id, result)
            }
            WorkerToCoreMessage::Transcript(result) => self.complete_transcription(result),
            WorkerToCoreMessage::Cancelled(result) => {
                self.complete_cancelled(result.session_id, result.request_id);
            }
            WorkerToCoreMessage::Error(error) => {
                self.complete_worker_error(envelope.session_id, envelope.request_id, error);
            }
            WorkerToCoreMessage::ShutdownComplete(_) => self.complete_shutdown(),
            WorkerToCoreMessage::Ready(_) => self.fail_process(protocol_error()),
        }
    }

    fn complete_health(&mut self, request_id: Option<Uuid>, result: HealthResult) {
        let Some(pending) = self.pending_health.take() else {
            self.fail_process(protocol_error());
            return;
        };
        if request_id != Some(pending.request_id)
            || result.engine_id != worker_engine(pending.engine)
            || result.model_id != pending.model_id
        {
            pending.response.complete(Err(protocol_error()));
            self.fail_process(protocol_error());
            return;
        }
        pending.response.complete(Ok(match result.status {
            HealthStatus::Healthy => EngineHealth::Healthy,
            HealthStatus::Unhealthy => EngineHealth::Unhealthy,
            HealthStatus::Missing => EngineHealth::Missing,
            HealthStatus::Incompatible => EngineHealth::Incompatible,
        }));
    }

    fn complete_transcription(&mut self, result: TranscriptResult) {
        let Some(pending) = self.pending_transcription.take() else {
            self.fail_process(protocol_error());
            return;
        };
        if pending.cancel_response.is_some()
            || result.session_id != pending.request.session_id.as_uuid()
            || result.request_id != pending.request.request_id.as_uuid()
            || result.engine_id != worker_engine(pending.request.engine)
            || result.model_id != pending.model_id
        {
            let cleanup_error = pending.grant.revoke().err();
            let terminal_error = cleanup_error.clone().unwrap_or_else(protocol_error);
            pending.response.complete(Err(terminal_error));
            if let Some(cancel) = pending.cancel_response {
                cancel.complete(Err(cleanup_error.unwrap_or_else(protocol_error)));
            }
            self.fail_process(protocol_error());
            return;
        }
        let cleanup = pending.grant.revoke();
        pending.response.complete(cleanup.map(|()| AsrResult {
            session_id: pending.request.session_id,
            request_id: pending.request.request_id,
            engine: pending.request.engine,
            final_text: result.final_text,
            detected_language: result.detected_language,
            inference_duration_ms: result.inference_duration_ms,
        }));
    }

    fn complete_cancelled(&mut self, session_id: Uuid, request_id: Uuid) {
        let Some(pending) = self.pending_transcription.take() else {
            self.fail_process(protocol_error());
            return;
        };
        if session_id != pending.request.session_id.as_uuid()
            || request_id != pending.request.request_id.as_uuid()
        {
            let cleanup_error = pending.grant.revoke().err();
            let terminal_error = cleanup_error.clone().unwrap_or_else(protocol_error);
            pending.response.complete(Err(terminal_error));
            if let Some(cancel) = pending.cancel_response {
                cancel.complete(Err(cleanup_error.unwrap_or_else(protocol_error)));
            }
            self.fail_process(protocol_error());
            return;
        }
        let terminal_error = pending.terminal_error.unwrap_or_else(cancelled_error);
        let cleanup_error = pending.grant.revoke().err();
        pending
            .response
            .complete(Err(cleanup_error.clone().unwrap_or(terminal_error)));
        if let Some(cancel) = pending.cancel_response {
            cancel.complete(cleanup_error.map_or(Ok(()), Err));
        }
    }

    fn complete_worker_error(
        &mut self,
        session_id: Option<Uuid>,
        request_id: Option<Uuid>,
        error: WorkerError,
    ) {
        let mapped = map_worker_error(&error);
        if let Some(pending) = self.pending_transcription.take_if(|pending| {
            session_id == Some(pending.request.session_id.as_uuid())
                && request_id == Some(pending.request.request_id.as_uuid())
        }) {
            let cleanup_error = pending.grant.revoke().err();
            let terminal_error = pending
                .terminal_error
                .clone()
                .unwrap_or_else(|| mapped.clone());
            pending
                .response
                .complete(Err(cleanup_error.clone().unwrap_or(terminal_error)));
            if let Some(cancel) = pending.cancel_response {
                cancel.complete(Err(cleanup_error.unwrap_or(mapped.clone())));
            }
        } else if let Some(pending) = self
            .pending_health
            .take_if(|pending| session_id.is_none() && request_id == Some(pending.request_id))
        {
            pending.response.complete(Err(mapped.clone()));
        } else {
            self.fail_process(protocol_error());
            return;
        }
        if error.fatal {
            self.fail_process(mapped);
        }
    }

    fn complete_shutdown(&mut self) {
        let Some(shutdown) = self.pending_shutdown.take() else {
            self.fail_process(protocol_error());
            return;
        };
        if let Some(pending) = self.pending_transcription.take() {
            let cleanup_error = pending.grant.revoke().err();
            pending.response.complete(Err(cleanup_error
                .clone()
                .unwrap_or_else(supervisor_closed_error)));
            if let Some(cancel) = pending.cancel_response {
                cancel.complete(cleanup_error.map_or(Ok(()), Err));
            }
        }
        let close_supervisor = shutdown.close_supervisor;
        shutdown.response.complete(Ok(()));
        self.terminate_process();
        self.closed = close_supervisor;
    }

    fn enforce_deadlines(&mut self) {
        let now = Instant::now();
        if self
            .pending_health
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline)
        {
            let pending = self.pending_health.take().expect("pending health");
            pending.response.complete(Err(timeout_error()));
            self.fail_process(timeout_error());
            return;
        }

        let request_expired = self
            .pending_transcription
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline && pending.cancel_deadline.is_none());
        if request_expired {
            self.cancel_for_deadline();
        }

        if self
            .pending_transcription
            .as_ref()
            .and_then(|pending| pending.cancel_deadline)
            .is_some_and(|deadline| now >= deadline)
        {
            self.fail_process(timeout_error());
            return;
        }

        if self
            .pending_shutdown
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline)
        {
            self.fail_process(timeout_error());
        }
    }

    fn cancel_for_deadline(&mut self) {
        let Some(pending) = self.pending_transcription.as_ref() else {
            return;
        };
        let envelope = core_envelope(
            Some(pending.request.session_id.as_uuid()),
            Some(pending.request.request_id.as_uuid()),
            CoreToWorkerMessage::Cancel(CancelRequest {
                session_id: pending.request.session_id.as_uuid(),
                request_id: pending.request.request_id.as_uuid(),
            }),
        );
        if let Err(error) = self.send_to_worker(&envelope) {
            self.fail_process(error);
            return;
        }
        let pending = self
            .pending_transcription
            .as_mut()
            .expect("pending transcription remains after deadline cancel");
        pending.terminal_error = Some(timeout_error());
        pending.cancel_deadline = Some(deadline_after(self.config.cancel_timeout));
    }

    fn fail_process(&mut self, error: PortError) {
        let closing = self
            .pending_shutdown
            .as_ref()
            .is_some_and(|pending| pending.close_supervisor);
        self.fail_all(error);
        self.terminate_process();
        if closing {
            self.closed = true;
        }
    }

    fn fail_all(&mut self, error: PortError) {
        if let Some(pending) = self.pending_health.take() {
            pending.response.complete(Err(error.clone()));
        }
        if let Some(pending) = self.pending_transcription.take() {
            let terminal_error = pending
                .grant
                .revoke()
                .err()
                .unwrap_or_else(|| error.clone());
            if let Some(cancel) = pending.cancel_response {
                cancel.complete(Err(terminal_error.clone()));
            }
            pending.response.complete(Err(terminal_error));
        }
        if let Some(pending) = self.pending_shutdown.take() {
            pending.response.complete(Err(error));
        }
    }

    fn terminate_process(&mut self) {
        if let Some(mut process) = self.process.take() {
            process.terminate();
        }
    }
}

struct PendingHealth {
    request_id: Uuid,
    engine: AsrEngine,
    model_id: String,
    deadline: Instant,
    response: ResponseSender<Result<EngineHealth, PortError>>,
}

struct PendingTranscription {
    request: AsrRequest,
    model_id: String,
    grant: ArtifactGrant,
    deadline: Instant,
    cancel_deadline: Option<Instant>,
    terminal_error: Option<PortError>,
    cancel_response: Option<ResponseSender<Result<(), PortError>>>,
    response: ResponseSender<Result<AsrResult, PortError>>,
}

struct PendingShutdown {
    deadline: Instant,
    close_supervisor: bool,
    response: ResponseSender<Result<(), PortError>>,
}

struct WorkerProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    responses: mpsc::Receiver<Result<WorkerToCoreEnvelope, ()>>,
    protocol: WorkerProtocolState,
}

impl WorkerProcess {
    fn spawn(config: &WorkerLaunchConfig) -> Result<Self, PortError> {
        let mut command = worker_command(config);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|_| worker_spawn_error())?;
        let stdin = child.stdin.take().ok_or_else(worker_spawn_error)?;
        let stdout = child.stdout.take().ok_or_else(worker_spawn_error)?;
        let stderr = child.stderr.take().ok_or_else(worker_spawn_error)?;
        let (responses_tx, responses) = mpsc::channel();
        spawn_worker_reader(stdout, responses_tx);
        spawn_stderr_drain(stderr);
        Ok(Self {
            child,
            stdin: Some(stdin),
            responses,
            protocol: WorkerProtocolState::new(),
        })
    }

    fn send(&mut self, envelope: &CoreToWorkerEnvelope) -> Result<(), PortError> {
        self.protocol
            .observe_core(envelope)
            .map_err(|_| protocol_error())?;
        let stdin = self.stdin.as_mut().ok_or_else(supervisor_closed_error)?;
        serde_json::to_writer(&mut *stdin, envelope).map_err(|_| worker_crashed_error())?;
        stdin
            .write_all(b"\n")
            .and_then(|()| stdin.flush())
            .map_err(|_| worker_crashed_error())
    }

    fn terminate(&mut self) {
        self.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn worker_command(config: &WorkerLaunchConfig) -> Command {
    let mut command = command_without_inherited_environment(&config.executable);
    // The model package is a read-only asset: `qwen-asr` otherwise writes its INT8
    // quantization cache (and a `.tmp.<pid>` file) straight into the model directory,
    // which breaks the per-file hashes the package was admitted under (DEC-MODEL-01).
    // The cache is a derived artifact — dropping it only costs a re-quantization on
    // cold load, so it must never live inside a verified package.
    command.env(QWEN_SIDECAR_CACHE_ENV, "0");
    if let Some(app_group_id) = &config.app_group_id {
        command.arg("--app-group-id").arg(app_group_id);
    } else {
        command.arg("--artifact-root").arg(&config.grant_root);
    }
    if let Some(model) = &config.qwen_model {
        command
            .arg("--qwen-model-id")
            .arg(&config.qwen_model_id)
            .arg("--qwen-model-version")
            .arg(&model.version)
            .arg("--qwen-model-dir")
            .arg(&model.path);
    }
    if let Some(model) = &config.whisper_model {
        command
            .arg("--whisper-model-id")
            .arg(&config.whisper_model_id)
            .arg("--whisper-model-version")
            .arg(&model.version)
            .arg("--whisper-model-file")
            .arg(&model.path);
    }
    command.args(&config.extra_args);
    command
}

fn command_without_inherited_environment(executable: &Path) -> Command {
    let mut command = Command::new(executable);
    command.env_clear();
    command
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[derive(Debug)]
struct GrantDirectory {
    root: PathBuf,
    #[cfg(unix)]
    descriptor: rustix::fd::OwnedFd,
}

impl GrantDirectory {
    fn prepare(root: &Path) -> Result<Self, PortError> {
        fs::create_dir_all(root).map_err(|_| artifact_error())?;
        let metadata = fs::symlink_metadata(root).map_err(|_| artifact_error())?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(artifact_error());
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            use rustix::fs::{Mode, OFlags, fchmod, open};

            let descriptor = open(
                root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| artifact_error())?;
            let opened_file =
                fs::File::from(rustix::io::dup(&descriptor).map_err(|_| artifact_error())?);
            let opened_metadata = opened_file.metadata().map_err(|_| artifact_error())?;
            let current_metadata = fs::symlink_metadata(root).map_err(|_| artifact_error())?;
            if current_metadata.file_type().is_symlink()
                || !current_metadata.is_dir()
                || current_metadata.dev() != opened_metadata.dev()
                || current_metadata.ino() != opened_metadata.ino()
            {
                return Err(artifact_error());
            }
            fchmod(&descriptor, Mode::from_raw_mode(0o700)).map_err(|_| artifact_error())?;
            let directory = Self {
                root: root.to_path_buf(),
                descriptor,
            };
            directory.remove_stale_grants()?;
            Ok(directory)
        }

        #[cfg(not(unix))]
        {
            set_private_directory_permissions(root, metadata.permissions())?;
            let directory = Self {
                root: root.to_path_buf(),
            };
            directory.remove_stale_grants()?;
            Ok(directory)
        }
    }

    fn open_existing_strict(root: &Path) -> Result<Self, PortError> {
        if !root.is_absolute() {
            return Err(invalid_worker_configuration());
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            use rustix::fs::{Mode, OFlags, open};

            let descriptor = open(
                root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| invalid_worker_configuration())?;
            let opened_file = fs::File::from(
                rustix::io::dup(&descriptor).map_err(|_| invalid_worker_configuration())?,
            );
            let opened_metadata = opened_file
                .metadata()
                .map_err(|_| invalid_worker_configuration())?;
            let current_metadata =
                fs::symlink_metadata(root).map_err(|_| invalid_worker_configuration())?;
            let canonical = fs::canonicalize(root).map_err(|_| invalid_worker_configuration())?;
            if current_metadata.file_type().is_symlink()
                || !current_metadata.is_dir()
                || current_metadata.dev() != opened_metadata.dev()
                || current_metadata.ino() != opened_metadata.ino()
                || canonical.as_os_str() != root.as_os_str()
            {
                return Err(invalid_worker_configuration());
            }
            let directory = Self {
                root: root.to_path_buf(),
                descriptor,
            };
            directory.remove_stale_grants()?;
            Ok(directory)
        }

        #[cfg(not(unix))]
        {
            let canonical = exact_canonical_directory(root)?;
            let directory = Self { root: canonical };
            directory.remove_stale_grants()?;
            Ok(directory)
        }
    }

    #[cfg(unix)]
    fn remove_stale_grants(&self) -> Result<(), PortError> {
        use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, fstat, openat, unlinkat};

        let mut names = Vec::new();
        let entries = Dir::read_from(&self.descriptor).map_err(|_| artifact_error())?;
        for entry in entries {
            let entry = entry.map_err(|_| artifact_error())?;
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            if !canonical_grant_name(name.to_bytes()) {
                return Err(artifact_error());
            }
            let entry_descriptor = openat(
                &self.descriptor,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| artifact_error())?;
            let entry_stat = fstat(&entry_descriptor).map_err(|_| artifact_error())?;
            if !FileType::from_raw_mode(entry_stat.st_mode).is_file() {
                return Err(artifact_error());
            }
            names.push(name.to_owned());
        }
        for name in names {
            unlinkat(&self.descriptor, &name, AtFlags::empty())
                .map_err(|_| artifact_cleanup_error())?;
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn remove_stale_grants(&self) -> Result<(), PortError> {
        let entries = fs::read_dir(&self.root).map_err(|_| artifact_error())?;
        for entry in entries {
            let entry = entry.map_err(|_| artifact_error())?;
            let name = entry.file_name();
            let entry_metadata =
                fs::symlink_metadata(entry.path()).map_err(|_| artifact_error())?;
            if !canonical_grant_name(name.to_string_lossy().as_bytes())
                || entry_metadata.file_type().is_symlink()
                || !entry_metadata.is_file()
            {
                return Err(artifact_error());
            }
            make_removable(&entry.path());
            fs::remove_file(entry.path()).map_err(|_| artifact_cleanup_error())?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn copy_grant(&self, name: &str, source: &Path) -> Result<(), PortError> {
        use rustix::fs::{FileType, Mode, OFlags, fchmod, fstat, open, openat};

        let source_descriptor = open(
            source,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| artifact_error())?;
        let source_stat = fstat(&source_descriptor).map_err(|_| artifact_error())?;
        if !FileType::from_raw_mode(source_stat.st_mode).is_file() {
            return Err(artifact_error());
        }
        let destination_descriptor = openat(
            &self.descriptor,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o400),
        )
        .map_err(|_| artifact_error())?;
        let mut source_file = fs::File::from(source_descriptor);
        let mut destination_file = fs::File::from(destination_descriptor);
        let copy_result = std::io::copy(&mut source_file, &mut destination_file)
            .map(|_| ())
            .and_then(|()| {
                fchmod(&destination_file, Mode::from_raw_mode(0o400)).map_err(std::io::Error::from)
            });
        drop(destination_file);
        if copy_result.is_err() {
            return Err(self.revoke_name(name).err().unwrap_or_else(artifact_error));
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn copy_grant(&self, name: &str, source: &Path) -> Result<(), PortError> {
        let source_metadata = fs::symlink_metadata(source).map_err(|_| artifact_error())?;
        if source_metadata.file_type().is_symlink() || !source_metadata.is_file() {
            return Err(artifact_error());
        }
        let destination = self.root.join(name);
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(artifact_error());
        }
        fs::copy(source, &destination).map_err(|_| artifact_error())?;
        let copied = fs::symlink_metadata(&destination).map_err(|_| artifact_error())?;
        if copied.file_type().is_symlink() || !copied.is_file() {
            let _ = fs::remove_file(&destination);
            return Err(artifact_error());
        }
        let mut permissions = copied.permissions();
        permissions.set_readonly(true);
        if fs::set_permissions(&destination, permissions).is_err() {
            let _ = fs::remove_file(&destination);
            return Err(artifact_error());
        }
        Ok(())
    }

    #[cfg(unix)]
    fn revoke_name(&self, name: &str) -> Result<(), PortError> {
        rustix::fs::unlinkat(&self.descriptor, name, rustix::fs::AtFlags::empty())
            .map_err(|_| artifact_cleanup_error())
    }

    #[cfg(not(unix))]
    fn revoke_name(&self, name: &str) -> Result<(), PortError> {
        let path = self.root.join(name);
        make_removable(&path);
        fs::remove_file(path).map_err(|_| artifact_cleanup_error())
    }
}

struct ArtifactGrant {
    id: AudioArtifactId,
    directory: Arc<GrantDirectory>,
    name: Option<String>,
}

impl ArtifactGrant {
    fn create(directory: Arc<GrantDirectory>, source: &Path) -> Result<Self, PortError> {
        let id = AudioArtifactId::random();
        let name = format!("{id}{GRANT_FILE_SUFFIX}");
        directory.copy_grant(&name, source)?;
        Ok(Self {
            id,
            directory,
            name: Some(name),
        })
    }

    fn revoke(mut self) -> Result<(), PortError> {
        let name = self.name.as_deref().expect("live grant retains its name");
        self.directory.revoke_name(name)?;
        self.name = None;
        Ok(())
    }

    #[cfg(test)]
    fn path(&self) -> PathBuf {
        self.directory
            .root
            .join(self.name.as_deref().expect("live grant retains its name"))
    }
}

impl Drop for ArtifactGrant {
    fn drop(&mut self) {
        if let Some(name) = self.name.take() {
            let _ = self.directory.revoke_name(&name);
        }
    }
}

fn canonical_grant_name(name: &[u8]) -> bool {
    std::str::from_utf8(name)
        .ok()
        .and_then(|name| name.strip_suffix(GRANT_FILE_SUFFIX))
        .is_some_and(|id| id.parse::<AudioArtifactId>().is_ok())
}

#[cfg(test)]
fn prepare_grant_root(root: &Path) -> Result<(), PortError> {
    GrantDirectory::prepare(root).map(|_| ())
}

#[cfg(test)]
fn prepare_shared_root(root: &Path) -> Result<(), PortError> {
    let models_root = root.join(MODELS_DIRECTORY);
    for directory in [
        root.to_path_buf(),
        models_root.clone(),
        models_root.join(ACTIVE_MODELS_DIRECTORY),
        models_root.join(CANDIDATE_MODELS_DIRECTORY),
        root.join(GRANTS_DIRECTORY),
    ] {
        fs::create_dir_all(&directory).map_err(|_| artifact_error())?;
        let metadata = fs::symlink_metadata(&directory).map_err(|_| artifact_error())?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(artifact_error());
        }
        set_private_directory_permissions(&directory, metadata.permissions())?;
    }
    Ok(())
}

fn valid_app_group_identifier(identifier: &str) -> bool {
    let bytes = identifier.as_bytes();
    (3..=255).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && !identifier.contains("..")
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

fn validate_shared_paths(
    shared_root: &Path,
    grant_root: &Path,
    qwen_model: Option<&WorkerModelLaunch>,
    whisper_model: Option<&WorkerModelLaunch>,
) -> Result<(), PortError> {
    let canonical_root = exact_canonical_directory(shared_root)?;
    let expected_grants = shared_root.join(GRANTS_DIRECTORY);
    if grant_root.as_os_str() != expected_grants.as_os_str() {
        return Err(invalid_worker_configuration());
    }
    let canonical_grants = exact_canonical_directory(grant_root)?;
    if !canonical_grants.starts_with(&canonical_root) {
        return Err(invalid_worker_configuration());
    }

    let models_root = shared_root.join(MODELS_DIRECTORY);
    let canonical_models_root = exact_canonical_directory(&models_root)?;
    let active_root = exact_canonical_directory(&models_root.join(ACTIVE_MODELS_DIRECTORY))?;
    if !canonical_models_root.starts_with(&canonical_root)
        || !active_root.starts_with(&canonical_models_root)
    {
        return Err(invalid_worker_configuration());
    }
    if let Some(model) = qwen_model {
        validate_model_path(&model.path, true, &active_root)?;
    }
    if let Some(model) = whisper_model {
        validate_model_path(&model.path, false, &active_root)?;
    }
    Ok(())
}

fn validate_model_path(
    path: &Path,
    expect_directory: bool,
    active_root: &Path,
) -> Result<(), PortError> {
    if !path.is_absolute() || !path.starts_with(active_root) {
        return Err(invalid_worker_configuration());
    }
    let canonical = fs::canonicalize(path).map_err(|_| invalid_worker_configuration())?;
    if canonical.as_os_str() != path.as_os_str() {
        return Err(invalid_worker_configuration());
    }

    let relative = path
        .strip_prefix(active_root)
        .map_err(|_| invalid_worker_configuration())?;
    let mut current = active_root.to_path_buf();
    let mut components = relative.components().peekable();
    if components.peek().is_none() {
        let metadata =
            fs::symlink_metadata(&current).map_err(|_| invalid_worker_configuration())?;
        return if (expect_directory && metadata.is_dir())
            || (!expect_directory && metadata.is_file())
        {
            Ok(())
        } else {
            Err(invalid_worker_configuration())
        };
    }
    while let Some(component) = components.next() {
        let std::path::Component::Normal(component) = component else {
            return Err(invalid_worker_configuration());
        };
        current.push(component);
        let metadata =
            fs::symlink_metadata(&current).map_err(|_| invalid_worker_configuration())?;
        if metadata.file_type().is_symlink() || (components.peek().is_some() && !metadata.is_dir())
        {
            return Err(invalid_worker_configuration());
        }
        if components.peek().is_none()
            && ((expect_directory && !metadata.is_dir())
                || (!expect_directory && !metadata.is_file()))
        {
            return Err(invalid_worker_configuration());
        }
    }
    Ok(())
}

fn exact_canonical_directory(path: &Path) -> Result<PathBuf, PortError> {
    if !path.is_absolute() {
        return Err(invalid_worker_configuration());
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| invalid_worker_configuration())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid_worker_configuration());
    }
    let canonical = fs::canonicalize(path).map_err(|_| invalid_worker_configuration())?;
    if canonical.as_os_str() == path.as_os_str() {
        Ok(canonical)
    } else {
        Err(invalid_worker_configuration())
    }
}

#[cfg(all(test, unix))]
fn set_private_directory_permissions(
    path: &Path,
    mut permissions: fs::Permissions,
) -> Result<(), PortError> {
    use std::os::unix::fs::PermissionsExt;

    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).map_err(|_| artifact_error())
}

#[cfg(not(unix))]
fn set_private_directory_permissions(
    _path: &Path,
    _permissions: fs::Permissions,
) -> Result<(), PortError> {
    Ok(())
}

#[cfg(not(unix))]
fn make_removable(path: &Path) {
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_readonly(false);
        let _ = fs::set_permissions(path, permissions);
    }
}

fn spawn_worker_reader<R: Read + Send + 'static>(
    reader: R,
    sender: mpsc::Sender<Result<WorkerToCoreEnvelope, ()>>,
) {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        loop {
            match read_protocol_frame(&mut reader) {
                Ok(Some(frame)) => match serde_json::from_slice(&frame) {
                    Ok(envelope) => {
                        if sender.send(Ok(envelope)).is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = sender.send(Err(()));
                        return;
                    }
                },
                Ok(None) | Err(()) => {
                    let _ = sender.send(Err(()));
                    return;
                }
            }
        }
    });
}

fn spawn_stderr_drain<R: Read + Send + 'static>(mut reader: R) {
    thread::spawn(move || {
        let mut buffer = [0_u8; 1024];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 {
                return;
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

fn validate_ready_engines(ready: &WorkerReady) -> Result<(), PortError> {
    if ready.supported_engines.contains(&WorkerEngineId::Qwen)
        && ready.supported_engines.contains(&WorkerEngineId::Whisper)
    {
        Ok(())
    } else {
        Err(port_error(
            "asr.worker.incompatible",
            "errors.asr.worker_incompatible",
            false,
        ))
    }
}

fn core_envelope(
    session_id: Option<Uuid>,
    request_id: Option<Uuid>,
    message: CoreToWorkerMessage,
) -> CoreToWorkerEnvelope {
    CoreToWorkerEnvelope {
        contract_version: CONTRACT_VERSION,
        message_id: Uuid::new_v4(),
        session_id,
        request_id,
        sent_at: now_rfc3339(),
        message,
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn required_capabilities() -> Vec<WorkerCapability> {
    vec![
        WorkerCapability::HealthCheck,
        WorkerCapability::FinalTranscript,
        WorkerCapability::Cancellation,
        WorkerCapability::GracefulShutdown,
    ]
}

fn worker_engine(engine: AsrEngine) -> WorkerEngineId {
    match engine {
        AsrEngine::Qwen => WorkerEngineId::Qwen,
        AsrEngine::Whisper => WorkerEngineId::Whisper,
    }
}

fn envelope_deadline_ms(envelope: &CoreToWorkerEnvelope) -> u64 {
    match &envelope.message {
        CoreToWorkerMessage::Transcribe(request) => request.deadline_ms,
        _ => 1,
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn deadline_after(duration: Duration) -> Instant {
    Instant::now()
        .checked_add(duration)
        .unwrap_or_else(Instant::now)
}

fn map_worker_error(error: &WorkerError) -> PortError {
    if error.safe_message_key == "worker.error.deadline_exceeded" {
        return timeout_error();
    }
    if error.code == WorkerErrorCode::InferenceFailed
        && matches!(
            error.safe_message_key.as_str(),
            "worker.qwen.empty_transcript" | "worker.whisper.empty_transcript"
        )
    {
        return port_error(ASR_NO_SPEECH_CODE, &error.safe_message_key, false);
    }
    let code = match error.code {
        WorkerErrorCode::ProtocolIncompatible => "asr.worker.incompatible",
        WorkerErrorCode::InvalidRequest => "asr.worker.invalid_request",
        WorkerErrorCode::EngineUnavailable => "asr.engine_unavailable",
        WorkerErrorCode::ModelMissing => "asr.model_missing",
        WorkerErrorCode::ModelIncompatible => "asr.model_incompatible",
        WorkerErrorCode::InferenceFailed => "asr.transcription_failed",
        WorkerErrorCode::CancellationFailed => "asr.cancel_failed",
        WorkerErrorCode::Internal => "asr.worker.internal",
    };
    PortError {
        code: code.to_owned(),
        safe_message_key: error.safe_message_key.clone(),
        retryable: error.retryable,
    }
}

fn port_error(code: &str, safe_message_key: &str, retryable: bool) -> PortError {
    PortError {
        code: code.to_owned(),
        safe_message_key: safe_message_key.to_owned(),
        retryable,
    }
}

fn invalid_worker_configuration() -> PortError {
    port_error(
        "asr.worker.invalid_configuration",
        "errors.asr.worker_invalid_configuration",
        false,
    )
}

fn worker_busy_error() -> PortError {
    port_error("asr.worker.busy", "errors.asr.worker_busy", true)
}

fn worker_spawn_error() -> PortError {
    port_error(
        "asr.worker.spawn_failed",
        "errors.asr.worker_unavailable",
        true,
    )
}

fn worker_crashed_error() -> PortError {
    port_error("asr.worker.crashed", "errors.asr.worker_crashed", true)
}

fn handshake_error() -> PortError {
    port_error(
        "asr.worker.handshake_failed",
        "errors.asr.worker_incompatible",
        true,
    )
}

fn protocol_error() -> PortError {
    port_error(
        "asr.worker.protocol_rejected",
        "errors.asr.worker_incompatible",
        false,
    )
}

fn timeout_error() -> PortError {
    port_error("asr.worker.timeout", "errors.asr.timeout", true)
}

fn cancelled_error() -> PortError {
    port_error("asr.cancelled", "errors.asr.cancelled", false)
}

fn supervisor_closed_error() -> PortError {
    port_error(
        "asr.worker.supervisor_closed",
        "errors.asr.worker_unavailable",
        true,
    )
}

fn artifact_error() -> PortError {
    port_error(
        "asr.audio_grant_failed",
        "errors.asr.audio_artifact_unavailable",
        true,
    )
}

fn artifact_cleanup_error() -> PortError {
    port_error(
        "asr.audio_grant_cleanup_failed",
        "errors.asr.audio_cleanup_failed",
        true,
    )
}

struct ResponseState<T> {
    value: Option<T>,
    waker: Option<Waker>,
}

struct ResponseSender<T> {
    state: Arc<Mutex<ResponseState<T>>>,
}

struct ResponseFuture<T> {
    state: Arc<Mutex<ResponseState<T>>>,
}

fn response_pair<T>() -> (ResponseSender<T>, ResponseFuture<T>) {
    let state = Arc::new(Mutex::new(ResponseState {
        value: None,
        waker: None,
    }));
    (
        ResponseSender {
            state: Arc::clone(&state),
        },
        ResponseFuture { state },
    )
}

impl<T> ResponseSender<T> {
    fn complete(self, value: T) {
        let waker = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.value.is_some() {
                return;
            }
            state.value = Some(value);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> ResponseFuture<T> {
    fn complete_if_pending(&self, value: T) {
        let waker = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.value.is_some() {
                return;
            }
            state.value = Some(value);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> Future for ResponseFuture<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(value) = state.value.take() {
            Poll::Ready(value)
        } else {
            if state
                .waker
                .as_ref()
                .is_none_or(|waker| !waker.will_wake(context.waker()))
            {
                state.waker = Some(context.waker().clone());
            }
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_transcripts_are_distinct_from_real_inference_failures() {
        for safe_message_key in [
            "worker.qwen.empty_transcript",
            "worker.whisper.empty_transcript",
        ] {
            let mapped = map_worker_error(&WorkerError {
                code: WorkerErrorCode::InferenceFailed,
                retryable: false,
                fatal: false,
                safe_message_key: safe_message_key.to_owned(),
            });
            assert_eq!(mapped.code, ASR_NO_SPEECH_CODE);
            assert_eq!(mapped.safe_message_key, safe_message_key);
            assert!(!mapped.retryable);
        }

        let mapped = map_worker_error(&WorkerError {
            code: WorkerErrorCode::InferenceFailed,
            retryable: true,
            fatal: false,
            safe_message_key: "worker.qwen.inference_failed".to_owned(),
        });
        assert_eq!(mapped.code, "asr.transcription_failed");
        assert!(mapped.retryable);
    }

    #[test]
    fn launch_config_rejects_empty_model_ids_and_zero_timeouts() {
        let root = test_root("config");
        let error = WorkerLaunchConfig::new("worker", &root, "0.1.0", "", "whisper")
            .expect_err("empty model id must fail");
        assert_eq!(error.code, "asr.worker.invalid_configuration");

        let config = WorkerLaunchConfig::new("worker", &root, "0.1.0", "qwen", "whisper")
            .expect("valid config")
            .with_timeouts(
                Duration::ZERO,
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
            );
        assert!(config.validate().is_err());
        fs::remove_dir_all(root).expect("remove config root");
    }

    #[test]
    fn artifact_grant_is_read_only_and_revoked() {
        let root = test_root("grant");
        let source = root.with_extension("source.wav");
        fs::write(&source, b"audio").expect("write source");
        let directory = Arc::new(GrantDirectory::prepare(&root).expect("prepare grant root"));
        let grant = ArtifactGrant::create(directory, &source).expect("create grant");
        let path = grant.path();
        assert!(path.is_file());
        assert!(
            fs::metadata(&path)
                .expect("grant metadata")
                .permissions()
                .readonly()
        );
        grant.revoke().expect("revoke grant");
        assert!(!path.exists());
        fs::remove_file(source).expect("remove source");
        fs::remove_dir_all(root).expect("remove grant root");
    }

    #[cfg(unix)]
    #[test]
    fn grant_directory_capability_does_not_follow_replacement_directory() {
        let root = test_root("grant-replaced-directory");
        let original = root.with_extension("original");
        let source = root.with_extension("source.wav");
        fs::write(&source, b"audio").expect("write source");
        let config = WorkerLaunchConfig::new("worker", &root, "0.1.0", "qwen", "whisper")
            .expect("valid config");

        fs::rename(&root, &original).expect("move the descriptor-bound directory");
        fs::create_dir(&root).expect("replace grant path with another directory");
        config.validate().expect("held capability remains valid");
        let grant = ArtifactGrant::create(Arc::clone(&config.grant_directory), &source)
            .expect("create through held directory descriptor");

        assert_eq!(fs::read_dir(&root).expect("read replacement").count(), 0);
        assert_eq!(fs::read_dir(&original).expect("read original").count(), 1);
        grant.revoke().expect("revoke through held descriptor");
        drop(config);
        fs::remove_dir(root).expect("remove replacement directory");
        fs::remove_dir(original).expect("remove original directory");
        fs::remove_file(source).expect("remove source");
    }

    #[cfg(unix)]
    #[test]
    fn grant_directory_capability_does_not_follow_replacement_symlink() {
        use std::os::unix::fs::symlink;

        let root = test_root("grant-replaced-symlink");
        let original = root.with_extension("original");
        let replacement = root.with_extension("replacement");
        let source = root.with_extension("source.wav");
        fs::write(&source, b"audio").expect("write source");
        let config = WorkerLaunchConfig::new("worker", &root, "0.1.0", "qwen", "whisper")
            .expect("valid config");

        fs::rename(&root, &original).expect("move the descriptor-bound directory");
        fs::create_dir(&replacement).expect("create symlink target");
        symlink(&replacement, &root).expect("replace grant path with symlink");
        config.validate().expect("held capability remains valid");
        let grant = ArtifactGrant::create(Arc::clone(&config.grant_directory), &source)
            .expect("create through held directory descriptor");

        assert_eq!(
            fs::read_dir(&replacement)
                .expect("read symlink target")
                .count(),
            0
        );
        assert_eq!(fs::read_dir(&original).expect("read original").count(), 1);
        grant.revoke().expect("revoke through held descriptor");
        drop(config);
        fs::remove_file(root).expect("remove replacement symlink");
        fs::remove_dir(replacement).expect("remove symlink target");
        fs::remove_dir(original).expect("remove original directory");
        fs::remove_file(source).expect("remove source");
    }

    #[test]
    fn explicit_grant_revoke_reports_cleanup_failure() {
        let root = test_root("grant-revoke-error");
        let source = root.with_extension("source.wav");
        fs::write(&source, b"audio").expect("write source");
        let directory = Arc::new(GrantDirectory::prepare(&root).expect("prepare grant root"));
        let grant = ArtifactGrant::create(directory, &source).expect("create grant");
        let path = grant.path();
        fs::remove_file(path).expect("remove grant before explicit revoke");

        assert_eq!(
            grant
                .revoke()
                .expect_err("failed explicit cleanup must be observable")
                .code,
            "asr.audio_grant_cleanup_failed"
        );
        fs::remove_file(source).expect("remove source");
        fs::remove_dir(root).expect("remove grant root");
    }

    #[test]
    fn sandboxed_launch_config_derives_fixed_roots_and_accepts_active_models() {
        let root = prepared_sandbox_root("sandboxed-config");
        let qwen = root.join("models/active/qwen/version");
        let whisper = root.join("models/active/whisper/model.bin");
        fs::create_dir_all(&qwen).expect("create Qwen model directory");
        fs::create_dir_all(whisper.parent().expect("Whisper parent"))
            .expect("create Whisper model directory");
        fs::write(&whisper, b"model").expect("write Whisper model");

        let config = WorkerLaunchConfig::new_sandboxed(
            "worker",
            "TEAM123456.remtene.asr",
            &root,
            "0.1.0",
            "qwen",
            "whisper",
        )
        .expect("sandboxed config")
        .with_qwen_model(&qwen, "qwen-version")
        .with_whisper_model(&whisper, "whisper-version");
        config.validate().expect("models remain inside shared root");
        assert_eq!(config.shared_root.as_deref(), Some(root.as_path()));
        assert_eq!(config.grant_root, root.join("grants"));
        fs::remove_dir_all(root).expect("remove shared root");
    }

    #[test]
    fn sandboxed_launch_config_rejects_models_outside_shared_root() {
        let root = prepared_sandbox_root("sandboxed-outside");
        let outside = root.with_extension("outside-model");
        fs::create_dir_all(&outside).expect("create outside model");
        let config = WorkerLaunchConfig::new_sandboxed(
            "worker",
            "TEAM123456.remtene.asr",
            &root,
            "0.1.0",
            "qwen",
            "whisper",
        )
        .expect("sandboxed config")
        .with_qwen_model(&outside, "qwen-version");

        assert_eq!(
            config.validate().expect_err("outside model must fail").code,
            "asr.worker.invalid_configuration"
        );
        fs::remove_dir_all(root).expect("remove shared root");
        fs::remove_dir_all(outside).expect("remove outside model");
    }

    #[test]
    fn sandboxed_launch_config_rejects_candidate_models_before_activation() {
        let root = prepared_sandbox_root("sandboxed-candidate");
        let candidate = root.join("models/candidates/qwen/version");
        fs::create_dir_all(&candidate).expect("create candidate model directory");
        let config = WorkerLaunchConfig::new_sandboxed(
            "worker",
            "TEAM123456.remtene.asr",
            &root,
            "0.1.0",
            "qwen",
            "whisper",
        )
        .expect("sandboxed config")
        .with_qwen_model(&candidate, "candidate-version");

        assert_eq!(
            config
                .validate()
                .expect_err("candidate must be activated before inference")
                .code,
            "asr.worker.invalid_configuration"
        );
        fs::remove_dir_all(root).expect("remove shared root");
    }

    #[cfg(unix)]
    #[test]
    fn sandboxed_launch_config_rejects_replaced_models_root() {
        use std::os::unix::fs::symlink;

        let root = prepared_sandbox_root("sandboxed-model-root-symlink");
        let original_models = root.join("original-models");
        let outside_models = root.with_extension("outside-models");
        let outside_qwen = outside_models.join("active/qwen/version");
        let config = WorkerLaunchConfig::new_sandboxed(
            "worker",
            "TEAM123456.remtene.asr",
            &root,
            "0.1.0",
            "qwen",
            "whisper",
        )
        .expect("sandboxed config");
        fs::rename(root.join("models"), &original_models).expect("move models root");
        fs::create_dir_all(&outside_qwen).expect("create outside active model");
        symlink(&outside_models, root.join("models")).expect("replace models root");
        let config = config.with_qwen_model(&outside_qwen, "outside-version");

        assert_eq!(
            config
                .validate()
                .expect_err("replaced models root must not become trusted")
                .code,
            "asr.worker.invalid_configuration"
        );
        fs::remove_file(root.join("models")).expect("remove models symlink");
        fs::remove_dir_all(root).expect("remove shared root");
        fs::remove_dir_all(outside_models).expect("remove outside models");
    }

    #[cfg(unix)]
    #[test]
    fn sandboxed_launch_config_rejects_a_symlinked_ancestor() {
        use std::os::unix::fs::symlink;

        let parent = canonical_test_directory("sandboxed-ancestor");
        let real_parent = parent.join("real-parent");
        let alias_parent = parent.join("alias-parent");
        let real_root = real_parent.join("asr-root");
        fs::create_dir_all(&real_parent).expect("create real parent");
        prepare_shared_root(&real_root).expect("prepare real shared layout");
        symlink(&real_parent, &alias_parent).expect("create ancestor symlink");
        let aliased_root = alias_parent.join("asr-root");

        assert_eq!(
            WorkerLaunchConfig::new_sandboxed(
                "worker",
                "TEAM123456.remtene.asr",
                &aliased_root,
                "0.1.0",
                "qwen",
                "whisper",
            )
            .expect_err("ancestor symlink must not become a sandbox trust root")
            .code,
            "asr.worker.invalid_configuration"
        );
        fs::remove_file(alias_parent).expect("remove ancestor symlink");
        fs::remove_dir_all(parent).expect("remove test root");
    }

    #[test]
    fn sandboxed_launch_config_rejects_a_noncanonical_root() {
        let root = prepared_sandbox_root("sandboxed-noncanonical");
        let noncanonical_root = root.join(".");

        assert_eq!(
            WorkerLaunchConfig::new_sandboxed(
                "worker",
                "TEAM123456.remtene.asr",
                &noncanonical_root,
                "0.1.0",
                "qwen",
                "whisper",
            )
            .expect_err("noncanonical root must not become a sandbox trust root")
            .code,
            "asr.worker.invalid_configuration"
        );
        fs::remove_dir_all(root).expect("remove shared root");
    }

    #[cfg(unix)]
    #[test]
    fn sandboxed_launch_config_rejects_symlinks_below_active_root() {
        use std::os::unix::fs::symlink;

        let root = prepared_sandbox_root("sandboxed-model-component-symlink");
        let real_model = root.join("models/active/real-qwen/version");
        let alias_model = root.join("models/active/alias-qwen");
        fs::create_dir_all(&real_model).expect("create real model directory");
        symlink(root.join("models/active/real-qwen"), &alias_model)
            .expect("create model component symlink");
        let config = WorkerLaunchConfig::new_sandboxed(
            "worker",
            "TEAM123456.remtene.asr",
            &root,
            "0.1.0",
            "qwen",
            "whisper",
        )
        .expect("sandboxed config")
        .with_qwen_model(alias_model.join("version"), "symlink-version");

        assert_eq!(
            config
                .validate()
                .expect_err("model path components must not be symlinks")
                .code,
            "asr.worker.invalid_configuration"
        );
        fs::remove_dir_all(root).expect("remove shared root");
    }

    #[test]
    fn grant_root_rejects_unknown_entries() {
        let root = test_root("grant-pollution");
        fs::create_dir_all(&root).expect("create grant root");
        fs::write(root.join("settings.json"), b"must not share").expect("write unexpected entry");

        assert_eq!(
            prepare_grant_root(&root)
                .expect_err("unknown grant entry must fail")
                .code,
            "asr.audio_grant_failed"
        );
        fs::remove_dir_all(root).expect("remove polluted grant root");
    }

    #[cfg(unix)]
    #[test]
    fn grant_root_rejects_a_symlink_named_like_a_grant() {
        use std::os::unix::fs::symlink;

        let root = test_root("grant-symlink");
        let outside = root.with_extension("outside.wav");
        fs::create_dir_all(&root).expect("create grant root");
        fs::write(&outside, b"outside").expect("write outside audio");
        let grant_name = format!("{}{}", AudioArtifactId::random(), GRANT_FILE_SUFFIX);
        symlink(&outside, root.join(grant_name)).expect("create grant symlink");

        assert_eq!(
            prepare_grant_root(&root)
                .expect_err("grant symlink must fail")
                .code,
            "asr.audio_grant_failed"
        );
        assert_eq!(fs::read(&outside).expect("outside remains"), b"outside");
        fs::remove_dir_all(root).expect("remove symlink root");
        fs::remove_file(outside).expect("remove outside audio");
    }

    #[test]
    fn response_future_wakes_without_an_async_runtime() {
        let (sender, mut future) = response_pair();
        sender.complete(42_u8);
        let mut context = Context::from_waker(Waker::noop());
        assert_eq!(Pin::new(&mut future).poll(&mut context), Poll::Ready(42));
    }

    #[cfg(unix)]
    #[test]
    fn worker_process_command_does_not_inherit_parent_environment() {
        let output = command_without_inherited_environment(Path::new("/usr/bin/env"))
            .output()
            .expect("run environment probe");
        assert!(output.status.success());
        assert!(
            output.stdout.is_empty(),
            "worker inherited environment data"
        );
    }

    #[cfg(unix)]
    #[test]
    fn worker_command_disables_the_qwen_cache_that_would_mutate_the_model_package() {
        // `qwen-asr` defaults to writing its INT8 cache into the model directory, which
        // invalidates the per-file hashes the package was admitted under (DEC-MODEL-01).
        // The Supervisor must hand the Worker an environment that turns this off.
        let root = test_root("qwen-sidecar-env");
        let config = WorkerLaunchConfig::new("worker", &root, "0.1.0", "qwen", "whisper")
            .expect("valid config");

        let command = worker_command(&config);
        let environment: Vec<(&std::ffi::OsStr, Option<&std::ffi::OsStr>)> =
            command.get_envs().collect();

        assert_eq!(
            environment,
            vec![(
                std::ffi::OsStr::new(QWEN_SIDECAR_CACHE_ENV),
                Some(std::ffi::OsStr::new("0"))
            )],
            "the Worker environment must carry the cache switch and nothing else"
        );

        drop(config);
        fs::remove_dir_all(root).expect("remove test root");
    }

    fn prepared_sandbox_root(label: &str) -> PathBuf {
        let root = test_root(label);
        prepare_shared_root(&root).expect("prepare platform-owned shared layout");
        fs::canonicalize(root).expect("canonicalize shared layout")
    }

    fn canonical_test_directory(label: &str) -> PathBuf {
        let root = test_root(label);
        fs::create_dir_all(&root).expect("create test directory");
        fs::canonicalize(root).expect("canonicalize test directory")
    }

    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("remtene-adapter-{label}-{}", Uuid::new_v4()))
    }
}
