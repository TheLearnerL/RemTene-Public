use std::{
    fs,
    future::Future,
    path::{Path, PathBuf},
    pin::pin,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    thread,
    time::{Duration, Instant},
};

use remtene_adapters::asr_worker::{AsrWorkerAdapter, AudioArtifactResolver, WorkerLaunchConfig};
use remtene_application::ports::{
    AsrEnginePort, AsrRequest, AudioFormat, AudioRef, EngineHealth, FinalizedAudio,
};
use remtene_domain::{AsrEngine, RequestId, SessionId};
#[cfg(all(target_os = "macos", feature = "whisper-runtime"))]
use remtene_platform::audio::{
    AudioWriterFactory, AudioWriterRequest, CANONICAL_ASR_AUDIO_FORMAT, FrameSinkError,
    Pcm16WavWriterFactory,
};
use uuid::Uuid;

const FUTURE_TIMEOUT: Duration = Duration::from_secs(4);

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires explicit REMTENE_RUN_LIVE_QWEN_WORKER_SMOKE=1 and local pinned model assets"]
fn live_qwen_worker_transcribes_the_public_chinese_fixture() {
    if std::env::var("REMTENE_RUN_LIVE_QWEN_WORKER_SMOKE").as_deref() != Ok("1") {
        return;
    }
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let model_dir = repository.join("pocs/asr/artifacts/models/qwen3-asr-0.6b-5eb1441");
    let source = repository.join("pocs/asr/artifacts/fixtures/public/BAC009S0764W0121.wav");
    assert!(model_dir.is_dir(), "pinned Qwen model is missing");
    assert!(source.is_file(), "public Chinese fixture is missing");

    let root = std::env::temp_dir().join(format!("remtene-qwen-live-{}", Uuid::new_v4()));
    let grant_root = root.join("grants");
    fs::create_dir_all(&grant_root).expect("create live grant root");
    let resolved_source = source.clone();
    let resolver: AudioArtifactResolver =
        Arc::new(move |_audio_ref| Ok(Some(resolved_source.clone())));
    let config = WorkerLaunchConfig::new(
        env!("CARGO_BIN_EXE_remtene-asr-worker"),
        &grant_root,
        "0.1.0-live-test",
        "qwen3_asr_0_6b",
        "whisper-test",
    )
    .expect("live Worker config")
    .with_qwen_model(&model_dir, "5eb144179a02acc5e5ba31e748d22b0cf3e303b0")
    .with_timeouts(
        Duration::from_secs(10),
        Duration::from_secs(10),
        Duration::from_secs(2),
        Duration::from_secs(2),
    );
    let adapter = AsrWorkerAdapter::start(config, resolver).expect("start live Supervisor");

    assert_eq!(
        block_on_with_timeout(adapter.health(AsrEngine::Qwen), Duration::from_secs(15))
            .expect("Qwen model health and prewarm"),
        EngineHealth::Healthy
    );
    let request = AsrRequest {
        session_id: SessionId::new(),
        request_id: RequestId::new(),
        engine: AsrEngine::Qwen,
        audio: FinalizedAudio {
            audio_ref: AudioRef::new(Uuid::new_v4().hyphenated().to_string()),
            format: AudioFormat {
                sample_rate_hz: 16_000,
                channels: 1,
                bits_per_sample: 16,
            },
            duration_ms: 4_204,
        },
        language_hint: Some("zh".to_owned()),
        deadline_ms: 60_000,
    };
    let result = block_on_with_timeout(adapter.transcribe(request), Duration::from_secs(70))
        .expect("Qwen transcription");
    eprintln!(
        "Qwen live inference_ms={} transcript={:?}",
        result.inference_duration_ms, result.final_text
    );
    assert_eq!(result.engine, AsrEngine::Qwen);
    assert_eq!(result.detected_language.as_deref(), Some("zh"));
    assert!(result.final_text.contains("交易"));
    assert!(result.final_text.contains("停滞"));
    assert_grant_root_empty(&grant_root);
    block_on_with_timeout(adapter.shutdown(), Duration::from_secs(5))
        .expect("shutdown Qwen Worker");
    fs::remove_dir_all(root).expect("remove live test root");
}

