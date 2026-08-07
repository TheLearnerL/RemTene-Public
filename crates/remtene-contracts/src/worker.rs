use std::{collections::BTreeSet, fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use uuid::Uuid;

use crate::CONTRACT_VERSION;

/// An opaque identifier for an audio artifact owned by the Rust Core.
///
/// This value is deliberately a non-nil UUID rather than a path-shaped string. Resolving it to a
/// private temporary resource remains the responsibility of the future Worker adapter; the
/// protocol does not select a transport or expose arbitrary filesystem paths.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AudioArtifactId(Uuid);

impl AudioArtifactId {
    #[must_use]
    pub fn random() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn from_uuid(value: Uuid) -> Result<Self, AudioArtifactIdError> {
        if value.is_nil() {
            Err(AudioArtifactIdError::Nil)
        } else {
            Ok(Self(value))
        }
    }

    #[must_use]
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl fmt::Display for AudioArtifactId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(formatter)
    }
}

impl FromStr for AudioArtifactId {
    type Err = AudioArtifactIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let uuid = Uuid::parse_str(value).map_err(|_| AudioArtifactIdError::InvalidFormat)?;
        if uuid.hyphenated().to_string() != value {
            return Err(AudioArtifactIdError::InvalidFormat);
        }
        Self::from_uuid(uuid)
    }
}

