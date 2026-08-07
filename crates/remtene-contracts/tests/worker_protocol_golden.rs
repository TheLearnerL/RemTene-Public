use remtene_contracts::{
    CONTRACT_VERSION, CoreToWorkerEnvelope, CoreToWorkerMessage, WorkerMessageKind,
    WorkerProtocolError, WorkerProtocolPhase, WorkerProtocolState, WorkerToCoreEnvelope,
    WorkerToCoreMessage,
};
use serde::Deserialize;
use serde_json::Value;

const GOLDEN_FIXTURE: &str = include_str!("../fixtures/worker-protocol-v1.json");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoldenFixture {
    contract_version: u16,
    core_to_worker: Vec<CoreToWorkerEnvelope>,
    worker_to_core: Vec<WorkerToCoreEnvelope>,
}

#[test]
fn golden_fixture_round_trips_and_validates_in_both_directions() {
    let source: Value = serde_json::from_str(GOLDEN_FIXTURE).expect("fixture JSON must be valid");
    let fixture: GoldenFixture =
        serde_json::from_value(source.clone()).expect("fixture must match Rust DTOs");

    assert_eq!(fixture.contract_version, CONTRACT_VERSION);
    fixture
        .core_to_worker
        .iter()
        .try_for_each(CoreToWorkerEnvelope::validate)
        .expect("every Core to Worker message must validate");
    fixture
        .worker_to_core
        .iter()
        .try_for_each(WorkerToCoreEnvelope::validate)
        .expect("every Worker to Core message must validate");

    let encoded = serde_json::to_value(&fixture.core_to_worker)
        .expect("Core to Worker messages must serialize");
    assert_eq!(encoded, source["core_to_worker"]);

    let encoded = serde_json::to_value(&fixture.worker_to_core)
        .expect("Worker to Core messages must serialize");
    assert_eq!(encoded, source["worker_to_core"]);
}

#[test]
fn golden_fixture_proves_handshake_and_shutdown_guards() {
    let fixture: GoldenFixture =
        serde_json::from_str(GOLDEN_FIXTURE).expect("fixture must match Rust DTOs");
    let mut state = WorkerProtocolState::new();

    state
        .observe_core(core_message(&fixture, WorkerMessageKind::Hello))
        .expect("hello must begin negotiation");
    assert_eq!(state.phase(), WorkerProtocolPhase::AwaitingReady);

    state
        .observe_worker(worker_message(&fixture, WorkerMessageKind::Ready))
        .expect("compatible ready must finish negotiation");
    assert_eq!(state.phase(), WorkerProtocolPhase::Ready);
    assert_eq!(state.negotiated_version(), Some(CONTRACT_VERSION));

    state
        .observe_core(core_message(&fixture, WorkerMessageKind::HealthCheck))
        .expect("health check must be legal after ready");
    state
        .observe_worker(worker_message(&fixture, WorkerMessageKind::HealthResult))
        .expect("health result must be legal after ready");

    let mut request_error_state = state.clone();
    request_error_state
        .observe_core(core_message(&fixture, WorkerMessageKind::Transcribe))
        .expect("transcription must register its envelope correlation");
    request_error_state
        .observe_worker(worker_message(&fixture, WorkerMessageKind::Error))
        .expect("a matching non-fatal request error must keep the protocol usable");
    assert_eq!(request_error_state.phase(), WorkerProtocolPhase::Ready);

    state
        .observe_core(core_message(&fixture, WorkerMessageKind::Transcribe))
        .expect("transcription must be legal after ready");
    state
        .observe_worker(worker_message(&fixture, WorkerMessageKind::Transcript))
        .expect("transcript must be legal after ready");
    state
        .observe_core(core_message(&fixture, WorkerMessageKind::Shutdown))
        .expect("shutdown must enter the closing phase");
    assert_eq!(state.phase(), WorkerProtocolPhase::ShuttingDown);
    state
        .observe_worker(worker_message(
            &fixture,
            WorkerMessageKind::ShutdownComplete,
        ))
        .expect("shutdown acknowledgement must close the protocol");
    assert_eq!(state.phase(), WorkerProtocolPhase::Closed);
}

#[test]
fn rust_decoder_rejects_unknown_envelope_and_payload_fields_for_every_variant() {
    let source: Value = serde_json::from_str(GOLDEN_FIXTURE).expect("fixture JSON must be valid");

    for envelope in source["core_to_worker"]
        .as_array()
        .expect("fixture must contain Core envelopes")
    {
        let mut invalid_envelope = envelope.clone();
        invalid_envelope
            .as_object_mut()
            .expect("envelope must be an object")
            .insert("unexpected_envelope_field".to_owned(), Value::Bool(true));
        assert!(
            serde_json::from_value::<CoreToWorkerEnvelope>(invalid_envelope).is_err(),
            "Core envelope kind {} accepted an unknown field",
            envelope["kind"]
        );

        let mut invalid_payload = envelope.clone();
        invalid_payload["payload"]
            .as_object_mut()
            .expect("payload must be an object")
            .insert("unexpected_payload_field".to_owned(), Value::Bool(true));
        assert!(
            serde_json::from_value::<CoreToWorkerEnvelope>(invalid_payload).is_err(),
            "Core payload kind {} accepted an unknown field",
            envelope["kind"]
        );
    }

    for envelope in source["worker_to_core"]
        .as_array()
        .expect("fixture must contain Worker envelopes")
    {
        let mut invalid_envelope = envelope.clone();
        invalid_envelope
            .as_object_mut()
            .expect("envelope must be an object")
            .insert("unexpected_envelope_field".to_owned(), Value::Bool(true));
        assert!(
            serde_json::from_value::<WorkerToCoreEnvelope>(invalid_envelope).is_err(),
            "Worker envelope kind {} accepted an unknown field",
            envelope["kind"]
        );

        let mut invalid_payload = envelope.clone();
        invalid_payload["payload"]
            .as_object_mut()
            .expect("payload must be an object")
            .insert("unexpected_payload_field".to_owned(), Value::Bool(true));
        assert!(
            serde_json::from_value::<WorkerToCoreEnvelope>(invalid_payload).is_err(),
            "Worker payload kind {} accepted an unknown field",
            envelope["kind"]
        );
    }

    let mut nested_audio_format = source["core_to_worker"][2].clone();
    nested_audio_format["payload"]["audio_format"]
        .as_object_mut()
        .expect("audio format must be an object")
        .insert("unexpected_audio_field".to_owned(), Value::Bool(true));
    assert!(serde_json::from_value::<CoreToWorkerEnvelope>(nested_audio_format).is_err());
}