#[cfg(all(target_os = "macos", feature = "whisper-runtime"))]
#[test]
#[ignore = "requires explicit REMTENE_RUN_LIVE_WHISPER_WORKER_SMOKE=1 and local pinned model assets"]
fn live_48khz_device_audio_is_normalized_then_transcribed_by_whisper() {
    if std::env::var("REMTENE_RUN_LIVE_WHISPER_WORKER_SMOKE").as_deref() != Ok("1") {
        return;
    }
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let model_file = repository.join(
        "pocs/asr/artifacts/models/whisper-large-v3-turbo-q5_0-5359861/ggml-large-v3-turbo-q5_0.bin",
    );
    let source = repository.join("pocs/asr/artifacts/fixtures/public/BAC009S0764W0121.wav");
    assert!(model_file.is_file(), "pinned Whisper model is missing");
    assert!(source.is_file(), "public Chinese fixture is missing");

    let root = std::env::temp_dir().join(format!("remtene-whisper-live-{}", Uuid::new_v4()));
    let grant_root = root.join("grants");
    fs::create_dir_all(&grant_root).expect("create live grant root");
    let resolved_source = root.join("48khz-device-to-canonical.wav");
    let normalized_frames = normalize_fixture_as_48khz_device_audio(&source, &resolved_source);
    let resolved_source_for_grant = resolved_source.clone();
    let resolver: AudioArtifactResolver =
        Arc::new(move |_audio_ref| Ok(Some(resolved_source_for_grant.clone())));
    let config = WorkerLaunchConfig::new(
        env!("CARGO_BIN_EXE_remtene-asr-worker"),
        &grant_root,
        "0.1.0-live-test",
        "qwen-test",
        "whisper_large_v3_turbo_q5_0",
    )
    .expect("live Worker config")
    .with_whisper_model(&model_file, "5359861d3e1f")
    .with_timeouts(
        Duration::from_secs(10),
        Duration::from_secs(60),
        Duration::from_secs(2),
        Duration::from_secs(2),
    );
    let adapter = AsrWorkerAdapter::start(config, resolver).expect("start live Supervisor");

    assert_eq!(
        block_on_with_timeout(adapter.health(AsrEngine::Whisper), Duration::from_secs(70))
            .expect("Whisper model health and prewarm"),
        EngineHealth::Healthy
    );
    let request = AsrRequest {
        session_id: SessionId::new(),
        request_id: RequestId::new(),
        engine: AsrEngine::Whisper,
        audio: FinalizedAudio {
            audio_ref: AudioRef::new(Uuid::new_v4().hyphenated().to_string()),
            format: CANONICAL_ASR_AUDIO_FORMAT,
            duration_ms: normalized_frames.saturating_mul(1_000) / 16_000,
        },
        language_hint: Some("zh".to_owned()),
        deadline_ms: 10_000,
    };
    let result = block_on_with_timeout(adapter.transcribe(request), Duration::from_secs(15))
        .expect("Whisper transcription");
    eprintln!(
        "Whisper live inference_ms={} transcript={:?}",
        result.inference_duration_ms, result.final_text
    );
    assert_eq!(result.engine, AsrEngine::Whisper);
    assert_eq!(result.detected_language.as_deref(), Some("zh"));
    assert!(result.final_text.contains("交易"));
    assert!(result.final_text.contains("停滞"));
    assert_grant_root_empty(&grant_root);
    block_on_with_timeout(adapter.shutdown(), Duration::from_secs(5))
        .expect("shutdown Whisper Worker");
    fs::remove_dir_all(root).expect("remove live test root");
}

#[cfg(all(target_os = "macos", feature = "whisper-runtime"))]
fn normalize_fixture_as_48khz_device_audio(source: &Path, destination: &Path) -> u64 {
    let mut reader = hound::WavReader::open(source).expect("open public fixture");
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 16_000);
    assert_eq!(spec.channels, 1);
    assert_eq!(spec.bits_per_sample, 16);
    let source_samples: Vec<i16> = reader
        .samples::<i16>()
        .map(|sample| sample.expect("decode fixture PCM16"))
        .collect();
    let device_samples: Vec<i16> = source_samples
        .into_iter()
        .flat_map(|sample| [sample; 3])
        .collect();

    let factory = Pcm16WavWriterFactory;
    let mut pipeline = factory
        .create(
            destination,
            AudioWriterRequest {
                session_id: SessionId::new(),
                source_format: AudioFormat {
                    sample_rate_hz: 48_000,
                    channels: 1,
                    bits_per_sample: 16,
                },
                target_format: CANONICAL_ASR_AUDIO_FORMAT,
            },
        )
        .expect("create streaming normalization pipeline");
    for chunk in device_samples.chunks(1_024) {
        loop {
            match pipeline.sink.try_write(chunk) {
                Ok(()) => break,
                Err(FrameSinkError::Overflow) => thread::sleep(Duration::from_millis(1)),
                Err(error) => panic!("normalization sink failed: {error:?}"),
            }
        }
    }
    let summary = pipeline.writer.finalize().expect("finalize canonical WAV");
    let normalized = hound::WavReader::open(destination).expect("open normalized fixture");
    assert_eq!(normalized.spec().sample_rate, 16_000);
    assert_eq!(normalized.spec().channels, 1);
    assert_eq!(normalized.spec().bits_per_sample, 16);
    summary.frames_written
}

