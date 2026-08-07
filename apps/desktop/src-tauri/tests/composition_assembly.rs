//! CORE-020 组装集成测试
//!
//! 验证桌面 CompositionRoot 能用一组 Port 正确组装出可用的
//! TranscriptionOrchestrator。此测试不依赖真实硬件（麦克风、AX、签名 Worker），
//! 但会通过本机回环 HTTP 服务验证真实 LLM Provider、加密 SecretStore 与完整
//! Session 的生产组装边界。

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use remtene_adapters::{
    clipboard_bridge::UnsupportedClipboardBridge,
    clipboard_text_writer::UnsupportedClipboardTextWriter,
    diagnostics::ConsoleDiagnosticsSink,
    file_history_store::FileHistoryStore,
    llm::OpenAiCompatiblePolicy,
    local_encrypted_secret_store::LocalEncryptedSecretStore,
    output_adapter::ConsoleOutputAdapter,
    settings_store::InMemorySettingsStore,
    stub_ports::{
        StubAsrEngine, StubAsrModelControl, StubAudioCapture, StubMicrophonePermission,
        StubRecordingHud, StubTargetContext, StubUserNotification,
    },
    temporary_text::StubTemporaryTextOutput,
};
use remtene_application::ports::{
    AsrEnginePort, AsrRequest, AsrResult, AudioCapture, AudioCaptureRef, AudioFormat, AudioRef,
    CapturedTarget, ClipboardBridge, ClipboardTextWriter, DiagnosticsControl, DiagnosticsSink,
    EngineHealth, FinalizedAudio, HistoryStore, InsertOutcome, LifecycleFence, MicrophoneAccess,
    MicrophonePermissionPort, OutputAdapter, PortError, PortFuture, RecordingHudPort, SecretStore,
    SelectionSnapshot, SettingsStore, TargetContextPort, TargetRevalidation, TargetSecurity,
    TargetSnapshotRef, TemporaryTextOutput, TemporaryTextStatus, ValidatedTargetRef,
};
use remtene_application::{
    DeliveryKind, DirectDeliveryReason, FinishOutcome, LlmApiKeyStatus, LlmConnectionTestOutcome,
    QuitOutcome, StartOutcome,
};
use remtene_domain::{
    AsrEngine, AsrPreference, HistoryPolicy, LlmNonSecretSettings, ProcessingMode, RecordingMode,
    SettingsSnapshot, SettingsSnapshotInput,
};

use remtene_desktop_lib::composition_root::CompositionRoot;

struct TestStorage {
    root: PathBuf,
}

impl TestStorage {
    fn new(tag: &str) -> Self {
        let temp_root = std::env::temp_dir();
        let root = temp_root
            .canonicalize()
            .unwrap_or(temp_root)
            .join(format!("remtene-assembly-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create test storage root");
        Self { root }
    }

    fn history_path(&self) -> PathBuf {
        self.root.join("history.json")
    }

    fn secret_root(&self) -> PathBuf {
        self.root.join("secrets")
    }
}

impl Drop for TestStorage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn test_composition_root(tag: &str) -> (CompositionRoot, TestStorage) {
    let storage = TestStorage::new(tag);
    let targets: Arc<dyn TargetContextPort> = Arc::new(StubTargetContext::new());
    let microphone: Arc<dyn MicrophonePermissionPort> = Arc::new(StubMicrophonePermission::new());
    let audio: Arc<dyn AudioCapture> = Arc::new(StubAudioCapture::new());
    let recording_hud: Arc<dyn RecordingHudPort> = Arc::new(StubRecordingHud::new());
    let asr: Arc<dyn AsrEnginePort> = Arc::new(StubAsrEngine::new());
    let output: Arc<dyn OutputAdapter> = Arc::new(ConsoleOutputAdapter::new());
    let history: Arc<dyn HistoryStore> =
        Arc::new(FileHistoryStore::new(storage.history_path()).unwrap());
    let settings: Arc<dyn SettingsStore> = Arc::new(InMemorySettingsStore::with_defaults());
    let secrets: Arc<dyn SecretStore> =
        Arc::new(LocalEncryptedSecretStore::new(storage.secret_root()).unwrap());
    let temporary_text: Arc<dyn TemporaryTextOutput> = Arc::new(StubTemporaryTextOutput::new());
    let user_notifications = Arc::new(StubUserNotification::new());
    let clipboard: Arc<dyn ClipboardBridge> = Arc::new(UnsupportedClipboardBridge::new());
    let clipboard_text_writer: Arc<dyn ClipboardTextWriter> =
        Arc::new(UnsupportedClipboardTextWriter::new());
    (
        CompositionRoot::new(
            targets,
            microphone,
            audio,
            recording_hud,
            asr,
            Arc::new(StubAsrModelControl::new()),
            output,
            history,
            settings,
            secrets,
            temporary_text,
            user_notifications,
            clipboard,
            clipboard_text_writer,
        ),
        storage,
    )
}

fn read_http_request(stream: &mut impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut buffer).expect("read request");
        assert!(read > 0, "request ended before headers");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
    let content_length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .map(str::trim)
        .map(|value| value.parse::<usize>().expect("content length"))
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut buffer).expect("read request body");
        assert!(read > 0, "request ended before body");
        bytes.extend_from_slice(&buffer[..read]);
    }
    bytes
}

