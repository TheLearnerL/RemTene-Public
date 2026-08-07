//! Audio artifact resolver adapter.
//!
//! Turns the real recording adapter into the `AudioArtifactResolver` the ASR Worker
//! Supervisor expects, so the Worker can only ever reach audio the recording adapter
//! finalized itself.

use std::sync::Arc;

use remtene_application::ports::AudioRef;

use crate::asr_worker::AudioArtifactResolver;

/// Build the Worker audio resolver from the real recording adapter.
///
/// `SafeAudioCapture` owns the authoritative artifact table and re-checks that the
/// registered path is still a caller-owned regular file before handing it out, so the
/// Worker can only ever reach an artifact the recording adapter finalized itself.
pub fn resolver_from_capture(
    capture: Arc<remtene_platform::audio::SafeAudioCapture>,
) -> AudioArtifactResolver {
    Arc::new(move |audio_ref: &AudioRef| {
        Ok(capture
            .resolve_artifact(audio_ref)?
            .map(|artifact| artifact.path().to_path_buf()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_resolver_returns_none_for_an_unfinalized_artifact() {
        use remtene_application::ports::{AudioFormat, PortError};
        use remtene_platform::audio::{
            AudioBackend, AudioWriterFactory, AudioWriterRequest, BackendStartOutcome,
            BackendStartRequest, SafeAudioCapture, WriterPipeline,
        };
        use std::path::Path;

        struct UnavailableBackend;
        impl AudioBackend for UnavailableBackend {
            fn start(&self, _request: BackendStartRequest) -> BackendStartOutcome {
                BackendStartOutcome::FailedClean(PortError {
                    code: "audio.test_backend_unavailable".to_owned(),
                    safe_message_key: "errors.audio.device_unavailable".to_owned(),
                    retryable: false,
                })
            }
        }

        struct UnusedWriters;
        impl AudioWriterFactory for UnusedWriters {
            fn create(
                &self,
                _partial_path: &Path,
                _request: AudioWriterRequest,
            ) -> Result<WriterPipeline, PortError> {
                Err(PortError {
                    code: "audio.test_writer_unavailable".to_owned(),
                    safe_message_key: "errors.audio.invalid_format".to_owned(),
                    retryable: false,
                })
            }
        }

        let root = std::env::temp_dir().join(format!("remtene-resolver-{}", uuid::Uuid::new_v4()));
        let capture = SafeAudioCapture::initialize(
            &root,
            AudioFormat {
                sample_rate_hz: 48_000,
                channels: 1,
                bits_per_sample: 16,
            },
            Arc::new(UnavailableBackend),
            Arc::new(UnusedWriters),
        )
        .expect("capture core must initialize without touching a device");

        let resolver = resolver_from_capture(Arc::new(capture));
        let unknown = AudioRef::new(uuid::Uuid::new_v4().to_string());
        assert_eq!(resolver(&unknown).expect("resolution must not fail"), None);

        let _ = std::fs::remove_dir_all(&root);
    }
}
