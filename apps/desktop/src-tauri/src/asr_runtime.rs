//! ASR Worker runtime initialization for desktop application.
//!
//! This module is responsible for:
//! - Resolving the Worker executable path within the application bundle
//! - Setting up App Group shared data paths (macOS)
//! - Creating the audio artifact registry
//! - Creating and starting the ASR Worker Adapter
//! - Managing the Worker lifecycle

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::Duration;

use futures::lock::Mutex as AsyncMutex;
use remtene_adapters::asr_worker::{AsrWorkerAdapter, AudioArtifactResolver, WorkerLaunchConfig};
use remtene_adapters::audio_resolver;
use remtene_application::ports::{
    AsrEnginePort, AsrModelControlPort, AsrModelPreparationError, AsrRequest, AsrResult,
    EngineHealth, PortError, PortFuture,
};
use remtene_contracts::{AppError, ErrorCategory, ErrorSeverity};
use remtene_domain::{AsrEngine, RequestId};
use tauri::Manager;

/// Model identities the Worker announces during the handshake.
///
/// A package only reaches the Worker after `models/active` yields a manifest whose
/// SHA-256 matches, so these constants never widen what the Worker may load.
pub(crate) const DEFAULT_QWEN_MODEL_ID: &str = "qwen3-asr-0.6b-v1";
pub(crate) const DEFAULT_WHISPER_MODEL_ID: &str = "whisper-large-v3-turbo-q5_0-v1";

/// Production Worker budgets for a user-triggered cold start.
///
/// Health is intentionally much longer than the warm inference target:
/// it loads and prewarms the selected model while audio capture is already
/// running. The first Whisper Metal pipeline has exceeded ten seconds in the
/// pinned runtime, so the generic adapter's two-second test-friendly defaults
/// would incorrectly consume the user's first recording trigger.
#[cfg(target_os = "macos")]
const MACOS_WORKER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(target_os = "macos")]
const MACOS_WORKER_HEALTH_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(target_os = "macos")]
const MACOS_WORKER_CANCEL_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(target_os = "macos")]
const MACOS_WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Persistent release identity for macOS.
///
/// ADR-0008 keeps these pre-RemTene values as legacy ABI so existing OS
/// permissions and App Group models remain available. Autostart is migrated
/// separately because its LaunchAgent is keyed by display name and executable path.
/// These values must match the Tauri configuration and signing scripts.
#[cfg(target_os = "macos")]
mod macos_identity {
    /// Legacy main application Bundle ID.
    #[cfg(test)]
    pub const LEGACY_MAIN_BUNDLE_ID: &str = "io.github.TheLearnerL.bard";

    /// Legacy ASR Worker Helper Bundle ID.
    #[allow(dead_code)] // Will be used for signing verification in the future
    pub const LEGACY_WORKER_BUNDLE_ID: &str = "io.github.TheLearnerL.bard.asr-worker";

    /// App Group frozen by the selected formal or ad-hoc build flavor.
    pub const APP_GROUP_ID: &str = env!("REMTENE_COMPILED_MACOS_APP_GROUP_ID");

    /// Return the exact identity embedded into the main and Helper entitlements.
    pub fn app_group_id() -> String {
        APP_GROUP_ID.to_owned()
    }
}

/// Return the App Group identity frozen into this exact desktop build.
///
/// UI commands use this accessor instead of accepting a group or path from the
/// Renderer, so ad-hoc and formally signed builds always resolve their own
/// isolated model container.
#[cfg(target_os = "macos")]
pub(crate) fn compiled_macos_app_group_id() -> &'static str {
    macos_identity::APP_GROUP_ID
}

/// Resolve the Worker executable path within the application bundle.
///
/// On macOS, the Worker is packaged as:
/// `Contents/Helpers/RemTeneASRWorker.app/Contents/MacOS/remtene-asr-worker`
#[cfg(target_os = "macos")]
fn resolve_worker_executable(app_handle: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    let resource_path = app_handle.path().resource_dir().map_err(|_| {
        AppError::new(
            "asr.worker.path_unavailable",
            ErrorCategory::Asr,
            ErrorSeverity::Error,
            false,
            "errors.asr.worker_unavailable",
        )
    })?;

    // Navigate from Resources up to Contents, then into Helpers
    let contents = resource_path.parent().ok_or_else(|| {
        AppError::new(
            "asr.worker.invalid_bundle_structure",
            ErrorCategory::Asr,
            ErrorSeverity::Error,
            false,
            "errors.asr.worker_unavailable",
        )
    })?;

    let worker_path = contents
        .join("Helpers")
        .join("RemTeneASRWorker.app")
        .join("Contents")
        .join("MacOS")
        .join("remtene-asr-worker");

    if !worker_path.exists() {
        return Err(AppError::new(
            "asr.worker.executable_missing",
            ErrorCategory::Asr,
            ErrorSeverity::Error,
            false,
            "errors.asr.worker_unavailable",
        ));
    }

    Ok(worker_path)
}