fn one_shot_llm_server(content: &str) -> (String, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake Provider");
    let address = listener.local_addr().expect("fake Provider address");
    let (request_tx, request_rx) = mpsc::channel();
    let content = content.to_owned();
    let response_body = serde_json::json!({
        "choices": [{
            "message": {
                "content": content,
            },
        }],
    })
    .to_string();

    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept fake Provider request");
        let request = read_http_request(&mut stream);
        request_tx.send(request).expect("capture Provider request");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body,
        );
        stream
            .write_all(response.as_bytes())
            .expect("write fake Provider response");
    });

    (format!("http://{address}/v1"), request_rx)
}

#[derive(Clone)]
enum FakeProviderReply {
    Http {
        status_line: &'static str,
        body: String,
        delay: Duration,
    },
    Disconnect,
}

struct CountingLlmServer {
    base_url: String,
    requests: mpsc::Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    address: std::net::SocketAddr,
    worker: Option<thread::JoinHandle<()>>,
}

impl CountingLlmServer {
    fn start(reply: FakeProviderReply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind counting fake Provider");
        listener
            .set_nonblocking(true)
            .expect("configure fake Provider");
        let address = listener.local_addr().expect("fake Provider address");
        let (request_tx, request_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if worker_stop.load(Ordering::Acquire) {
                            break;
                        }
                        stream
                            .set_nonblocking(false)
                            .expect("configure accepted Provider connection");
                        let request = read_http_request(&mut stream);
                        if request_tx.send(request).is_err() {
                            break;
                        }
                        match &reply {
                            FakeProviderReply::Http {
                                status_line,
                                body,
                                delay,
                            } => {
                                if !delay.is_zero() {
                                    thread::sleep(*delay);
                                }
                                let response = format!(
                                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                    body.len(),
                                    body,
                                );
                                let _ = stream.write_all(response.as_bytes());
                            }
                            FakeProviderReply::Disconnect => {}
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            base_url: format!("http://{address}/v1"),
            requests: request_rx,
            stop,
            address,
            worker: Some(worker),
        }
    }

    fn canonical(final_text: &str, delay: Duration) -> Self {
        Self::content(&canonical_response(final_text), delay)
    }

    fn content(content: &str, delay: Duration) -> Self {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": content,
                },
            }],
        })
        .to_string();
        Self::start(FakeProviderReply::Http {
            status_line: "200 OK",
            body,
            delay,
        })
    }

    fn status(status_line: &'static str) -> Self {
        Self::start(FakeProviderReply::Http {
            status_line,
            body: r#"{"error":"provider detail must not escape"}"#.to_owned(),
            delay: Duration::ZERO,
        })
    }

    fn disconnect() -> Self {
        Self::start(FakeProviderReply::Disconnect)
    }
}

impl Drop for CountingLlmServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn canonical_response(final_text: &str) -> String {
    serde_json::json!({
        "schema_version": 1,
        "intent": "dictation",
        "final_text": final_text,
    })
    .to_string()
}

const TEST_API_KEY: &str = "assembly-test-api-key";
const RAW_ASR_TEXT: &str = "张三说如果预算不超过 12.5 万元，我们才不会取消项目，因为风险仍不确定。";

#[derive(Default)]
struct SessionPorts {
    inserted_texts: Mutex<Vec<String>>,
    temporary_texts: Mutex<Vec<(String, TemporaryTextStatus)>>,
}

impl SessionPorts {
    fn inserted_texts(&self) -> Vec<String> {
        self.inserted_texts
            .lock()
            .expect("inserted text lock")
            .clone()
    }

