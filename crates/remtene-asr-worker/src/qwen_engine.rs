use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use qwen_asr::{context::QwenModel, transcribe};
use remtene_contracts::{
    HealthCheckRequest, HealthResult, HealthStatus, TranscribeRequest, WorkerEngineId,
    WorkerErrorCode,
};

use crate::{EngineBackend, EngineError, EngineTranscript};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QwenEngineConfig {
    model_id: String,
    model_version: String,
    model_dir: PathBuf,
}

impl QwenEngineConfig {
    pub fn new(
        model_id: impl Into<String>,
        model_version: impl Into<String>,
        model_dir: impl Into<PathBuf>,
    ) -> Result<Self, EngineError> {
        let config = Self {
            model_id: model_id.into(),
            model_version: model_version.into(),
            model_dir: model_dir.into(),
        };
        if config.model_id.trim().is_empty()
            || config.model_version.trim().is_empty()
            || config.model_dir.as_os_str().is_empty()
        {
            return Err(engine_error(
                WorkerErrorCode::InvalidRequest,
                false,
                "worker.qwen.invalid_configuration",
            ));
        }
        Ok(config)
    }
}

pub struct QwenEngineBackend {
    config: QwenEngineConfig,
    keep_alive: Duration,
    state: Mutex<QwenState>,
}

struct QwenState {
    model: Option<Arc<QwenModel>>,
    in_flight: usize,
    last_used: Option<Instant>,
}

impl QwenEngineBackend {
    #[must_use]
    pub fn start(config: QwenEngineConfig, keep_alive: Duration) -> Arc<Self> {
        let backend = Arc::new(Self {
            config,
            keep_alive,
            state: Mutex::new(QwenState {
                model: None,
                in_flight: 0,
                last_used: None,
            }),
        });
        spawn_reaper(&backend);
        backend
    }

    fn health_result(&self, request: &HealthCheckRequest) -> HealthResult {
        if request.engine_id != WorkerEngineId::Qwen || request.model_id != self.config.model_id {
            return missing_health(request, "worker.qwen.model_not_configured");
        }
        if !required_model_files_exist(&self.config.model_dir) {
            return missing_health(request, "worker.qwen.model_missing");
        }
        match self.ensure_loaded() {
            Ok(()) => HealthResult {
                engine_id: request.engine_id,
                model_id: request.model_id.clone(),
                model_version: self.config.model_version.clone(),
                status: HealthStatus::Healthy,
                device_class: "cpu_accelerate".to_owned(),
                safe_error_code: None,
            },
            Err(error) => HealthResult {
                engine_id: request.engine_id,
                model_id: request.model_id.clone(),
                model_version: self.config.model_version.clone(),
                status: HealthStatus::Incompatible,
                device_class: "cpu_accelerate".to_owned(),
                safe_error_code: Some(error.safe_message_key),
            },
        }
    }

    fn ensure_loaded(&self) -> Result<(), EngineError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.model.is_none() {
            let model_dir = self.config.model_dir.to_str().ok_or_else(|| {
                engine_error(
                    WorkerErrorCode::ModelIncompatible,
                    false,
                    "worker.qwen.model_path_invalid",
                )
            })?;
            state.model = QwenModel::load(model_dir);
            if state.model.is_none() {
                return Err(engine_error(
                    WorkerErrorCode::ModelIncompatible,
                    false,
                    "worker.qwen.model_load_failed",
                ));
            }
        }
        state.last_used = Some(Instant::now());
        Ok(())
    }

    fn acquire_model(&self) -> Result<ModelLease<'_>, EngineError> {
        self.ensure_loaded()?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let model = state.model.as_ref().cloned().ok_or_else(|| {
            engine_error(
                WorkerErrorCode::Internal,
                true,
                "worker.qwen.model_state_invalid",
            )
        })?;
        state.in_flight += 1;
        Ok(ModelLease {
            backend: self,
            model,
        })
    }

    fn release_model(&self) {
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
            && state.model.is_some()
            && state
                .last_used
                .is_some_and(|last_used| last_used.elapsed() >= self.keep_alive);
        if expired {
            state.model = None;
            state.last_used = None;
        }
    }

    fn unload_now(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.in_flight == 0 {
            state.model = None;
            state.last_used = None;
        }
    }
}

impl EngineBackend for QwenEngineBackend {
    fn supported_engines(&self) -> Vec<WorkerEngineId> {
        vec![WorkerEngineId::Qwen, WorkerEngineId::Whisper]
    }

    fn health(&self, request: &HealthCheckRequest) -> HealthResult {
        match request.engine_id {
            WorkerEngineId::Qwen => self.health_result(request),
            WorkerEngineId::Whisper => missing_health(request, "worker.whisper.runtime_not_linked"),
        }
    }