/// A model package that passed identity, shape and SHA-256 verification in `models/active`.
#[cfg(target_os = "macos")]
#[derive(Clone, Debug, Eq, PartialEq)]
struct VerifiedModel {
    model_id: String,
    version: String,
    path: PathBuf,
    integrity_fingerprint: String,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, Eq, PartialEq)]
enum ModelPackageIssue {
    Missing,
    HashMismatch,
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct VerifiedModels {
    qwen: Option<VerifiedModel>,
    whisper: Option<VerifiedModel>,
}

#[cfg(target_os = "macos")]
struct ModelScan {
    qwen: Result<VerifiedModel, ModelPackageIssue>,
    whisper: Result<VerifiedModel, ModelPackageIssue>,
}

#[cfg(target_os = "macos")]
impl ModelScan {
    fn scan(active_root: &Path) -> Self {
        Self {
            qwen: resolve_model(
                active_root,
                DEFAULT_QWEN_MODEL_ID,
                remtene_platform::model_manifest::ModelEngine::Qwen,
            ),
            whisper: resolve_model(
                active_root,
                DEFAULT_WHISPER_MODEL_ID,
                remtene_platform::model_manifest::ModelEngine::Whisper,
            ),
        }
    }

    fn target(&self, engine: AsrEngine) -> Result<&VerifiedModel, ModelPackageIssue> {
        match engine {
            AsrEngine::Qwen => self.qwen.as_ref().map_err(Clone::clone),
            AsrEngine::Whisper => self.whisper.as_ref().map_err(Clone::clone),
        }
    }

    fn verified(&self) -> VerifiedModels {
        VerifiedModels {
            qwen: self.qwen.as_ref().ok().cloned(),
            whisper: self.whisper.as_ref().ok().cloned(),
        }
    }
}

#[cfg(target_os = "macos")]
fn resolve_model(
    active_root: &Path,
    model_id: &str,
    engine: remtene_platform::model_manifest::ModelEngine,
) -> Result<VerifiedModel, ModelPackageIssue> {
    use remtene_platform::{
        model_manifest::ManifestError,
        model_registry::{ModelRegistry, RegistryError},
    };

    let mut registry = ModelRegistry::new();
    let entry = registry
        .load_model(active_root, model_id, engine, true)
        .map_err(|error| match error {
            RegistryError::Manifest(ManifestError::PackageHashMismatch { .. }) => {
                ModelPackageIssue::HashMismatch
            }
            _ => ModelPackageIssue::Missing,
        })?;
    let integrity_fingerprint = entry.manifest.package_sha256.clone().unwrap_or_else(|| {
        entry
            .manifest
            .package_files
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|file| format!("{}:{}", file.path, file.sha256))
            .collect::<Vec<_>>()
            .join("|")
    });
    Ok(VerifiedModel {
        model_id: entry.manifest.model_id,
        version: entry.manifest.version,
        path: entry.package_path,
        integrity_fingerprint,
    })
}

#[cfg(target_os = "macos")]
struct MacWorkerContext {
    worker_executable: PathBuf,
    app_group_id: String,
    shared_root: PathBuf,
    active_root: PathBuf,
    resolver: AudioArtifactResolver,
}

#[cfg(target_os = "macos")]
struct MacRuntimeState {
    worker: Arc<AsrWorkerAdapter>,
    models: VerifiedModels,
}

/// Stable ASR Port identity that can rebuild an idle Worker after the user adds or replaces a
/// verified model package. Application gates guarantee that reconfiguration never races a Session.
#[cfg(target_os = "macos")]
pub struct MacAsrRuntime {
    context: MacWorkerContext,
    state: RwLock<MacRuntimeState>,
    initial_scan: StdMutex<Option<ModelScan>>,
    reconfiguration: AsyncMutex<()>,
}