    fn temporary_texts(&self) -> Vec<(String, TemporaryTextStatus)> {
        self.temporary_texts
            .lock()
            .expect("temporary text lock")
            .clone()
    }
}

fn test_port_error(code: &str) -> PortError {
    PortError {
        code: code.to_owned(),
        safe_message_key: format!("errors.{code}"),
        retryable: false,
    }
}

impl TargetContextPort for SessionPorts {
    fn capture(&self) -> PortFuture<'_, Result<CapturedTarget, PortError>> {
        Box::pin(async {
            Ok(CapturedTarget {
                target_ref: TargetSnapshotRef::new("assembly-target"),
                security: TargetSecurity::Safe,
                has_selection: false,
                display_hint: None,
            })
        })
    }

    fn read_selected_text(
        &self,
        _target: &TargetSnapshotRef,
    ) -> PortFuture<'_, Result<SelectionSnapshot, PortError>> {
        Box::pin(async {
            Ok(SelectionSnapshot {
                text: None,
                anchor_normalized_to_end: true,
                exceeded_limit: false,
            })
        })
    }

    fn revalidate(
        &self,
        _target: &TargetSnapshotRef,
    ) -> PortFuture<'_, Result<TargetRevalidation, PortError>> {
        Box::pin(async {
            Ok(TargetRevalidation::Valid(ValidatedTargetRef::new(
                "assembly-target-valid",
            )))
        })
    }
}

impl MicrophonePermissionPort for SessionPorts {
    fn request_recording_access(&self) -> PortFuture<'_, Result<MicrophoneAccess, PortError>> {
        Box::pin(async { Ok(MicrophoneAccess::Granted) })
    }
}

impl AudioCapture for SessionPorts {
    fn start(
        &self,
        _session_id: remtene_domain::SessionId,
    ) -> PortFuture<'_, Result<AudioCaptureRef, PortError>> {
        Box::pin(async { Ok(AudioCaptureRef::new("assembly-capture")) })
    }

    fn finish(
        &self,
        _capture: AudioCaptureRef,
    ) -> PortFuture<'_, Result<FinalizedAudio, PortError>> {
        Box::pin(async {
            Ok(FinalizedAudio {
                audio_ref: AudioRef::new("assembly-audio"),
                format: AudioFormat {
                    sample_rate_hz: 16_000,
                    channels: 1,
                    bits_per_sample: 16,
                },
                duration_ms: 1_000,
            })
        })
    }

    fn cancel(&self, _capture: AudioCaptureRef) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }

    fn cleanup(&self, _audio_ref: AudioRef) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }
}

impl AsrEnginePort for SessionPorts {
    fn health(&self, engine: AsrEngine) -> PortFuture<'_, Result<EngineHealth, PortError>> {
        Box::pin(async move {
            Ok(match engine {
                AsrEngine::Qwen => EngineHealth::Healthy,
                AsrEngine::Whisper => EngineHealth::Missing,
            })
        })
    }

    fn transcribe(&self, request: AsrRequest) -> PortFuture<'_, Result<AsrResult, PortError>> {
        Box::pin(async move {
            Ok(AsrResult {
                session_id: request.session_id,
                request_id: request.request_id,
                engine: request.engine,
                final_text: RAW_ASR_TEXT.to_owned(),
                detected_language: Some("zh".to_owned()),
                inference_duration_ms: 10,
            })
        })
    }

    fn cancel(
        &self,
        _request_id: remtene_domain::RequestId,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async { Ok(()) })
    }
}

impl OutputAdapter for SessionPorts {
    fn insert(
        &self,
        _target: ValidatedTargetRef,
        text: String,
        _delivery_id: remtene_domain::DeliveryId,
        lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<InsertOutcome, PortError>> {
        Box::pin(async move {
            let Some(_commit_guard) = lifecycle.begin_commit() else {
                return Err(test_port_error("lifecycle.invalidated"));
            };
            self.inserted_texts
                .lock()
                .expect("inserted text lock")
                .push(text);
            Ok(InsertOutcome::Inserted)
        })
    }
}

impl TemporaryTextOutput for SessionPorts {
    fn show(
        &self,
        _session_id: remtene_domain::SessionId,
        _delivery_id: remtene_domain::DeliveryId,
        final_text: String,
        status: TemporaryTextStatus,
        lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            let Some(_commit_guard) = lifecycle.begin_commit() else {
                return Err(test_port_error("lifecycle.invalidated"));
            };
            self.temporary_texts
                .lock()
                .expect("temporary text lock")
                .push((final_text, status));
            Ok(())
        })
    }
}

