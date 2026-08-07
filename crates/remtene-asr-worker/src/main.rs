use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use remtene_asr_worker::{
    CompositeEngineBackend, EngineBackend, UnavailableEngineBackend, WorkerRuntimeConfig,
    run_worker,
};

const ASR_COMPONENT_DIRECTORY: &str = "RemTene/ASR";
const GRANTS_DIRECTORY: &str = "grants";
const MODELS_DIRECTORY: &str = "models";
const ACTIVE_MODELS_DIRECTORY: &str = "active";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => {
            eprintln!("remtene-asr-worker stopped: {code}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), &'static str> {
    let mut arguments = parse_args()?;
    resolve_runtime_paths(&mut arguments)?;
    let artifact_root = arguments
        .artifact_root
        .take()
        .ok_or("artifact_root_unavailable")?;
    let metadata = fs::symlink_metadata(&artifact_root).map_err(|_| "artifact_root_unavailable")?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("artifact_root_unavailable");
    }

    let backend = select_backend(
        arguments.test_backend,
        arguments.qwen_model.clone(),
        arguments.whisper_model.clone(),
    )?;
    let config = WorkerRuntimeConfig::new(
        artifact_root,
        runtime_id(
            arguments.test_backend,
            arguments.qwen_model.is_some(),
            arguments.whisper_model.is_some(),
        ),
        format!("remtene-asr-worker-{}", env!("CARGO_PKG_VERSION")),
    )
    .map_err(|_| "invalid_worker_configuration")?;

    run_worker(io::stdin(), io::stdout(), config, backend).map_err(|_| "worker_runtime_failed")
}

#[derive(Clone, Copy)]
enum TestBackend {
    None,
    #[cfg(debug_assertions)]
    Deterministic,
    #[cfg(debug_assertions)]
    CrashOnTranscribe,
}

#[derive(Clone)]
struct QwenModelArguments {
    model_id: String,
    model_version: String,
    model_dir: PathBuf,
}

#[derive(Clone)]
struct WhisperModelArguments {
    model_id: String,
    model_version: String,
    model_file: PathBuf,
}

struct ParsedArguments {
    artifact_root: Option<PathBuf>,
    app_group_id: Option<String>,
    test_backend: TestBackend,
    qwen_model: Option<QwenModelArguments>,
    whisper_model: Option<WhisperModelArguments>,
}