#[cfg(target_os = "macos")]
impl MacAsrRuntime {
    fn current_worker(&self) -> Arc<AsrWorkerAdapter> {
        Arc::clone(
            &self
                .state
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .worker,
        )
    }

    async fn prepare_model(&self, engine: AsrEngine) -> Result<(), AsrModelPreparationError> {
        let _exclusive = self.reconfiguration.lock().await;
        if let Some(initial_scan) = self
            .initial_scan
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            return initial_scan
                .target(engine)
                .map(|_| ())
                .map_err(preparation_error);
        }

        let (model_id, expected_engine) = match engine {
            AsrEngine::Qwen => (
                DEFAULT_QWEN_MODEL_ID,
                remtene_platform::model_manifest::ModelEngine::Qwen,
            ),
            AsrEngine::Whisper => (
                DEFAULT_WHISPER_MODEL_ID,
                remtene_platform::model_manifest::ModelEngine::Whisper,
            ),
        };
        let target = resolve_model(&self.context.active_root, model_id, expected_engine)
            .map_err(preparation_error)?;
        let mut models = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .models
            .clone();
        match engine {
            AsrEngine::Qwen => models.qwen = Some(target),
            AsrEngine::Whisper => models.whisper = Some(target),
        }
        if self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .models
            == models
        {
            return Ok(());
        }

        // Starting a Supervisor does not spawn or load the Worker process. The old process first
        // releases its resident model; the next Health call alone loads the selected target.
        let replacement = Arc::new(
            start_worker(&self.context, &models).map_err(AsrModelPreparationError::Runtime)?,
        );
        let previous = self.current_worker();
        previous
            .release_idle_resources()
            .await
            .map_err(AsrModelPreparationError::Runtime)?;
        *self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = MacRuntimeState {
            worker: replacement,
            models,
        };
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn preparation_error(issue: ModelPackageIssue) -> AsrModelPreparationError {
    match issue {
        ModelPackageIssue::Missing => AsrModelPreparationError::Missing,
        ModelPackageIssue::HashMismatch => AsrModelPreparationError::HashMismatch,
    }
}

#[cfg(target_os = "macos")]
impl AsrEnginePort for MacAsrRuntime {
    fn health(&self, engine: AsrEngine) -> PortFuture<'_, Result<EngineHealth, PortError>> {
        let worker = self.current_worker();
        Box::pin(async move { worker.health(engine).await })
    }

    fn transcribe(&self, request: AsrRequest) -> PortFuture<'_, Result<AsrResult, PortError>> {
        let worker = self.current_worker();
        Box::pin(async move { worker.transcribe(request).await })
    }

    fn cancel(&self, request_id: RequestId) -> PortFuture<'_, Result<(), PortError>> {
        let worker = self.current_worker();
        Box::pin(async move { worker.cancel(request_id).await })
    }
}

#[cfg(target_os = "macos")]
impl AsrModelControlPort for MacAsrRuntime {
    fn prepare(&self, engine: AsrEngine) -> PortFuture<'_, Result<(), AsrModelPreparationError>> {
        Box::pin(async move { self.prepare_model(engine).await })
    }
}

#[cfg(target_os = "macos")]
fn start_worker(
    context: &MacWorkerContext,
    models: &VerifiedModels,
) -> Result<AsrWorkerAdapter, PortError> {
    let qwen = models.qwen.as_ref();
    let whisper = models.whisper.as_ref();

    let core_version = env!("CARGO_PKG_VERSION");
    let mut config = WorkerLaunchConfig::new_sandboxed(
        &context.worker_executable,
        &context.app_group_id,
        &context.shared_root,
        core_version,
        qwen.map_or(DEFAULT_QWEN_MODEL_ID, |model| model.model_id.as_str()),
        whisper.map_or(DEFAULT_WHISPER_MODEL_ID, |model| model.model_id.as_str()),
    )?
    .with_timeouts(
        MACOS_WORKER_HANDSHAKE_TIMEOUT,
        MACOS_WORKER_HEALTH_TIMEOUT,
        MACOS_WORKER_CANCEL_TIMEOUT,
        MACOS_WORKER_SHUTDOWN_TIMEOUT,
    );

    if let Some(model) = qwen {
        config = config.with_qwen_model(&model.path, &model.version);
    }
    if let Some(model) = whisper {
        config = config.with_whisper_model(&model.path, &model.version);
    }
    AsrWorkerAdapter::start(config, Arc::clone(&context.resolver))
}