fn session_settings(processing_mode: ProcessingMode) -> SettingsSnapshot {
    SettingsSnapshot::new(SettingsSnapshotInput {
        version: 0,
        recording_mode: RecordingMode::Toggle,
        max_recording_duration: Duration::from_secs(60),
        recording_shortcut: None,
        processing_mode,
        asr_preference: AsrPreference::Qwen,
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
    .expect("valid assembly settings")
}

fn session_composition_root(
    tag: &str,
) -> (
    CompositionRoot,
    Arc<SessionPorts>,
    Arc<FileHistoryStore>,
    TestStorage,
) {
    session_composition_root_with(
        tag,
        ProcessingMode::Faithful,
        OpenAiCompatiblePolicy::default(),
    )
}

fn session_composition_root_with(
    tag: &str,
    processing_mode: ProcessingMode,
    llm_policy: OpenAiCompatiblePolicy,
) -> (
    CompositionRoot,
    Arc<SessionPorts>,
    Arc<FileHistoryStore>,
    TestStorage,
) {
    let storage = TestStorage::new(tag);
    let session_ports = Arc::new(SessionPorts::default());
    let targets: Arc<dyn TargetContextPort> = session_ports.clone();
    let microphone: Arc<dyn MicrophonePermissionPort> = session_ports.clone();
    let audio: Arc<dyn AudioCapture> = session_ports.clone();
    let recording_hud: Arc<dyn RecordingHudPort> = Arc::new(StubRecordingHud::new());
    let asr: Arc<dyn AsrEnginePort> = session_ports.clone();
    let output: Arc<dyn OutputAdapter> = session_ports.clone();
    let history = Arc::new(FileHistoryStore::new(storage.history_path()).unwrap());
    let history_port: Arc<dyn HistoryStore> = history.clone();
    let settings: Arc<dyn SettingsStore> = Arc::new(InMemorySettingsStore::new(session_settings(
        processing_mode,
    )));
    let secrets: Arc<dyn SecretStore> =
        Arc::new(LocalEncryptedSecretStore::new(storage.secret_root()).unwrap());
    let temporary_text: Arc<dyn TemporaryTextOutput> = session_ports.clone();
    let user_notifications = Arc::new(StubUserNotification::new());
    let clipboard: Arc<dyn ClipboardBridge> = Arc::new(UnsupportedClipboardBridge::new());
    let clipboard_text_writer: Arc<dyn ClipboardTextWriter> =
        Arc::new(UnsupportedClipboardTextWriter::new());
    let diagnostics_impl = Arc::new(ConsoleDiagnosticsSink::new());
    let diagnostics: Arc<dyn DiagnosticsSink> = diagnostics_impl.clone();
    let diagnostics_control: Arc<dyn DiagnosticsControl> = diagnostics_impl;

    (
        CompositionRoot::new_with_llm_policy(
            targets,
            microphone,
            audio,
            recording_hud,
            asr,
            Arc::new(StubAsrModelControl::new()),
            output,
            history_port,
            settings,
            secrets,
            temporary_text,
            user_notifications,
            clipboard,
            clipboard_text_writer,
            llm_policy,
            diagnostics,
            diagnostics_control,
        ),
        session_ports,
        history,
        storage,
    )
}

async fn configure_llm(root: &CompositionRoot, base_url: &str) {
    root.llm_configuration
        .set_llm_settings(
            0,
            Some(LlmNonSecretSettings::new(base_url, "assembly-model").unwrap()),
        )
        .await
        .expect("save Provider settings");
    assert_eq!(
        root.llm_configuration
            .set_api_key(remtene_application::ports::SecretValue::new(TEST_API_KEY))
            .await
            .expect("save encrypted API key"),
        LlmApiKeyStatus::Configured
    );
}

async fn run_one_session(root: &CompositionRoot) -> FinishOutcome {
    let session_id = match root.orchestrator.start().await.expect("start Session") {
        StartOutcome::Started { session_id } => session_id,
        outcome => panic!("expected Session start, got {outcome:?}"),
    };
    root.orchestrator
        .finish_recording(session_id)
        .await
        .expect("finish Session")
}

fn request_parts(request_rx: &mpsc::Receiver<Vec<u8>>) -> (String, String) {
    let request = request_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("captured real Provider request");
    let request = String::from_utf8(request).expect("HTTP request is UTF-8");
    let (headers, body) = request
        .split_once("\r\n\r\n")
        .expect("HTTP request has body");
    (headers.to_owned(), body.to_owned())
}

fn assert_minimal_session_request(headers: &str, body: &str, mode: &str) {
    let authorization_count = headers
        .lines()
        .filter(|line| line.to_ascii_lowercase().starts_with("authorization:"))
        .count();
    assert_eq!(authorization_count, 1);
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("authorization: bearer assembly-test-api-key")
    );
    assert!(!body.contains(TEST_API_KEY));

    let body: serde_json::Value = serde_json::from_str(body).expect("valid Provider request JSON");
    let mut body_keys = body
        .as_object()
        .expect("Provider request object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    body_keys.sort_unstable();
    assert_eq!(body_keys, vec!["messages", "model"]);
    assert_eq!(body["model"], "assembly-model");

    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 2);
    for message in messages {
        let mut keys = message
            .as_object()
            .expect("message object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(keys, vec!["content", "role"]);
    }
    let user_content = messages[1]["content"].as_str().expect("user prompt");
    let input: serde_json::Value = serde_json::from_str(
        user_content
            .strip_prefix("[3. Input and goal]\n")
            .expect("canonical input heading"),
    )
    .expect("canonical prompt input JSON");
    let mut input_keys = input
        .as_object()
        .expect("prompt input object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    input_keys.sort_unstable();
    assert_eq!(
        input_keys,
        vec![
            "goal",
            "intent_preference",
            "processing_mode",
            "selected_text",
            "selection_available",
            "spoken_text",
            "system_rules_version",
        ]
    );
    assert_eq!(input["spoken_text"], RAW_ASR_TEXT);
    assert_eq!(input["processing_mode"], mode);
    assert_eq!(input["selected_text"], serde_json::Value::Null);
    assert_eq!(input["selection_available"], false);

    let body_lower = serde_json::to_string(&body)
        .expect("serialize request")
        .to_ascii_lowercase();
    for forbidden in [
        "api_key",
        "audio_ref",
        "audio_path",
        "history",
        "target_ref",
        "target_handle",
        "delivery_id",
        "assembly-target",
    ] {
        assert!(
            !body_lower.contains(forbidden),
            "request body leaked forbidden field {forbidden}"
        );
    }
}

/// 用测试替身组装一个 CompositionRoot，验证组装成功。
#[test]
fn composition_root_assembles_with_test_doubles() {
    let (root, _storage) = test_composition_root("assemble");

    // 组装成功意味着真实 Provider、Controller 与 Orchestrator 已建立。
    assert!(Arc::strong_count(&root.orchestrator) >= 1);
    let configured_settings = futures::executor::block_on(root.llm_configuration.set_llm_settings(
        0,
        Some(LlmNonSecretSettings::new("https://provider.invalid/v1", "assembly-model").unwrap()),
    ))
    .unwrap();
    assert_eq!(configured_settings.version(), 1);
    assert_eq!(
        futures::executor::block_on(root.llm_configuration.set_api_key(
            remtene_application::ports::SecretValue::new("assembly-secret-value"),
        ))
        .unwrap(),
        LlmApiKeyStatus::Configured
    );
    assert_eq!(
        futures::executor::block_on(root.llm_configuration.api_key_status()),
        LlmApiKeyStatus::Configured
    );
    let revealed = futures::executor::block_on(root.llm_configuration.reveal_api_key()).unwrap();
    assert_eq!(revealed.expose(), "assembly-secret-value");
}

/// 证明生产组装使用同一个本地加密 SecretStore 与真实 HTTP Provider。
#[test]
fn production_llm_assembly_reaches_real_provider_with_encrypted_secret() {
    let response = canonical_response("Connection verified.");
    let (base_url, request_rx) = one_shot_llm_server(&response);
    let (root, _storage) = test_composition_root("real-provider");
    tauri::async_runtime::block_on(configure_llm(&root, &base_url));

    assert_eq!(
        tauri::async_runtime::block_on(root.llm_configuration.test_connection()),
        LlmConnectionTestOutcome::Succeeded
    );

    let (headers, body) = request_parts(&request_rx);
    assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1"));
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("authorization: bearer assembly-test-api-key")
    );
    assert!(!body.contains(TEST_API_KEY));

    let body: serde_json::Value = serde_json::from_str(&body).expect("valid Provider request JSON");
    assert_eq!(body["model"], "assembly-model");
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(2));
    assert!(
        body["messages"][1]["content"]
            .as_str()
            .is_some_and(|content| content.contains("RemTene connection test."))
    );
}

/// 证明真实 Provider 的结果能穿过完整 Session，并被插入与保存到历史。
#[test]
fn production_session_uses_real_provider_and_saves_verified_result() {
    let final_text = "张三说：如果预算不超过 12.5 万元，我们才不会取消项目，因为风险仍不确定。";
    let server = CountingLlmServer::canonical(final_text, Duration::ZERO);
    let (root, surfaces, history, _storage) = session_composition_root("session-success");

    let outcome = tauri::async_runtime::block_on(async {
        configure_llm(&root, &server.base_url).await;
        run_one_session(&root).await
    });

    let FinishOutcome::Completed(completion) = outcome else {
        panic!("expected completed Session, got {outcome:?}");
    };
    assert_eq!(completion.final_text, final_text);
    assert_eq!(completion.delivery, DeliveryKind::Inserted);
    assert_eq!(completion.direct_delivery_reason, None);
    for preserved in ["张三", "12.5", "不超过", "才不会取消", "因为", "仍不确定"] {
        assert!(completion.final_text.contains(preserved));
    }
    assert_eq!(surfaces.inserted_texts(), vec![final_text.to_owned()]);
    assert!(surfaces.temporary_texts().is_empty());

    let records = futures::executor::block_on(history.list()).expect("list Session history");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].final_text, final_text);

    let (headers, body) = request_parts(&server.requests);
    assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1"));
    assert_minimal_session_request(&headers, &body, "faithful");
    let request_body: serde_json::Value = serde_json::from_str(&body).expect("request JSON");
    let system_prompt = request_body["messages"][0]["content"]
        .as_str()
        .expect("system prompt");
    assert!(system_prompt.contains("[Faithful examples]"));
    assert!(!system_prompt.contains("[Structured examples]"));
    assert!(system_prompt.contains("[Faithful reconstruction principle]"));
    assert!(system_prompt.contains("what the whole supplied passage is about"));
    assert!(system_prompt.contains("缓成没有失效"));
    assert!(system_prompt.contains("嗯，然后我想说的是"));
    assert!(system_prompt.contains("这个版本可能不能删"));
    assert!(system_prompt.contains("这个版本绝对绝对不能删除"));
    let user_prompt = request_body["messages"][1]["content"]
        .as_str()
        .expect("user prompt");
    assert!(user_prompt.contains("remtene-llm-system-v3"));
    assert!(
        server
            .requests
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "one Session must not send a second LLM request"
    );
}