impl Serialize for AudioArtifactId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for AudioArtifactId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AudioArtifactIdError {
    #[error("audio artifact id must be a canonical UUID")]
    InvalidFormat,
    #[error("audio artifact id must not be nil")]
    Nil,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerEngineId {
    Qwen,
    Whisper,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerCapability {
    HealthCheck,
    FinalTranscript,
    Cancellation,
    GracefulShutdown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerMessageKind {
    Hello,
    HealthCheck,
    Transcribe,
    Cancel,
    Shutdown,
    Ready,
    HealthResult,
    Transcript,
    Cancelled,
    Error,
    ShutdownComplete,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum CoreToWorkerMessage {
    Hello(CoreHello),
    HealthCheck(HealthCheckRequest),
    Transcribe(TranscribeRequest),
    Cancel(CancelRequest),
    Shutdown(ShutdownRequest),
}

impl CoreToWorkerMessage {
    #[must_use]
    pub const fn kind(&self) -> WorkerMessageKind {
        match self {
            Self::Hello(_) => WorkerMessageKind::Hello,
            Self::HealthCheck(_) => WorkerMessageKind::HealthCheck,
            Self::Transcribe(_) => WorkerMessageKind::Transcribe,
            Self::Cancel(_) => WorkerMessageKind::Cancel,
            Self::Shutdown(_) => WorkerMessageKind::Shutdown,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum WorkerToCoreMessage {
    Ready(WorkerReady),
    HealthResult(HealthResult),
    Transcript(TranscriptResult),
    Cancelled(CancelledResult),
    Error(WorkerError),
    ShutdownComplete(ShutdownComplete),
}

impl WorkerToCoreMessage {
    #[must_use]
    pub const fn kind(&self) -> WorkerMessageKind {
        match self {
            Self::Ready(_) => WorkerMessageKind::Ready,
            Self::HealthResult(_) => WorkerMessageKind::HealthResult,
            Self::Transcript(_) => WorkerMessageKind::Transcript,
            Self::Cancelled(_) => WorkerMessageKind::Cancelled,
            Self::Error(_) => WorkerMessageKind::Error,
            Self::ShutdownComplete(_) => WorkerMessageKind::ShutdownComplete,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CoreToWorkerEnvelope {
    pub contract_version: u16,
    pub message_id: Uuid,
    pub session_id: Option<Uuid>,
    pub request_id: Option<Uuid>,
    pub sent_at: String,
    #[serde(flatten)]
    pub message: CoreToWorkerMessage,
}

impl<'de> Deserialize<'de> for CoreToWorkerEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        CoreToWorkerEnvelopeWire::deserialize(deserializer).map(Into::into)
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CoreToWorkerEnvelopeWire {
    Hello {
        contract_version: u16,
        message_id: Uuid,
        session_id: Option<Uuid>,
        request_id: Option<Uuid>,
        sent_at: String,
        payload: CoreHello,
    },
    HealthCheck {
        contract_version: u16,
        message_id: Uuid,
        session_id: Option<Uuid>,
        request_id: Option<Uuid>,
        sent_at: String,
        payload: HealthCheckRequest,
    },
    Transcribe {
        contract_version: u16,
        message_id: Uuid,
        session_id: Option<Uuid>,
        request_id: Option<Uuid>,
        sent_at: String,
        payload: TranscribeRequest,
    },
    Cancel {
        contract_version: u16,
        message_id: Uuid,
        session_id: Option<Uuid>,
        request_id: Option<Uuid>,
        sent_at: String,
        payload: CancelRequest,
    },
    Shutdown {
        contract_version: u16,
        message_id: Uuid,
        session_id: Option<Uuid>,
        request_id: Option<Uuid>,
        sent_at: String,
        payload: ShutdownRequest,
    },
}

impl From<CoreToWorkerEnvelopeWire> for CoreToWorkerEnvelope {
    fn from(value: CoreToWorkerEnvelopeWire) -> Self {
        match value {
            CoreToWorkerEnvelopeWire::Hello {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                payload,
            } => Self {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                message: CoreToWorkerMessage::Hello(payload),
            },
            CoreToWorkerEnvelopeWire::HealthCheck {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                payload,
            } => Self {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                message: CoreToWorkerMessage::HealthCheck(payload),
            },
            CoreToWorkerEnvelopeWire::Transcribe {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                payload,
            } => Self {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                message: CoreToWorkerMessage::Transcribe(payload),
            },
            CoreToWorkerEnvelopeWire::Cancel {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                payload,
            } => Self {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                message: CoreToWorkerMessage::Cancel(payload),
            },
            CoreToWorkerEnvelopeWire::Shutdown {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                payload,
            } => Self {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                message: CoreToWorkerMessage::Shutdown(payload),
            },
        }
    }
}

impl CoreToWorkerEnvelope {
    pub fn validate(&self) -> Result<(), WorkerProtocolError> {
        validate_common_envelope(
            self.contract_version,
            self.message_id,
            self.session_id,
            self.request_id,
            &self.sent_at,
        )?;

        match &self.message {
            CoreToWorkerMessage::Hello(payload) => {
                require_correlations(self.session_id, self.request_id, false, false)?;
                validate_hello(payload)
            }
            CoreToWorkerMessage::HealthCheck(payload) => {
                require_correlations(self.session_id, self.request_id, false, true)?;
                require_non_empty("health_check.model_id", &payload.model_id)
            }
            CoreToWorkerMessage::Transcribe(payload) => {
                require_correlations(self.session_id, self.request_id, true, true)?;
                require_matching_id("session_id", self.session_id, payload.session_id)?;
                require_matching_id("request_id", self.request_id, payload.request_id)?;
                require_non_empty("transcribe.model_id", &payload.model_id)?;
                if payload.audio_format.sample_rate_hz == 0
                    || payload.audio_format.channels == 0
                    || payload.audio_format.bits_per_sample == 0
                {
                    return Err(WorkerProtocolError::InvalidField("audio_format"));
                }
                if payload.deadline_ms == 0 {
                    return Err(WorkerProtocolError::InvalidField("deadline_ms"));
                }
                if payload
                    .language_hint
                    .as_ref()
                    .is_some_and(|value| value.trim().is_empty())
                {
                    return Err(WorkerProtocolError::InvalidField("language_hint"));
                }
                Ok(())
            }
            CoreToWorkerMessage::Cancel(payload) => {
                require_correlations(self.session_id, self.request_id, true, true)?;
                require_matching_id("session_id", self.session_id, payload.session_id)?;
                require_matching_id("request_id", self.request_id, payload.request_id)
            }
            CoreToWorkerMessage::Shutdown(_) => {
                require_correlations(self.session_id, self.request_id, false, false)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkerToCoreEnvelope {
    pub contract_version: u16,
    pub message_id: Uuid,
    pub session_id: Option<Uuid>,
    pub request_id: Option<Uuid>,
    pub sent_at: String,
    #[serde(flatten)]
    pub message: WorkerToCoreMessage,
}

impl<'de> Deserialize<'de> for WorkerToCoreEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        WorkerToCoreEnvelopeWire::deserialize(deserializer).map(Into::into)
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WorkerToCoreEnvelopeWire {
    Ready {
        contract_version: u16,
        message_id: Uuid,
        session_id: Option<Uuid>,
        request_id: Option<Uuid>,
        sent_at: String,
        payload: WorkerReady,
    },
    HealthResult {
        contract_version: u16,
        message_id: Uuid,
        session_id: Option<Uuid>,
        request_id: Option<Uuid>,
        sent_at: String,
        payload: HealthResult,
    },
    Transcript {
        contract_version: u16,
        message_id: Uuid,
        session_id: Option<Uuid>,
        request_id: Option<Uuid>,
        sent_at: String,
        payload: TranscriptResult,
    },
    Cancelled {
        contract_version: u16,
        message_id: Uuid,
        session_id: Option<Uuid>,
        request_id: Option<Uuid>,
        sent_at: String,
        payload: CancelledResult,
    },
    Error {
        contract_version: u16,
        message_id: Uuid,
        session_id: Option<Uuid>,
        request_id: Option<Uuid>,
        sent_at: String,
        payload: WorkerError,
    },
    ShutdownComplete {
        contract_version: u16,
        message_id: Uuid,
        session_id: Option<Uuid>,
        request_id: Option<Uuid>,
        sent_at: String,
        payload: ShutdownComplete,
    },
}

impl From<WorkerToCoreEnvelopeWire> for WorkerToCoreEnvelope {
    fn from(value: WorkerToCoreEnvelopeWire) -> Self {
        match value {
            WorkerToCoreEnvelopeWire::Ready {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                payload,
            } => Self {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                message: WorkerToCoreMessage::Ready(payload),
            },
            WorkerToCoreEnvelopeWire::HealthResult {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                payload,
            } => Self {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                message: WorkerToCoreMessage::HealthResult(payload),
            },
            WorkerToCoreEnvelopeWire::Transcript {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                payload,
            } => Self {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                message: WorkerToCoreMessage::Transcript(payload),
            },
            WorkerToCoreEnvelopeWire::Cancelled {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                payload,
            } => Self {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                message: WorkerToCoreMessage::Cancelled(payload),
            },
            WorkerToCoreEnvelopeWire::Error {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                payload,
            } => Self {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                message: WorkerToCoreMessage::Error(payload),
            },
            WorkerToCoreEnvelopeWire::ShutdownComplete {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                payload,
            } => Self {
                contract_version,
                message_id,
                session_id,
                request_id,
                sent_at,
                message: WorkerToCoreMessage::ShutdownComplete(payload),
            },
        }
    }
}

impl WorkerToCoreEnvelope {
    pub fn validate(&self) -> Result<(), WorkerProtocolError> {
        validate_common_envelope(
            self.contract_version,
            self.message_id,
            self.session_id,
            self.request_id,
            &self.sent_at,
        )?;

        match &self.message {
            WorkerToCoreMessage::Ready(payload) => {
                require_correlations(self.session_id, self.request_id, false, false)?;
                validate_ready(payload)
            }
            WorkerToCoreMessage::HealthResult(payload) => {
                require_correlations(self.session_id, self.request_id, false, true)?;
                require_non_empty("health_result.model_id", &payload.model_id)?;
                require_non_empty("health_result.model_version", &payload.model_version)?;
                require_non_empty("health_result.device_class", &payload.device_class)?;
                if payload
                    .safe_error_code
                    .as_ref()
                    .is_some_and(|value| value.trim().is_empty())
                {
                    return Err(WorkerProtocolError::InvalidField("safe_error_code"));
                }
                Ok(())
            }
            WorkerToCoreMessage::Transcript(payload) => {
                require_correlations(self.session_id, self.request_id, true, true)?;
                require_matching_id("session_id", self.session_id, payload.session_id)?;
                require_matching_id("request_id", self.request_id, payload.request_id)?;
                require_non_empty("transcript.model_id", &payload.model_id)?;
                require_non_empty("transcript.final_text", &payload.final_text)?;
                if payload
                    .detected_language
                    .as_ref()
                    .is_some_and(|value| value.trim().is_empty())
                {
                    return Err(WorkerProtocolError::InvalidField("detected_language"));
                }
                Ok(())
            }
            WorkerToCoreMessage::Cancelled(payload) => {
                require_correlations(self.session_id, self.request_id, true, true)?;
                require_matching_id("session_id", self.session_id, payload.session_id)?;
                require_matching_id("request_id", self.request_id, payload.request_id)
            }
            WorkerToCoreMessage::Error(payload) => {
                validate_error_correlations(self.session_id, self.request_id)?;
                require_non_empty("worker_error.safe_message_key", &payload.safe_message_key)
            }
            WorkerToCoreMessage::ShutdownComplete(payload) => {
                require_correlations(self.session_id, self.request_id, false, false)?;
                require_non_empty("shutdown_complete.worker_version", &payload.worker_version)
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoreHello {
    pub supported_protocol_versions: Vec<u16>,
    pub core_version: String,
    pub required_capabilities: Vec<WorkerCapability>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheckRequest {
    pub engine_id: WorkerEngineId,
    pub model_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioFormatDto {
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub bits_per_sample: u8,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TranscribeRequest {
    pub session_id: Uuid,
    pub request_id: Uuid,
    pub engine_id: WorkerEngineId,
    pub model_id: String,
    pub audio_artifact_id: AudioArtifactId,
    pub audio_format: AudioFormatDto,
    pub language_hint: Option<String>,
    pub deadline_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {
    pub session_id: Uuid,
    pub request_id: Uuid,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownRequest {
    pub grace_period_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerReady {
    pub protocol_version: u16,
    pub worker_version: String,
    pub supported_engines: Vec<WorkerEngineId>,
    pub runtime_id: String,
    pub capabilities: Vec<WorkerCapability>,
    pub build_signature_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Healthy,
    Unhealthy,
    Missing,
    Incompatible,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HealthResult {
    pub engine_id: WorkerEngineId,
    pub model_id: String,
    pub model_version: String,
    pub status: HealthStatus,
    pub device_class: String,
    pub safe_error_code: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptResult {
    pub session_id: Uuid,
    pub request_id: Uuid,
    pub engine_id: WorkerEngineId,
    pub model_id: String,
    pub final_text: String,
    pub detected_language: Option<String>,
    pub audio_duration_ms: u64,
    pub inference_duration_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CancelledResult {
    pub session_id: Uuid,
    pub request_id: Uuid,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerErrorCode {
    ProtocolIncompatible,
    InvalidRequest,
    EngineUnavailable,
    ModelMissing,
    ModelIncompatible,
    InferenceFailed,
    CancellationFailed,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerError {
    pub code: WorkerErrorCode,
    pub retryable: bool,
    pub fatal: bool,
    pub safe_message_key: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownComplete {
    pub worker_version: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerProtocolPhase {
    AwaitingHello,
    AwaitingReady,
    Ready,
    ShuttingDown,
    Closed,
}

/// Core-side protocol lifecycle guard.
///
/// The formal ASR Worker does not exist in M0. Its future decoder must consume these same DTOs and
/// apply equivalent state guards; this type currently proves the Core-side handshake semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerProtocolState {
    phase: WorkerProtocolPhase,
    offered_versions: Vec<u16>,
    required_capabilities: Vec<WorkerCapability>,
    negotiated_version: Option<u16>,
    active_requests: BTreeSet<(Uuid, Uuid)>,
}

impl Default for WorkerProtocolState {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkerProtocolState {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            phase: WorkerProtocolPhase::AwaitingHello,
            offered_versions: Vec::new(),
            required_capabilities: Vec::new(),
            negotiated_version: None,
            active_requests: BTreeSet::new(),
        }
    }

    #[must_use]
    pub const fn phase(&self) -> WorkerProtocolPhase {
        self.phase
    }

    #[must_use]
    pub const fn negotiated_version(&self) -> Option<u16> {
        self.negotiated_version
    }

    pub fn observe_core(
        &mut self,
        envelope: &CoreToWorkerEnvelope,
    ) -> Result<(), WorkerProtocolError> {
        envelope.validate()?;
        let kind = envelope.message.kind();

        match (self.phase, &envelope.message) {
            (WorkerProtocolPhase::AwaitingHello, CoreToWorkerMessage::Hello(hello)) => {
                self.offered_versions
                    .clone_from(&hello.supported_protocol_versions);
                self.required_capabilities
                    .clone_from(&hello.required_capabilities);
                self.phase = WorkerProtocolPhase::AwaitingReady;
                Ok(())
            }
            (WorkerProtocolPhase::AwaitingReady, CoreToWorkerMessage::Shutdown(_))
            | (WorkerProtocolPhase::Ready, CoreToWorkerMessage::Shutdown(_)) => {
                self.phase = WorkerProtocolPhase::ShuttingDown;
                Ok(())
            }
            (WorkerProtocolPhase::Ready, CoreToWorkerMessage::HealthCheck(_)) => Ok(()),
            (WorkerProtocolPhase::Ready, CoreToWorkerMessage::Transcribe(_)) => {
                let correlation = request_correlation(envelope.session_id, envelope.request_id)?;
                if self.active_requests.insert(correlation) {
                    Ok(())
                } else {
                    Err(WorkerProtocolError::DuplicateRequestCorrelation {
                        session_id: correlation.0,
                        request_id: correlation.1,
                    })
                }
            }
            (WorkerProtocolPhase::Ready, CoreToWorkerMessage::Cancel(_)) => {
                let correlation = request_correlation(envelope.session_id, envelope.request_id)?;
                self.require_active_request(correlation)
            }
            _ => Err(WorkerProtocolError::UnexpectedMessage {
                phase: self.phase,
                direction: ProtocolDirection::CoreToWorker,
                kind,
            }),
        }
    }

    pub fn observe_worker(
        &mut self,
        envelope: &WorkerToCoreEnvelope,
    ) -> Result<(), WorkerProtocolError> {
        envelope.validate()?;
        let kind = envelope.message.kind();

        match (self.phase, &envelope.message) {
            (WorkerProtocolPhase::AwaitingReady, WorkerToCoreMessage::Ready(ready)) => {
                if !self.offered_versions.contains(&ready.protocol_version) {
                    self.phase = WorkerProtocolPhase::Closed;
                    return Err(WorkerProtocolError::NegotiationFailed(
                        "worker selected an unsupported protocol version",
                    ));
                }
                if let Some(missing) = self
                    .required_capabilities
                    .iter()
                    .find(|capability| !ready.capabilities.contains(capability))
                {
                    self.phase = WorkerProtocolPhase::Closed;
                    return Err(WorkerProtocolError::MissingCapability(*missing));
                }
                self.negotiated_version = Some(ready.protocol_version);
                self.phase = WorkerProtocolPhase::Ready;
                Ok(())
            }
            (WorkerProtocolPhase::AwaitingReady, WorkerToCoreMessage::Error(error))
            | (WorkerProtocolPhase::Ready, WorkerToCoreMessage::Error(error))
            | (WorkerProtocolPhase::ShuttingDown, WorkerToCoreMessage::Error(error)) => {
                if let Some(correlation) =
                    error_request_correlation(envelope.session_id, envelope.request_id)?
                {
                    self.complete_active_request(correlation)?;
                }
                if error.fatal {
                    self.active_requests.clear();
                    self.phase = WorkerProtocolPhase::Closed;
                }
                Ok(())
            }
            (WorkerProtocolPhase::Ready, WorkerToCoreMessage::HealthResult(_)) => Ok(()),
            (
                WorkerProtocolPhase::Ready,
                WorkerToCoreMessage::Transcript(_) | WorkerToCoreMessage::Cancelled(_),
            ) => {
                let correlation = request_correlation(envelope.session_id, envelope.request_id)?;
                self.complete_active_request(correlation)
            }
            (WorkerProtocolPhase::ShuttingDown, WorkerToCoreMessage::Cancelled(_)) => {
                let correlation = request_correlation(envelope.session_id, envelope.request_id)?;
                self.complete_active_request(correlation)
            }
            (WorkerProtocolPhase::ShuttingDown, WorkerToCoreMessage::ShutdownComplete(_)) => {
                self.active_requests.clear();
                self.phase = WorkerProtocolPhase::Closed;
                Ok(())
            }
            _ => Err(WorkerProtocolError::UnexpectedMessage {
                phase: self.phase,
                direction: ProtocolDirection::WorkerToCore,
                kind,
            }),
        }
    }

    fn require_active_request(&self, correlation: (Uuid, Uuid)) -> Result<(), WorkerProtocolError> {
        if self.active_requests.contains(&correlation) {
            Ok(())
        } else {
            Err(WorkerProtocolError::UnknownRequestCorrelation {
                session_id: correlation.0,
                request_id: correlation.1,
            })
        }
    }

    fn complete_active_request(
        &mut self,
        correlation: (Uuid, Uuid),
    ) -> Result<(), WorkerProtocolError> {
        if self.active_requests.remove(&correlation) {
            Ok(())
        } else {
            Err(WorkerProtocolError::UnknownRequestCorrelation {
                session_id: correlation.0,
                request_id: correlation.1,
            })
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolDirection {
    CoreToWorker,
    WorkerToCore,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum WorkerProtocolError {
    #[error("unsupported contract version {actual}; expected {expected}")]
    UnsupportedContractVersion { expected: u16, actual: u16 },
    #[error("{0} must not be nil")]
    NilIdentifier(&'static str),
    #[error("missing required {0}")]
    MissingCorrelation(&'static str),
    #[error("unexpected {0}")]
    UnexpectedCorrelation(&'static str),
    #[error("payload {0} does not match its envelope")]
    CorrelationMismatch(&'static str),
    #[error("duplicate Worker request correlation {session_id}/{request_id}")]
    DuplicateRequestCorrelation { session_id: Uuid, request_id: Uuid },
    #[error("unknown or completed Worker request correlation {session_id}/{request_id}")]
    UnknownRequestCorrelation { session_id: Uuid, request_id: Uuid },
    #[error("invalid or empty protocol field: {0}")]
    InvalidField(&'static str),
    #[error("duplicate protocol value in {0}")]
    DuplicateValue(&'static str),
    #[error("unexpected {direction:?} message {kind:?} while in {phase:?}")]
    UnexpectedMessage {
        phase: WorkerProtocolPhase,
        direction: ProtocolDirection,
        kind: WorkerMessageKind,
    },
    #[error("protocol negotiation failed: {0}")]
    NegotiationFailed(&'static str),
    #[error("worker is missing required capability {0:?}")]
    MissingCapability(WorkerCapability),
}

fn validate_common_envelope(
    contract_version: u16,
    message_id: Uuid,
    session_id: Option<Uuid>,
    request_id: Option<Uuid>,
    sent_at: &str,
) -> Result<(), WorkerProtocolError> {
    if contract_version != CONTRACT_VERSION {
        return Err(WorkerProtocolError::UnsupportedContractVersion {
            expected: CONTRACT_VERSION,
            actual: contract_version,
        });
    }
    if message_id.is_nil() {
        return Err(WorkerProtocolError::NilIdentifier("message_id"));
    }
    if session_id.is_some_and(|value| value.is_nil()) {
        return Err(WorkerProtocolError::NilIdentifier("session_id"));
    }
    if request_id.is_some_and(|value| value.is_nil()) {
        return Err(WorkerProtocolError::NilIdentifier("request_id"));
    }
    if !is_utc_rfc3339(sent_at) {
        return Err(WorkerProtocolError::InvalidField("sent_at"));
    }
    Ok(())
}

fn validate_hello(payload: &CoreHello) -> Result<(), WorkerProtocolError> {
    require_non_empty("hello.core_version", &payload.core_version)?;
    if payload.supported_protocol_versions.is_empty()
        || !payload
            .supported_protocol_versions
            .contains(&CONTRACT_VERSION)
    {
        return Err(WorkerProtocolError::NegotiationFailed(
            "core does not offer the current protocol version",
        ));
    }
    require_unique(
        "supported_protocol_versions",
        &payload.supported_protocol_versions,
    )?;
    require_unique("required_capabilities", &payload.required_capabilities)
}

fn validate_ready(payload: &WorkerReady) -> Result<(), WorkerProtocolError> {
    require_non_empty("ready.worker_version", &payload.worker_version)?;
    require_non_empty("ready.runtime_id", &payload.runtime_id)?;
    require_non_empty("ready.build_signature_id", &payload.build_signature_id)?;
    if payload.protocol_version == 0 || payload.supported_engines.is_empty() {
        return Err(WorkerProtocolError::InvalidField("ready capabilities"));
    }
    require_unique("supported_engines", &payload.supported_engines)?;
    require_unique("capabilities", &payload.capabilities)
}

fn require_unique<T: Ord + Copy>(
    field: &'static str,
    values: &[T],
) -> Result<(), WorkerProtocolError> {
    let mut unique = BTreeSet::new();
    if values.iter().copied().all(|value| unique.insert(value)) {
        Ok(())
    } else {
        Err(WorkerProtocolError::DuplicateValue(field))
    }
}

fn require_non_empty(field: &'static str, value: &str) -> Result<(), WorkerProtocolError> {
    if value.trim().is_empty() {
        Err(WorkerProtocolError::InvalidField(field))
    } else {
        Ok(())
    }
}

fn require_correlations(
    session_id: Option<Uuid>,
    request_id: Option<Uuid>,
    requires_session: bool,
    requires_request: bool,
) -> Result<(), WorkerProtocolError> {
    match (requires_session, session_id) {
        (true, None) => return Err(WorkerProtocolError::MissingCorrelation("session_id")),
        (false, Some(_)) => return Err(WorkerProtocolError::UnexpectedCorrelation("session_id")),
        _ => {}
    }
    match (requires_request, request_id) {
        (true, None) => Err(WorkerProtocolError::MissingCorrelation("request_id")),
        (false, Some(_)) => Err(WorkerProtocolError::UnexpectedCorrelation("request_id")),
        _ => Ok(()),
    }
}

fn validate_error_correlations(
    session_id: Option<Uuid>,
    request_id: Option<Uuid>,
) -> Result<(), WorkerProtocolError> {
    error_request_correlation(session_id, request_id).map(|_| ())
}

fn error_request_correlation(
    session_id: Option<Uuid>,
    request_id: Option<Uuid>,
) -> Result<Option<(Uuid, Uuid)>, WorkerProtocolError> {
    match (session_id, request_id) {
        (None, None) => Ok(None),
        (Some(session_id), Some(request_id)) => Ok(Some((session_id, request_id))),
        (None, Some(_)) => Err(WorkerProtocolError::MissingCorrelation("session_id")),
        (Some(_), None) => Err(WorkerProtocolError::MissingCorrelation("request_id")),
    }
}

fn request_correlation(
    session_id: Option<Uuid>,
    request_id: Option<Uuid>,
) -> Result<(Uuid, Uuid), WorkerProtocolError> {
    error_request_correlation(session_id, request_id)?.ok_or(
        WorkerProtocolError::MissingCorrelation("session_id and request_id"),
    )
}

fn require_matching_id(
    field: &'static str,
    envelope_id: Option<Uuid>,
    payload_id: Uuid,
) -> Result<(), WorkerProtocolError> {
    if envelope_id == Some(payload_id) {
        Ok(())
    } else {
        Err(WorkerProtocolError::CorrelationMismatch(field))
    }
}

fn is_utc_rfc3339(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes.last() != Some(&b'Z')
    {
        return false;
    }

    let fixed_digits = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
    if !fixed_digits
        .iter()
        .all(|index| bytes[*index].is_ascii_digit())
    {
        return false;
    }

    let fractional = &bytes[19..bytes.len() - 1];
    fractional.is_empty()
        || (fractional[0] == b'.'
            && fractional.len() > 1
            && fractional[1..].iter().all(u8::is_ascii_digit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_artifact_id_rejects_paths_nil_and_non_canonical_uuids() {
        for invalid in [
            "../recording.wav",
            "/tmp/recording.wav",
            "recording.wav",
            "00000000-0000-0000-0000-000000000000",
            "11111111222243338444555555555555",
            "11111111-2222-4333-8444-AAAAAAAAAAAA",
        ] {
            assert!(invalid.parse::<AudioArtifactId>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn business_message_is_rejected_before_ready() {
        let mut state = WorkerProtocolState::new();
        let envelope = CoreToWorkerEnvelope {
            contract_version: CONTRACT_VERSION,
            message_id: Uuid::new_v4(),
            session_id: None,
            request_id: Some(Uuid::new_v4()),
            sent_at: "2026-07-21T00:00:00Z".to_owned(),
            message: CoreToWorkerMessage::HealthCheck(HealthCheckRequest {
                engine_id: WorkerEngineId::Qwen,
                model_id: "default".to_owned(),
            }),
        };

        assert!(matches!(
            state.observe_core(&envelope),
            Err(WorkerProtocolError::UnexpectedMessage { .. })
        ));
    }
}