fn parse_args() -> Result<ParsedArguments, &'static str> {
    let mut args = env::args_os().skip(1);
    let mut artifact_root = None;
    let mut app_group_id = None;
    #[cfg(debug_assertions)]
    let mut test_backend = TestBackend::None;
    #[cfg(not(debug_assertions))]
    let test_backend = TestBackend::None;
    let mut qwen_model_id = None;
    let mut qwen_model_version = None;
    let mut qwen_model_dir = None;
    let mut whisper_model_id = None;
    let mut whisper_model_version = None;
    let mut whisper_model_file = None;

    while let Some(argument) = args.next() {
        if argument == "--artifact-root" {
            artifact_root = Some(PathBuf::from(args.next().ok_or("missing_artifact_root")?));
        } else if argument == "--app-group-id" {
            app_group_id = Some(required_utf8_arg(&mut args, "missing_app_group_id")?);
        } else if argument == "--qwen-model-id" {
            qwen_model_id = Some(required_utf8_arg(&mut args, "missing_qwen_model_id")?);
        } else if argument == "--qwen-model-version" {
            qwen_model_version = Some(required_utf8_arg(&mut args, "missing_qwen_model_version")?);
        } else if argument == "--qwen-model-dir" {
            qwen_model_dir = Some(PathBuf::from(args.next().ok_or("missing_qwen_model_dir")?));
        } else if argument == "--whisper-model-id" {
            whisper_model_id = Some(required_utf8_arg(&mut args, "missing_whisper_model_id")?);
        } else if argument == "--whisper-model-version" {
            whisper_model_version = Some(required_utf8_arg(
                &mut args,
                "missing_whisper_model_version",
            )?);
        } else if argument == "--whisper-model-file" {
            whisper_model_file = Some(PathBuf::from(
                args.next().ok_or("missing_whisper_model_file")?,
            ));
        } else if argument == "--deterministic-test-backend" {
            #[cfg(debug_assertions)]
            {
                test_backend = TestBackend::Deterministic;
            }
            #[cfg(not(debug_assertions))]
            return Err("unsupported_argument");
        } else if argument == "--crash-on-transcribe-test-backend" {
            #[cfg(debug_assertions)]
            {
                test_backend = TestBackend::CrashOnTranscribe;
            }
            #[cfg(not(debug_assertions))]
            return Err("unsupported_argument");
        } else {
            return Err("unsupported_argument");
        }
    }

    let qwen_model = match (qwen_model_id, qwen_model_version, qwen_model_dir) {
        (None, None, None) => None,
        (Some(model_id), Some(model_version), Some(model_dir)) => Some(QwenModelArguments {
            model_id,
            model_version,
            model_dir,
        }),
        _ => return Err("incomplete_qwen_model_configuration"),
    };
    let whisper_model = match (whisper_model_id, whisper_model_version, whisper_model_file) {
        (None, None, None) => None,
        (Some(model_id), Some(model_version), Some(model_file)) => Some(WhisperModelArguments {
            model_id,
            model_version,
            model_file,
        }),
        _ => return Err("incomplete_whisper_model_configuration"),
    };
    if !matches!(test_backend, TestBackend::None)
        && (qwen_model.is_some() || whisper_model.is_some())
    {
        return Err("test_backend_rejects_model_configuration");
    }
    if artifact_root.is_some() == app_group_id.is_some()
        || app_group_id
            .as_deref()
            .is_some_and(|identifier| !valid_app_group_identifier(identifier))
    {
        return Err("invalid_data_root_configuration");
    }
    if artifact_root.is_some() && !unrestricted_artifact_root_allowed() {
        return Err("artifact_root_disabled");
    }

    Ok(ParsedArguments {
        artifact_root,
        app_group_id,
        test_backend,
        qwen_model,
        whisper_model,
    })
}

fn unrestricted_artifact_root_allowed() -> bool {
    !cfg!(target_os = "macos") || cfg!(debug_assertions)
}

fn resolve_runtime_paths(arguments: &mut ParsedArguments) -> Result<(), &'static str> {
    let Some(app_group_id) = arguments.app_group_id.as_deref() else {
        return Ok(());
    };
    let shared_root = resolve_macos_app_group_root(app_group_id)?;
    apply_shared_root(arguments, &shared_root)
}

#[cfg(target_os = "macos")]
fn resolve_macos_app_group_root(app_group_id: &str) -> Result<PathBuf, &'static str> {
    use objc2_foundation::{NSFileManager, NSString};

    let manager = NSFileManager::defaultManager();
    let identifier = NSString::from_str(app_group_id);
    let container = manager
        .containerURLForSecurityApplicationGroupIdentifier(&identifier)
        .ok_or("app_group_unavailable")?;
    let container_path = container
        .path()
        .map(|path| PathBuf::from(path.to_string()))
        .ok_or("app_group_unavailable")?;
    let canonical_container =
        validate_directory(&container_path).map_err(|_| "app_group_unavailable")?;
    Ok(canonical_container.join(ASR_COMPONENT_DIRECTORY))
}

#[cfg(not(target_os = "macos"))]
fn resolve_macos_app_group_root(_app_group_id: &str) -> Result<PathBuf, &'static str> {
    Err("app_group_unsupported")
}