/// Initialize the stable, reconfigurable macOS ASR Runtime.
///
/// Invalid packages are omitted independently, so a broken spare model cannot disable the valid
/// selected model. The user-triggered prepare path reports the selected package's precise class.
#[cfg(target_os = "macos")]
pub fn initialize_macos_worker(
    app_handle: &tauri::AppHandle,
    capture: Arc<remtene_platform::audio::SafeAudioCapture>,
) -> Result<MacAsrRuntime, AppError> {
    let worker_executable = resolve_worker_executable(app_handle)?;
    let app_group_id = macos_identity::app_group_id();
    let shared_paths = remtene_platform::asr_shared_data::resolve_macos_app_group(&app_group_id)
        .map_err(|_| {
            AppError::new(
                "asr.worker.app_group_unavailable",
                ErrorCategory::Asr,
                ErrorSeverity::Error,
                false,
                "errors.asr.worker_unavailable",
            )
        })?;
    let context = MacWorkerContext {
        worker_executable,
        app_group_id,
        shared_root: shared_paths.root().to_path_buf(),
        active_root: shared_paths.active_models_root().to_path_buf(),
        resolver: audio_resolver::resolver_from_capture(capture),
    };
    let scan = ModelScan::scan(&context.active_root);
    let models = scan.verified();
    if models.qwen.is_none() && models.whisper.is_none() {
        eprintln!("⚠ no verified model package in models/active; ASR will report unavailable");
    }
    let worker = start_worker(&context, &models).map_err(|error| {
        AppError::new(
            "asr.worker.start_failed",
            ErrorCategory::Asr,
            ErrorSeverity::Error,
            true,
            format!("Failed to start ASR Worker: {}", error.code),
        )
    })?;
    Ok(MacAsrRuntime {
        context,
        state: RwLock::new(MacRuntimeState {
            worker: Arc::new(worker),
            models,
        }),
        // Initialization has just verified every discovered package. The first startup Health
        // consumes this exact result instead of hashing multi-gigabyte files a second time.
        initial_scan: StdMutex::new(Some(scan)),
        reconfiguration: AsyncMutex::new(()),
    })
}

/// Placeholder for Windows Worker initialization.
///
/// Windows Worker runtime is tracked by ASR-WIN-001 and is not yet implemented.
#[cfg(target_os = "windows")]
pub fn initialize_windows_worker(
    _app_handle: &tauri::AppHandle,
) -> Result<AsrWorkerAdapter, AppError> {
    Err(AppError::new(
        "asr.worker.windows_not_implemented",
        ErrorCategory::Asr,
        ErrorSeverity::Error,
        false,
        "errors.asr.worker_unavailable",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_app_group_id_uses_a_valid_injected_team_prefix() {
        let app_group_id = macos_identity::app_group_id();
        let (team_id, suffix) = app_group_id
            .split_once('.')
            .expect("App Group must contain a Team ID prefix");
        assert_eq!(team_id.len(), 10);
        assert!(
            team_id
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        );
        assert_eq!(suffix, "io.github.TheLearnerL.bard.asr");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_identity_constants_are_stable() {
        assert_eq!(
            macos_identity::LEGACY_MAIN_BUNDLE_ID,
            "io.github.TheLearnerL.bard"
        );
        assert_eq!(
            macos_identity::LEGACY_WORKER_BUNDLE_ID,
            "io.github.TheLearnerL.bard.asr-worker"
        );
        assert!(macos_identity::APP_GROUP_ID.ends_with(".io.github.TheLearnerL.bard.asr"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_worker_budgets_cover_recorded_cold_prewarm() {
        assert!(MACOS_WORKER_HANDSHAKE_TIMEOUT >= Duration::from_secs(10));
        assert!(MACOS_WORKER_HEALTH_TIMEOUT >= Duration::from_secs(60));
        assert!(MACOS_WORKER_CANCEL_TIMEOUT >= Duration::from_secs(2));
        assert!(MACOS_WORKER_SHUTDOWN_TIMEOUT >= Duration::from_secs(2));
    }
}