/// 两种 AI 模式共用同一条生产链路，但 Structured 必须携带同等级纠错、
/// 信息无损重组、情绪保护和条件表格规则，最终文字仍只由严格规范响应进入插入与历史。
#[test]
fn structured_session_preserves_meaning_and_saves_the_formatted_result() {
    let final_text =
        "项目条件：\n- 张三表示，预算不超过 12.5 万元时才不会取消项目；\n- 因为风险仍不确定。";
    let server = CountingLlmServer::canonical(final_text, Duration::ZERO);
    let (root, surfaces, history, _storage) = session_composition_root_with(
        "session-structured",
        ProcessingMode::Structured,
        OpenAiCompatiblePolicy::default(),
    );

    let outcome = tauri::async_runtime::block_on(async {
        configure_llm(&root, &server.base_url).await;
        run_one_session(&root).await
    });
    let FinishOutcome::Completed(completion) = outcome else {
        panic!("expected Structured completion, got {outcome:?}");
    };
    assert_eq!(completion.final_text, final_text);
    assert_eq!(completion.delivery, DeliveryKind::Inserted);
    for preserved in ["张三", "12.5", "不超过", "才不会取消", "因为", "仍不确定"] {
        assert!(completion.final_text.contains(preserved));
    }
    assert_eq!(surfaces.inserted_texts(), vec![final_text.to_owned()]);
    let records = futures::executor::block_on(history.list()).expect("list Structured history");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].final_text, final_text);

    let (headers, body) = request_parts(&server.requests);
    assert_minimal_session_request(&headers, &body, "structured");
    let body: serde_json::Value = serde_json::from_str(&body).expect("request JSON");
    let system_prompt = body["messages"][0]["content"]
        .as_str()
        .expect("system prompt");
    assert!(system_prompt.contains("[Structured examples]"));
    assert!(system_prompt.contains("sentence structure, grammar"));
    assert!(system_prompt.contains("compress redundant emotional wording"));
    assert!(system_prompt.contains("stable, comparable fields"));
    assert!(system_prompt.contains("我真的真的非常失望"));
    assert!(system_prompt.contains("A 方案预算十万"));
    assert!(system_prompt.contains("Carry forward names, numbers, negations, conditions"));
    assert!(!system_prompt.contains("[Faithful examples]"));
    assert!(!system_prompt.contains("麦克 OS"));
}