fn apply_shared_root(
    arguments: &mut ParsedArguments,
    shared_root: &Path,
) -> Result<(), &'static str> {
    let canonical_root = validate_directory(shared_root)?;
    if canonical_root != shared_root {
        return Err("shared_data_path_rejected");
    }

    let grants = shared_root.join(GRANTS_DIRECTORY);
    let canonical_grants = validate_directory(&grants)?;
    if canonical_grants != grants || !canonical_grants.starts_with(&canonical_root) {
        return Err("shared_data_path_rejected");
    }

    let models_root = shared_root.join(MODELS_DIRECTORY);
    let canonical_models_root = validate_directory(&models_root)?;
    let active_root = validate_directory(&models_root.join(ACTIVE_MODELS_DIRECTORY))?;
    if canonical_models_root != models_root
        || !canonical_models_root.starts_with(&canonical_root)
        || active_root != models_root.join(ACTIVE_MODELS_DIRECTORY)
        || !active_root.starts_with(&canonical_models_root)
    {
        return Err("shared_data_path_rejected");
    }
    if let Some(model) = arguments.qwen_model.as_ref() {
        validate_model_path(&model.model_dir, true, &active_root)?;
    }
    if let Some(model) = arguments.whisper_model.as_ref() {
        validate_model_path(&model.model_file, false, &active_root)?;
    }
    arguments.artifact_root = Some(grants);
    Ok(())
}

fn validate_directory(path: &Path) -> Result<PathBuf, &'static str> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "shared_data_path_rejected")?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || !path.is_absolute() {
        return Err("shared_data_path_rejected");
    }
    fs::canonicalize(path).map_err(|_| "shared_data_path_rejected")
}

fn validate_model_path(
    path: &Path,
    expect_directory: bool,
    active_root: &Path,
) -> Result<(), &'static str> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "shared_data_path_rejected")?;
    if metadata.file_type().is_symlink()
        || (expect_directory && !metadata.is_dir())
        || (!expect_directory && !metadata.is_file())
    {
        return Err("shared_data_path_rejected");
    }
    let canonical = fs::canonicalize(path).map_err(|_| "shared_data_path_rejected")?;
    if !canonical.starts_with(active_root) {
        return Err("shared_data_path_rejected");
    }

    let relative = path
        .strip_prefix(active_root)
        .map_err(|_| "shared_data_path_rejected")?;
    let mut current = active_root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err("shared_data_path_rejected");
        };
        current.push(component);
        let component_metadata =
            fs::symlink_metadata(&current).map_err(|_| "shared_data_path_rejected")?;
        if component_metadata.file_type().is_symlink() {
            return Err("shared_data_path_rejected");
        }
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

fn required_utf8_arg(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    error: &'static str,
) -> Result<String, &'static str> {
    args.next()
        .ok_or(error)?
        .into_string()
        .map_err(|_| "argument_must_be_utf8")
}

fn runtime_id(
    test_backend: TestBackend,
    qwen_configured: bool,
    whisper_configured: bool,
) -> &'static str {
    match test_backend {
        TestBackend::None if qwen_configured && whisper_configured => {
            "qwen_asr_0_9_0+whisper_rs_0_14_3"
        }
        TestBackend::None if qwen_configured => "qwen_asr_0_9_0",
        TestBackend::None if whisper_configured => "whisper_rs_0_14_3",
        TestBackend::None => "runtime_pending",
        #[cfg(debug_assertions)]
        TestBackend::Deterministic => "deterministic_test",
        #[cfg(debug_assertions)]
        TestBackend::CrashOnTranscribe => "crash_on_transcribe_test",
    }
}

fn select_backend(
    test_backend: TestBackend,
    qwen_model: Option<QwenModelArguments>,
    whisper_model: Option<WhisperModelArguments>,
) -> Result<Arc<dyn EngineBackend>, &'static str> {
    match test_backend {
        TestBackend::None => production_backend(qwen_model, whisper_model),
        #[cfg(debug_assertions)]
        TestBackend::Deterministic => Ok(Arc::new(remtene_asr_worker::DeterministicTestBackend)),
        #[cfg(debug_assertions)]
        TestBackend::CrashOnTranscribe => {
            Ok(Arc::new(remtene_asr_worker::CrashOnTranscribeTestBackend))
        }
    }
}

