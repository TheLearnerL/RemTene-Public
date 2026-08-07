//! Safe ownership and artifact lifecycle for platform microphone capture.
//!
//! The concrete macOS backend is intentionally injected. A future CPAL/CoreAudio
//! implementation only has to implement [`AudioBackend`] and must keep its
//! real-time callback non-blocking. Paths never cross the application Port: the
//! [`AudioCaptureRef`] and [`AudioRef`] values are canonical UUID strings backed
//! by this module's private registry.

#[cfg(target_os = "macos")]
mod macos_coreaudio;

#[cfg(target_os = "macos")]
pub use macos_coreaudio::{
    MacOsCoreAudioBackend, Pcm16WavWriterFactory, create_default_macos_audio_capture,
};

use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU8, Ordering},
    },
};

use remtene_application::ports::{
    AUDIO_EMPTY_CAPTURE_CODE, AudioCapture, AudioCaptureRef, AudioFormat, AudioRef, FinalizedAudio,
    PortError, PortFuture,
};
use remtene_domain::SessionId;

const PARTIAL_SUFFIX: &str = ".wav.partial";
const FINAL_SUFFIX: &str = ".wav";

/// Canonical local artifact consumed by both Whisper and Qwen ASR workers.
pub const CANONICAL_ASR_AUDIO_FORMAT: AudioFormat = AudioFormat {
    sample_rate_hz: 16_000,
    channels: 1,
    bits_per_sample: 16,
};

/// A non-blocking sample destination owned by an injected writer pipeline.
///
/// A real CoreAudio callback must only call this method and return. The sink is
/// expected to enqueue into a preallocated bounded buffer; disk I/O, allocation,
/// resampling, and synchronization with the writer thread belong elsewhere.
pub trait AudioFrameSink: Send + Sync {
    fn try_write(&self, interleaved_pcm16: &[i16]) -> Result<(), FrameSinkError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameSinkError {
    Overflow,
    Closed,
    WriteFailed,
}

/// A thread-safe, content-free fault channel for a native audio callback.
#[derive(Clone, Default)]
pub struct CaptureFaultReporter {
    state: Arc<AtomicU8>,
}

impl CaptureFaultReporter {
    pub fn report_overflow(&self) {
        self.record(CaptureFault::Overflow);
    }

    pub fn report_device_error(&self) {
        self.record(CaptureFault::DeviceError);
    }