#[test]
fn error_association_is_all_or_nothing_and_uses_the_envelope_as_authority() {
    let fixture: GoldenFixture =
        serde_json::from_str(GOLDEN_FIXTURE).expect("fixture must match Rust DTOs");
    let request_error = worker_message(&fixture, WorkerMessageKind::Error).clone();

    let mut global_error = request_error.clone();
    global_error.session_id = None;
    global_error.request_id = None;
    global_error
        .validate()
        .expect("global errors must carry no request correlation");

    let mut missing_session = request_error.clone();
    missing_session.session_id = None;
    assert!(matches!(
        missing_session.validate(),
        Err(WorkerProtocolError::MissingCorrelation("session_id"))
    ));

    let mut missing_request = request_error.clone();
    missing_request.request_id = None;
    assert!(matches!(
        missing_request.validate(),
        Err(WorkerProtocolError::MissingCorrelation("request_id"))
    ));

    let WorkerToCoreMessage::Error(payload) = &request_error.message else {
        panic!("fixture error must contain a WorkerError payload")
    };
    let payload = serde_json::to_value(payload).expect("WorkerError must serialize");
    assert!(payload.get("session_id").is_none());
    assert!(payload.get("request_id").is_none());

    let mut state = ready_state(&fixture);
    state
        .observe_core(core_message(&fixture, WorkerMessageKind::Transcribe))
        .expect("transcription must register its envelope correlation");

    let mut stale_error = request_error.clone();
    stale_error.request_id = Some(uuid::Uuid::new_v4());
    let WorkerToCoreMessage::Error(stale_payload) = &mut stale_error.message else {
        panic!("fixture error must contain a WorkerError payload")
    };
    stale_payload.fatal = true;
    stale_error
        .validate()
        .expect("the envelope is structurally a request-level error");
    assert!(matches!(
        state.observe_worker(&stale_error),
        Err(WorkerProtocolError::UnknownRequestCorrelation { .. })
    ));
    assert_eq!(state.phase(), WorkerProtocolPhase::Ready);

    state
        .observe_worker(&request_error)
        .expect("the matching envelope correlation must complete the request");
    assert!(matches!(
        state.observe_worker(&request_error),
        Err(WorkerProtocolError::UnknownRequestCorrelation { .. })
    ));
}

#[test]
fn mismatched_correlations_and_versions_fail_closed() {
    let fixture: GoldenFixture =
        serde_json::from_str(GOLDEN_FIXTURE).expect("fixture must match Rust DTOs");
    let mut transcribe = core_message(&fixture, WorkerMessageKind::Transcribe).clone();
    transcribe.request_id = Some(uuid::Uuid::new_v4());
    assert!(transcribe.validate().is_err());

    let mut hello = core_message(&fixture, WorkerMessageKind::Hello).clone();
    hello.contract_version = CONTRACT_VERSION + 1;
    assert!(hello.validate().is_err());
}

fn core_message(fixture: &GoldenFixture, kind: WorkerMessageKind) -> &CoreToWorkerEnvelope {
    fixture
        .core_to_worker
        .iter()
        .find(|envelope| envelope.message.kind() == kind)
        .expect("fixture must contain every Core message kind")
}

fn worker_message(fixture: &GoldenFixture, kind: WorkerMessageKind) -> &WorkerToCoreEnvelope {
    fixture
        .worker_to_core
        .iter()
        .find(|envelope| envelope.message.kind() == kind)
        .expect("fixture must contain every Worker message kind")
}

fn ready_state(fixture: &GoldenFixture) -> WorkerProtocolState {
    let mut state = WorkerProtocolState::new();
    state
        .observe_core(core_message(fixture, WorkerMessageKind::Hello))
        .expect("hello must begin negotiation");
    state
        .observe_worker(worker_message(fixture, WorkerMessageKind::Ready))
        .expect("ready must complete negotiation");
    state
}

#[test]
fn fixture_contains_every_closed_message_variant() {
    let fixture: GoldenFixture =
        serde_json::from_str(GOLDEN_FIXTURE).expect("fixture must match Rust DTOs");

    assert!(
        fixture
            .core_to_worker
            .iter()
            .any(|envelope| matches!(envelope.message, CoreToWorkerMessage::Cancel(_)))
    );
    assert!(
        fixture
            .worker_to_core
            .iter()
            .any(|envelope| matches!(envelope.message, WorkerToCoreMessage::Cancelled(_)))
    );
}
