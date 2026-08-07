use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use hound::{SampleFormat, WavReader};
use remtene_contracts::{
    HealthCheckRequest, HealthResult, HealthStatus, TranscribeRequest, WorkerEngineId,
    WorkerErrorCode,
};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters,
    convert_integer_to_float_audio, get_lang_str, install_logging_hooks,
};

use crate::{EngineBackend, EngineError, EngineTranscript};

const REQUIRED_SAMPLE_RATE_HZ: u32 = 16_000;
const REQUIRED_CHANNELS: u8 = 1;
const REQUIRED_BITS_PER_SAMPLE: u8 = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WhisperEngineConfig {
    model_id: String,
    model_version: String,
    model_file: PathBuf,
}

impl WhisperEngineConfig {
    pub fn new(
        model_id: impl Into<String>,
        model_version: impl Into<String>,
        model_file: impl Into<PathBuf>,
    ) -> Result<Self, EngineError> {
        let config = Self {
            model_id: model_id.into(),
            model_version: model_version.into(),
            model_file: model_file.into(),
        };
        if config.model_id.trim().is_empty()
            || config.model_version.trim().is_empty()
            || config.model_file.as_os_str().is_empty()
        {
            return Err(engine_error(
                WorkerErrorCode::InvalidRequest,
                false,
                "worker.whisper.invalid_configuration",
            ));
        }
        Ok(config)
    }
}

pub struct WhisperEngineBackend {
    config: WhisperEngineConfig,
    keep_alive: Duration,
    state: Mutex<WhisperState>,
}

struct WhisperState {
    context: Option<Arc<WhisperContext>>,
    in_flight: usize,
    last_used: Option<Instant>,
}

impl WhisperEngineBackend {
    #[must_use]
    pub fn start(config: WhisperEngineConfig, keep_alive: Duration) -> Arc<Self> {
        let backend = Arc::new(Self {
            config,
            keep_alive,
            state: Mutex::new(WhisperState {
                context: None,
                in_flight: 0,
                last_used: None,
            }),
        });
        spawn_reaper(&backend);
        backend
    }

    fn health_result(&self, request: &HealthCheckRequest) -> HealthResult {
        if request.engine_id != WorkerEngineId::Whisper || request.model_id != self.config.model_id
        {
            return missing_health(request, "worker.whisper.model_not_configured");
        }
        if !regular_file(&self.config.model_file) {
            return missing_health(request, "worker.whisper.model_missing");
        }
        match self.ensure_loaded() {
            Ok(()) => HealthResult {
                engine_id: request.engine_id,
                model_id: request.model_id.clone(),
                model_version: self.config.model_version.clone(),
                status: HealthStatus::Healthy,
                device_class: "metal".to_owned(),
                safe_error_code: None,
            },
            Err(error) => HealthResult {
                engine_id: request.engine_id,
                model_id: request.model_id.clone(),
                model_version: self.config.model_version.clone(),
                status: HealthStatus::Incompatible,
                device_class: "metal".to_owned(),
                safe_error_code: Some(error.safe_message_key),
            },
        }
    }

    fn ensure_loaded(&self) -> Result<(), EngineError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.context.is_none() {
            let model_file = self.config.model_file.to_str().ok_or_else(|| {
                engine_error(
                    WorkerErrorCode::ModelIncompatible,
                    false,
                    "worker.whisper.model_path_invalid",
                )
            })?;
            install_logging_hooks();
            let mut parameters = WhisperContextParameters::default();
            parameters.use_gpu(true).flash_attn(true);
            let context =
                WhisperContext::new_with_params(model_file, parameters).map_err(|_| {
                    engine_error(
                        WorkerErrorCode::ModelIncompatible,
                        false,
                        "worker.whisper.model_load_failed",
                    )
                })?;
            prewarm_context(&context)?;
            state.context = Some(Arc::new(context));
        }
        state.last_used = Some(Instant::now());
        Ok(())
    }

    fn acquire_context(&self) -> Result<ContextLease<'_>, EngineError> {
        self.ensure_loaded()?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let context = state.context.as_ref().cloned().ok_or_else(|| {
            engine_error(
                WorkerErrorCode::Internal,
                true,
                "worker.whisper.model_state_invalid",
            )
        })?;
        state.in_flight += 1;
        Ok(ContextLease {
            backend: self,
            context,
        })
    }

    fn release_context(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.in_flight = state.in_flight.saturating_sub(1);
        state.last_used = Some(Instant::now());
    }

    fn unload_if_idle(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let expired = state.in_flight == 0
            && state.context.is_some()
            && state
                .last_used
                .is_some_and(|last_used| last_used.elapsed() >= self.keep_alive);
        if expired {
            state.context = None;
            state.last_used = None;
        }
    }

    fn unload_now(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.in_flight == 0 {
            state.context = None;
            state.last_used = None;
        }
    }
}

