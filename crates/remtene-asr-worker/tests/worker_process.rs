use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

use remtene_contracts::{
    AudioArtifactId, AudioFormatDto, CONTRACT_VERSION, CancelRequest, CoreHello,
    CoreToWorkerEnvelope, CoreToWorkerMessage, HealthCheckRequest, HealthStatus, ShutdownRequest,
    TranscribeRequest, WorkerCapability, WorkerEngineId, WorkerErrorCode, WorkerToCoreEnvelope,
    WorkerToCoreMessage,
};
use uuid::Uuid;

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(3);

#[test]
fn worker_process_handshakes_transcribes_and_shuts_down() {
    let mut worker = TestWorker::spawn();
    worker.send(hello());
    assert!(matches!(
        worker.receive().message,
        WorkerToCoreMessage::Ready(_)
    ));

    let health_request_id = Uuid::new_v4();
    worker.send(envelope(
        None,
        Some(health_request_id),
        CoreToWorkerMessage::HealthCheck(HealthCheckRequest {
            engine_id: WorkerEngineId::Qwen,
            model_id: "qwen-test".to_owned(),
        }),
    ));
    let health = worker.receive();
    assert_eq!(health.request_id, Some(health_request_id));
    let WorkerToCoreMessage::HealthResult(health) = health.message else {
        panic!("expected health result")
    };
    assert_eq!(health.status, HealthStatus::Healthy);

    let request = worker.create_request(2_000);
    worker.send(transcribe(&request));
    let transcript = worker.receive();
    assert_eq!(transcript.session_id, Some(request.session_id));
    assert_eq!(transcript.request_id, Some(request.request_id));
    let WorkerToCoreMessage::Transcript(transcript) = transcript.message else {
        panic!("expected final transcript")
    };
    assert_eq!(transcript.final_text, "deterministic worker transcript");

    worker.send(shutdown(500));
    assert!(matches!(
        worker.receive().message,
        WorkerToCoreMessage::ShutdownComplete(_)
    ));
    worker.assert_success_and_no_stderr();
}

#[test]
fn worker_process_cancels_the_active_request_without_a_transcript() {
    let mut worker = TestWorker::spawn();
    worker.send(hello());
    assert!(matches!(
        worker.receive().message,
        WorkerToCoreMessage::Ready(_)
    ));

    let request = worker.create_request(2_000);
    worker.send(transcribe(&request));
    worker.send(envelope(
        Some(request.session_id),
        Some(request.request_id),
        CoreToWorkerMessage::Cancel(CancelRequest {
            session_id: request.session_id,
            request_id: request.request_id,
        }),
    ));
    let cancelled = worker.receive();
    assert!(matches!(
        cancelled.message,
        WorkerToCoreMessage::Cancelled(_)
    ));

    worker.send(shutdown(500));
    assert!(matches!(
        worker.receive().message,
        WorkerToCoreMessage::ShutdownComplete(_)
    ));
    worker.assert_success_and_no_stderr();
}

#[test]
fn worker_process_rejects_an_unknown_field_and_exits_without_echoing_input() {
    let mut worker = TestWorker::spawn();
    let marker = "PRIVATE_AUDIO_OR_TEXT_MARKER";
    worker.send_raw(&format!(
        "{{\"contract_version\":1,\"message_id\":\"{}\",\"session_id\":null,\"request_id\":null,\"sent_at\":\"2026-07-22T00:00:00Z\",\"kind\":\"hello\",\"payload\":{{\"supported_protocol_versions\":[1],\"core_version\":\"0.1.0\",\"required_capabilities\":[],\"unknown\":\"{marker}\"}}}}\n",
        Uuid::new_v4()
    ));
    let response = worker.receive();
    let WorkerToCoreMessage::Error(error) = response.message else {
        panic!("expected protocol error")
    };
    assert_eq!(error.code, WorkerErrorCode::InvalidRequest);
    assert!(error.fatal);
    worker.assert_failure_without_marker(marker);
}

struct TestWorker {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    responses: mpsc::Receiver<Result<WorkerToCoreEnvelope, String>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    root: PathBuf,
}

impl TestWorker {
    fn spawn() -> Self {
        let root = std::env::temp_dir().join(format!("remtene-worker-process-{}", Uuid::new_v4()));
        fs::create_dir(&root).expect("create Worker grant root");
        let mut child = Command::new(env!("CARGO_BIN_EXE_remtene-asr-worker"))
            .arg("--artifact-root")
            .arg(&root)
            .arg("--deterministic-test-backend")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn formal Worker process");
        let stdin = child.stdin.take().expect("Worker stdin");
        let stdout = child.stdout.take().expect("Worker stdout");
        let child_stderr = child.stderr.take().expect("Worker stderr");

        let (responses_tx, responses) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => return,
                    Ok(_) => {
                        let decoded = serde_json::from_str::<WorkerToCoreEnvelope>(&line)
                            .map_err(|_| "invalid Worker response".to_owned());
                        if responses_tx.send(decoded).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });

        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stderr_sink = Arc::clone(&stderr);
        thread::spawn(move || {
            let mut reader = BufReader::new(child_stderr);
            let mut bytes = Vec::new();
            let _ = reader.read_to_end(&mut bytes);
            *stderr_sink.lock().expect("stderr lock") = bytes;
        });