/// 非秘密设置存在但没有 API Key 时，不建立网络请求，直接交付本地 ASR。
#[test]
fn session_without_api_key_delivers_local_asr_without_network() {
    let server = CountingLlmServer::canonical("must not be requested", Duration::ZERO);
    let (root, surfaces, history, _storage) = session_composition_root("session-without-api-key");
    let outcome = tauri::async_runtime::block_on(async {
        root.llm_configuration
            .set_llm_settings(
                0,
                Some(
                    LlmNonSecretSettings::new(&server.base_url, "assembly-model")
                        .expect("valid settings"),
                ),
            )
            .await
            .expect("save non-secret settings");
        run_one_session(&root).await
    });

    let FinishOutcome::Completed(completion) = outcome else {
        panic!("expected local-ASR completion, got {outcome:?}");
    };
    assert_eq!(completion.final_text, RAW_ASR_TEXT);
    assert_eq!(completion.delivery, DeliveryKind::Inserted);
    assert_eq!(
        completion.direct_delivery_reason,
        Some(DirectDeliveryReason::LlmNotConfigured)
    );
    assert_eq!(surfaces.inserted_texts(), vec![RAW_ASR_TEXT.to_owned()]);
    assert!(surfaces.temporary_texts().is_empty());
    let records = futures::executor::block_on(history.list()).expect("list local-ASR history");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].final_text, RAW_ASR_TEXT);
    assert!(
        server
            .requests
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "missing API Key must prevent the HTTP request"
    );
}