impl EngineBackend for WhisperEngineBackend {
    fn supported_engines(&self) -> Vec<WorkerEngineId> {
        vec![WorkerEngineId::Qwen, WorkerEngineId::Whisper]
    }

    fn health(&self, request: &HealthCheckRequest) -> HealthResult {
        match request.engine_id {
            WorkerEngineId::Qwen => missing_health(request, "worker.qwen.runtime_not_linked"),
            WorkerEngineId::Whisper => self.health_result(request),
        }
    }

    fn transcribe(
        &self,
        request: &TranscribeRequest,
        audio_path: &Path,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<EngineTranscript, EngineError> {
        use std::sync::atomic::Ordering;

        if request.engine_id != WorkerEngineId::Whisper || request.model_id != self.config.model_id
        {
            return Err(engine_error(
                WorkerErrorCode::EngineUnavailable,
                false,
                "worker.whisper.model_not_configured",
            ));
        }
        if request.audio_format.sample_rate_hz != REQUIRED_SAMPLE_RATE_HZ
            || request.audio_format.channels != REQUIRED_CHANNELS
            || request.audio_format.bits_per_sample != REQUIRED_BITS_PER_SAMPLE
        {
            return Err(engine_error(
                WorkerErrorCode::InvalidRequest,
                false,
                "worker.whisper.audio_format_unsupported",
            ));
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(cancelled_error());
        }

        let samples = read_pcm16_mono_wav(audio_path)?;
        let audio_duration_ms = u64::try_from(samples.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(1_000)
            / u64::from(REQUIRED_SAMPLE_RATE_HZ);
        let mut audio = vec![0.0_f32; samples.len()];
        convert_integer_to_float_audio(&samples, &mut audio).map_err(|_| {
            engine_error(
                WorkerErrorCode::InvalidRequest,
                false,
                "worker.whisper.audio_decode_failed",
            )
        })?;
        if cancelled.load(Ordering::Acquire) {
            return Err(cancelled_error());
        }

        let lease = self.acquire_context()?;
        let mut state = lease.context.create_state().map_err(|_| {
            engine_error(
                WorkerErrorCode::InferenceFailed,
                true,
                "worker.whisper.state_create_failed",
            )
        })?;
        let mut params = FullParams::new(SamplingStrategy::BeamSearch {
            beam_size: 5,
            patience: -1.0,
        });
        params.set_n_threads(worker_threads());
        params.set_translate(false);
        params.set_no_context(true);
        params.set_no_timestamps(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        match request.language_hint.as_deref() {
            Some(language) if whisper_rs::get_lang_id(language).is_some() => {
                params.set_language(Some(language));
            }
            Some(_) => {
                return Err(engine_error(
                    WorkerErrorCode::InvalidRequest,
                    false,
                    "worker.whisper.language_unsupported",
                ));
            }
            None => {
                params.set_language(None);
                params.set_detect_language(true);
            }
        }

        let started = Instant::now();
        state.full(params, &audio).map_err(|_| {
            engine_error(
                WorkerErrorCode::InferenceFailed,
                true,
                "worker.whisper.inference_failed",
            )
        })?;
        let inference_duration_ms = duration_ms(started.elapsed());
        if cancelled.load(Ordering::Acquire) {
            return Err(cancelled_error());
        }

        let segment_count = state.full_n_segments().map_err(|_| {
            engine_error(
                WorkerErrorCode::InferenceFailed,
                true,
                "worker.whisper.result_unavailable",
            )
        })?;
        let mut final_text = String::new();
        for segment in 0..segment_count {
            let text = state.full_get_segment_text(segment).map_err(|_| {
                engine_error(
                    WorkerErrorCode::InferenceFailed,
                    true,
                    "worker.whisper.result_unavailable",
                )
            })?;
            final_text.push_str(&text);
        }
        let final_text = final_text.trim().to_owned();
        if final_text.is_empty() {
            return Err(engine_error(
                WorkerErrorCode::InferenceFailed,
                false,
                "worker.whisper.empty_transcript",
            ));
        }
        let detected_language = state
            .full_lang_id_from_state()
            .ok()
            .and_then(get_lang_str)
            .map(str::to_owned);
        Ok(EngineTranscript {
            final_text,
            detected_language,
            audio_duration_ms,
            inference_duration_ms,
        })
    }

    fn unload(&self) {
        self.unload_now();
    }
}

struct ContextLease<'a> {
    backend: &'a WhisperEngineBackend,
    context: Arc<WhisperContext>,
}

impl Drop for ContextLease<'_> {
    fn drop(&mut self) {
        self.backend.release_context();
    }
}

fn spawn_reaper(backend: &Arc<WhisperEngineBackend>) {
    let weak = Arc::downgrade(backend);
    let interval = backend
        .keep_alive
        .checked_div(10)
        .unwrap_or(Duration::from_secs(1))
        .clamp(Duration::from_secs(1), Duration::from_secs(30));
    thread::spawn(move || {
        loop {
            thread::sleep(interval);
            let Some(backend) = weak.upgrade() else {
                return;
            };
            backend.unload_if_idle();
        }
    });
}

fn read_pcm16_mono_wav(path: &Path) -> Result<Vec<i16>, EngineError> {
    let mut reader = WavReader::open(path).map_err(|_| audio_decode_error())?;
    let spec = reader.spec();
    if spec.channels != u16::from(REQUIRED_CHANNELS)
        || spec.sample_rate != REQUIRED_SAMPLE_RATE_HZ
        || spec.bits_per_sample != u16::from(REQUIRED_BITS_PER_SAMPLE)
        || spec.sample_format != SampleFormat::Int
    {
        return Err(engine_error(
            WorkerErrorCode::InvalidRequest,
            false,
            "worker.whisper.audio_format_unsupported",
        ));
    }
    let samples = reader
        .samples::<i16>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| audio_decode_error())?;
    if samples.is_empty() {
        return Err(audio_decode_error());
    }
    Ok(samples)
}