#[test]
fn supervisor_maps_health_transcript_and_revokes_the_audio_grant() {
    let fixture = SupervisorFixture::new("happy", "--deterministic-test-backend");
    let adapter = fixture.adapter();

    assert_eq!(
        block_on(adapter.health(AsrEngine::Qwen)).expect("Qwen health"),
        EngineHealth::Healthy
    );
    let request = fixture.request(AsrEngine::Qwen, 2_000);
    let expected_session = request.session_id;
    let expected_request = request.request_id;
    let result = block_on(adapter.transcribe(request)).expect("transcribe");
    assert_eq!(result.session_id, expected_session);
    assert_eq!(result.request_id, expected_request);
    assert_eq!(result.engine, AsrEngine::Qwen);
    assert_eq!(result.final_text, "deterministic worker transcript");
    assert_grant_root_empty(&fixture.grant_root);

    block_on(adapter.shutdown()).expect("shutdown Worker");
}

#[test]
fn supervisor_reuses_one_worker_for_sequential_transcriptions() {
    let fixture = SupervisorFixture::new("sequential", "--deterministic-test-backend");
    let adapter = fixture.adapter();

    assert_eq!(
        block_on(adapter.health(AsrEngine::Qwen)).expect("Qwen health"),
        EngineHealth::Healthy
    );
    let first = block_on(adapter.transcribe(fixture.request(AsrEngine::Qwen, 2_000)))
        .expect("first transcription");
    assert_grant_root_empty(&fixture.grant_root);
    let second = block_on(adapter.transcribe(fixture.request(AsrEngine::Qwen, 2_000)))
        .expect("second transcription");

    assert_ne!(first.request_id, second.request_id);
    assert_eq!(first.final_text, "deterministic worker transcript");
    assert_eq!(second.final_text, "deterministic worker transcript");
    assert_grant_root_empty(&fixture.grant_root);
    block_on(adapter.shutdown()).expect("shutdown reused Worker");
}

#[test]
fn supervisor_releases_an_idle_worker_and_restarts_it_on_demand() {
    let fixture = SupervisorFixture::new("release-idle", "--deterministic-test-backend");
    let adapter = fixture.adapter();

    assert_eq!(
        block_on(adapter.health(AsrEngine::Qwen)).expect("initial Qwen health"),
        EngineHealth::Healthy
    );
    block_on(adapter.release_idle_resources()).expect("release idle Worker");
    assert_eq!(
        block_on(adapter.health(AsrEngine::Qwen)).expect("replacement Qwen health"),
        EngineHealth::Healthy
    );
    let result = block_on(adapter.transcribe(fixture.request(AsrEngine::Qwen, 2_000)))
        .expect("replacement Worker transcription");
    assert_eq!(result.final_text, "deterministic worker transcript");
    assert_grant_root_empty(&fixture.grant_root);
    block_on(adapter.shutdown()).expect("shutdown replacement Worker");
}

#[test]
fn idle_resource_release_never_cancels_an_active_transcription() {
    let fixture = SupervisorFixture::new("release-busy", "--deterministic-test-backend");
    let adapter = fixture.adapter();
    let transcription = adapter.transcribe(fixture.request(AsrEngine::Qwen, 2_000));
    thread::sleep(Duration::from_millis(20));

    let error = block_on(adapter.release_idle_resources()).expect_err("active Worker is busy");
    assert_eq!(error.code, "asr.worker.busy");
    let result = block_on(transcription).expect("active transcription remains intact");
    assert_eq!(result.final_text, "deterministic worker transcript");
    assert_grant_root_empty(&fixture.grant_root);
    block_on(adapter.shutdown()).expect("shutdown Worker");
}

#[test]
fn supervisor_waits_for_worker_cancellation_before_revoking_audio() {
    let fixture = SupervisorFixture::new("cancel", "--deterministic-test-backend");
    let adapter = fixture.adapter();
    let request = fixture.request(AsrEngine::Whisper, 2_000);
    let request_id = request.request_id;
    let transcription = adapter.transcribe(request);
    thread::sleep(Duration::from_millis(20));

    block_on(adapter.cancel(request_id)).expect("Worker confirms cancellation");
    let error = block_on(transcription).expect_err("cancelled request has no transcript");
    assert_eq!(error.code, "asr.cancelled");
    assert_grant_root_empty(&fixture.grant_root);
    block_on(adapter.shutdown()).expect("shutdown Worker");
}