    fn record(&self, fault: CaptureFault) {
        let _ = self.state.compare_exchange(
            CaptureFault::None as u8,
            fault as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn current(&self) -> CaptureFault {
        match self.state.load(Ordering::Acquire) {
            value if value == CaptureFault::Overflow as u8 => CaptureFault::Overflow,
            value if value == CaptureFault::DeviceError as u8 => CaptureFault::DeviceError,
            _ => CaptureFault::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum CaptureFault {
    None = 0,
    Overflow = 1,
    DeviceError = 2,
}

struct GuardedFrameSink {
    inner: Arc<dyn AudioFrameSink>,
    faults: CaptureFaultReporter,
}

impl AudioFrameSink for GuardedFrameSink {
    fn try_write(&self, interleaved_pcm16: &[i16]) -> Result<(), FrameSinkError> {
        let result = self.inner.try_write(interleaved_pcm16);
        if let Err(error) = result {
            match error {
                FrameSinkError::Overflow => self.faults.report_overflow(),
                FrameSinkError::Closed | FrameSinkError::WriteFailed => {
                    self.faults.report_device_error();
                }
            }
        }
        result
    }
}

pub struct BackendStartRequest {
    pub session_id: SessionId,
    pub format: AudioFormat,
    pub sink: Arc<dyn AudioFrameSink>,
    pub faults: CaptureFaultReporter,
}

/// Type-safe result of attempting to start a native microphone backend.
///
/// Only [`BackendStartOutcome::Started`] means audio capture actually started.
/// A failed start either proves that no native resource remains, or transfers
/// its cleanup handle to [`SafeAudioCapture`] for an internally owned retry.
pub enum BackendStartOutcome {
    Started(Box<dyn AudioBackendHandle>),
    FailedClean(PortError),
    FailedOwned {
        error: PortError,
        handle: Box<dyn AudioBackendHandle>,
    },
}

/// Native microphone backend contract.
///
/// A backend must never disguise a failed native start as [`BackendStartOutcome::Started`].
/// [`BackendStartOutcome::FailedClean`] is valid only after proving that no
/// native handle remains. [`BackendStartOutcome::FailedOwned`] transfers that
/// handle to the caller, which must retain it until `stop` proves release.
/// `AudioBackendHandle::stop` returning `Ok` is the only proof that its handle
/// is closed. A stop error must leave the handle valid for a later retry.
pub trait AudioBackend: Send + Sync {
    /// Resolves the canonical format for the next recording.
    ///
    /// Fixed-format backends may use the configured fallback. Device-backed
    /// implementations must resolve this again for every start so a new
    /// default input is never paired with a stale WAV format.
    fn capture_format(&self, configured_fallback: AudioFormat) -> Result<AudioFormat, PortError> {
        Ok(configured_fallback)
    }

    fn start(&self, request: BackendStartRequest) -> BackendStartOutcome;
}

pub trait AudioBackendHandle: Send {
    fn stop(&mut self) -> Result<(), PortError>;
}

pub struct WriterPipeline {
    pub sink: Arc<dyn AudioFrameSink>,
    pub writer: Box<dyn AudioArtifactWriter>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioWriterRequest {
    pub session_id: SessionId,
    pub source_format: AudioFormat,
    pub target_format: AudioFormat,
}

impl WriterPipeline {
    #[must_use]
    pub fn new(sink: Arc<dyn AudioFrameSink>, writer: Box<dyn AudioArtifactWriter>) -> Self {
        Self { sink, writer }
    }
}

/// Creates a writer for a Core-owned `.partial` path.
///
/// Returning `Err` must not leave an open writer handle. The adapter still
/// attempts to delete a partial file in case creation failed after touching it.
pub trait AudioWriterFactory: Send + Sync {
    fn create(
        &self,
        partial_path: &Path,
        request: AudioWriterRequest,
    ) -> Result<WriterPipeline, PortError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriterSummary {
    pub frames_written: u64,
}

/// Control side of a writer pipeline. Both operations are retryable ownership
/// boundaries: an error must leave the writer valid for the same operation.
pub trait AudioArtifactWriter: Send {
    fn finalize(&mut self) -> Result<WriterSummary, PortError>;
    fn abort(&mut self) -> Result<(), PortError>;
}

/// Rust-only resolution result for the future local ASR Worker adapter.
///
/// This type must never be serialized or exposed to a Renderer.
#[derive(Clone, Eq, PartialEq)]
pub struct ResolvedAudioArtifact {
    path: PathBuf,
    pub format: AudioFormat,
    pub duration_ms: u64,
}

impl ResolvedAudioArtifact {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Platform-neutral lifecycle core used by the macOS capture adapter.
///
/// It deliberately contains no CPAL or Objective-C imports so its failure and
/// concurrency behavior can be proven with deterministic fakes.
pub struct SafeAudioCapture {
    backend: Arc<dyn AudioBackend>,
    writers: Arc<dyn AudioWriterFactory>,
    artifact_root: PathBuf,
    format: AudioFormat,
    registry: Mutex<ArtifactRegistry>,
}

impl SafeAudioCapture {
    /// Creates the dedicated artifact root and removes files left by a prior
    /// abnormal exit before accepting any new recording.
    pub fn initialize(
        artifact_root: impl Into<PathBuf>,
        format: AudioFormat,
        backend: Arc<dyn AudioBackend>,
        writers: Arc<dyn AudioWriterFactory>,
    ) -> Result<Self, PortError> {
        validate_format(format)?;
        let artifact_root = artifact_root.into();
        prepare_artifact_root(&artifact_root)?;
        cleanup_stale_audio_artifacts(&artifact_root)?;
        Ok(Self {
            backend,
            writers,
            artifact_root,
            format,
            registry: Mutex::new(ArtifactRegistry::default()),
        })
    }

    #[must_use]
    pub fn active_capture_count(&self) -> usize {
        self.registry().map_or(0, |registry| registry.open_handles)
    }

    #[must_use]
    pub fn finalized_artifact_count(&self) -> usize {
        self.registry()
            .map_or(0, |registry| registry.artifacts.len())
    }

    /// Resolves an opaque audio ID inside Rust Core after validating that the
    /// registered file is still a regular, non-symlink file.
    pub fn resolve_artifact(
        &self,
        audio_ref: &AudioRef,
    ) -> Result<Option<ResolvedAudioArtifact>, PortError> {
        validate_opaque_uuid(audio_ref.as_str())?;
        let registry = self.registry()?;
        let Some(artifact) = registry.artifacts.get(audio_ref.as_str()) else {
            return Ok(None);
        };
        ensure_owned_regular_file(&artifact.path)?;
        Ok(Some(ResolvedAudioArtifact {
            path: artifact.path.clone(),
            format: artifact.format,
            duration_ms: artifact.duration_ms,
        }))
    }

    fn start_sync(&self, session_id: SessionId) -> Result<AudioCaptureRef, PortError> {
        prepare_artifact_root(&self.artifact_root)?;
        let id = session_id.to_string();
        validate_opaque_uuid(&id)?;

        let mut registry = self.registry()?;
        retry_orphan_cleanup(&mut registry)?;
        if registry.captures.contains_key(&id) || registry.artifacts.contains_key(&id) {
            return Err(port_error(
                "audio.capture_exists",
                "errors.audio.capture_exists",
                false,
            ));
        }

        let partial_path = self.artifact_root.join(format!("{id}{PARTIAL_SUFFIX}"));
        let final_path = self.artifact_root.join(format!("{id}{FINAL_SUFFIX}"));
        ensure_path_absent(&partial_path)?;
        ensure_path_absent(&final_path)?;

        let source_format = match self.backend.capture_format(self.format) {
            Ok(format) => format,
            Err(error) => {
                crate::trace::delivery(
                    "audio.format",
                    "不可用",
                    &format!("当前默认输入格式解析失败（{}）", error.code),
                );
                return Err(error);
            }
        };
        validate_format(source_format)?;
        let target_format = self.format;
        crate::trace::delivery(
            "audio.format",
            "已解析",
            &format!(
                "source_sample_rate={} source_channels={} source_bits={} target_sample_rate={} target_channels={} target_bits={}",
                source_format.sample_rate_hz,
                source_format.channels,
                source_format.bits_per_sample,
                target_format.sample_rate_hz,
                target_format.channels,
                target_format.bits_per_sample,
            ),
        );

        let pipeline = match self.writers.create(
            &partial_path,
            AudioWriterRequest {
                session_id,
                source_format,
                target_format,
            },
        ) {
            Ok(pipeline) => pipeline,
            Err(error) => {
                remember_if_cleanup_fails(&mut registry, &partial_path);
                return Err(error);
            }
        };

        let faults = CaptureFaultReporter::default();
        let guarded_sink: Arc<dyn AudioFrameSink> = Arc::new(GuardedFrameSink {
            inner: Arc::clone(&pipeline.sink),
            faults: faults.clone(),
        });
        let handle = match self.backend.start(BackendStartRequest {
            session_id,
            format: source_format,
            sink: guarded_sink,
            faults: faults.clone(),
        }) {
            BackendStartOutcome::Started(handle) => handle,
            BackendStartOutcome::FailedClean(error) => {
                let WriterPipeline {
                    sink: _,
                    mut writer,
                } = pipeline;
                if writer.abort().is_err() {
                    registry.orphans.push(OrphanRecord {
                        path: partial_path,
                        handle: None,
                        writer: Some(writer),
                    });
                } else {
                    drop(writer);
                    remember_if_cleanup_fails(&mut registry, &partial_path);
                }
                return Err(error);
            }
            BackendStartOutcome::FailedOwned { error, handle } => {
                let WriterPipeline { sink: _, writer } = pipeline;
                registry.orphans.push(OrphanRecord {
                    path: partial_path,
                    handle: Some(handle),
                    writer: Some(writer),
                });
                return Err(error);
            }
        };

        registry.open_handles += 1;
        registry.captures.insert(
            id.clone(),
            CaptureRecord {
                partial_path,
                final_path,
                format: target_format,
                handle: Some(handle),
                writer: Some(pipeline.writer),
                faults,
                finalized_summary: None,
            },
        );
        Ok(AudioCaptureRef::new(id))
    }

    fn finish_sync(&self, capture: AudioCaptureRef) -> Result<FinalizedAudio, PortError> {
        let id = capture.as_str().to_owned();
        validate_opaque_uuid(&id)?;
        let mut registry = self.registry()?;

        let stopped_now = {
            let record = registry.captures.get_mut(&id).ok_or_else(|| {
                port_error(
                    "audio.capture_not_found",
                    "errors.audio.capture_not_found",
                    false,
                )
            })?;
            stop_backend(record)?
        };
        if stopped_now {
            registry.open_handles = registry.open_handles.saturating_sub(1);
        }

        let record = registry
            .captures
            .get_mut(&id)
            .expect("capture exists while finish owns registry lock");
        match record.faults.current() {
            CaptureFault::None => {}
            CaptureFault::Overflow => {
                return Err(port_error(
                    "audio.capture_overflow",
                    "errors.audio.capture_overflow",
                    false,
                ));
            }
            CaptureFault::DeviceError => {
                return Err(port_error(
                    "audio.device_failed",
                    "errors.audio.device_failed",
                    true,
                ));
            }
        }

        if record.finalized_summary.is_none() {
            let summary = record
                .writer
                .as_mut()
                .expect("unfinished capture retains writer ownership")
                .finalize()?;
            record.writer = None;
            record.finalized_summary = Some(summary);
        }
        let summary = record
            .finalized_summary
            .expect("successful finalization records summary");
        if summary.frames_written == 0 {
            return Err(port_error(
                AUDIO_EMPTY_CAPTURE_CODE,
                "errors.audio.empty_capture",
                false,
            ));
        }

        rename_owned_artifact(&record.partial_path, &record.final_path)?;
        let duration_ms = duration_ms(summary.frames_written, record.format.sample_rate_hz);
        let record = registry
            .captures
            .remove(&id)
            .expect("capture remains registered until rename succeeds");
        registry.artifacts.insert(
            id.clone(),
            FinalizedArtifactRecord {
                path: record.final_path,
                format: record.format,
                duration_ms,
            },
        );

        Ok(FinalizedAudio {
            audio_ref: AudioRef::new(id),
            format: record.format,
            duration_ms,
        })
    }

    fn cancel_sync(&self, capture: AudioCaptureRef) -> Result<(), PortError> {
        let id = capture.as_str().to_owned();
        validate_opaque_uuid(&id)?;
        let mut registry = self.registry()?;
        let Some(record) = registry.captures.get_mut(&id) else {
            return Ok(());
        };

        let stopped_now = stop_backend(record).map_err(as_retryable)?;
        if stopped_now {
            registry.open_handles = registry.open_handles.saturating_sub(1);
        }
        let record = registry
            .captures
            .get_mut(&id)
            .expect("capture remains registered during cancellation");
        if let Some(writer) = record.writer.as_mut() {
            writer.abort().map_err(as_retryable)?;
            record.writer = None;
        }
        remove_owned_file(&record.partial_path).map_err(as_retryable)?;
        registry.captures.remove(&id);
        Ok(())
    }

    fn cleanup_sync(&self, audio_ref: AudioRef) -> Result<(), PortError> {
        let id = audio_ref.as_str().to_owned();
        validate_opaque_uuid(&id)?;
        let mut registry = self.registry()?;
        let Some(artifact) = registry.artifacts.get(&id) else {
            return Ok(());
        };
        remove_owned_file(&artifact.path).map_err(as_retryable)?;
        registry.artifacts.remove(&id);
        Ok(())
    }

    fn registry(&self) -> Result<MutexGuard<'_, ArtifactRegistry>, PortError> {
        self.registry.lock().map_err(|_| {
            port_error(
                "audio.registry_unavailable",
                "errors.audio.registry_unavailable",
                true,
            )
        })
    }
}

impl AudioCapture for SafeAudioCapture {
    fn start(&self, session_id: SessionId) -> PortFuture<'_, Result<AudioCaptureRef, PortError>> {
        Box::pin(async move { self.start_sync(session_id) })
    }

    fn finish(
        &self,
        capture: AudioCaptureRef,
    ) -> PortFuture<'_, Result<FinalizedAudio, PortError>> {
        Box::pin(async move { self.finish_sync(capture) })
    }

    fn cancel(&self, capture: AudioCaptureRef) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move { self.cancel_sync(capture) })
    }

    fn cleanup(&self, audio_ref: AudioRef) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move { self.cleanup_sync(audio_ref) })
    }
}

#[derive(Default)]
struct ArtifactRegistry {
    captures: HashMap<String, CaptureRecord>,
    artifacts: HashMap<String, FinalizedArtifactRecord>,
    orphans: Vec<OrphanRecord>,
    open_handles: usize,
}

struct OrphanRecord {
    path: PathBuf,
    handle: Option<Box<dyn AudioBackendHandle>>,
    writer: Option<Box<dyn AudioArtifactWriter>>,
}

struct CaptureRecord {
    partial_path: PathBuf,
    final_path: PathBuf,
    format: AudioFormat,
    handle: Option<Box<dyn AudioBackendHandle>>,
    writer: Option<Box<dyn AudioArtifactWriter>>,
    faults: CaptureFaultReporter,
    finalized_summary: Option<WriterSummary>,
}

struct FinalizedArtifactRecord {
    path: PathBuf,
    format: AudioFormat,
    duration_ms: u64,
}

fn stop_backend(record: &mut CaptureRecord) -> Result<bool, PortError> {
    let Some(handle) = record.handle.as_mut() else {
        return Ok(false);
    };
    handle.stop()?;
    record.handle = None;
    Ok(true)
}

fn retry_orphan_cleanup(registry: &mut ArtifactRegistry) -> Result<(), PortError> {
    let orphans = std::mem::take(&mut registry.orphans);
    let mut remaining = Vec::new();
    let mut first_error = None;
    for mut orphan in orphans {
        if let Some(handle) = orphan.handle.as_mut() {
            if let Err(error) = handle.stop() {
                first_error.get_or_insert_with(|| as_retryable(error));
                remaining.push(orphan);
                continue;
            }
            orphan.handle = None;
        }
        if let Some(writer) = orphan.writer.as_mut()
            && let Err(error) = writer.abort()
        {
            first_error.get_or_insert_with(|| as_retryable(error));
            remaining.push(orphan);
            continue;
        }
        orphan.writer = None;
        if let Err(error) = remove_owned_file(&orphan.path) {
            first_error.get_or_insert(error);
            remaining.push(orphan);
        }
    }
    registry.orphans = remaining;
    first_error.map_or(Ok(()), Err)
}

fn remember_if_cleanup_fails(registry: &mut ArtifactRegistry, path: &Path) {
    if remove_owned_file(path).is_err() {
        registry.orphans.push(OrphanRecord {
            path: path.to_path_buf(),
            handle: None,
            writer: None,
        });
    }
}

fn validate_format(format: AudioFormat) -> Result<(), PortError> {
    if format.sample_rate_hz == 0 || format.channels == 0 || format.bits_per_sample == 0 {
        Err(port_error(
            "audio.invalid_format",
            "errors.audio.invalid_format",
            false,
        ))
    } else {
        Ok(())
    }
}

fn duration_ms(frames_written: u64, sample_rate_hz: u32) -> u64 {
    frames_written.saturating_mul(1_000) / u64::from(sample_rate_hz)
}

fn prepare_artifact_root(root: &Path) -> Result<(), PortError> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(unsafe_artifact_error());
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(root).map_err(artifact_io_error)?;
        }
        Err(error) => return Err(artifact_io_error(error)),
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).map_err(artifact_io_error)?;
    }
    Ok(())
}