/// 所有 Provider 失败都丢弃远端输出，保留本地 ASR，并走同一个临时文本安全回退。
#[test]
fn configured_provider_failures_preserve_local_asr_and_never_insert_untrusted_text() {
    let timeout_policy = OpenAiCompatiblePolicy {
        request_timeout: Duration::from_millis(25),
        connect_timeout: Duration::from_millis(100),
        max_response_bytes: 256 * 1024,
    };
    let cases = vec![
        (
            "authentication",
            CountingLlmServer::status("401 Unauthorized"),
            OpenAiCompatiblePolicy::default(),
        ),
        (
            "rate-limit",
            CountingLlmServer::status("429 Too Many Requests"),
            OpenAiCompatiblePolicy::default(),
        ),
        (
            "invalid-response",
            CountingLlmServer::content("not canonical JSON", Duration::ZERO),
            OpenAiCompatiblePolicy::default(),
        ),
        (
            "network-disconnect",
            CountingLlmServer::disconnect(),
            OpenAiCompatiblePolicy::default(),
        ),
        (
            "timeout",
            CountingLlmServer::canonical("late untrusted text", Duration::from_millis(120)),
            timeout_policy,
        ),
    ];

    for (tag, server, policy) in cases {
        let (root, surfaces, history, _storage) =
            session_composition_root_with(tag, ProcessingMode::Faithful, policy);
        let outcome = tauri::async_runtime::block_on(async {
            configure_llm(&root, &server.base_url).await;
            run_one_session(&root).await
        });

        let FinishOutcome::Completed(completion) = outcome else {
            panic!("{tag}: expected safe fallback completion, got {outcome:?}");
        };
        assert_eq!(completion.final_text, RAW_ASR_TEXT, "{tag}");
        assert_eq!(completion.delivery, DeliveryKind::TemporaryText, "{tag}");
        assert!(surfaces.inserted_texts().is_empty(), "{tag}");
        assert_eq!(
            surfaces.temporary_texts(),
            vec![(RAW_ASR_TEXT.to_owned(), TemporaryTextStatus::LlmFallback)],
            "{tag}"
        );
        let records = futures::executor::block_on(history.list()).expect("list fallback history");
        assert_eq!(records.len(), 1, "{tag}");
        assert_eq!(records[0].final_text, RAW_ASR_TEXT, "{tag}");

        let (headers, body) = request_parts(&server.requests);
        assert_minimal_session_request(&headers, &body, "faithful");
        assert!(
            server
                .requests
                .recv_timeout(Duration::from_millis(30))
                .is_err(),
            "{tag}: one Session must not retry the LLM"
        );
    }
}

