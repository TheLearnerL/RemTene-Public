//! Application use cases for the single global OpenAI-compatible Provider.
//!
//! Ordinary settings and encrypted secrets live in different stores. This
//! controller is the only production writer allowed to coordinate them. It
//! shares the Orchestrator's asynchronous configuration gate, so a mutation is
//! ordered either entirely before a Session freezes its route or after that
//! Session has ended.

use std::sync::Arc;

use futures::lock::Mutex as AsyncMutex;
use remtene_domain::{
    LlmNonSecretSettings, ProcessingMode, RequestId, SessionId, SettingsSnapshot,
};
use thiserror::Error;

use crate::ports::{
    LlmProvider, LlmRouteCandidate, LlmRouteResolution, LlmUpstreamError, PortError,
    ResolvedLlmRoute, SecretMaterialState, SecretStore, SecretValue, SettingsStore,
    TextProcessingRequest,
};
use crate::{OrchestratorError, TranscriptionOrchestrator};

const MAX_API_KEY_BYTES: usize = 8 * 1024;
const LLM_SECRET_NAMESPACE: &str = "llm.openai_compatible.";
const CONNECTION_TEST_TEXT: &str =
    "RemTene connection test. Return this sentence without adding new information.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LlmApiKeyStatus {
    NotConfigured,
    Configured,
    RecoveryRequired,
    Unavailable,
}