#[test]
fn supervisor_turns_a_deadline_into_cancel_without_falling_back() {
    for iteration in 0..12 {
        let fixture = SupervisorFixture::new("deadline", "--deterministic-test-backend");
        let adapter = fixture.adapter();
        let request = fixture.request(AsrEngine::Qwen, 20);
        let error = block_on(adapter.transcribe(request)).expect_err("deadline must fail");
        assert_eq!(
            error.code, "asr.worker.timeout",
            "deadline iteration {iteration}"
        );
        assert_grant_root_empty(&fixture.grant_root);

        assert_eq!(
            block_on(adapter.health(AsrEngine::Qwen)).expect("same Worker remains healthy"),
            EngineHealth::Healthy
        );
        block_on(adapter.shutdown()).expect("shutdown Worker");
    }
}

#[test]
fn supervisor_contains_a_worker_crash_and_restarts_only_for_a_later_operation() {
    let fixture = SupervisorFixture::new("crash", "--crash-on-transcribe-test-backend");
    let adapter = fixture.adapter();
    let request = fixture.request(AsrEngine::Qwen, 2_000);
    let error = block_on(adapter.transcribe(request)).expect_err("Worker crash must fail request");
    assert_eq!(error.code, "asr.worker.protocol_rejected");
    assert_grant_root_empty(&fixture.grant_root);

    assert_eq!(
        block_on(adapter.health(AsrEngine::Qwen)).expect("later operation starts a new Worker"),
        EngineHealth::Healthy
    );
    block_on(adapter.shutdown()).expect("shutdown replacement Worker");
}

struct SupervisorFixture {
    source: PathBuf,
    grant_root: PathBuf,
    backend_arg: &'static str,
}

impl SupervisorFixture {
    fn new(label: &str, backend_arg: &'static str) -> Self {
        let root =
            std::env::temp_dir().join(format!("remtene-supervisor-{label}-{}", Uuid::new_v4()));
        let source = root.join("source.wav");
        let grant_root = root.join("grants");
        fs::create_dir_all(&grant_root).expect("create grant root");
        fs::write(&source, b"deterministic-supervisor-audio").expect("write source audio");
        Self {
            source,
            grant_root,
            backend_arg,
        }
    }

    fn adapter(&self) -> AsrWorkerAdapter {
        let source = self.source.clone();
        let resolver: AudioArtifactResolver = Arc::new(move |_audio_ref| Ok(Some(source.clone())));
        let config = WorkerLaunchConfig::new(
            env!("CARGO_BIN_EXE_remtene-asr-worker"),
            &self.grant_root,
            "0.1.0-test",
            "qwen-test",
            "whisper-test",
        )
        .expect("Worker config")
        .with_extra_arg(self.backend_arg)
        .with_timeouts(
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_millis(300),
            Duration::from_millis(500),
        );
        AsrWorkerAdapter::start(config, resolver).expect("start Supervisor")
    }

    fn request(&self, engine: AsrEngine, deadline_ms: u64) -> AsrRequest {
        AsrRequest {
            session_id: SessionId::new(),
            request_id: RequestId::new(),
            engine,
            audio: FinalizedAudio {
                audio_ref: AudioRef::new(Uuid::new_v4().hyphenated().to_string()),
                format: AudioFormat {
                    sample_rate_hz: 16_000,
                    channels: 1,
                    bits_per_sample: 16,
                },
                duration_ms: 500,
            },
            language_hint: Some("zh".to_owned()),
            deadline_ms,
        }
    }
}

impl Drop for SupervisorFixture {
    fn drop(&mut self) {
        if let Some(root) = self.source.parent() {
            let _ = fs::remove_dir_all(root);
        }
    }
}

fn assert_grant_root_empty(root: &Path) {
    assert_eq!(
        fs::read_dir(root).expect("read grant root").count(),
        0,
        "audio grant must be revoked"
    );
}

fn block_on<F: Future>(future: F) -> F::Output {
    block_on_with_timeout(future, FUTURE_TIMEOUT)
}

fn block_on_with_timeout<F: Future>(future: F, timeout: Duration) -> F::Output {
    let deadline = Instant::now() + timeout;
    let current = thread::current();
    let waker = Waker::from(Arc::new(ThreadWaker(current)));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        assert!(Instant::now() < deadline, "Supervisor future timed out");
        thread::park_timeout(Duration::from_millis(20));
    }
}

struct ThreadWaker(thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}