        Self {
            child: Some(child),
            stdin: Some(stdin),
            responses,
            stderr,
            root,
        }
    }

    fn create_request(&self, deadline_ms: u64) -> TranscribeRequest {
        let artifact_id = AudioArtifactId::random();
        fs::write(
            self.root.join(format!("{artifact_id}.wav")),
            b"deterministic-test-audio",
        )
        .expect("create granted artifact");
        TranscribeRequest {
            session_id: Uuid::new_v4(),
            request_id: Uuid::new_v4(),
            engine_id: WorkerEngineId::Qwen,
            model_id: "qwen-test".to_owned(),
            audio_artifact_id: artifact_id,
            audio_format: AudioFormatDto {
                sample_rate_hz: 16_000,
                channels: 1,
                bits_per_sample: 16,
            },
            language_hint: Some("en".to_owned()),
            deadline_ms,
        }
    }

    fn send(&mut self, envelope: CoreToWorkerEnvelope) {
        let mut encoded = serde_json::to_vec(&envelope).expect("encode Core envelope");
        encoded.push(b'\n');
        self.stdin
            .as_mut()
            .expect("Worker stdin remains open")
            .write_all(&encoded)
            .and_then(|()| self.stdin.as_mut().expect("stdin").flush())
            .expect("send Core envelope");
    }

    fn send_raw(&mut self, value: &str) {
        self.stdin
            .as_mut()
            .expect("Worker stdin remains open")
            .write_all(value.as_bytes())
            .and_then(|()| self.stdin.as_mut().expect("stdin").flush())
            .expect("send raw Worker frame");
    }

    fn receive(&self) -> WorkerToCoreEnvelope {
        self.responses
            .recv_timeout(RESPONSE_TIMEOUT)
            .expect("Worker response timeout")
            .expect("Worker response must decode")
    }

    fn assert_success_and_no_stderr(mut self) {
        self.stdin.take();
        let status = self
            .child
            .as_mut()
            .expect("Worker child")
            .wait()
            .expect("wait for Worker");
        assert!(status.success(), "Worker exited with {status}");
        self.child.take();
        thread::sleep(Duration::from_millis(20));
        assert!(self.stderr.lock().expect("stderr lock").is_empty());
    }

    fn assert_failure_without_marker(mut self, marker: &str) {
        self.stdin.take();
        let status = self
            .child
            .as_mut()
            .expect("Worker child")
            .wait()
            .expect("wait for Worker");
        assert!(!status.success(), "invalid protocol must fail the Worker");
        self.child.take();
        thread::sleep(Duration::from_millis(20));
        let stderr = self.stderr.lock().expect("stderr lock");
        let stderr = String::from_utf8_lossy(&stderr);
        assert!(!stderr.contains(marker), "stderr echoed private input");
    }
}

impl Drop for TestWorker {
    fn drop(&mut self) {
        self.stdin.take();
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child.take();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn hello() -> CoreToWorkerEnvelope {
    envelope(
        None,
        None,
        CoreToWorkerMessage::Hello(CoreHello {
            supported_protocol_versions: vec![CONTRACT_VERSION],
            core_version: "0.1.0-test".to_owned(),
            required_capabilities: vec![
                WorkerCapability::HealthCheck,
                WorkerCapability::FinalTranscript,
                WorkerCapability::Cancellation,
                WorkerCapability::GracefulShutdown,
            ],
        }),
    )
}

fn transcribe(request: &TranscribeRequest) -> CoreToWorkerEnvelope {
    envelope(
        Some(request.session_id),
        Some(request.request_id),
        CoreToWorkerMessage::Transcribe(request.clone()),
    )
}

fn shutdown(grace_period_ms: u64) -> CoreToWorkerEnvelope {
    envelope(
        None,
        None,
        CoreToWorkerMessage::Shutdown(ShutdownRequest { grace_period_ms }),
    )
}

fn envelope(
    session_id: Option<Uuid>,
    request_id: Option<Uuid>,
    message: CoreToWorkerMessage,
) -> CoreToWorkerEnvelope {
    CoreToWorkerEnvelope {
        contract_version: CONTRACT_VERSION,
        message_id: Uuid::new_v4(),
        session_id,
        request_id,
        sent_at: "2026-07-22T00:00:00Z".to_owned(),
        message,
    }
}