impl LlmApiKeyStatus {
    #[must_use]
    pub const fn is_configured(self) -> bool {
        matches!(self, Self::Configured)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LlmConnectionFailure {
    Busy,
    RuntimeUnavailable,
    SettingsUnavailable,
    NotConfigured,
    RecoveryRequired,
    SecretUnavailable,
    InvalidConfiguration,
    AuthenticationFailed,
    PermissionDenied,
    RateLimited,
    Timeout,
    Network,
    ProviderUnavailable,
    RequestRejected,
    InvalidResponse,
    ResponseTooLarge,
    Cancelled,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LlmConnectionTestOutcome {
    Succeeded,
    Failed(LlmConnectionFailure),
    FailedWithUpstream {
        failure: LlmConnectionFailure,
        upstream: LlmUpstreamError,
    },
}

#[derive(Debug, Error)]
pub enum LlmConfigurationError {
    #[error("an input Session is active")]
    Busy,
    #[error("LLM settings are not configured")]
    NotConfigured,
    #[error("encrypted secret material requires explicit recovery")]
    RecoveryRequired,
    #[error("API key input is invalid")]
    InvalidSecret,
    #[error("LLM configuration is invalid: {0}")]
    InvalidConfiguration(PortError),
    #[error("secret mutation could not be authenticated")]
    SecretVerificationFailed,
    #[error("orchestrator state is unavailable")]
    RuntimeUnavailable,
    #[error(transparent)]
    Port(#[from] PortError),
}

type ActiveWorkProbe = dyn Fn() -> Result<bool, OrchestratorError> + Send + Sync + 'static;

pub struct LlmConfigurationController {
    settings: Arc<dyn SettingsStore>,
    secrets: Arc<dyn SecretStore>,
    llm: Arc<dyn LlmProvider>,
    configuration_gate: Arc<AsyncMutex<()>>,
    active_work: Arc<ActiveWorkProbe>,
    operations: AsyncMutex<()>,
}

impl LlmConfigurationController {
    #[must_use]
    pub fn new(
        orchestrator: Arc<TranscriptionOrchestrator>,
        settings: Arc<dyn SettingsStore>,
        secrets: Arc<dyn SecretStore>,
        llm: Arc<dyn LlmProvider>,
    ) -> Self {
        let configuration_gate = orchestrator.configuration_gate();
        let active_orchestrator = Arc::clone(&orchestrator);
        Self {
            settings,
            secrets,
            llm,
            configuration_gate,
            active_work: Arc::new(move || active_orchestrator.has_active_work()),
            operations: AsyncMutex::new(()),
        }
    }

    pub async fn get_settings(&self) -> Result<SettingsSnapshot, LlmConfigurationError> {
        self.settings.load().await.map_err(Into::into)
    }

    /// Keeps the pre-existing delivery setting on the same serialized
    /// SettingsStore write path as LLM settings, preventing unrelated CAS
    /// updates from racing an endpoint transition.
    pub async fn set_clipboard_bridge_allowed(
        &self,
        allowed: bool,
    ) -> Result<SettingsSnapshot, LlmConfigurationError> {
        let _operation = self.operations.lock().await;
        let _configuration = self.configuration_gate.lock().await;
        let current = self.settings.load().await?;
        if current.clipboard_bridge_allowed() == allowed {
            return Ok(current);
        }

        let expected_version = current.version();
        let mut input = current.into_input();
        input.clipboard_bridge_allowed = allowed;
        input.version = expected_version;
        let candidate = SettingsSnapshot::new(input)
            .map_err(|_| invalid_configuration_error("settings.invalid"))?;
        self.settings
            .replace(expected_version, candidate)
            .await
            .map_err(Into::into)
    }

    /// Atomically updates the two text-processing preferences surfaced on the
    /// Status page. The shared configuration gate ensures an active Session
    /// either freezes the old pair or starts after the complete new pair is
    /// durable; it can never observe a half-updated combination.
    pub async fn set_text_processing_settings(
        &self,
        expected_version: u64,
        processing_mode: ProcessingMode,
        read_selected_text: bool,
    ) -> Result<SettingsSnapshot, LlmConfigurationError> {
        let _operation = self.operations.lock().await;
        let _configuration = self.configuration_gate.lock().await;
        self.ensure_idle()?;

        let current = self.settings.load().await?;
        if current.version() != expected_version {
            return Err(port_error("settings.version_conflict", false).into());
        }
        if current.processing_mode() == processing_mode
            && current.read_selected_text() == read_selected_text
        {
            return Ok(current);
        }

        let mut input = current.into_input();
        input.version = expected_version;
        input.processing_mode = processing_mode;
        input.read_selected_text = read_selected_text;
        let candidate = SettingsSnapshot::new(input)
            .map_err(|_| invalid_configuration_error("settings.invalid"))?;
        self.settings
            .replace(expected_version, candidate)
            .await
            .map_err(Into::into)
    }

    pub async fn set_llm_settings(
        &self,
        expected_version: u64,
        llm_settings: Option<LlmNonSecretSettings>,
    ) -> Result<SettingsSnapshot, LlmConfigurationError> {
        let _operation = self.operations.lock().await;
        let _configuration = self.configuration_gate.lock().await;
        self.ensure_idle()?;

        let current = self.settings.load().await?;
        if current.version() != expected_version {
            return Err(port_error("settings.version_conflict", false).into());
        }
        if current.llm() == llm_settings.as_ref() {
            return Ok(current);
        }
        self.ensure_secret_store_mutable().await?;

        let old_route = self
            .validated_route_for_settings(&current)
            .await
            .ok()
            .flatten();
        let new_route = match llm_settings.as_ref() {
            Some(settings) => Some(self.validated_route(settings).await?),
            None => None,
        };
        let endpoint_changed = old_route.as_ref().map(ResolvedLlmRoute::secret_id)
            != new_route.as_ref().map(ResolvedLlmRoute::secret_id);

        if endpoint_changed && let Some(new_route) = new_route.as_ref() {
            // Delete any orphan previously bound to the destination before
            // making that endpoint current. If this fails, neither current
            // settings nor their active key changes.
            self.secrets.delete(new_route.secret_id()).await?;
        }

        let mut input = current.clone().into_input();
        input.version = expected_version;
        input.llm = llm_settings;
        let candidate = SettingsSnapshot::new(input)
            .map_err(|_| invalid_configuration_error("settings.invalid"))?;
        let stored = self
            .settings
            .replace(expected_version, candidate)
            .await
            .map_err(LlmConfigurationError::from)?;

        if endpoint_changed && let Some(old_route) = old_route.as_ref() {
            // Settings are already durable. Failure here leaves only an
            // unreachable orphan. A future switch back pre-deletes that
            // destination, so the orphan can never silently reactivate.
            let _ = self.secrets.delete(old_route.secret_id()).await;
        }
        Ok(stored)
    }

    pub async fn api_key_status(&self) -> LlmApiKeyStatus {
        let _configuration = self.configuration_gate.lock().await;
        let Ok(settings) = self.settings.load().await else {
            return LlmApiKeyStatus::Unavailable;
        };
        self.status_for_settings(&settings).await
    }

    pub async fn is_llm_ready(&self) -> bool {
        let _configuration = self.configuration_gate.lock().await;
        let Ok(settings) = self.settings.load().await else {
            return false;
        };
        matches!(
            self.llm_resolution(&settings).await,
            LlmRouteResolution::Ready(_)
        )
    }

    pub async fn set_api_key(
        &self,
        value: SecretValue,
    ) -> Result<LlmApiKeyStatus, LlmConfigurationError> {
        validate_secret_value(value.expose())?;
        let _operation = self.operations.lock().await;
        let _configuration = self.configuration_gate.lock().await;
        self.ensure_idle()?;

        let settings = self.settings.load().await?;
        let route = self.require_validated_route(&settings).await?;
        self.ensure_secret_store_mutable().await?;
        self.secrets
            .replace_namespace(LLM_SECRET_NAMESPACE, route.secret_id(), value)
            .await?;
        match self.secrets.inspect(route.secret_id()).await? {
            SecretMaterialState::Configured => Ok(LlmApiKeyStatus::Configured),
            SecretMaterialState::RecoveryRequired => Err(LlmConfigurationError::RecoveryRequired),
            SecretMaterialState::NotConfigured => {
                Err(LlmConfigurationError::SecretVerificationFailed)
            }
        }
    }

    pub async fn reveal_api_key(&self) -> Result<SecretValue, LlmConfigurationError> {
        let _operation = self.operations.lock().await;
        let _configuration = self.configuration_gate.lock().await;
        let settings = self.settings.load().await?;
        let route = self.require_validated_route(&settings).await?;

        match self.secrets.inspect(route.secret_id()).await? {
            SecretMaterialState::NotConfigured => {
                return Err(LlmConfigurationError::NotConfigured);
            }
            SecretMaterialState::RecoveryRequired => {
                return Err(LlmConfigurationError::RecoveryRequired);
            }
            SecretMaterialState::Configured => {}
        }
        self.secrets
            .read(route.secret_id())
            .await?
            .ok_or(LlmConfigurationError::SecretVerificationFailed)
    }

    pub async fn delete_api_key(&self) -> Result<LlmApiKeyStatus, LlmConfigurationError> {
        let _operation = self.operations.lock().await;
        let _configuration = self.configuration_gate.lock().await;
        self.ensure_idle()?;
        match self.secrets.inspect_store().await? {
            SecretMaterialState::RecoveryRequired => {
                return Err(LlmConfigurationError::RecoveryRequired);
            }
            SecretMaterialState::NotConfigured => {
                return Ok(LlmApiKeyStatus::NotConfigured);
            }
            SecretMaterialState::Configured => {}
        }
        self.secrets.delete_namespace(LLM_SECRET_NAMESPACE).await?;
        Ok(LlmApiKeyStatus::NotConfigured)
    }

    pub async fn reset_unrecoverable_secrets(
        &self,
    ) -> Result<LlmApiKeyStatus, LlmConfigurationError> {
        let _operation = self.operations.lock().await;
        let _configuration = self.configuration_gate.lock().await;
        self.ensure_idle()?;
        if self.secrets.inspect_store().await? != SecretMaterialState::RecoveryRequired {
            return Err(port_error("secret.reset_not_required", false).into());
        }
        self.secrets.reset_unrecoverable_store().await?;
        match self.secrets.inspect_store().await? {
            SecretMaterialState::NotConfigured => Ok(LlmApiKeyStatus::NotConfigured),
            SecretMaterialState::RecoveryRequired | SecretMaterialState::Configured => {
                Err(LlmConfigurationError::SecretVerificationFailed)
            }
        }
    }

    pub async fn test_connection(&self) -> LlmConnectionTestOutcome {
        let _operation = self.operations.lock().await;
        let configuration_guard = self.configuration_gate.lock().await;
        match self.ensure_idle() {
            Ok(()) => {}
            Err(LlmConfigurationError::Busy) => {
                return LlmConnectionTestOutcome::Failed(LlmConnectionFailure::Busy);
            }
            Err(LlmConfigurationError::RuntimeUnavailable) => {
                return LlmConnectionTestOutcome::Failed(LlmConnectionFailure::RuntimeUnavailable);
            }
            Err(_) => {
                return LlmConnectionTestOutcome::Failed(LlmConnectionFailure::Internal);
            }
        }
        let settings = match self.settings.load().await {
            Ok(settings) => settings,
            Err(_) => {
                return LlmConnectionTestOutcome::Failed(LlmConnectionFailure::SettingsUnavailable);
            }
        };
        let route = match self.llm_resolution(&settings).await {
            LlmRouteResolution::Ready(route) => route,
            LlmRouteResolution::NoConfiguration | LlmRouteResolution::MissingSecret(_) => {
                return LlmConnectionTestOutcome::Failed(LlmConnectionFailure::NotConfigured);
            }
            LlmRouteResolution::Unavailable {
                route: Some(route),
                error,
            } => {
                let failure = match self.secrets.inspect(route.secret_id()).await {
                    Ok(SecretMaterialState::RecoveryRequired) => {
                        LlmConnectionFailure::RecoveryRequired
                    }
                    Ok(SecretMaterialState::NotConfigured) => LlmConnectionFailure::NotConfigured,
                    Ok(SecretMaterialState::Configured) => map_connection_error(&error),
                    Err(_) => LlmConnectionFailure::SecretUnavailable,
                };
                return LlmConnectionTestOutcome::Failed(failure);
            }
            LlmRouteResolution::Unavailable { route: None, error } => {
                return LlmConnectionTestOutcome::Failed(map_connection_error(&error));
            }
        };
        // Configuration mutations remain serialized by `operations`, while the
        // start hotkey is not blocked behind a potentially long network test.
        drop(configuration_guard);

        let session_id = SessionId::new();
        let request_id = RequestId::new();
        let result = self
            .llm
            .probe_connection(
                route.clone(),
                TextProcessingRequest {
                    session_id,
                    request_id,
                    processing_mode: ProcessingMode::Faithful,
                    raw_transcript: CONNECTION_TEST_TEXT.to_owned(),
                    selected_text: None,
                },
            )
            .await;
        match result {
            Ok(result)
                if result.session_id == session_id
                    && result.request_id == request_id
                    && !result.final_text.trim().is_empty() =>
            {
                LlmConnectionTestOutcome::Succeeded
            }
            Ok(_) => LlmConnectionTestOutcome::Failed(LlmConnectionFailure::InvalidResponse),
            Err(error) if error.error.code == "llm.invalid_canonical_response" => {
                LlmConnectionTestOutcome::Succeeded
            }
            Err(error) => {
                let failure = map_connection_error(&error.error);
                match error.upstream {
                    Some(upstream) => {
                        LlmConnectionTestOutcome::FailedWithUpstream { failure, upstream }
                    }
                    None => LlmConnectionTestOutcome::Failed(failure),
                }
            }
        }
    }

    fn ensure_idle(&self) -> Result<(), LlmConfigurationError> {
        match (self.active_work)() {
            Ok(true) => Err(LlmConfigurationError::Busy),
            Ok(false) => Ok(()),
            Err(_) => Err(LlmConfigurationError::RuntimeUnavailable),
        }
    }

    async fn status_for_settings(&self, settings: &SettingsSnapshot) -> LlmApiKeyStatus {
        match self.secrets.inspect_store().await {
            Ok(SecretMaterialState::RecoveryRequired) => {
                return LlmApiKeyStatus::RecoveryRequired;
            }
            Err(_) => return LlmApiKeyStatus::Unavailable,
            Ok(SecretMaterialState::NotConfigured | SecretMaterialState::Configured) => {}
        }
        match self.llm_resolution(settings).await {
            LlmRouteResolution::NoConfiguration | LlmRouteResolution::MissingSecret(_) => {
                LlmApiKeyStatus::NotConfigured
            }
            LlmRouteResolution::Ready(_) => LlmApiKeyStatus::Configured,
            LlmRouteResolution::Unavailable {
                route: Some(route), ..
            } => match self.secrets.inspect(route.secret_id()).await {
                Ok(SecretMaterialState::NotConfigured) => LlmApiKeyStatus::NotConfigured,
                Ok(SecretMaterialState::RecoveryRequired) => LlmApiKeyStatus::RecoveryRequired,
                Ok(SecretMaterialState::Configured) => LlmApiKeyStatus::Configured,
                Err(_) => LlmApiKeyStatus::Unavailable,
            },
            LlmRouteResolution::Unavailable { route: None, .. } => LlmApiKeyStatus::Unavailable,
        }
    }

    async fn llm_resolution(&self, settings: &SettingsSnapshot) -> LlmRouteResolution {
        let candidate = settings
            .llm()
            .map(|llm| LlmRouteCandidate::new(llm.base_url(), llm.model()));
        self.llm.resolve_route(candidate).await
    }

    async fn validated_route(
        &self,
        settings: &LlmNonSecretSettings,
    ) -> Result<ResolvedLlmRoute, LlmConfigurationError> {
        match self
            .llm
            .resolve_route(Some(LlmRouteCandidate::new(
                settings.base_url(),
                settings.model(),
            )))
            .await
        {
            LlmRouteResolution::Ready(route)
            | LlmRouteResolution::MissingSecret(route)
            | LlmRouteResolution::Unavailable {
                route: Some(route), ..
            } => Ok(route),
            LlmRouteResolution::NoConfiguration => Err(LlmConfigurationError::NotConfigured),
            LlmRouteResolution::Unavailable { route: None, error } => {
                Err(LlmConfigurationError::InvalidConfiguration(error))
            }
        }
    }

    async fn validated_route_for_settings(
        &self,
        settings: &SettingsSnapshot,
    ) -> Result<Option<ResolvedLlmRoute>, LlmConfigurationError> {
        match settings.llm() {
            Some(llm) => self.validated_route(llm).await.map(Some),
            None => Ok(None),
        }
    }

    async fn require_validated_route(
        &self,
        settings: &SettingsSnapshot,
    ) -> Result<ResolvedLlmRoute, LlmConfigurationError> {
        self.validated_route_for_settings(settings)
            .await?
            .ok_or(LlmConfigurationError::NotConfigured)
    }

    async fn ensure_secret_store_mutable(&self) -> Result<(), LlmConfigurationError> {
        match self.secrets.inspect_store().await? {
            SecretMaterialState::RecoveryRequired => Err(LlmConfigurationError::RecoveryRequired),
            SecretMaterialState::NotConfigured | SecretMaterialState::Configured => Ok(()),
        }
    }
}

fn validate_secret_value(value: &str) -> Result<(), LlmConfigurationError> {
    // Reqwest ultimately places this value after `Bearer ` in an HTTP header.
    // Accept only visible ASCII without whitespace so a key can never be
    // persisted as "configured" and then fail HeaderValue construction.
    if value.is_empty()
        || value.len() > MAX_API_KEY_BYTES
        || !value
            .as_bytes()
            .iter()
            .all(|byte| (0x21..=0x7e).contains(byte))
    {
        return Err(LlmConfigurationError::InvalidSecret);
    }
    Ok(())
}

fn map_connection_error(error: &PortError) -> LlmConnectionFailure {
    match error.code.as_str() {
        "llm.api_key_missing" => LlmConnectionFailure::NotConfigured,
        "llm.secret_unavailable" => LlmConnectionFailure::SecretUnavailable,
        "llm.secret_recovery_required" => LlmConnectionFailure::RecoveryRequired,
        "llm.invalid_config" => LlmConnectionFailure::InvalidConfiguration,
        "llm.invalid_request" => LlmConnectionFailure::Internal,
        "llm.authentication_failed" => LlmConnectionFailure::AuthenticationFailed,
        "llm.permission_denied" => LlmConnectionFailure::PermissionDenied,
        "llm.rate_limited" => LlmConnectionFailure::RateLimited,
        "llm.timeout" => LlmConnectionFailure::Timeout,
        "llm.network" => LlmConnectionFailure::Network,
        "llm.provider_unavailable" | "llm.client_unavailable" => {
            LlmConnectionFailure::ProviderUnavailable
        }
        "llm.request_rejected" => LlmConnectionFailure::RequestRejected,
        "llm.invalid_response" => LlmConnectionFailure::InvalidResponse,
        "llm.response_too_large" => LlmConnectionFailure::ResponseTooLarge,
        "llm.cancelled" => LlmConnectionFailure::Cancelled,
        _ => LlmConnectionFailure::Internal,
    }
}

fn invalid_configuration_error(code: &str) -> LlmConfigurationError {
    LlmConfigurationError::InvalidConfiguration(port_error(code, false))
}

fn port_error(code: &str, retryable: bool) -> PortError {
    PortError {
        code: code.to_owned(),
        safe_message_key: format!("errors.{code}"),
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    use futures::executor::block_on;
    use remtene_domain::{
        AsrPreference, HistoryPolicy, IntentDecision, RecordingMode, SettingsSnapshotInput,
    };

    use crate::ports::{LlmConnectionProbeError, PortFuture, TextProcessingResult};

    use super::*;

    const ENDPOINT_A: &str = "https://endpoint-a.test/v1";
    const ENDPOINT_B: &str = "https://endpoint-b.test/v1";
    const SECRET_A: &str = "llm.openai_compatible.endpoint_a";
    const SECRET_B: &str = "llm.openai_compatible.endpoint_b";

    struct ReplaceBarrier {
        started: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    struct MockSettingsState {
        snapshot: SettingsSnapshot,
        replace_error: Option<PortError>,
        replace_barrier: Option<ReplaceBarrier>,
        load_error: Option<PortError>,
    }

    struct MockSettingsStore {
        state: Mutex<MockSettingsState>,
        load_calls: AtomicUsize,
        replace_calls: AtomicUsize,
    }

    impl MockSettingsStore {
        fn new(snapshot: SettingsSnapshot) -> Self {
            Self {
                state: Mutex::new(MockSettingsState {
                    snapshot,
                    replace_error: None,
                    replace_barrier: None,
                    load_error: None,
                }),
                load_calls: AtomicUsize::new(0),
                replace_calls: AtomicUsize::new(0),
            }
        }

        fn snapshot(&self) -> SettingsSnapshot {
            self.state
                .lock()
                .expect("settings state lock")
                .snapshot
                .clone()
        }

        fn fail_replace_with(&self, error: PortError) {
            self.state
                .lock()
                .expect("settings state lock")
                .replace_error = Some(error);
        }

        fn fail_load_with(&self, error: PortError) {
            self.state.lock().expect("settings state lock").load_error = Some(error);
        }

        fn install_replace_barrier(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
            let (started_sender, started_receiver) = mpsc::channel();
            let (release_sender, release_receiver) = mpsc::channel();
            self.state
                .lock()
                .expect("settings state lock")
                .replace_barrier = Some(ReplaceBarrier {
                started: started_sender,
                release: release_receiver,
            });
            (started_receiver, release_sender)
        }
    }

    impl SettingsStore for MockSettingsStore {
        fn load(&self) -> PortFuture<'_, Result<SettingsSnapshot, PortError>> {
            self.load_calls.fetch_add(1, Ordering::SeqCst);
            let result = {
                let state = self.state.lock().expect("settings state lock");
                state
                    .load_error
                    .clone()
                    .map_or_else(|| Ok(state.snapshot.clone()), Err)
            };
            Box::pin(async move { result })
        }

        fn replace(
            &self,
            expected_version: u64,
            settings: SettingsSnapshot,
        ) -> PortFuture<'_, Result<SettingsSnapshot, PortError>> {
            self.replace_calls.fetch_add(1, Ordering::SeqCst);
            let barrier = self
                .state
                .lock()
                .expect("settings state lock")
                .replace_barrier
                .take();
            if let Some(barrier) = barrier {
                barrier.started.send(()).expect("signal replace start");
                barrier.release.recv().expect("release settings replace");
            }

            let result = {
                let mut state = self.state.lock().expect("settings state lock");
                if let Some(error) = state.replace_error.clone() {
                    Err(error)
                } else if state.snapshot.version() != expected_version {
                    Err(test_error("settings.version_conflict"))
                } else {
                    let mut input = settings.into_input();
                    input.version = expected_version
                        .checked_add(1)
                        .expect("test settings version must not overflow");
                    let stored =
                        SettingsSnapshot::new(input).expect("replacement settings must be valid");
                    state.snapshot = stored.clone();
                    Ok(stored)
                }
            };
            Box::pin(async move { result })
        }
    }

    #[derive(Default)]
    struct SecretCalls {
        inspect: AtomicUsize,
        inspect_store: AtomicUsize,
        read: AtomicUsize,
        replace: AtomicUsize,
        replace_namespace: AtomicUsize,
        delete: AtomicUsize,
        delete_namespace: AtomicUsize,
        reset: AtomicUsize,
    }

    #[derive(Default)]
    struct MockSecretState {
        values: HashMap<String, String>,
        store_result: Option<Result<SecretMaterialState, PortError>>,
        inspect_results: HashMap<String, Result<SecretMaterialState, PortError>>,
        delete_errors: HashMap<String, PortError>,
        delete_namespace_error: Option<PortError>,
        replace_namespace_error: Option<PortError>,
    }

    #[derive(Default)]
    struct MockSecretStore {
        state: Mutex<MockSecretState>,
        calls: SecretCalls,
    }

    impl MockSecretStore {
        fn put(&self, secret_id: &str, value: &str) {
            self.state
                .lock()
                .expect("secret state lock")
                .values
                .insert(secret_id.to_owned(), value.to_owned());
        }

        fn value(&self, secret_id: &str) -> Option<String> {
            self.state
                .lock()
                .expect("secret state lock")
                .values
                .get(secret_id)
                .cloned()
        }

        fn set_store_result(&self, result: Result<SecretMaterialState, PortError>) {
            self.state.lock().expect("secret state lock").store_result = Some(result);
        }

        fn set_inspect_result(
            &self,
            secret_id: &str,
            result: Result<SecretMaterialState, PortError>,
        ) {
            self.state
                .lock()
                .expect("secret state lock")
                .inspect_results
                .insert(secret_id.to_owned(), result);
        }

        fn fail_delete_for(&self, secret_id: &str, error: PortError) {
            self.state
                .lock()
                .expect("secret state lock")
                .delete_errors
                .insert(secret_id.to_owned(), error);
        }

        fn clear_delete_error(&self, secret_id: &str) {
            self.state
                .lock()
                .expect("secret state lock")
                .delete_errors
                .remove(secret_id);
        }

        fn fail_replace_namespace_with(&self, error: PortError) {
            self.state
                .lock()
                .expect("secret state lock")
                .replace_namespace_error = Some(error);
        }
    }

    impl SecretStore for MockSecretStore {
        fn is_configured(&self, secret_id: &str) -> PortFuture<'_, Result<bool, PortError>> {
            let configured = self
                .state
                .lock()
                .expect("secret state lock")
                .values
                .contains_key(secret_id);
            Box::pin(async move { Ok(configured) })
        }

        fn inspect(
            &self,
            secret_id: &str,
        ) -> PortFuture<'_, Result<SecretMaterialState, PortError>> {
            self.calls.inspect.fetch_add(1, Ordering::SeqCst);
            let result = {
                let state = self.state.lock().expect("secret state lock");
                state
                    .inspect_results
                    .get(secret_id)
                    .cloned()
                    .unwrap_or_else(|| {
                        Ok(if state.values.contains_key(secret_id) {
                            SecretMaterialState::Configured
                        } else {
                            SecretMaterialState::NotConfigured
                        })
                    })
            };
            Box::pin(async move { result })
        }

        fn inspect_store(&self) -> PortFuture<'_, Result<SecretMaterialState, PortError>> {
            self.calls.inspect_store.fetch_add(1, Ordering::SeqCst);
            let result = {
                let state = self.state.lock().expect("secret state lock");
                state.store_result.clone().unwrap_or_else(|| {
                    Ok(if state.values.is_empty() {
                        SecretMaterialState::NotConfigured
                    } else {
                        SecretMaterialState::Configured
                    })
                })
            };
            Box::pin(async move { result })
        }

        fn read(&self, secret_id: &str) -> PortFuture<'_, Result<Option<SecretValue>, PortError>> {
            self.calls.read.fetch_add(1, Ordering::SeqCst);
            let value = self
                .state
                .lock()
                .expect("secret state lock")
                .values
                .get(secret_id)
                .cloned()
                .map(SecretValue::new);
            Box::pin(async move { Ok(value) })
        }

        fn replace(
            &self,
            secret_id: &str,
            value: SecretValue,
        ) -> PortFuture<'_, Result<(), PortError>> {
            self.calls.replace.fetch_add(1, Ordering::SeqCst);
            self.state
                .lock()
                .expect("secret state lock")
                .values
                .insert(secret_id.to_owned(), value.expose().to_owned());
            Box::pin(async { Ok(()) })
        }