    fn transcribe(
        &self,
        request: &TranscribeRequest,
        audio_path: &Path,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> Result<EngineTranscript, EngineError> {
        use std::sync::atomic::Ordering;

        if request.engine_id != WorkerEngineId::Qwen || request.model_id != self.config.model_id {
            return Err(engine_error(
                WorkerErrorCode::EngineUnavailable,
                false,
                "worker.qwen.model_not_configured",
            ));
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(engine_error(
                WorkerErrorCode::InferenceFailed,
                false,
                "worker.qwen.cancelled",
            ));
        }
        let audio_path = audio_path.to_str().ok_or_else(|| {
            engine_error(
                WorkerErrorCode::InvalidRequest,
                false,
                "worker.qwen.audio_path_invalid",
            )
        })?;
        let lease = self.acquire_model()?;
        let mut context = lease.model.new_session();
        let forced_language = match request.language_hint.as_deref() {
            Some(hint) => {
                let language = qwen_language_name(hint).ok_or_else(|| {
                    engine_error(
                        WorkerErrorCode::InvalidRequest,
                        false,
                        "worker.qwen.language_unsupported",
                    )
                })?;
                context.set_force_language(language).map_err(|()| {
                    engine_error(
                        WorkerErrorCode::InvalidRequest,
                        false,
                        "worker.qwen.language_unsupported",
                    )
                })?;
                Some(language)
            }
            None => {
                context.want_language_detection = true;
                None
            }
        };

        let started = Instant::now();
        let text = transcribe::transcribe(&mut context, audio_path).ok_or_else(|| {
            engine_error(
                WorkerErrorCode::InferenceFailed,
                true,
                "worker.qwen.inference_failed",
            )
        })?;
        let inference_duration_ms = duration_ms(started.elapsed());
        if cancelled.load(Ordering::Acquire) {
            return Err(engine_error(
                WorkerErrorCode::InferenceFailed,
                false,
                "worker.qwen.cancelled",
            ));
        }
        let final_text = text.trim().to_owned();
        if final_text.is_empty() {
            return Err(engine_error(
                WorkerErrorCode::InferenceFailed,
                false,
                "worker.qwen.empty_transcript",
            ));
        }
        let language_name = context.detected_language.as_deref().or(forced_language);
        let detected_language = language_name
            .and_then(qwen_asr::config::language_to_iso639)
            .map(str::to_owned);
        Ok(EngineTranscript {
            final_text,
            detected_language,
            audio_duration_ms: non_negative_ms(context.perf_audio_ms),
            inference_duration_ms,
        })
    }

    fn unload(&self) {
        self.unload_now();
    }
}

struct ModelLease<'a> {
    backend: &'a QwenEngineBackend,
    model: Arc<QwenModel>,
}

impl Drop for ModelLease<'_> {
    fn drop(&mut self) {
        self.backend.release_model();
    }
}

fn spawn_reaper(backend: &Arc<QwenEngineBackend>) {
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

fn required_model_files_exist(model_dir: &Path) -> bool {
    // Only the read-only package assets count. The INT8 sidecar is a derived cache the
    // Supervisor disables (`QWEN_ASR_SIDECAR=0`) so it can never mutate a verified
    // package; requiring it here would report a healthy model as missing.
    regular_file(&model_dir.join("model.safetensors"))
        && regular_file(&model_dir.join("vocab.json"))
        && regular_file(&model_dir.join("merges.txt"))
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
        device_class: "cpu_accelerate".to_owned(),
        safe_error_code: Some(safe_error_code.to_owned()),
    }
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

fn non_negative_ms(value: f64) -> u64 {
    if value.is_finite() && value > 0.0 {
        value.round().min(u64::MAX as f64) as u64
    } else {
        0
    }
}

fn qwen_language_name(hint: &str) -> Option<&'static str> {
    const LANGUAGES: &[(&str, &str)] = &[
        ("zh", "Chinese"),
        ("en", "English"),
        ("yue", "Cantonese"),
        ("ar", "Arabic"),
        ("de", "German"),
        ("fr", "French"),
        ("es", "Spanish"),
        ("pt", "Portuguese"),
        ("id", "Indonesian"),
        ("it", "Italian"),
        ("ko", "Korean"),
        ("ru", "Russian"),
        ("th", "Thai"),
        ("vi", "Vietnamese"),
        ("ja", "Japanese"),
        ("tr", "Turkish"),
        ("hi", "Hindi"),
        ("ms", "Malay"),
        ("nl", "Dutch"),
        ("sv", "Swedish"),
        ("da", "Danish"),
        ("fi", "Finnish"),
        ("pl", "Polish"),
        ("cs", "Czech"),
        ("fil", "Filipino"),
        ("fa", "Persian"),
        ("el", "Greek"),
        ("ro", "Romanian"),
        ("hu", "Hungarian"),
        ("mk", "Macedonian"),
    ];
    let normalized = hint.trim().to_ascii_lowercase();
    LANGUAGES.iter().find_map(|(code, name)| {
        (*code == normalized || name.to_ascii_lowercase() == normalized).then_some(*name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_hints_map_iso_codes_and_names_without_guessing() {
        assert_eq!(qwen_language_name("zh"), Some("Chinese"));
        assert_eq!(qwen_language_name("EN"), Some("English"));
        assert_eq!(qwen_language_name("Japanese"), Some("Japanese"));
        assert_eq!(qwen_language_name("unknown"), None);
    }

    #[test]
    fn missing_qwen_model_is_reported_before_runtime_loading() {
        let model_id = "qwen-test";
        let backend = QwenEngineBackend::start(
            QwenEngineConfig::new(
                model_id,
                "test",
                std::env::temp_dir().join(format!("remtene-qwen-missing-{}", uuid::Uuid::new_v4())),
            )
            .expect("test config"),
            Duration::from_secs(1),
        );
        let health = backend.health(&HealthCheckRequest {
            engine_id: WorkerEngineId::Qwen,
            model_id: model_id.to_owned(),
        });
        assert_eq!(health.status, HealthStatus::Missing);
        assert_eq!(
            health.safe_error_code.as_deref(),
            Some("worker.qwen.model_missing")
        );
    }
}
