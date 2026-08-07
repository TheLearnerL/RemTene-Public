import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import {
  parseWorkerProtocolGoldenFixture,
  WORKER_CONTRACT_VERSION,
} from "./worker-contract.ts";

const fixtureUrl = new URL(
  "../../../../crates/remtene-contracts/fixtures/worker-protocol-v1.json",
  import.meta.url,
);

function readFixture(): unknown {
  return JSON.parse(readFileSync(fixtureUrl, "utf8")) as unknown;
}

void test("the shared Worker golden fixture validates in TypeScript", () => {
  const fixture = parseWorkerProtocolGoldenFixture(readFixture());

  assert.equal(fixture.contract_version, WORKER_CONTRACT_VERSION);
  assert.deepEqual(
    fixture.core_to_worker.map((envelope) => envelope.kind),
    ["hello", "health_check", "transcribe", "cancel", "shutdown"],
  );
  assert.deepEqual(
    fixture.worker_to_core.map((envelope) => envelope.kind),
    ["ready", "health_result", "transcript", "cancelled", "error", "shutdown_complete"],
  );
});

void test("an audio path cannot cross the Worker contract as an artifact id", () => {
  const fixture = parseWorkerProtocolGoldenFixture(readFixture());
  const transcribe = fixture.core_to_worker.find((envelope) => envelope.kind === "transcribe");
  assert.ok(transcribe);
  if (transcribe.kind !== "transcribe") assert.fail("expected the transcribe fixture");
  transcribe.payload.audio_artifact_id = "../recording.wav";

  assert.throws(() => parseWorkerProtocolGoldenFixture(fixture), /non-nil UUID/);
});

void test("unknown kinds and extra fields fail closed", () => {
  const fixture = readFixture();
  if (typeof fixture !== "object" || fixture === null) assert.fail("expected fixture object");
  const invalid = structuredClone(fixture) as Record<string, unknown>;
  const coreMessages = invalid.core_to_worker;
  if (!Array.isArray(coreMessages) || coreMessages.length === 0) assert.fail("expected messages");
  const first: unknown = coreMessages[0];
  if (typeof first !== "object" || first === null || Array.isArray(first)) {
    assert.fail("expected envelope");
  }
  const firstRecord = first as Record<string, unknown>;
  firstRecord.kind = "arbitrary_command";
  firstRecord.secret = "must-not-cross";

  assert.throws(() => parseWorkerProtocolGoldenFixture(invalid), /unexpected fields/);
});

void test("every Worker envelope and payload rejects unknown fields", () => {
  const source = readFixture();
  const directions = ["core_to_worker", "worker_to_core"] as const;

  for (const direction of directions) {
    const messages = expectTestArray(expectTestRecord(source)[direction]);
    for (let index = 0; index < messages.length; index += 1) {
      const invalidEnvelope = structuredClone(source);
      const envelope = expectTestRecord(
        expectTestArray(expectTestRecord(invalidEnvelope)[direction])[index],
      );
      envelope.unexpected_envelope_field = true;
      assert.throws(
        () => parseWorkerProtocolGoldenFixture(invalidEnvelope),
        /unexpected fields/,
      );

      const invalidPayload = structuredClone(source);
      const payloadEnvelope = expectTestRecord(
        expectTestArray(expectTestRecord(invalidPayload)[direction])[index],
      );
      const payload = expectTestRecord(payloadEnvelope.payload);
      payload.unexpected_payload_field = true;
      assert.throws(
        () => parseWorkerProtocolGoldenFixture(invalidPayload),
        /unexpected fields/,
      );
    }
  }
});

void test("Worker errors use all-or-nothing envelope correlation", () => {
  const parsed = parseWorkerProtocolGoldenFixture(readFixture());
  const requestError = parsed.worker_to_core.find((envelope) => envelope.kind === "error");
  assert.ok(requestError);
  if (requestError.kind !== "error") assert.fail("expected the error fixture");
  assert.ok(requestError.session_id);
  assert.ok(requestError.request_id);
  assert.equal("session_id" in requestError.payload, false);
  assert.equal("request_id" in requestError.payload, false);

  const missingSession = structuredClone(readFixture());
  const missingSessionError = findTestErrorEnvelope(missingSession);
  missingSessionError.session_id = null;
  assert.throws(
    () => parseWorkerProtocolGoldenFixture(missingSession),
    /must contain both session_id and request_id or neither/,
  );

  const missingRequest = structuredClone(readFixture());
  const missingRequestError = findTestErrorEnvelope(missingRequest);
  missingRequestError.request_id = null;
  assert.throws(
    () => parseWorkerProtocolGoldenFixture(missingRequest),
    /must contain both session_id and request_id or neither/,
  );

  const globalError = structuredClone(readFixture());
  const globalErrorEnvelope = findTestErrorEnvelope(globalError);
  globalErrorEnvelope.session_id = null;
  globalErrorEnvelope.request_id = null;
  const parsedGlobal = parseWorkerProtocolGoldenFixture(globalError);
  const accepted = parsedGlobal.worker_to_core.find((envelope) => envelope.kind === "error");
  assert.ok(accepted);
  assert.equal(accepted.session_id, null);
  assert.equal(accepted.request_id, null);
});

function findTestErrorEnvelope(fixture: unknown): Record<string, unknown> {
  const messages = expectTestArray(expectTestRecord(fixture).worker_to_core);
  const error = messages.find((message) => expectTestRecord(message).kind === "error");
  assert.ok(error);
  return expectTestRecord(error);
}

function expectTestRecord(value: unknown): Record<string, unknown> {
  assert.equal(typeof value, "object");
  assert.notEqual(value, null);
  assert.equal(Array.isArray(value), false);
  return value as Record<string, unknown>;
}

function expectTestArray(value: unknown): unknown[] {
  assert.ok(Array.isArray(value));
  return value;
}