/// 正式退出会取消正在等待的 HTTP 请求；服务端随后返回的迟到结果不能插入或写历史。
#[test]
fn quit_cancels_in_flight_llm_and_rejects_the_late_response() {
    let server = CountingLlmServer::canonical("迟到的不可信模型文字", Duration::from_millis(180));
    let policy = OpenAiCompatiblePolicy {
        request_timeout: Duration::from_secs(1),
        connect_timeout: Duration::from_millis(100),
        max_response_bytes: 256 * 1024,
    };
    let (root, surfaces, history, _storage) =
        session_composition_root_with("session-late", ProcessingMode::Faithful, policy);
    tauri::async_runtime::block_on(configure_llm(&root, &server.base_url));
    let session_id =
        match tauri::async_runtime::block_on(root.orchestrator.start()).expect("start Session") {
            StartOutcome::Started { session_id } => session_id,
            outcome => panic!("expected Session start, got {outcome:?}"),
        };

    let finishing = Arc::clone(&root.orchestrator);
    let finish_worker = thread::spawn(move || {
        tauri::async_runtime::block_on(finishing.finish_recording(session_id))
            .expect("finish workflow")
    });
    let (headers, body) = request_parts(&server.requests);
    assert_minimal_session_request(&headers, &body, "faithful");

    assert_eq!(
        tauri::async_runtime::block_on(root.orchestrator.quit()).expect("formal quit"),
        QuitOutcome::Terminated(session_id)
    );
    assert_eq!(
        finish_worker.join().expect("finish worker"),
        FinishOutcome::Discarded
    );
    assert!(surfaces.inserted_texts().is_empty());
    assert!(surfaces.temporary_texts().is_empty());
    assert!(
        futures::executor::block_on(history.list())
            .expect("history after quit")
            .is_empty()
    );

    thread::sleep(Duration::from_millis(220));
    assert!(surfaces.inserted_texts().is_empty());
    assert!(surfaces.temporary_texts().is_empty());
    assert!(
        futures::executor::block_on(history.list())
            .expect("history after late response")
            .is_empty()
    );
}

/// 证明真实 Provider 返回不可解析内容时，完整 Session 保留本地 ASR 原文且不误插入。
#[test]
fn production_session_preserves_local_asr_when_provider_response_is_invalid() {
    let (base_url, request_rx) = one_shot_llm_server("not canonical JSON");
    let (root, surfaces, history, _storage) = session_composition_root("session-fallback");

    let outcome = tauri::async_runtime::block_on(async {
        configure_llm(&root, &base_url).await;
        run_one_session(&root).await
    });

    let FinishOutcome::Completed(completion) = outcome else {
        panic!("expected completed fallback Session, got {outcome:?}");
    };
    assert_eq!(completion.final_text, RAW_ASR_TEXT);
    assert_eq!(completion.delivery, DeliveryKind::TemporaryText);
    assert!(surfaces.inserted_texts().is_empty());
    assert_eq!(
        surfaces.temporary_texts(),
        vec![(RAW_ASR_TEXT.to_owned(), TemporaryTextStatus::LlmFallback)]
    );

    let records = futures::executor::block_on(history.list()).expect("list fallback history");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].final_text, RAW_ASR_TEXT);

    let (headers, body) = request_parts(&request_rx);
    assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1"));
    assert!(!body.contains(TEST_API_KEY));
}

/// 验证真实 FileHistoryStore 能作为组装的一部分正常读写。
#[test]
fn assembled_history_store_persists_records() {
    use remtene_application::ports::{HistoryRecord, LifecycleFence};
    use remtene_domain::{DeliveryId, TimestampMs};

    let storage = TestStorage::new("persist");
    let history = FileHistoryStore::new(storage.history_path()).unwrap();

    let record = HistoryRecord {
        delivery_id: DeliveryId::new(),
        final_text: "组装测试文本".to_owned(),
        created_at: TimestampMs::new(1_700_000_000_000),
    };

    futures::executor::block_on(history.save_with_policy(
        record,
        &session_settings(ProcessingMode::Raw),
        LifecycleFence::new(),
    ))
    .unwrap();
    let listed = futures::executor::block_on(history.list()).unwrap();

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].final_text, "组装测试文本");
    assert_eq!(listed[0].created_at.get(), 1_700_000_000_000);
}