/// Deletes only canonical UUID-named, direct-child regular audio files.
/// Unknown entries, directories, and symlinks fail closed without being followed.
pub fn cleanup_stale_audio_artifacts(root: &Path) -> Result<(), PortError> {
    prepare_artifact_root(root)?;
    let entries = fs::read_dir(root).map_err(artifact_io_error)?;
    for entry in entries {
        let entry = entry.map_err(artifact_io_error)?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| unsafe_artifact_error())?;
        if !is_owned_artifact_name(&name) {
            return Err(unsafe_artifact_error());
        }
        let file_type = entry.file_type().map_err(artifact_io_error)?;
        if file_type.is_symlink() || !file_type.is_file() {
            return Err(unsafe_artifact_error());
        }
        fs::remove_file(entry.path()).map_err(artifact_io_error)?;
    }
    Ok(())
}

fn is_owned_artifact_name(name: &str) -> bool {
    let id = name
        .strip_suffix(PARTIAL_SUFFIX)
        .or_else(|| name.strip_suffix(FINAL_SUFFIX));
    id.is_some_and(is_canonical_non_nil_uuid)
}

fn validate_opaque_uuid(value: &str) -> Result<(), PortError> {
    if is_canonical_non_nil_uuid(value) {
        Ok(())
    } else {
        Err(port_error(
            "audio.invalid_ref",
            "errors.audio.invalid_ref",
            false,
        ))
    }
}