#[cfg(target_os = "macos")]
fn production_backend(
    qwen_model: Option<QwenModelArguments>,
    whisper_model: Option<WhisperModelArguments>,
) -> Result<Arc<dyn EngineBackend>, &'static str> {
    if qwen_model.is_none() && whisper_model.is_none() {
        return Ok(Arc::new(UnavailableEngineBackend));
    }
    let qwen_backend = qwen_model
        .map(|model| {
            let config = remtene_asr_worker::QwenEngineConfig::new(
                model.model_id,
                model.model_version,
                model.model_dir,
            )
            .map_err(|_| "invalid_qwen_model_configuration")?;
            Ok::<Arc<dyn EngineBackend>, &'static str>(
                remtene_asr_worker::QwenEngineBackend::start(config, Duration::from_secs(5 * 60)),
            )
        })
        .transpose()?;
    let whisper_backend = configured_whisper_backend(whisper_model)?;
    Ok(CompositeEngineBackend::start(qwen_backend, whisper_backend))
}

#[cfg(all(target_os = "macos", feature = "whisper-runtime"))]
fn configured_whisper_backend(
    model: Option<WhisperModelArguments>,
) -> Result<Option<Arc<dyn EngineBackend>>, &'static str> {
    model
        .map(|model| {
            let config = remtene_asr_worker::WhisperEngineConfig::new(
                model.model_id,
                model.model_version,
                model.model_file,
            )
            .map_err(|_| "invalid_whisper_model_configuration")?;
            Ok::<Arc<dyn EngineBackend>, &'static str>(
                remtene_asr_worker::WhisperEngineBackend::start(
                    config,
                    Duration::from_secs(5 * 60),
                ),
            )
        })
        .transpose()
}

