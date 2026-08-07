//! Local ASR health check use case.
//!
//! Reading the public application snapshot must stay side-effect free. This controller is the
//! separate Application entry point shared by the one automatic startup verification and each
//! explicit user recheck. It shares the Session configuration gate and lifecycle barrier, so model
//! warmup cannot race Session start or outlive application shutdown.

use std::sync::Arc;

use futures::lock::Mutex as AsyncMutex;
use remtene_domain::{AsrEngine, AsrPreference, SettingsSnapshot};
use thiserror::Error;

use crate::{
    OrchestratorError, TranscriptionOrchestrator,
    ports::{
        AsrEnginePort, AsrModelControlPort, AsrModelPreparationError, CommitGuard, EngineHealth,
        PortError, SettingsStore,
    },
    transcription_orchestrator::ApplicationActivity,
};

type ActivityProbe =
    dyn Fn() -> Result<ApplicationActivity, OrchestratorError> + Send + Sync + 'static;
type ApplicationOperationEntry = dyn Fn() -> Option<CommitGuard> + Send + Sync + 'static;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AsrHealthCheckOutcome {
    pub preference: AsrPreference,
    pub qwen: Option<EngineHealth>,
    pub whisper: Option<EngineHealth>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AsrHealthCheckError {
    #[error("another ASR health check or input Session is active")]
    Busy,
    #[error("the application is quitting")]
    Quitting,
    #[error("orchestrator state is unavailable")]
    RuntimeUnavailable,
    #[error("ASR preference could not be loaded: {0}")]
    Settings(PortError),
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AsrModelSwitchError {
    #[error("another model operation or input Session is active")]
    Busy,
    #[error("the application is quitting")]
    Quitting,
    #[error("orchestrator state is unavailable")]
    RuntimeUnavailable,
    #[error("the selected model package is missing or invalid")]
    Missing,
    #[error("the selected model package hash does not match its manifest")]
    HashMismatch,
    #[error("the selected model did not become healthy")]
    Unhealthy,
    #[error("the ASR runtime could not switch models: {0}")]
    Runtime(PortError),
    #[error("the selected ASR model could not be persisted: {0}")]
    Settings(PortError),
}

pub struct AsrHealthController {
    settings: Arc<dyn SettingsStore>,
    asr: Arc<dyn AsrEnginePort>,
    model_control: Arc<dyn AsrModelControlPort>,
    configuration_gate: Arc<AsyncMutex<()>>,
    activity: Arc<ActivityProbe>,
    application_operation: Arc<ApplicationOperationEntry>,
    operations: AsyncMutex<()>,
}

impl AsrHealthController {
    #[must_use]
    pub fn new(
        orchestrator: Arc<TranscriptionOrchestrator>,
        settings: Arc<dyn SettingsStore>,
        asr: Arc<dyn AsrEnginePort>,
        model_control: Arc<dyn AsrModelControlPort>,
    ) -> Self {
        let configuration_gate = orchestrator.configuration_gate();
        let activity_orchestrator = Arc::clone(&orchestrator);
        let operation_orchestrator = Arc::clone(&orchestrator);
        Self {
            settings,
            asr,
            model_control,
            configuration_gate,
            activity: Arc::new(move || activity_orchestrator.application_activity()),
            application_operation: Arc::new(move || {
                operation_orchestrator.enter_external_operation()
            }),
            operations: AsyncMutex::new(()),
        }
    }

    /// Checks only the engine explicitly selected in persisted settings.
    ///
    /// Port failures are a completed health result, not an IPC read failure. The observed ASR
    /// decorator records them as `Unhealthy`. No microphone, target, selection, LLM, or network
    /// port is reachable here, and failure never probes or falls back to the other engine.
    pub async fn check(&self) -> Result<AsrHealthCheckOutcome, AsrHealthCheckError> {
        let _exclusive = self
            .operations
            .try_lock()
            .ok_or(AsrHealthCheckError::Busy)?;
        let Some(_application_operation) = (self.application_operation)() else {
            return Err(AsrHealthCheckError::Quitting);
        };
        let _configuration = self.configuration_gate.lock().await;
        self.ensure_idle()?;
        let settings = self
            .settings
            .load()
            .await
            .map_err(AsrHealthCheckError::Settings)?;

        let preference = settings.asr_preference();
        let health = self.prepare_and_check(preference.engine()).await;
        Ok(single_engine_outcome(preference, health))
    }

    /// Atomically switches the explicit ASR selection for future Sessions.
    ///
    /// The target package is verified and prewarmed before the settings CAS is committed. If the
    /// commit fails, the previous engine is prepared and warmed again before the error returns.
    /// The configuration gate prevents a Session from observing the temporary preparation state.
    pub async fn switch_to(
        &self,
        engine: AsrEngine,
    ) -> Result<AsrHealthCheckOutcome, AsrModelSwitchError> {
        let _exclusive = self
            .operations
            .try_lock()
            .ok_or(AsrModelSwitchError::Busy)?;
        let Some(_application_operation) = (self.application_operation)() else {
            return Err(AsrModelSwitchError::Quitting);
        };
        let _configuration = self.configuration_gate.lock().await;
        self.ensure_idle().map_err(switch_activity_error)?;
        let current = self
            .settings
            .load()
            .await
            .map_err(AsrModelSwitchError::Settings)?;
        let previous = current.asr_preference().engine();
        let preference = AsrPreference::from(engine);
        let replacement = if current.asr_preference() != preference {
            let expected_version = current.version();
            let mut input = current.into_input();
            input.asr_preference = preference;
            let next = SettingsSnapshot::new(input).map_err(|_| {
                AsrModelSwitchError::Settings(PortError {
                    code: "settings.invalid".to_owned(),
                    safe_message_key: "errors.settings.invalid".to_owned(),
                    retryable: false,
                })
            })?;
            Some((expected_version, next))
        } else {
            None
        };

        self.model_control
            .prepare(engine)
            .await
            .map_err(switch_preparation_error)?;
        let health = match self.asr.health(engine).await {
            Ok(health) => health,
            Err(error) => {
                self.restore_previous(previous).await;
                return Err(AsrModelSwitchError::Runtime(error));
            }
        };
        if health != EngineHealth::Healthy {
            self.restore_previous(previous).await;
            return Err(match health {
                EngineHealth::Missing | EngineHealth::Incompatible => AsrModelSwitchError::Missing,
                EngineHealth::Unhealthy => AsrModelSwitchError::Unhealthy,
                EngineHealth::Healthy => unreachable!("healthy handled above"),
            });
        }

        if let Some((expected_version, next)) = replacement
            && let Err(error) = self.settings.replace(expected_version, next).await
        {
            self.restore_previous(previous).await;
            return Err(AsrModelSwitchError::Settings(error));
        }

        Ok(single_engine_outcome(preference, health))
    }

    fn ensure_idle(&self) -> Result<(), AsrHealthCheckError> {
        match (self.activity)() {
            Ok(ApplicationActivity::Idle) => Ok(()),
            Ok(ApplicationActivity::Busy) => Err(AsrHealthCheckError::Busy),
            Ok(ApplicationActivity::Quitting) => Err(AsrHealthCheckError::Quitting),
            Err(_) => Err(AsrHealthCheckError::RuntimeUnavailable),
        }
    }

    async fn engine_health(&self, engine: AsrEngine) -> EngineHealth {
        self.asr
            .health(engine)
            .await
            .unwrap_or(EngineHealth::Unhealthy)
    }

    async fn prepare_and_check(&self, engine: AsrEngine) -> EngineHealth {
        match self.model_control.prepare(engine).await {
            Ok(()) => self.engine_health(engine).await,
            Err(AsrModelPreparationError::Missing) => EngineHealth::Missing,
            Err(AsrModelPreparationError::HashMismatch) => EngineHealth::Incompatible,
            Err(AsrModelPreparationError::Runtime(_)) => EngineHealth::Unhealthy,
        }
    }

    async fn restore_previous(&self, engine: AsrEngine) {
        if self.model_control.prepare(engine).await.is_ok() {
            let _ = self.asr.health(engine).await;
        }
    }
}

fn single_engine_outcome(preference: AsrPreference, health: EngineHealth) -> AsrHealthCheckOutcome {
    match preference {
        AsrPreference::Qwen => AsrHealthCheckOutcome {
            preference,
            qwen: Some(health),
            whisper: None,
        },
        AsrPreference::Whisper => AsrHealthCheckOutcome {
            preference,
            qwen: None,
            whisper: Some(health),
        },
    }
}

fn switch_activity_error(error: AsrHealthCheckError) -> AsrModelSwitchError {
    match error {
        AsrHealthCheckError::Busy => AsrModelSwitchError::Busy,
        AsrHealthCheckError::Quitting => AsrModelSwitchError::Quitting,
        AsrHealthCheckError::RuntimeUnavailable => AsrModelSwitchError::RuntimeUnavailable,
        AsrHealthCheckError::Settings(error) => AsrModelSwitchError::Settings(error),
    }
}

fn switch_preparation_error(error: AsrModelPreparationError) -> AsrModelSwitchError {
    match error {
        AsrModelPreparationError::Missing => AsrModelSwitchError::Missing,
        AsrModelPreparationError::HashMismatch => AsrModelSwitchError::HashMismatch,
        AsrModelPreparationError::Runtime(error) => AsrModelSwitchError::Runtime(error),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use futures::executor::block_on;
    use remtene_domain::{
        AsrEngine, AsrPreference, HistoryPolicy, ProcessingMode, RecordingMode, RequestId,
        SettingsSnapshot, SettingsSnapshotInput,
    };

    use crate::ports::{AsrRequest, AsrResult, LifecycleFence, PortFuture};

    use super::*;

    struct StaticSettings(SettingsSnapshot);

    impl SettingsStore for StaticSettings {
        fn load(&self) -> PortFuture<'_, Result<SettingsSnapshot, PortError>> {
            let snapshot = self.0.clone();
            Box::pin(async move { Ok(snapshot) })
        }

        fn replace(
            &self,
            _expected_version: u64,
            _settings: SettingsSnapshot,
        ) -> PortFuture<'_, Result<SettingsSnapshot, PortError>> {
            Box::pin(async { Err(test_error("settings.read_only")) })
        }
    }

    struct MutableSettings {
        snapshot: Mutex<SettingsSnapshot>,
        replace_error: Option<PortError>,
    }

    impl MutableSettings {
        fn new(snapshot: SettingsSnapshot, replace_error: Option<PortError>) -> Self {
            Self {
                snapshot: Mutex::new(snapshot),
                replace_error,
            }
        }

        fn preference(&self) -> AsrPreference {
            self.snapshot
                .lock()
                .expect("settings snapshot")
                .asr_preference()
        }
    }

    impl SettingsStore for MutableSettings {
        fn load(&self) -> PortFuture<'_, Result<SettingsSnapshot, PortError>> {
            let snapshot = self.snapshot.lock().expect("settings snapshot").clone();
            Box::pin(async move { Ok(snapshot) })
        }

        fn replace(
            &self,
            expected_version: u64,
            settings: SettingsSnapshot,
        ) -> PortFuture<'_, Result<SettingsSnapshot, PortError>> {
            let result = if let Some(error) = self.replace_error.clone() {
                Err(error)
            } else {
                let mut input = settings.into_input();
                input.version = expected_version + 1;
                let saved = SettingsSnapshot::new(input).expect("valid replacement settings");
                *self.snapshot.lock().expect("settings snapshot") = saved.clone();
                Ok(saved)
            };
            Box::pin(async move { result })
        }
    }

    struct ScriptedAsr {
        qwen: Result<EngineHealth, PortError>,
        whisper: Result<EngineHealth, PortError>,
        calls: Mutex<Vec<AsrEngine>>,
    }

    impl ScriptedAsr {
        fn new(
            qwen: Result<EngineHealth, PortError>,
            whisper: Result<EngineHealth, PortError>,
        ) -> Self {
            Self {
                qwen,
                whisper,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<AsrEngine> {
            self.calls.lock().expect("ASR call log").clone()
        }
    }

    impl AsrEnginePort for ScriptedAsr {
        fn health(&self, engine: AsrEngine) -> PortFuture<'_, Result<EngineHealth, PortError>> {
            self.calls.lock().expect("ASR call log").push(engine);
            let result = match engine {
                AsrEngine::Qwen => self.qwen.clone(),
                AsrEngine::Whisper => self.whisper.clone(),
            };
            Box::pin(async move { result })
        }

        fn transcribe(&self, _request: AsrRequest) -> PortFuture<'_, Result<AsrResult, PortError>> {
            Box::pin(async { Err(test_error("asr.transcribe_not_expected")) })
        }

        fn cancel(&self, _request_id: RequestId) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async { Ok(()) })
        }
    }

    struct ScriptedModelControl {
        result: Result<(), AsrModelPreparationError>,
        calls: Mutex<Vec<AsrEngine>>,
    }

    impl ScriptedModelControl {
        fn ready() -> Self {
            Self {
                result: Ok(()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn failing(error: AsrModelPreparationError) -> Self {
            Self {
                result: Err(error),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<AsrEngine> {
            self.calls.lock().expect("model control calls").clone()
        }
    }

    impl AsrModelControlPort for ScriptedModelControl {
        fn prepare(
            &self,
            engine: AsrEngine,
        ) -> PortFuture<'_, Result<(), AsrModelPreparationError>> {
            self.calls.lock().expect("model control calls").push(engine);
            let result = self.result.clone();
            Box::pin(async move { result })
        }
    }

    fn controller(preference: AsrPreference, asr: Arc<dyn AsrEnginePort>) -> AsrHealthController {
        AsrHealthController {
            settings: Arc::new(StaticSettings(settings(preference))),
            asr,
            model_control: Arc::new(ScriptedModelControl::ready()),
            configuration_gate: Arc::new(AsyncMutex::new(())),
            activity: Arc::new(|| Ok(ApplicationActivity::Idle)),
            application_operation: Arc::new(|| LifecycleFence::new().begin_commit()),
            operations: AsyncMutex::new(()),
        }
    }

    fn settings(preference: AsrPreference) -> SettingsSnapshot {
        SettingsSnapshot::new(SettingsSnapshotInput {
            version: 0,
            recording_mode: RecordingMode::Toggle,
            max_recording_duration: Duration::from_secs(600),
            recording_shortcut: None,
            processing_mode: ProcessingMode::Raw,
            asr_preference: preference,
            llm: None,
            read_selected_text: false,
            clipboard_bridge_allowed: false,
            auto_copy_result: false,
            local_diagnostics_enabled: true,
            history_policy: HistoryPolicy {
                enabled: true,
                limit: 10,
                retention_days: None,
            },
        })
        .expect("valid test settings")
    }

    fn test_error(code: &str) -> PortError {
        PortError {
            code: code.to_owned(),
            safe_message_key: format!("errors.{code}"),
            retryable: true,
        }
    }

    #[test]
    fn healthy_qwen_does_not_warm_whisper() {
        let asr = Arc::new(ScriptedAsr::new(
            Ok(EngineHealth::Healthy),
            Ok(EngineHealth::Healthy),
        ));
        let outcome =
            block_on(controller(AsrPreference::Qwen, asr.clone()).check()).expect("health check");

        assert_eq!(outcome.qwen, Some(EngineHealth::Healthy));
        assert_eq!(outcome.whisper, None);
        assert_eq!(outcome.preference, AsrPreference::Qwen);
        assert_eq!(asr.calls(), vec![AsrEngine::Qwen]);
    }

    #[test]
    fn unhealthy_qwen_does_not_fall_back_to_whisper() {
        let asr = Arc::new(ScriptedAsr::new(
            Ok(EngineHealth::Missing),
            Ok(EngineHealth::Healthy),
        ));
        let outcome =
            block_on(controller(AsrPreference::Qwen, asr.clone()).check()).expect("health check");

        assert_eq!(outcome.qwen, Some(EngineHealth::Missing));
        assert_eq!(outcome.whisper, None);
        assert_eq!(asr.calls(), vec![AsrEngine::Qwen]);
    }

    #[test]
    fn whisper_selection_never_calls_qwen() {
        let asr = Arc::new(ScriptedAsr::new(
            Ok(EngineHealth::Healthy),
            Ok(EngineHealth::Incompatible),
        ));
        let outcome = block_on(controller(AsrPreference::Whisper, asr.clone()).check())
            .expect("health check");

        assert_eq!(outcome.qwen, None);
        assert_eq!(outcome.whisper, Some(EngineHealth::Incompatible));
        assert_eq!(outcome.preference, AsrPreference::Whisper);
        assert_eq!(asr.calls(), vec![AsrEngine::Whisper]);
    }

    #[test]
    fn duplicate_check_is_rejected_before_reaching_the_worker() {
        let asr = Arc::new(ScriptedAsr::new(
            Ok(EngineHealth::Healthy),
            Ok(EngineHealth::Healthy),
        ));
        let controller = controller(AsrPreference::Qwen, asr.clone());
        let _held = controller
            .operations
            .try_lock()
            .expect("exclusive health lock");

        assert_eq!(block_on(controller.check()), Err(AsrHealthCheckError::Busy),);
        assert!(asr.calls().is_empty());
    }

    #[test]
    fn health_port_error_is_unhealthy_without_fallback() {
        let asr = Arc::new(ScriptedAsr::new(
            Err(test_error("asr.worker_failed")),
            Ok(EngineHealth::Healthy),
        ));
        let outcome =
            block_on(controller(AsrPreference::Qwen, asr.clone()).check()).expect("health check");

        assert_eq!(outcome.qwen, Some(EngineHealth::Unhealthy));
        assert_eq!(outcome.whisper, None);
        assert_eq!(asr.calls(), vec![AsrEngine::Qwen]);
    }

    #[test]
    fn quitting_after_the_operation_barrier_rejects_before_health() {
        let asr = Arc::new(ScriptedAsr::new(
            Ok(EngineHealth::Healthy),
            Ok(EngineHealth::Healthy),
        ));
        let mut controller = controller(AsrPreference::Qwen, asr.clone());
        controller.activity = Arc::new(|| Ok(ApplicationActivity::Quitting));

        assert_eq!(
            block_on(controller.check()),
            Err(AsrHealthCheckError::Quitting),
        );
        assert!(asr.calls().is_empty());
    }

    #[test]
    fn successful_switch_prewarms_before_persisting_the_explicit_selection() {
        let settings = Arc::new(MutableSettings::new(settings(AsrPreference::Qwen), None));
        let asr = Arc::new(ScriptedAsr::new(
            Ok(EngineHealth::Healthy),
            Ok(EngineHealth::Healthy),
        ));
        let model_control = Arc::new(ScriptedModelControl::ready());
        let controller = AsrHealthController {
            settings: settings.clone(),
            asr: asr.clone(),
            model_control: model_control.clone(),
            configuration_gate: Arc::new(AsyncMutex::new(())),
            activity: Arc::new(|| Ok(ApplicationActivity::Idle)),
            application_operation: Arc::new(|| LifecycleFence::new().begin_commit()),
            operations: AsyncMutex::new(()),
        };

        let outcome = block_on(controller.switch_to(AsrEngine::Whisper)).expect("model switch");

        assert_eq!(outcome.preference, AsrPreference::Whisper);
        assert_eq!(outcome.whisper, Some(EngineHealth::Healthy));
        assert_eq!(settings.preference(), AsrPreference::Whisper);
        assert_eq!(model_control.calls(), vec![AsrEngine::Whisper]);
        assert_eq!(asr.calls(), vec![AsrEngine::Whisper]);
    }

    #[test]
    fn missing_and_hash_mismatch_are_distinct_and_preserve_the_previous_selection() {
        for (preparation, expected) in [
            (
                AsrModelPreparationError::Missing,
                AsrModelSwitchError::Missing,
            ),
            (
                AsrModelPreparationError::HashMismatch,
                AsrModelSwitchError::HashMismatch,
            ),
        ] {
            let settings = Arc::new(MutableSettings::new(settings(AsrPreference::Qwen), None));
            let asr = Arc::new(ScriptedAsr::new(
                Ok(EngineHealth::Healthy),
                Ok(EngineHealth::Healthy),
            ));
            let controller = AsrHealthController {
                settings: settings.clone(),
                asr: asr.clone(),
                model_control: Arc::new(ScriptedModelControl::failing(preparation)),
                configuration_gate: Arc::new(AsyncMutex::new(())),
                activity: Arc::new(|| Ok(ApplicationActivity::Idle)),
                application_operation: Arc::new(|| LifecycleFence::new().begin_commit()),
                operations: AsyncMutex::new(()),
            };

            assert_eq!(
                block_on(controller.switch_to(AsrEngine::Whisper)),
                Err(expected)
            );
            assert_eq!(settings.preference(), AsrPreference::Qwen);
            assert!(asr.calls().is_empty());
        }
    }

    #[test]
    fn persistence_failure_rewarms_the_previous_model_before_returning() {
        let settings = Arc::new(MutableSettings::new(
            settings(AsrPreference::Qwen),
            Some(test_error("settings.write_failed")),
        ));
        let asr = Arc::new(ScriptedAsr::new(
            Ok(EngineHealth::Healthy),
            Ok(EngineHealth::Healthy),
        ));
        let model_control = Arc::new(ScriptedModelControl::ready());
        let controller = AsrHealthController {
            settings: settings.clone(),
            asr: asr.clone(),
            model_control: model_control.clone(),
            configuration_gate: Arc::new(AsyncMutex::new(())),
            activity: Arc::new(|| Ok(ApplicationActivity::Idle)),
            application_operation: Arc::new(|| LifecycleFence::new().begin_commit()),
            operations: AsyncMutex::new(()),
        };

        assert!(matches!(
            block_on(controller.switch_to(AsrEngine::Whisper)),
            Err(AsrModelSwitchError::Settings(_))
        ));
        assert_eq!(settings.preference(), AsrPreference::Qwen);
        assert_eq!(
            model_control.calls(),
            vec![AsrEngine::Whisper, AsrEngine::Qwen]
        );
        assert_eq!(asr.calls(), vec![AsrEngine::Whisper, AsrEngine::Qwen]);
    }

    #[test]
    fn target_health_error_rewarms_the_previous_model_before_returning() {
        let settings = Arc::new(MutableSettings::new(settings(AsrPreference::Qwen), None));
        let asr = Arc::new(ScriptedAsr::new(
            Ok(EngineHealth::Healthy),
            Err(test_error("asr.worker_failed")),
        ));
        let model_control = Arc::new(ScriptedModelControl::ready());
        let controller = AsrHealthController {
            settings: settings.clone(),
            asr: asr.clone(),
            model_control: model_control.clone(),
            configuration_gate: Arc::new(AsyncMutex::new(())),
            activity: Arc::new(|| Ok(ApplicationActivity::Idle)),
            application_operation: Arc::new(|| LifecycleFence::new().begin_commit()),
            operations: AsyncMutex::new(()),
        };

        assert!(matches!(
            block_on(controller.switch_to(AsrEngine::Whisper)),
            Err(AsrModelSwitchError::Runtime(error)) if error.code == "asr.worker_failed"
        ));
        assert_eq!(settings.preference(), AsrPreference::Qwen);
        assert_eq!(
            model_control.calls(),
            vec![AsrEngine::Whisper, AsrEngine::Qwen]
        );
        assert_eq!(asr.calls(), vec![AsrEngine::Whisper, AsrEngine::Qwen]);
    }

    #[test]
    fn active_session_blocks_switch_before_model_or_settings_access() {
        let asr = Arc::new(ScriptedAsr::new(
            Ok(EngineHealth::Healthy),
            Ok(EngineHealth::Healthy),
        ));
        let model_control = Arc::new(ScriptedModelControl::ready());
        let mut controller = controller(AsrPreference::Qwen, asr.clone());
        controller.model_control = model_control.clone();
        controller.activity = Arc::new(|| Ok(ApplicationActivity::Busy));

        assert_eq!(
            block_on(controller.switch_to(AsrEngine::Whisper)),
            Err(AsrModelSwitchError::Busy)
        );
        assert!(asr.calls().is_empty());
        assert!(model_control.calls().is_empty());
    }
}