fn prewarm_context(context: &WhisperContext) -> Result<(), EngineError> {
    let mut state = context.create_state().map_err(|_| {
        engine_error(
            WorkerErrorCode::ModelIncompatible,
            false,
            "worker.whisper.prewarm_failed",
        )
    })?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(worker_threads());
    params.set_translate(false);
    params.set_no_context(true);
    params.set_no_timestamps(true);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_language(Some("en"));
    let silence = vec![0.0_f32; REQUIRED_SAMPLE_RATE_HZ as usize];
    state.full(params, &silence).map_err(|_| {
        engine_error(
            WorkerErrorCode::ModelIncompatible,
            false,
            "worker.whisper.prewarm_failed",
        )
    })?;
    Ok(())
}

fn regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

fn missing_health(request: &HealthCheckRequest, safe_error_code: &str) -> HealthResult {
    HealthResult {
        engine_id: request.engine_id,
        model_id: request.model_id.clone(),
        model_version: "unavailable".to_owned(),
        status: HealthStatus::Missing,
        device_class: "metal".to_owned(),
        safe_error_code: Some(safe_error_code.to_owned()),
    }
}

fn worker_threads() -> i32 {
    i32::try_from(
        std::thread::available_parallelism()
            .map_or(4, std::num::NonZero::get)
            .min(4),
    )
    .unwrap_or(4)
}

fn cancelled_error() -> EngineError {
    engine_error(
        WorkerErrorCode::InferenceFailed,
        false,
        "worker.whisper.cancelled",
    )
}

fn audio_decode_error() -> EngineError {
    engine_error(
        WorkerErrorCode::InvalidRequest,
        false,
        "worker.whisper.audio_decode_failed",
    )
}

fn engine_error(code: WorkerErrorCode, retryable: bool, safe_message_key: &str) -> EngineError {
    EngineError {
        code,
        retryable,
        safe_message_key: safe_message_key.to_owned(),
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_corrupt_whisper_models_fail_closed_without_a_transcript() {
        let root = std::env::temp_dir().join(format!(
            "remtene-whisper-model-health-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("create model health root");
        let model_id = "whisper-test";
        let model_file = root.join("model.bin");
        let backend = WhisperEngineBackend::start(
            WhisperEngineConfig::new(model_id, "test", &model_file).expect("test config"),
            Duration::from_secs(1),
        );
        let request = HealthCheckRequest {
            engine_id: WorkerEngineId::Whisper,
            model_id: model_id.to_owned(),
        };

        let missing = backend.health(&request);
        assert_eq!(missing.status, HealthStatus::Missing);
        assert_eq!(
            missing.safe_error_code.as_deref(),
            Some("worker.whisper.model_missing")
        );

        std::fs::write(&model_file, b"not-a-whisper-model").expect("write corrupt model");
        let corrupt = backend.health(&request);
        assert_eq!(corrupt.status, HealthStatus::Incompatible);
        assert_eq!(
            corrupt.safe_error_code.as_deref(),
            Some("worker.whisper.model_load_failed")
        );
        std::fs::remove_dir_all(root).expect("remove model health root");
    }
}