fn is_canonical_non_nil_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36
        || ![8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
    {
        return false;
    }
    let mut saw_non_zero = false;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if [8, 13, 18, 23].contains(&index) {
            continue;
        }
        if !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte) {
            return false;
        }
        saw_non_zero |= byte != b'0';
    }
    saw_non_zero
}

fn ensure_path_absent(path: &Path) -> Result<(), PortError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(artifact_io_error(error)),
        Ok(_) => Err(unsafe_artifact_error()),
    }
}

fn ensure_owned_regular_file(path: &Path) -> Result<(), PortError> {
    let metadata = fs::symlink_metadata(path).map_err(artifact_io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        Err(unsafe_artifact_error())
    } else {
        Ok(())
    }
}

fn rename_owned_artifact(partial_path: &Path, final_path: &Path) -> Result<(), PortError> {
    ensure_owned_regular_file(partial_path)?;
    ensure_path_absent(final_path)?;
    fs::rename(partial_path, final_path).map_err(artifact_io_error)
}

fn remove_owned_file(path: &Path) -> Result<(), PortError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(artifact_io_error(error)),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(unsafe_artifact_error())
        }
        Ok(_) => fs::remove_file(path).map_err(artifact_io_error),
    }
}

fn as_retryable(mut error: PortError) -> PortError {
    error.retryable = true;
    error
}

fn artifact_io_error(_error: io::Error) -> PortError {
    port_error("audio.artifact_io", "errors.audio.artifact_io", true)
}

fn unsafe_artifact_error() -> PortError {
    port_error(
        "audio.artifact_unsafe",
        "errors.audio.artifact_unsafe",
        false,
    )
}