        fn replace_namespace(
            &self,
            namespace: &str,
            secret_id: &str,
            value: SecretValue,
        ) -> PortFuture<'_, Result<(), PortError>> {
            self.calls.replace_namespace.fetch_add(1, Ordering::SeqCst);
            let result = {
                let mut state = self.state.lock().expect("secret state lock");
                if let Some(error) = state.replace_namespace_error.clone() {
                    Err(error)
                } else {
                    state.values.retain(|id, _| !id.starts_with(namespace));
                    state
                        .values
                        .insert(secret_id.to_owned(), value.expose().to_owned());
                    Ok(())
                }
            };
            Box::pin(async move { result })
        }

        fn delete(&self, secret_id: &str) -> PortFuture<'_, Result<(), PortError>> {
            self.calls.delete.fetch_add(1, Ordering::SeqCst);
            let result = {
                let mut state = self.state.lock().expect("secret state lock");
                if let Some(error) = state.delete_errors.get(secret_id).cloned() {
                    Err(error)
                } else {
                    state.values.remove(secret_id);
                    Ok(())
                }
            };
            Box::pin(async move { result })
        }

        fn delete_namespace(&self, namespace: &str) -> PortFuture<'_, Result<u64, PortError>> {
            self.calls.delete_namespace.fetch_add(1, Ordering::SeqCst);
            let result = {
                let mut state = self.state.lock().expect("secret state lock");
                if let Some(error) = state.delete_namespace_error.clone() {
                    Err(error)
                } else {
                    let before = state.values.len();
                    state.values.retain(|id, _| !id.starts_with(namespace));
                    let deleted = before
                        .checked_sub(state.values.len())
                        .expect("secret count cannot grow while deleting");
                    Ok(u64::try_from(deleted).expect("test secret count must fit u64"))
                }
            };
            Box::pin(async move { result })
        }

        fn reset_unrecoverable_store(&self) -> PortFuture<'_, Result<(), PortError>> {
            self.calls.reset.fetch_add(1, Ordering::SeqCst);
            let mut state = self.state.lock().expect("secret state lock");
            state.values.clear();
            state.store_result = None;
            Box::pin(async { Ok(()) })
        }
    }

    #[derive(Clone)]
    enum ResolveBehavior {
        Ready,
        MissingSecret,
        UnavailableWithRoute(PortError),
        UnavailableWithoutRoute(PortError),
    }

    #[derive(Clone)]
    enum ProcessBehavior {
        Echo,
        Empty,
        WrongIdentity,
        Error(PortError),
    }

    struct ProcessBarrier {
        started: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    struct MockLlmProvider {
        resolve_behavior: Mutex<ResolveBehavior>,
        process_behavior: Mutex<ProcessBehavior>,
        probe_upstream: Mutex<Option<LlmUpstreamError>>,
        process_barrier: Mutex<Option<ProcessBarrier>>,
        resolve_calls: AtomicUsize,
        process_calls: AtomicUsize,
        cancel_calls: AtomicUsize,
        captured: Mutex<Vec<(ResolvedLlmRoute, TextProcessingRequest)>>,
    }

    impl MockLlmProvider {
        fn new(resolve_behavior: ResolveBehavior) -> Self {
            Self {
                resolve_behavior: Mutex::new(resolve_behavior),
                process_behavior: Mutex::new(ProcessBehavior::Echo),
                probe_upstream: Mutex::new(None),
                process_barrier: Mutex::new(None),
                resolve_calls: AtomicUsize::new(0),
                process_calls: AtomicUsize::new(0),
                cancel_calls: AtomicUsize::new(0),
                captured: Mutex::new(Vec::new()),
            }
        }

        fn set_resolve_behavior(&self, behavior: ResolveBehavior) {
            *self.resolve_behavior.lock().expect("resolve behavior lock") = behavior;
        }

        fn set_process_behavior(&self, behavior: ProcessBehavior) {
            *self.process_behavior.lock().expect("process behavior lock") = behavior;
        }

        fn set_probe_upstream(&self, upstream: LlmUpstreamError) {
            *self.probe_upstream.lock().expect("probe upstream lock") = Some(upstream);
        }

        fn install_process_barrier(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
            let (started_sender, started_receiver) = mpsc::channel();
            let (release_sender, release_receiver) = mpsc::channel();
            *self.process_barrier.lock().expect("process barrier lock") = Some(ProcessBarrier {
                started: started_sender,
                release: release_receiver,
            });
            (started_receiver, release_sender)
        }

        fn captured(&self) -> Vec<(ResolvedLlmRoute, TextProcessingRequest)> {
            self.captured.lock().expect("captured calls lock").clone()
        }
    }

    impl LlmProvider for MockLlmProvider {
        fn resolve_route(
            &self,
            candidate: Option<LlmRouteCandidate>,
        ) -> PortFuture<'_, LlmRouteResolution> {
            self.resolve_calls.fetch_add(1, Ordering::SeqCst);
            let behavior = self
                .resolve_behavior
                .lock()
                .expect("resolve behavior lock")
                .clone();
            let resolution = match candidate {
                None => LlmRouteResolution::NoConfiguration,
                Some(candidate) => {
                    if candidate.base_url().starts_with("invalid:") {
                        LlmRouteResolution::Unavailable {
                            route: None,
                            error: test_error("llm.invalid_config"),
                        }
                    } else {
                        let route = route_for(&candidate);
                        match behavior {
                            ResolveBehavior::Ready => LlmRouteResolution::Ready(route),
                            ResolveBehavior::MissingSecret => {
                                LlmRouteResolution::MissingSecret(route)
                            }
                            ResolveBehavior::UnavailableWithRoute(error) => {
                                LlmRouteResolution::Unavailable {
                                    route: Some(route),
                                    error,
                                }
                            }
                            ResolveBehavior::UnavailableWithoutRoute(error) => {
                                LlmRouteResolution::Unavailable { route: None, error }
                            }
                        }
                    }
                }
            };
            Box::pin(async move { resolution })
        }

        fn process(
            &self,
            route: ResolvedLlmRoute,
            request: TextProcessingRequest,
        ) -> PortFuture<'_, Result<TextProcessingResult, PortError>> {
            self.process_calls.fetch_add(1, Ordering::SeqCst);
            self.captured
                .lock()
                .expect("captured calls lock")
                .push((route, request.clone()));
            if let Some(barrier) = self
                .process_barrier
                .lock()
                .expect("process barrier lock")
                .take()
            {
                barrier.started.send(()).expect("signal process start");
                barrier.release.recv().expect("release provider process");
            }
            let behavior = self
                .process_behavior
                .lock()
                .expect("process behavior lock")
                .clone();
            let result = match behavior {
                ProcessBehavior::Echo => Ok(TextProcessingResult {
                    session_id: request.session_id,
                    request_id: request.request_id,
                    intent: IntentDecision::Dictation,
                    final_text: request.raw_transcript.clone(),
                }),
                ProcessBehavior::Empty => Ok(TextProcessingResult {
                    session_id: request.session_id,
                    request_id: request.request_id,
                    intent: IntentDecision::Dictation,
                    final_text: "  ".to_owned(),
                }),
                ProcessBehavior::WrongIdentity => Ok(TextProcessingResult {
                    session_id: SessionId::new(),
                    request_id: RequestId::new(),
                    intent: IntentDecision::Dictation,
                    final_text: request.raw_transcript.clone(),
                }),
                ProcessBehavior::Error(error) => Err(error),
            };
            Box::pin(async move { result })
        }

        fn probe_connection(
            &self,
            route: ResolvedLlmRoute,
            request: TextProcessingRequest,
        ) -> PortFuture<'_, Result<TextProcessingResult, LlmConnectionProbeError>> {
            Box::pin(async move {
                self.process(route, request).await.map_err(|error| {
                    match self
                        .probe_upstream
                        .lock()
                        .expect("probe upstream lock")
                        .take()
                    {
                        Some(upstream) => LlmConnectionProbeError::with_upstream(error, upstream),
                        None => LlmConnectionProbeError::from_port(error),
                    }
                })
            })
        }

        fn cancel(&self, _request_id: RequestId) -> PortFuture<'_, Result<(), PortError>> {
            self.cancel_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    fn settings(version: u64, llm: Option<(&str, &str)>) -> SettingsSnapshot {
        SettingsSnapshot::new(SettingsSnapshotInput {
            version,
            recording_mode: RecordingMode::Toggle,
            max_recording_duration: Duration::from_secs(60),
            recording_shortcut: None,
            processing_mode: ProcessingMode::Faithful,
            asr_preference: AsrPreference::Qwen,
            llm: llm.map(|(base_url, model)| {
                LlmNonSecretSettings::new(base_url, model).expect("test LLM settings")
            }),
            read_selected_text: false,
            clipboard_bridge_allowed: false,
            auto_copy_result: false,
            local_diagnostics_enabled: true,
            history_policy: HistoryPolicy {
                enabled: false,
                limit: 10,
                retention_days: None,
            },
        })
        .expect("test settings must be valid")
    }

    fn llm_settings(base_url: &str, model: &str) -> LlmNonSecretSettings {
        LlmNonSecretSettings::new(base_url, model).expect("test LLM settings")
    }

    fn route_for(candidate: &LlmRouteCandidate) -> ResolvedLlmRoute {
        let secret_id = match candidate.base_url() {
            ENDPOINT_A => SECRET_A,
            ENDPOINT_B => SECRET_B,
            _ => "llm.openai_compatible.other",
        };
        ResolvedLlmRoute::new(
            "openai_compatible",
            candidate.base_url(),
            candidate.model(),
            secret_id,
        )
    }

    fn test_error(code: &str) -> PortError {
        PortError {
            code: code.to_owned(),
            safe_message_key: format!("errors.{code}"),
            retryable: false,
        }
    }

    fn controller(
        settings: Arc<MockSettingsStore>,
        secrets: Arc<MockSecretStore>,
        llm: Arc<MockLlmProvider>,
        configuration_gate: Arc<AsyncMutex<()>>,
        active: Arc<AtomicBool>,
    ) -> Arc<LlmConfigurationController> {
        let settings_port: Arc<dyn SettingsStore> = settings;
        let secrets_port: Arc<dyn SecretStore> = secrets;
        let llm_port: Arc<dyn LlmProvider> = llm;
        Arc::new(LlmConfigurationController {
            settings: settings_port,
            secrets: secrets_port,
            llm: llm_port,
            configuration_gate,
            active_work: Arc::new(move || Ok(active.load(Ordering::SeqCst))),
            operations: AsyncMutex::new(()),
        })
    }

    type StandardController = (
        Arc<LlmConfigurationController>,
        Arc<MockSettingsStore>,
        Arc<MockSecretStore>,
        Arc<MockLlmProvider>,
        Arc<AtomicBool>,
    );

    fn standard_controller(initial: SettingsSnapshot) -> StandardController {
        let settings = Arc::new(MockSettingsStore::new(initial));
        let secrets = Arc::new(MockSecretStore::default());
        let provider = Arc::new(MockLlmProvider::new(ResolveBehavior::Ready));
        let active = Arc::new(AtomicBool::new(false));
        let controller = controller(
            Arc::clone(&settings),
            Arc::clone(&secrets),
            Arc::clone(&provider),
            Arc::new(AsyncMutex::new(())),
            Arc::clone(&active),
        );
        (controller, settings, secrets, provider, active)
    }

    #[test]
    fn active_session_blocks_all_llm_configuration_and_secret_mutations() {
        let (controller, settings, secrets, _provider, active) =
            standard_controller(settings(1, Some((ENDPOINT_A, "model-a"))));
        active.store(true, Ordering::SeqCst);

        let results = [
            block_on(controller.set_llm_settings(1, Some(llm_settings(ENDPOINT_B, "model-b"))))
                .map(|_| ()),
            block_on(controller.set_api_key(SecretValue::new("new-key"))).map(|_| ()),
            block_on(controller.delete_api_key()).map(|_| ()),
            block_on(controller.reset_unrecoverable_secrets()).map(|_| ()),
        ];

        assert!(
            results
                .iter()
                .all(|result| matches!(result, Err(LlmConfigurationError::Busy)))
        );
        assert_eq!(settings.replace_calls.load(Ordering::SeqCst), 0);
        assert_eq!(secrets.calls.replace_namespace.load(Ordering::SeqCst), 0);
        assert_eq!(secrets.calls.delete.load(Ordering::SeqCst), 0);
        assert_eq!(secrets.calls.delete_namespace.load(Ordering::SeqCst), 0);
        assert_eq!(secrets.calls.reset.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn text_processing_settings_update_is_atomic_and_preserves_unrelated_fields() {
        let initial = settings(4, Some((ENDPOINT_A, "model-a")));
        let (controller, settings_store, _secrets, _provider, _active) =
            standard_controller(initial.clone());

        let stored =
            block_on(controller.set_text_processing_settings(4, ProcessingMode::Structured, true))
                .expect("text processing settings should update");

        assert_eq!(stored.version(), 5);
        assert_eq!(stored.processing_mode(), ProcessingMode::Structured);
        assert!(stored.read_selected_text());
        assert_eq!(stored.llm(), initial.llm());
        assert_eq!(
            stored.clipboard_bridge_allowed(),
            initial.clipboard_bridge_allowed()
        );
        assert_eq!(settings_store.snapshot(), stored);
        assert_eq!(settings_store.replace_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn text_processing_settings_check_version_before_noop() {
        let (controller, settings_store, _secrets, _provider, _active) =
            standard_controller(settings(4, None));

        let result =
            block_on(controller.set_text_processing_settings(3, ProcessingMode::Faithful, false));

        assert!(matches!(
            result,
            Err(LlmConfigurationError::Port(PortError { ref code, .. }))
                if code == "settings.version_conflict"
        ));
        assert_eq!(settings_store.replace_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn text_processing_settings_noop_keeps_version() {
        let (controller, settings_store, _secrets, _provider, _active) =
            standard_controller(settings(4, None));

        let stored =
            block_on(controller.set_text_processing_settings(4, ProcessingMode::Faithful, false))
                .expect("matching settings are a valid no-op");

        assert_eq!(stored.version(), 4);
        assert_eq!(settings_store.replace_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn active_session_blocks_text_processing_settings_update() {
        let (controller, settings_store, _secrets, _provider, active) =
            standard_controller(settings(1, None));
        active.store(true, Ordering::SeqCst);

        let result =
            block_on(controller.set_text_processing_settings(1, ProcessingMode::Raw, true));

        assert!(matches!(result, Err(LlmConfigurationError::Busy)));
        assert_eq!(settings_store.replace_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn session_that_acquires_configuration_gate_first_makes_mutation_observe_busy() {
        let settings_store = Arc::new(MockSettingsStore::new(settings(
            1,
            Some((ENDPOINT_A, "model-a")),
        )));
        let secrets = Arc::new(MockSecretStore::default());
        let provider = Arc::new(MockLlmProvider::new(ResolveBehavior::Ready));
        let active = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(AsyncMutex::new(()));
        let controller = controller(
            Arc::clone(&settings_store),
            secrets,
            provider,
            Arc::clone(&gate),
            Arc::clone(&active),
        );

        let session_guard = block_on(gate.lock());
        active.store(true, Ordering::SeqCst);
        let (attempted_sender, attempted_receiver) = mpsc::channel();
        let (result_sender, result_receiver) = mpsc::channel();
        let mutation_controller = Arc::clone(&controller);
        let mutation = thread::spawn(move || {
            attempted_sender.send(()).expect("signal mutation attempt");
            let result = block_on(
                mutation_controller.set_llm_settings(1, Some(llm_settings(ENDPOINT_A, "model-b"))),
            );
            result_sender.send(result).expect("send mutation result");
        });
        attempted_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("mutation thread should start");
        assert!(matches!(
            result_receiver.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        drop(session_guard);
        assert!(matches!(
            result_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("mutation should finish"),
            Err(LlmConfigurationError::Busy)
        ));
        mutation.join().expect("mutation thread");
        assert_eq!(settings_store.replace_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn mutation_that_acquires_configuration_gate_first_is_fully_visible_to_next_session() {
        let settings_store = Arc::new(MockSettingsStore::new(settings(
            1,
            Some((ENDPOINT_A, "model-a")),
        )));
        let (replace_started, release_replace) = settings_store.install_replace_barrier();
        let secrets = Arc::new(MockSecretStore::default());
        let provider = Arc::new(MockLlmProvider::new(ResolveBehavior::Ready));
        let active = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(AsyncMutex::new(()));
        let controller = controller(
            Arc::clone(&settings_store),
            secrets,
            provider,
            Arc::clone(&gate),
            active,
        );

        let (mutation_sender, mutation_receiver) = mpsc::channel();
        let mutation_controller = Arc::clone(&controller);
        let mutation = thread::spawn(move || {
            mutation_sender
                .send(block_on(mutation_controller.set_llm_settings(
                    1,
                    Some(llm_settings(ENDPOINT_A, "model-b")),
                )))
                .expect("send mutation result");
        });
        replace_started
            .recv_timeout(Duration::from_secs(1))
            .expect("mutation should reach settings CAS while holding gate");

        let (session_started_sender, session_started_receiver) = mpsc::channel();
        let (session_sender, session_receiver) = mpsc::channel();
        let session_gate = Arc::clone(&gate);
        let session_settings = Arc::clone(&settings_store);
        let session = thread::spawn(move || {
            session_started_sender
                .send(())
                .expect("signal session attempt");
            let _guard = block_on(session_gate.lock());
            session_sender
                .send(session_settings.snapshot())
                .expect("send session-visible settings");
        });
        session_started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("session thread should start");
        assert!(matches!(
            session_receiver.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        release_replace.send(()).expect("release settings CAS");
        let stored = mutation_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("mutation should finish")
            .expect("mutation should succeed");
        let session_visible = session_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("session should acquire gate after mutation");
        mutation.join().expect("mutation thread");
        session.join().expect("session thread");

        assert_eq!(stored.version(), 2);
        assert_eq!(
            session_visible.llm().expect("LLM settings").model(),
            "model-b"
        );
        assert_eq!(session_visible.version(), 2);
    }

    #[test]
    fn endpoint_switch_predeletes_destination_and_never_assigns_old_key_to_it() {
        let (controller, settings_store, secrets, _provider, _active) =
            standard_controller(settings(1, Some((ENDPOINT_A, "model-a"))));
        secrets.put(SECRET_A, "key-a");
        secrets.put(SECRET_B, "orphan-key-b");

        let stored =
            block_on(controller.set_llm_settings(1, Some(llm_settings(ENDPOINT_B, "model-b"))))
                .expect("endpoint switch should succeed");

        assert_eq!(stored.llm().expect("new settings").base_url(), ENDPOINT_B);
        assert_eq!(settings_store.snapshot(), stored);
        assert_eq!(secrets.value(SECRET_A), None);
        assert_eq!(secrets.value(SECRET_B), None);

        assert_eq!(
            block_on(controller.set_api_key(SecretValue::new("key-b")))
                .expect("set destination key"),
            LlmApiKeyStatus::Configured
        );
        assert_eq!(secrets.value(SECRET_A), None);
        assert_eq!(secrets.value(SECRET_B).as_deref(), Some("key-b"));
    }

    #[test]
    fn first_endpoint_configuration_predeletes_any_destination_orphan() {
        let (controller, _settings, secrets, _provider, _active) =
            standard_controller(settings(1, None));
        secrets.put(SECRET_B, "orphan-key-b");

        let stored =
            block_on(controller.set_llm_settings(1, Some(llm_settings(ENDPOINT_B, "model-b"))))
                .expect("first endpoint configuration should succeed");

        assert_eq!(stored.llm().expect("new settings").base_url(), ENDPOINT_B);
        assert_eq!(secrets.value(SECRET_B), None);
    }

    #[test]
    fn destination_predelete_failure_preserves_current_settings_and_key() {
        let (controller, settings_store, secrets, _provider, _active) =
            standard_controller(settings(1, Some((ENDPOINT_A, "model-a"))));
        secrets.put(SECRET_A, "key-a");
        secrets.put(SECRET_B, "orphan-key-b");
        secrets.fail_delete_for(SECRET_B, test_error("secret.delete_failed"));

        let result =
            block_on(controller.set_llm_settings(1, Some(llm_settings(ENDPOINT_B, "model-b"))));

        assert!(matches!(
            result,
            Err(LlmConfigurationError::Port(PortError { ref code, .. }))
                if code == "secret.delete_failed"
        ));
        assert_eq!(
            settings_store
                .snapshot()
                .llm()
                .expect("old settings")
                .base_url(),
            ENDPOINT_A
        );
        assert_eq!(settings_store.replace_calls.load(Ordering::SeqCst), 0);
        assert_eq!(secrets.value(SECRET_A).as_deref(), Some("key-a"));
        assert_eq!(secrets.value(SECRET_B).as_deref(), Some("orphan-key-b"));
    }

    #[test]
    fn settings_cas_failure_after_destination_predelete_keeps_current_key() {
        let (controller, settings_store, secrets, _provider, _active) =
            standard_controller(settings(1, Some((ENDPOINT_A, "model-a"))));
        secrets.put(SECRET_A, "key-a");
        secrets.put(SECRET_B, "orphan-key-b");
        settings_store.fail_replace_with(test_error("settings.write_failed"));

        let result =
            block_on(controller.set_llm_settings(1, Some(llm_settings(ENDPOINT_B, "model-b"))));

        assert!(result.is_err());
        assert_eq!(
            settings_store
                .snapshot()
                .llm()
                .expect("old settings")
                .base_url(),
            ENDPOINT_A
        );
        assert_eq!(secrets.value(SECRET_A).as_deref(), Some("key-a"));
        assert_eq!(secrets.value(SECRET_B), None);
    }

    #[test]
    fn failed_old_key_cleanup_returns_new_settings_but_cannot_reactivate_orphan() {
        let (controller, settings_store, secrets, _provider, _active) =
            standard_controller(settings(1, Some((ENDPOINT_A, "model-a"))));
        secrets.put(SECRET_A, "key-a");
        secrets.fail_delete_for(SECRET_A, test_error("secret.delete_failed"));

        let stored =
            block_on(controller.set_llm_settings(1, Some(llm_settings(ENDPOINT_B, "model-b"))))
                .expect("durable endpoint switch should remain successful");

        assert_eq!(stored.llm().expect("new settings").base_url(), ENDPOINT_B);
        assert_eq!(secrets.value(SECRET_A).as_deref(), Some("key-a"));
        assert_eq!(secrets.value(SECRET_B), None);

        secrets.clear_delete_error(SECRET_A);
        secrets.put(SECRET_B, "key-b");
        let switched_back =
            block_on(controller.set_llm_settings(2, Some(llm_settings(ENDPOINT_A, "model-a"))))
                .expect("switching back should predelete the orphan");
        assert_eq!(
            switched_back.llm().expect("restored settings").base_url(),
            ENDPOINT_A
        );
        assert_eq!(secrets.value(SECRET_A), None);
        assert_eq!(secrets.value(SECRET_B), None);
        assert_eq!(settings_store.snapshot(), switched_back);
    }

    #[test]
    fn model_change_on_same_endpoint_preserves_api_key() {
        let (controller, _settings, secrets, _provider, _active) =
            standard_controller(settings(1, Some((ENDPOINT_A, "model-a"))));
        secrets.put(SECRET_A, "key-a");

        block_on(controller.set_llm_settings(1, Some(llm_settings(ENDPOINT_A, "model-b"))))
            .expect("model-only change should succeed");

        assert_eq!(secrets.value(SECRET_A).as_deref(), Some("key-a"));
        assert_eq!(secrets.calls.delete.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn atomic_namespace_replace_failure_preserves_existing_key() {
        let (controller, _settings, secrets, _provider, _active) =
            standard_controller(settings(1, Some((ENDPOINT_A, "model-a"))));
        secrets.put(SECRET_A, "old-key");
        secrets.fail_replace_namespace_with(test_error("secret.write_failed"));

        let result = block_on(controller.set_api_key(SecretValue::new("new-key")));

        assert!(result.is_err());
        assert_eq!(secrets.value(SECRET_A).as_deref(), Some("old-key"));
        assert_eq!(secrets.calls.replace.load(Ordering::SeqCst), 0);
        assert_eq!(secrets.calls.replace_namespace.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unavailable_provider_does_not_hide_or_prevent_deleting_current_secret() {
        let (controller, _settings, secrets, provider, _active) =
            standard_controller(settings(1, Some((ENDPOINT_A, "model-a"))));
        secrets.put(SECRET_A, "key-a");
        provider.set_resolve_behavior(ResolveBehavior::UnavailableWithRoute(test_error(
            "llm.client_unavailable",
        )));

        assert_eq!(
            block_on(controller.api_key_status()),
            LlmApiKeyStatus::Configured
        );
        assert_eq!(secrets.calls.read.load(Ordering::SeqCst), 0);
        assert_eq!(
            block_on(controller.reveal_api_key())
                .expect("configured secret should remain revealable")
                .expose(),
            "key-a"
        );
        assert_eq!(secrets.calls.read.load(Ordering::SeqCst), 1);
        assert_eq!(
            block_on(controller.delete_api_key()).expect("delete current secret"),
            LlmApiKeyStatus::NotConfigured
        );
        assert_eq!(secrets.value(SECRET_A), None);
    }

    #[test]
    fn api_key_status_authenticates_with_inspect_and_never_reads_plaintext() {
        let (controller, _settings, secrets, provider, _active) =
            standard_controller(settings(1, Some((ENDPOINT_A, "model-a"))));
        secrets.put(SECRET_A, "key-a");
        provider.set_resolve_behavior(ResolveBehavior::UnavailableWithRoute(test_error(
            "llm.provider_unavailable",
        )));

        assert_eq!(
            block_on(controller.api_key_status()),
            LlmApiKeyStatus::Configured
        );
        assert_eq!(secrets.calls.inspect_store.load(Ordering::SeqCst), 1);
        assert_eq!(secrets.calls.inspect.load(Ordering::SeqCst), 1);
        assert_eq!(secrets.calls.read.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn api_key_status_surfaces_store_wide_recovery_without_reading_plaintext() {
        let (controller, _settings, secrets, provider, _active) =
            standard_controller(settings(1, Some((ENDPOINT_A, "model-a"))));
        secrets.put(SECRET_A, "unreadable-ciphertext-placeholder");
        secrets.set_store_result(Ok(SecretMaterialState::RecoveryRequired));

        assert_eq!(
            block_on(controller.api_key_status()),
            LlmApiKeyStatus::RecoveryRequired
        );
        assert_eq!(secrets.calls.inspect_store.load(Ordering::SeqCst), 1);
        assert_eq!(secrets.calls.inspect.load(Ordering::SeqCst), 0);
        assert_eq!(secrets.calls.read.load(Ordering::SeqCst), 0);
        assert_eq!(provider.resolve_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn read_only_status_is_not_blocked_by_long_running_connection_test() {
        let (controller, _settings, secrets, provider, _active) =
            standard_controller(settings(1, Some((ENDPOINT_A, "model-a"))));
        secrets.put(SECRET_A, "key-a");
        let (process_started, release_process) = provider.install_process_barrier();
        let connection_controller = Arc::clone(&controller);
        let connection = thread::spawn(move || block_on(connection_controller.test_connection()));
        process_started
            .recv_timeout(Duration::from_secs(1))
            .expect("connection test should reach Provider");

        assert_eq!(
            block_on(controller.api_key_status()),
            LlmApiKeyStatus::Configured
        );
        assert!(block_on(controller.is_llm_ready()));

        release_process.send(()).expect("release Provider");
        assert_eq!(
            connection.join().expect("connection thread"),
            LlmConnectionTestOutcome::Succeeded
        );
    }

    #[test]
    fn connection_test_uses_only_provider_with_fixed_non_user_content() {
        let (controller, settings_store, secrets, provider, _active) =
            standard_controller(settings(1, Some((ENDPOINT_A, "model-a"))));

        assert_eq!(
            block_on(controller.test_connection()),
            LlmConnectionTestOutcome::Succeeded
        );

        let captured = provider.captured();
        assert_eq!(captured.len(), 1);
        let (route, request) = &captured[0];
        assert_eq!(route.endpoint(), ENDPOINT_A);
        assert_eq!(request.processing_mode, ProcessingMode::Faithful);
        assert_eq!(request.raw_transcript, CONNECTION_TEST_TEXT);
        assert_eq!(request.selected_text, None);
        assert_eq!(settings_store.replace_calls.load(Ordering::SeqCst), 0);
        assert_eq!(secrets.calls.inspect.load(Ordering::SeqCst), 0);
        assert_eq!(secrets.calls.inspect_store.load(Ordering::SeqCst), 0);
        assert_eq!(secrets.calls.read.load(Ordering::SeqCst), 0);
        assert_eq!(secrets.calls.replace.load(Ordering::SeqCst), 0);
        assert_eq!(secrets.calls.replace_namespace.load(Ordering::SeqCst), 0);
        assert_eq!(secrets.calls.delete.load(Ordering::SeqCst), 0);
        assert_eq!(provider.process_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.cancel_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn connection_test_maps_provider_errors_and_rejects_invalid_successes() {
        let (controller, _settings, _secrets, provider, _active) =
            standard_controller(settings(1, Some((ENDPOINT_A, "model-a"))));
        let cases = [
            ("llm.api_key_missing", LlmConnectionFailure::NotConfigured),
            (
                "llm.secret_unavailable",
                LlmConnectionFailure::SecretUnavailable,
            ),
            (
                "llm.secret_recovery_required",
                LlmConnectionFailure::RecoveryRequired,
            ),
            (
                "llm.invalid_config",
                LlmConnectionFailure::InvalidConfiguration,
            ),
            ("llm.invalid_request", LlmConnectionFailure::Internal),
            (
                "llm.authentication_failed",
                LlmConnectionFailure::AuthenticationFailed,
            ),
            (
                "llm.permission_denied",
                LlmConnectionFailure::PermissionDenied,
            ),
            ("llm.rate_limited", LlmConnectionFailure::RateLimited),
            ("llm.timeout", LlmConnectionFailure::Timeout),
            ("llm.network", LlmConnectionFailure::Network),
            (
                "llm.provider_unavailable",
                LlmConnectionFailure::ProviderUnavailable,
            ),
            (
                "llm.client_unavailable",
                LlmConnectionFailure::ProviderUnavailable,
            ),
            (
                "llm.request_rejected",
                LlmConnectionFailure::RequestRejected,
            ),
            (
                "llm.invalid_response",
                LlmConnectionFailure::InvalidResponse,
            ),
            (
                "llm.response_too_large",
                LlmConnectionFailure::ResponseTooLarge,
            ),
            ("llm.cancelled", LlmConnectionFailure::Cancelled),
            ("llm.unknown", LlmConnectionFailure::Internal),
        ];

        for (code, expected) in cases {
            provider.set_process_behavior(ProcessBehavior::Error(test_error(code)));
            assert_eq!(
                block_on(controller.test_connection()),
                LlmConnectionTestOutcome::Failed(expected),
                "unexpected mapping for {code}"
            );
        }

        provider.set_process_behavior(ProcessBehavior::Error(test_error(
            "llm.invalid_canonical_response",
        )));
        assert_eq!(
            block_on(controller.test_connection()),
            LlmConnectionTestOutcome::Succeeded,
            "a valid non-empty assistant response proves OpenAI-Compatible connectivity"
        );

        provider.set_process_behavior(ProcessBehavior::Empty);
        assert_eq!(
            block_on(controller.test_connection()),
            LlmConnectionTestOutcome::Failed(LlmConnectionFailure::InvalidResponse)
        );
        provider.set_process_behavior(ProcessBehavior::WrongIdentity);
        assert_eq!(
            block_on(controller.test_connection()),
            LlmConnectionTestOutcome::Failed(LlmConnectionFailure::InvalidResponse)
        );

        provider.set_process_behavior(ProcessBehavior::Error(test_error(
            "llm.authentication_failed",
        )));
        provider.set_probe_upstream(LlmUpstreamError::new(
            401,
            r#"{"error":"upstream credential policy"}"#,
            false,
        ));
        let diagnostic = block_on(controller.test_connection());
        assert_eq!(
            diagnostic,
            LlmConnectionTestOutcome::FailedWithUpstream {
                failure: LlmConnectionFailure::AuthenticationFailed,
                upstream: LlmUpstreamError::new(
                    401,
                    r#"{"error":"upstream credential policy"}"#,
                    false,
                ),
            }
        );
        assert!(!format!("{diagnostic:?}").contains("upstream credential policy"));
    }

    #[test]
    fn connection_test_distinguishes_busy_runtime_and_settings_failures() {
        let (controller, settings_store, _secrets, _provider, active) =
            standard_controller(settings(1, Some((ENDPOINT_A, "model-a"))));
        active.store(true, Ordering::SeqCst);
        assert_eq!(
            block_on(controller.test_connection()),
            LlmConnectionTestOutcome::Failed(LlmConnectionFailure::Busy)
        );
        active.store(false, Ordering::SeqCst);
        settings_store.fail_load_with(test_error("settings.unavailable"));
        assert_eq!(
            block_on(controller.test_connection()),
            LlmConnectionTestOutcome::Failed(LlmConnectionFailure::SettingsUnavailable)
        );

        let runtime_controller = Arc::new(LlmConfigurationController {
            settings: settings_store,
            secrets: Arc::new(MockSecretStore::default()),
            llm: Arc::new(MockLlmProvider::new(ResolveBehavior::Ready)),
            configuration_gate: Arc::new(AsyncMutex::new(())),
            active_work: Arc::new(|| Err(OrchestratorError::RuntimeLockPoisoned)),
            operations: AsyncMutex::new(()),
        });
        assert_eq!(
            block_on(runtime_controller.test_connection()),
            LlmConnectionTestOutcome::Failed(LlmConnectionFailure::RuntimeUnavailable)
        );
    }

    #[test]
    fn unavailable_route_errors_are_classified_before_any_network_request() {
        let (controller, _settings, secrets, provider, _active) =
            standard_controller(settings(1, Some((ENDPOINT_A, "model-a"))));
        provider.set_resolve_behavior(ResolveBehavior::UnavailableWithRoute(test_error(
            "llm.network",
        )));

        assert_eq!(
            block_on(controller.test_connection()),
            LlmConnectionTestOutcome::Failed(LlmConnectionFailure::NotConfigured)
        );
        secrets.put(SECRET_A, "key-a");
        assert_eq!(
            block_on(controller.test_connection()),
            LlmConnectionTestOutcome::Failed(LlmConnectionFailure::Network)
        );
        secrets.set_inspect_result(SECRET_A, Ok(SecretMaterialState::RecoveryRequired));
        assert_eq!(
            block_on(controller.test_connection()),
            LlmConnectionTestOutcome::Failed(LlmConnectionFailure::RecoveryRequired)
        );
        provider.set_resolve_behavior(ResolveBehavior::UnavailableWithoutRoute(test_error(
            "llm.invalid_config",
        )));
        assert_eq!(
            block_on(controller.test_connection()),
            LlmConnectionTestOutcome::Failed(LlmConnectionFailure::InvalidConfiguration)
        );
        assert_eq!(provider.process_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn missing_route_is_not_configured_without_calling_provider_process() {
        let (controller, _settings, _secrets, provider, _active) =
            standard_controller(settings(1, Some((ENDPOINT_A, "model-a"))));
        provider.set_resolve_behavior(ResolveBehavior::MissingSecret);

        assert_eq!(
            block_on(controller.test_connection()),
            LlmConnectionTestOutcome::Failed(LlmConnectionFailure::NotConfigured)
        );
        assert_eq!(provider.process_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn api_key_validation_accepts_only_bearer_header_safe_visible_ascii() {
        for valid in ["sk-test", "abc.DEF_123-~+/="] {
            assert!(validate_secret_value(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            " ",
            " leading",
            "trailing ",
            "line\nbreak",
            "tab\tvalue",
            "非 ASCII",
        ] {
            assert!(
                matches!(
                    validate_secret_value(invalid),
                    Err(LlmConfigurationError::InvalidSecret)
                ),
                "{invalid:?}"
            );
        }
        assert!(matches!(
            validate_secret_value(&"a".repeat(MAX_API_KEY_BYTES + 1)),
            Err(LlmConfigurationError::InvalidSecret)
        ));
    }
}