#[cfg(all(target_os = "macos", not(feature = "whisper-runtime")))]
fn configured_whisper_backend(
    model: Option<WhisperModelArguments>,
) -> Result<Option<Arc<dyn EngineBackend>>, &'static str> {
    if let Some(model) = model {
        let _rejected_configuration = (model.model_id, model.model_version, model.model_file);
        Err("whisper_runtime_not_linked")
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn unrestricted_artifact_root_is_disabled_in_macos_release_builds() {
        assert_eq!(
            unrestricted_artifact_root_allowed(),
            !cfg!(target_os = "macos") || cfg!(debug_assertions)
        );
    }

    #[test]
    fn shared_root_derives_grants_and_accepts_active_models() {
        let root = test_shared_root("accepted");
        let qwen = root.join("models/active/qwen/version");
        let whisper = root.join("models/active/whisper/model.bin");
        fs::create_dir_all(&qwen).expect("create Qwen model");
        fs::create_dir_all(whisper.parent().expect("Whisper parent"))
            .expect("create Whisper model parent");
        fs::write(&whisper, b"model").expect("write Whisper model");
        let mut arguments = arguments_with_models(qwen, whisper);

        apply_shared_root(&mut arguments, &root).expect("apply valid shared root");
        assert_eq!(arguments.artifact_root, Some(root.join("grants")));
        fs::remove_dir_all(root).expect("remove shared root");
    }

    #[test]
    fn shared_root_rejects_a_model_outside_the_container() {
        let root = test_shared_root("outside");
        let outside = root.with_extension("outside");
        let whisper = root.join("models/active/whisper/model.bin");
        fs::create_dir_all(&outside).expect("create outside Qwen model");
        fs::create_dir_all(whisper.parent().expect("Whisper parent"))
            .expect("create Whisper model parent");
        fs::write(&whisper, b"model").expect("write Whisper model");
        let mut arguments = arguments_with_models(outside.clone(), whisper);

        assert_eq!(
            apply_shared_root(&mut arguments, &root),
            Err("shared_data_path_rejected")
        );
        fs::remove_dir_all(root).expect("remove shared root");
        fs::remove_dir_all(outside).expect("remove outside model");
    }

    #[test]
    fn shared_root_rejects_a_candidate_model_before_activation() {
        let root = test_shared_root("candidate");
        let qwen = root.join("models/candidates/qwen/version");
        let whisper = root.join("models/active/whisper/model.bin");
        fs::create_dir_all(&qwen).expect("create candidate Qwen model");
        fs::create_dir_all(whisper.parent().expect("Whisper parent"))
            .expect("create active Whisper model parent");
        fs::write(&whisper, b"model").expect("write Whisper model");
        let mut arguments = arguments_with_models(qwen, whisper);

        assert_eq!(
            apply_shared_root(&mut arguments, &root),
            Err("shared_data_path_rejected")
        );
        fs::remove_dir_all(root).expect("remove shared root");
    }

    #[cfg(unix)]
    #[test]
    fn shared_root_rejects_an_intermediate_model_symlink() {
        use std::os::unix::fs::symlink;

        let root = test_shared_root("intermediate-symlink");
        let real = root.join("models/active/real/qwen/version");
        let linked_parent = root.join("models/active/linked");
        let whisper = root.join("models/active/whisper/model.bin");
        fs::create_dir_all(&real).expect("create real Qwen model");
        symlink(root.join("models/active/real"), &linked_parent)
            .expect("create intermediate symlink");
        fs::create_dir_all(whisper.parent().expect("Whisper parent"))
            .expect("create Whisper model parent");
        fs::write(&whisper, b"model").expect("write Whisper model");
        let mut arguments = arguments_with_models(linked_parent.join("qwen/version"), whisper);

        assert_eq!(
            apply_shared_root(&mut arguments, &root),
            Err("shared_data_path_rejected")
        );
        fs::remove_dir_all(root).expect("remove shared root");
    }

    #[cfg(unix)]
    #[test]
    fn shared_root_rejects_a_symlinked_model_directory() {
        use std::os::unix::fs::symlink;

        let root = test_shared_root("symlink");
        let outside = root.with_extension("outside");
        let qwen = root.join("models/active/qwen");
        let whisper = root.join("models/active/whisper/model.bin");
        fs::create_dir_all(&outside).expect("create outside model");
        fs::create_dir_all(qwen.parent().expect("Qwen parent")).expect("create Qwen parent");
        symlink(&outside, &qwen).expect("create Qwen symlink");
        fs::create_dir_all(whisper.parent().expect("Whisper parent"))
            .expect("create Whisper model parent");
        fs::write(&whisper, b"model").expect("write Whisper model");
        let mut arguments = arguments_with_models(qwen, whisper);

        assert_eq!(
            apply_shared_root(&mut arguments, &root),
            Err("shared_data_path_rejected")
        );
        fs::remove_dir_all(root).expect("remove shared root");
        fs::remove_dir_all(outside).expect("remove outside model");
    }

    fn test_shared_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "remtene-worker-shared-{label}-{}",
            Uuid::new_v4().hyphenated()
        ));
        for directory in [
            root.join("grants"),
            root.join("models/active"),
            root.join("models/candidates"),
        ] {
            fs::create_dir_all(directory).expect("create shared layout");
        }
        fs::canonicalize(root).expect("canonicalize shared layout")
    }

    fn arguments_with_models(qwen: PathBuf, whisper: PathBuf) -> ParsedArguments {
        ParsedArguments {
            artifact_root: None,
            app_group_id: Some("TEAM123456.remtene.asr".to_owned()),
            test_backend: TestBackend::None,
            qwen_model: Some(QwenModelArguments {
                model_id: "qwen".to_owned(),
                model_version: "version".to_owned(),
                model_dir: qwen,
            }),
            whisper_model: Some(WhisperModelArguments {
                model_id: "whisper".to_owned(),
                model_version: "version".to_owned(),
                model_file: whisper,
            }),
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn production_backend(
    qwen_model: Option<QwenModelArguments>,
    whisper_model: Option<WhisperModelArguments>,
) -> Result<Arc<dyn EngineBackend>, &'static str> {
    if qwen_model.is_some() || whisper_model.is_some() {
        Err("asr_runtime_not_supported_on_platform")
    } else {
        Ok(Arc::new(UnavailableEngineBackend))
    }
}