fn port_error(code: &str, safe_message_key: &str, retryable: bool) -> PortError {
    PortError {
        code: code.to_owned(),
        safe_message_key: safe_message_key.to_owned(),
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::Pin,
        sync::{
            Barrier,
            atomic::{AtomicU64, AtomicUsize},
        },
        task::{Context, Poll, Waker},
        thread,
    };

    use super::*;

    fn block_on<T>(mut future: Pin<Box<dyn Future<Output = T> + Send + '_>>) -> T {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("audio adapter test futures must complete synchronously"),
        }
    }

    fn test_format() -> AudioFormat {
        AudioFormat {
            sample_rate_hz: 16_000,
            channels: 1,
            bits_per_sample: 16,
        }
    }

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "remtene-audio-test-{}-{}",
                std::process::id(),
                SessionId::new()
            ));
            fs::create_dir_all(&path).expect("test directory must be created");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[derive(Default)]
    struct FakeBackendState {
        start_failures: AtomicUsize,
        retained_start_failures: AtomicUsize,
        stop_failures: AtomicUsize,
        starts: AtomicUsize,
        stops: AtomicUsize,
        start_formats: Mutex<Vec<AudioFormat>>,
        last_sink: Mutex<Option<Arc<dyn AudioFrameSink>>>,
        last_faults: Mutex<Option<CaptureFaultReporter>>,
    }

    #[derive(Clone, Default)]
    struct FakeBackend {
        state: Arc<FakeBackendState>,
    }

    impl FakeBackend {
        fn fail_next_start(&self) {
            self.state.start_failures.fetch_add(1, Ordering::SeqCst);
        }

        fn fail_next_start_with_owned_cleanup(&self) {
            self.state
                .retained_start_failures
                .fetch_add(1, Ordering::SeqCst);
        }

        fn fail_next_stop(&self) {
            self.state.stop_failures.fetch_add(1, Ordering::SeqCst);
        }

        fn report_device_error(&self) {
            self.state
                .last_faults
                .lock()
                .expect("fault lock")
                .as_ref()
                .expect("capture fault reporter")
                .report_device_error();
        }

        fn push_frame(&self) -> Result<(), FrameSinkError> {
            self.state
                .last_sink
                .lock()
                .expect("sink lock")
                .as_ref()
                .expect("capture sink")
                .try_write(&[1])
        }
    }

    impl AudioBackend for FakeBackend {
        fn start(&self, request: BackendStartRequest) -> BackendStartOutcome {
            self.state.starts.fetch_add(1, Ordering::SeqCst);
            self.state
                .start_formats
                .lock()
                .expect("start formats lock")
                .push(request.format);
            if consume_failure(&self.state.start_failures) {
                return BackendStartOutcome::FailedClean(test_port_error("audio.backend_start"));
            }
            if consume_failure(&self.state.retained_start_failures) {
                return BackendStartOutcome::FailedOwned {
                    error: test_port_error("audio.backend_start"),
                    handle: Box::new(FakeBackendHandle {
                        state: Arc::clone(&self.state),
                    }),
                };
            }
            *self.state.last_sink.lock().expect("sink lock") = Some(request.sink);
            *self.state.last_faults.lock().expect("fault lock") = Some(request.faults);
            BackendStartOutcome::Started(Box::new(FakeBackendHandle {
                state: Arc::clone(&self.state),
            }))
        }
    }

    #[derive(Clone)]
    struct SwitchingFormatBackend {
        backend: FakeBackend,
        current_format: Arc<Mutex<AudioFormat>>,
    }

    impl SwitchingFormatBackend {
        fn new(format: AudioFormat) -> Self {
            Self {
                backend: FakeBackend::default(),
                current_format: Arc::new(Mutex::new(format)),
            }
        }

        fn set_format(&self, format: AudioFormat) {
            *self.current_format.lock().expect("current format lock") = format;
        }
    }

    impl AudioBackend for SwitchingFormatBackend {
        fn capture_format(
            &self,
            _configured_fallback: AudioFormat,
        ) -> Result<AudioFormat, PortError> {
            Ok(*self.current_format.lock().expect("current format lock"))
        }

        fn start(&self, request: BackendStartRequest) -> BackendStartOutcome {
            self.backend.start(request)
        }
    }

    struct FakeBackendHandle {
        state: Arc<FakeBackendState>,
    }

    impl AudioBackendHandle for FakeBackendHandle {
        fn stop(&mut self) -> Result<(), PortError> {
            self.state.stops.fetch_add(1, Ordering::SeqCst);
            if consume_failure(&self.state.stop_failures) {
                Err(test_port_error("audio.backend_stop"))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Default)]
    struct FakeWriterState {
        creates: AtomicUsize,
        finalizes: AtomicUsize,
        aborts: AtomicUsize,
        finalize_failures: AtomicUsize,
        abort_failures: AtomicUsize,
        frames: AtomicU64,
        sink_error: AtomicU8,
        requests: Mutex<Vec<AudioWriterRequest>>,
    }

    #[derive(Clone, Default)]
    struct FakeWriterFactory {
        state: Arc<FakeWriterState>,
    }

    impl FakeWriterFactory {
        fn with_frames(frames: u64) -> Self {
            let factory = Self::default();
            factory.state.frames.store(frames, Ordering::SeqCst);
            factory
        }

        fn fail_next_finalize(&self) {
            self.state.finalize_failures.fetch_add(1, Ordering::SeqCst);
        }

        fn fail_next_abort(&self) {
            self.state.abort_failures.fetch_add(1, Ordering::SeqCst);
        }

        fn set_sink_error(&self, error: FrameSinkError) {
            let value = match error {
                FrameSinkError::Overflow => 1,
                FrameSinkError::Closed => 2,
                FrameSinkError::WriteFailed => 3,
            };
            self.state.sink_error.store(value, Ordering::SeqCst);
        }
    }

    impl AudioWriterFactory for FakeWriterFactory {
        fn create(
            &self,
            partial_path: &Path,
            request: AudioWriterRequest,
        ) -> Result<WriterPipeline, PortError> {
            self.state.creates.fetch_add(1, Ordering::SeqCst);
            self.state
                .requests
                .lock()
                .expect("writer requests lock")
                .push(request);
            fs::write(partial_path, b"partial audio").map_err(artifact_io_error)?;
            Ok(WriterPipeline::new(
                Arc::new(FakeSink {
                    state: Arc::clone(&self.state),
                }),
                Box::new(FakeWriter {
                    state: Arc::clone(&self.state),
                }),
            ))
        }
    }

    struct FakeSink {
        state: Arc<FakeWriterState>,
    }

    impl AudioFrameSink for FakeSink {
        fn try_write(&self, _interleaved_pcm16: &[i16]) -> Result<(), FrameSinkError> {
            match self.state.sink_error.load(Ordering::SeqCst) {
                1 => Err(FrameSinkError::Overflow),
                2 => Err(FrameSinkError::Closed),
                3 => Err(FrameSinkError::WriteFailed),
                _ => Ok(()),
            }
        }
    }

    struct FakeWriter {
        state: Arc<FakeWriterState>,
    }

    impl AudioArtifactWriter for FakeWriter {
        fn finalize(&mut self) -> Result<WriterSummary, PortError> {
            self.state.finalizes.fetch_add(1, Ordering::SeqCst);
            if consume_failure(&self.state.finalize_failures) {
                return Err(test_port_error("audio.writer_finalize"));
            }
            Ok(WriterSummary {
                frames_written: self.state.frames.load(Ordering::SeqCst),
            })
        }

        fn abort(&mut self) -> Result<(), PortError> {
            self.state.aborts.fetch_add(1, Ordering::SeqCst);
            if consume_failure(&self.state.abort_failures) {
                Err(test_port_error("audio.writer_abort"))
            } else {
                Ok(())
            }
        }
    }

    fn consume_failure(counter: &AtomicUsize) -> bool {
        counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                if value > 0 { Some(value - 1) } else { None }
            })
            .is_ok()
    }

    fn test_port_error(code: &str) -> PortError {
        port_error(code, "errors.audio.test", true)
    }

    fn adapter(
        directory: &TestDirectory,
        backend: &FakeBackend,
        writers: &FakeWriterFactory,
    ) -> SafeAudioCapture {
        SafeAudioCapture::initialize(
            directory.path(),
            test_format(),
            Arc::new(backend.clone()),
            Arc::new(writers.clone()),
        )
        .expect("audio adapter must initialize")
    }

    #[test]
    fn finish_closes_handle_atomically_renames_and_registers_artifact() {
        let directory = TestDirectory::new();
        let backend = FakeBackend::default();
        let writers = FakeWriterFactory::with_frames(16_000);
        let capture_adapter = adapter(&directory, &backend, &writers);
        let capture = block_on(capture_adapter.start(SessionId::new())).expect("start");
        assert!(is_canonical_non_nil_uuid(capture.as_str()));
        assert_eq!(capture_adapter.active_capture_count(), 1);
        assert!(
            directory
                .path()
                .join(format!("{}{PARTIAL_SUFFIX}", capture.as_str()))
                .is_file()
        );

        let audio = block_on(capture_adapter.finish(capture.clone())).expect("finish");
        assert_eq!(audio.audio_ref.as_str(), capture.as_str());
        assert_eq!(audio.duration_ms, 1_000);
        assert_eq!(capture_adapter.active_capture_count(), 0);
        assert_eq!(capture_adapter.finalized_artifact_count(), 1);
        let resolved = capture_adapter
            .resolve_artifact(&audio.audio_ref)
            .expect("resolve")
            .expect("registered artifact");
        assert!(resolved.path().is_file());
        assert!(resolved.path().to_string_lossy().ends_with(FINAL_SUFFIX));

        block_on(capture_adapter.cleanup(audio.audio_ref.clone())).expect("cleanup");
        block_on(capture_adapter.cleanup(audio.audio_ref)).expect("idempotent cleanup");
        assert_eq!(capture_adapter.finalized_artifact_count(), 0);
        assert_eq!(backend.state.stops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn zero_frame_finish_returns_the_benign_empty_capture_code_and_remains_cleanup_owned() {
        let directory = TestDirectory::new();
        let backend = FakeBackend::default();
        let writers = FakeWriterFactory::with_frames(0);
        let capture_adapter = adapter(&directory, &backend, &writers);
        let capture = block_on(capture_adapter.start(SessionId::new())).expect("start");
        let partial = directory
            .path()
            .join(format!("{}{PARTIAL_SUFFIX}", capture.as_str()));

        let error = block_on(capture_adapter.finish(capture.clone()))
            .expect_err("zero-frame capture has no ASR artifact");
        assert_eq!(error.code, AUDIO_EMPTY_CAPTURE_CODE);
        assert!(!error.retryable);
        assert_eq!(capture_adapter.active_capture_count(), 0);
        assert_eq!(capture_adapter.finalized_artifact_count(), 0);
        assert!(partial.is_file());

        block_on(capture_adapter.cancel(capture)).expect("cancel removes the retained partial");
        assert!(!partial.exists());
        assert_eq!(writers.state.finalizes.load(Ordering::SeqCst), 1);
        assert_eq!(backend.state.stops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn each_capture_uses_current_device_input_but_keeps_canonical_artifact_format() {
        let directory = TestDirectory::new();
        let initial = test_format();
        let switched = AudioFormat {
            sample_rate_hz: 48_000,
            channels: 1,
            bits_per_sample: 16,
        };
        let backend = SwitchingFormatBackend::new(initial);
        let writers = FakeWriterFactory::with_frames(16_000);
        let capture_adapter = SafeAudioCapture::initialize(
            directory.path(),
            initial,
            Arc::new(backend.clone()),
            Arc::new(writers.clone()),
        )
        .expect("audio adapter");

        let first = block_on(capture_adapter.start(SessionId::new())).expect("first device start");
        block_on(capture_adapter.cancel(first)).expect("cancel first device capture");

        backend.set_format(switched);
        let second =
            block_on(capture_adapter.start(SessionId::new())).expect("switched device start");
        let audio = block_on(capture_adapter.finish(second)).expect("finish switched capture");

        assert_eq!(audio.format, initial);
        assert_eq!(audio.duration_ms, 1_000);
        assert_eq!(
            *backend
                .backend
                .state
                .start_formats
                .lock()
                .expect("start formats lock"),
            vec![initial, switched]
        );
        assert_eq!(
            writers
                .state
                .requests
                .lock()
                .expect("writer requests lock")
                .iter()
                .map(|request| (request.source_format, request.target_format))
                .collect::<Vec<_>>(),
            vec![(initial, initial), (switched, initial)]
        );
        block_on(capture_adapter.cleanup(audio.audio_ref)).expect("cleanup switched artifact");
    }

    #[test]
    fn invalid_switched_format_fails_before_opening_and_the_next_start_recovers() {
        let directory = TestDirectory::new();
        let initial = test_format();
        let backend = SwitchingFormatBackend::new(AudioFormat {
            sample_rate_hz: 0,
            channels: 1,
            bits_per_sample: 16,
        });
        let writers = FakeWriterFactory::with_frames(10);
        let capture_adapter = SafeAudioCapture::initialize(
            directory.path(),
            initial,
            Arc::new(backend.clone()),
            Arc::new(writers.clone()),
        )
        .expect("audio adapter");

        let error = block_on(capture_adapter.start(SessionId::new()))
            .expect_err("invalid switched format must fail");
        assert_eq!(error.code, "audio.invalid_format");
        assert_eq!(backend.backend.state.starts.load(Ordering::SeqCst), 0);
        assert_eq!(writers.state.creates.load(Ordering::SeqCst), 0);

        backend.set_format(initial);
        let recovered =
            block_on(capture_adapter.start(SessionId::new())).expect("valid device recovers");
        block_on(capture_adapter.cancel(recovered)).expect("cancel recovered capture");
        assert_eq!(backend.backend.state.starts.load(Ordering::SeqCst), 1);
        assert_eq!(writers.state.creates.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancel_closes_aborts_deletes_and_is_idempotent() {
        let directory = TestDirectory::new();
        let backend = FakeBackend::default();
        let writers = FakeWriterFactory::with_frames(10);
        let capture_adapter = adapter(&directory, &backend, &writers);
        let capture = block_on(capture_adapter.start(SessionId::new())).expect("start");
        let partial = directory
            .path()
            .join(format!("{}{PARTIAL_SUFFIX}", capture.as_str()));

        block_on(capture_adapter.cancel(capture.clone())).expect("cancel");
        block_on(capture_adapter.cancel(capture)).expect("idempotent cancel");
        assert!(!partial.exists());
        assert_eq!(capture_adapter.active_capture_count(), 0);
        assert_eq!(capture_adapter.finalized_artifact_count(), 0);
        assert_eq!(backend.state.stops.load(Ordering::SeqCst), 1);
        assert_eq!(writers.state.aborts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stop_failure_retains_live_handle_until_cancel_retry_succeeds() {
        let directory = TestDirectory::new();
        let backend = FakeBackend::default();
        backend.fail_next_stop();
        let writers = FakeWriterFactory::with_frames(10);
        let capture_adapter = adapter(&directory, &backend, &writers);
        let capture = block_on(capture_adapter.start(SessionId::new())).expect("start");

        let error = block_on(capture_adapter.finish(capture.clone())).expect_err("stop fails");
        assert_eq!(error.code, "audio.backend_stop");
        assert_eq!(capture_adapter.active_capture_count(), 1);

        block_on(capture_adapter.cancel(capture)).expect("retry closes and cleans");
        assert_eq!(capture_adapter.active_capture_count(), 0);
        assert_eq!(backend.state.stops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn writer_failure_retains_stopped_capture_for_cancel_cleanup() {
        let directory = TestDirectory::new();
        let backend = FakeBackend::default();
        let writers = FakeWriterFactory::with_frames(10);
        writers.fail_next_finalize();
        let capture_adapter = adapter(&directory, &backend, &writers);
        let capture = block_on(capture_adapter.start(SessionId::new())).expect("start");
        let partial = directory
            .path()
            .join(format!("{}{PARTIAL_SUFFIX}", capture.as_str()));

        assert!(block_on(capture_adapter.finish(capture.clone())).is_err());
        assert_eq!(capture_adapter.active_capture_count(), 0);
        assert!(partial.is_file());
        block_on(capture_adapter.cancel(capture)).expect("cancel retained writer");
        assert!(!partial.exists());
        assert_eq!(writers.state.aborts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancel_abort_failure_retains_writer_and_partial_for_retry() {
        let directory = TestDirectory::new();
        let backend = FakeBackend::default();
        let writers = FakeWriterFactory::with_frames(10);
        writers.fail_next_abort();
        let capture_adapter = adapter(&directory, &backend, &writers);
        let capture = block_on(capture_adapter.start(SessionId::new())).expect("start");
        let partial = directory
            .path()
            .join(format!("{}{PARTIAL_SUFFIX}", capture.as_str()));

        let error = block_on(capture_adapter.cancel(capture.clone()))
            .expect_err("failed abort retains capture");
        assert!(error.retryable);
        assert!(partial.is_file());
        assert_eq!(capture_adapter.active_capture_count(), 0);

        block_on(capture_adapter.cancel(capture)).expect("abort retry succeeds");
        assert!(!partial.exists());
        assert_eq!(writers.state.aborts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn overflow_and_device_faults_fail_closed_before_finalization() {
        for fault in [FrameSinkError::Overflow, FrameSinkError::WriteFailed] {
            let directory = TestDirectory::new();
            let backend = FakeBackend::default();
            let writers = FakeWriterFactory::with_frames(10);
            writers.set_sink_error(fault);
            let capture_adapter = adapter(&directory, &backend, &writers);
            let capture = block_on(capture_adapter.start(SessionId::new())).expect("start");
            assert_eq!(backend.push_frame(), Err(fault));

            let error = block_on(capture_adapter.finish(capture.clone())).expect_err("fault fails");
            let expected = if fault == FrameSinkError::Overflow {
                "audio.capture_overflow"
            } else {
                "audio.device_failed"
            };
            assert_eq!(error.code, expected);
            assert_eq!(writers.state.finalizes.load(Ordering::SeqCst), 0);
            block_on(capture_adapter.cancel(capture)).expect("fault cleanup");
        }

        let directory = TestDirectory::new();
        let backend = FakeBackend::default();
        let writers = FakeWriterFactory::with_frames(10);
        let capture_adapter = adapter(&directory, &backend, &writers);
        let capture = block_on(capture_adapter.start(SessionId::new())).expect("start");
        backend.report_device_error();
        assert_eq!(
            block_on(capture_adapter.finish(capture.clone()))
                .expect_err("device error fails")
                .code,
            "audio.device_failed"
        );
        block_on(capture_adapter.cancel(capture)).expect("device cleanup");
    }

    #[test]
    fn rename_failure_keeps_capture_owned_and_retryable_by_cancel() {
        let directory = TestDirectory::new();
        let backend = FakeBackend::default();
        let writers = FakeWriterFactory::with_frames(10);
        let capture_adapter = adapter(&directory, &backend, &writers);
        let capture = block_on(capture_adapter.start(SessionId::new())).expect("start");
        let final_path = directory
            .path()
            .join(format!("{}{FINAL_SUFFIX}", capture.as_str()));
        fs::create_dir(&final_path).expect("collision directory");

        assert_eq!(
            block_on(capture_adapter.finish(capture.clone()))
                .expect_err("unsafe collision")
                .code,
            "audio.artifact_unsafe"
        );
        assert_eq!(capture_adapter.finalized_artifact_count(), 0);
        fs::remove_dir(final_path).expect("remove collision");
        block_on(capture_adapter.cancel(capture)).expect("cancel partial");
    }

    #[test]
    fn backend_start_failure_does_not_leave_audio_artifact() {
        let directory = TestDirectory::new();
        let backend = FakeBackend::default();
        backend.fail_next_start();
        let writers = FakeWriterFactory::with_frames(10);
        let capture_adapter = adapter(&directory, &backend, &writers);

        assert!(block_on(capture_adapter.start(SessionId::new())).is_err());
        assert_eq!(capture_adapter.active_capture_count(), 0);
        assert_eq!(
            fs::read_dir(directory.path()).expect("read root").count(),
            0
        );
    }

    #[test]
    fn failed_start_with_owned_cleanup_never_registers_a_capture() {
        let directory = TestDirectory::new();
        let backend = FakeBackend::default();
        backend.fail_next_start_with_owned_cleanup();
        let writers = FakeWriterFactory::with_frames(10);
        let capture_adapter = adapter(&directory, &backend, &writers);

        let error = block_on(capture_adapter.start(SessionId::new()))
            .expect_err("failed native start must not produce a capture reference");
        assert_eq!(error.code, "audio.backend_start");
        assert_eq!(capture_adapter.active_capture_count(), 0);
        assert_eq!(backend.state.starts.load(Ordering::SeqCst), 1);
        assert_eq!(backend.state.stops.load(Ordering::SeqCst), 0);
        assert_eq!(writers.state.aborts.load(Ordering::SeqCst), 0);
        assert_eq!(
            fs::read_dir(directory.path()).expect("read root").count(),
            1,
            "the retained writer artifact stays internally owned with the cleanup handle"
        );

        let capture = block_on(capture_adapter.start(SessionId::new()))
            .expect("the next start retries cleanup before opening a new capture");
        assert_eq!(backend.state.starts.load(Ordering::SeqCst), 2);
        assert_eq!(backend.state.stops.load(Ordering::SeqCst), 1);
        assert_eq!(writers.state.aborts.load(Ordering::SeqCst), 1);
        assert_eq!(capture_adapter.active_capture_count(), 1);
        block_on(capture_adapter.cancel(capture)).expect("cancel replacement capture");
    }

    #[test]
    fn failed_start_cleanup_error_retains_handle_and_blocks_new_start_until_retry() {
        let directory = TestDirectory::new();
        let backend = FakeBackend::default();
        backend.fail_next_start_with_owned_cleanup();
        backend.fail_next_stop();
        let writers = FakeWriterFactory::with_frames(10);
        let capture_adapter = adapter(&directory, &backend, &writers);

        assert!(block_on(capture_adapter.start(SessionId::new())).is_err());
        assert_eq!(capture_adapter.active_capture_count(), 0);

        let cleanup_error = block_on(capture_adapter.start(SessionId::new()))
            .expect_err("failed cleanup must block a second native start");
        assert_eq!(cleanup_error.code, "audio.backend_stop");
        assert!(cleanup_error.retryable);
        assert_eq!(backend.state.starts.load(Ordering::SeqCst), 1);
        assert_eq!(backend.state.stops.load(Ordering::SeqCst), 1);
        assert_eq!(writers.state.aborts.load(Ordering::SeqCst), 0);
        assert_eq!(capture_adapter.active_capture_count(), 0);

        let capture = block_on(capture_adapter.start(SessionId::new()))
            .expect("cleanup retry succeeds before opening a new capture");
        assert_eq!(backend.state.starts.load(Ordering::SeqCst), 2);
        assert_eq!(backend.state.stops.load(Ordering::SeqCst), 2);
        assert_eq!(writers.state.aborts.load(Ordering::SeqCst), 1);
        block_on(capture_adapter.cancel(capture)).expect("cancel replacement capture");
    }

    #[test]
    fn failed_start_retains_writer_orphan_and_blocks_until_cleanup_retry() {
        let directory = TestDirectory::new();
        let backend = FakeBackend::default();
        backend.fail_next_start();
        let writers = FakeWriterFactory::with_frames(10);
        writers.fail_next_abort();
        let capture_adapter = adapter(&directory, &backend, &writers);

        assert!(block_on(capture_adapter.start(SessionId::new())).is_err());
        assert_eq!(
            fs::read_dir(directory.path()).expect("read root").count(),
            1
        );

        let capture = block_on(capture_adapter.start(SessionId::new()))
            .expect("next start first retries retained orphan");
        assert_eq!(writers.state.aborts.load(Ordering::SeqCst), 2);
        assert_eq!(
            fs::read_dir(directory.path()).expect("read root").count(),
            1
        );
        block_on(capture_adapter.cancel(capture)).expect("cancel second capture");
    }

    #[test]
    fn startup_removes_only_valid_direct_child_artifacts() {
        let directory = TestDirectory::new();
        let stale_partial = directory
            .path()
            .join(format!("{}{PARTIAL_SUFFIX}", SessionId::new()));
        let stale_final = directory
            .path()
            .join(format!("{}{FINAL_SUFFIX}", SessionId::new()));
        fs::write(&stale_partial, b"partial").expect("write stale partial");
        fs::write(&stale_final, b"final").expect("write stale final");

        let backend = FakeBackend::default();
        let writers = FakeWriterFactory::with_frames(10);
        let _capture_adapter = adapter(&directory, &backend, &writers);
        assert!(!stale_partial.exists());
        assert!(!stale_final.exists());
    }

    #[cfg(unix)]
    #[test]
    fn startup_rejects_symlink_without_following_or_deleting_target() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        let outside = TestDirectory::new();
        let target = outside.path().join("must-survive.wav");
        fs::write(&target, b"outside").expect("write outside target");
        let link = directory
            .path()
            .join(format!("{}{FINAL_SUFFIX}", SessionId::new()));
        symlink(&target, &link).expect("create symlink");

        let result = SafeAudioCapture::initialize(
            directory.path(),
            test_format(),
            Arc::new(FakeBackend::default()),
            Arc::new(FakeWriterFactory::with_frames(10)),
        );
        let error = match result {
            Ok(_) => panic!("symlink must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.code, "audio.artifact_unsafe");
        assert!(target.is_file());
        assert!(link.is_symlink());
    }

    #[test]
    fn cleanup_refuses_replaced_directory_and_retains_registry_ownership() {
        let directory = TestDirectory::new();
        let backend = FakeBackend::default();
        let writers = FakeWriterFactory::with_frames(10);
        let capture_adapter = adapter(&directory, &backend, &writers);
        let capture = block_on(capture_adapter.start(SessionId::new())).expect("start");
        let audio = block_on(capture_adapter.finish(capture)).expect("finish");
        let artifact_path = capture_adapter
            .resolve_artifact(&audio.audio_ref)
            .expect("resolve")
            .expect("artifact")
            .path;
        fs::remove_file(&artifact_path).expect("remove artifact for tamper test");
        fs::create_dir(&artifact_path).expect("replace with directory");

        let error = block_on(capture_adapter.cleanup(audio.audio_ref.clone()))
            .expect_err("directory replacement must fail closed");
        assert_eq!(error.code, "audio.artifact_unsafe");
        assert_eq!(capture_adapter.finalized_artifact_count(), 1);
        fs::remove_dir(&artifact_path).expect("remove collision");
        block_on(capture_adapter.cleanup(audio.audio_ref)).expect("missing file is idempotent");
        assert_eq!(capture_adapter.finalized_artifact_count(), 0);
    }

    #[test]
    fn concurrent_finish_and_cancel_stop_the_native_handle_once() {
        let directory = TestDirectory::new();
        let backend = FakeBackend::default();
        let writers = FakeWriterFactory::with_frames(10);
        let capture_adapter = Arc::new(adapter(&directory, &backend, &writers));
        let capture = block_on(capture_adapter.start(SessionId::new())).expect("start");
        let barrier = Arc::new(Barrier::new(3));

        let finish_handle = {
            let capture_adapter = Arc::clone(&capture_adapter);
            let capture = capture.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                block_on(capture_adapter.finish(capture))
            })
        };
        let cancel_handle = {
            let capture_adapter = Arc::clone(&capture_adapter);
            let capture = capture.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                block_on(capture_adapter.cancel(capture))
            })
        };
        barrier.wait();
        let finish_result = finish_handle.join().expect("finish thread");
        cancel_handle
            .join()
            .expect("cancel thread")
            .expect("cancel is idempotent");

        assert_eq!(backend.state.stops.load(Ordering::SeqCst), 1);
        assert_eq!(capture_adapter.active_capture_count(), 0);
        if let Ok(audio) = finish_result {
            block_on(capture_adapter.cleanup(audio.audio_ref)).expect("cleanup winner artifact");
        }
        assert_eq!(
            fs::read_dir(directory.path()).expect("read root").count(),
            0
        );
    }

    #[test]
    fn invalid_or_path_shaped_refs_never_resolve_to_filesystem_paths() {
        let directory = TestDirectory::new();
        let backend = FakeBackend::default();
        let writers = FakeWriterFactory::with_frames(10);
        let capture_adapter = adapter(&directory, &backend, &writers);

        for invalid in [
            "../recording",
            "/tmp/recording.wav",
            "00000000-0000-0000-0000-000000000000",
            "11111111-2222-4333-8444-AAAAAAAAAAAA",
        ] {
            let error = block_on(capture_adapter.cleanup(AudioRef::new(invalid)))
                .expect_err("invalid ref must be rejected");
            assert_eq!(error.code, "audio.invalid_ref");
        }
    }
}
