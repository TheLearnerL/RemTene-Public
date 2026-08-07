/**
 * Test-only mirror of the local ASR Worker protocol.
 *
 * The Renderer must never call the Worker. These types and validators only make the checked-in
 * golden fixture participate in TypeScript type checking until the formal Worker exists and
 * consumes `remtene-contracts` directly.
 */

export const WORKER_CONTRACT_VERSION = 1 as const;

const CORE_MESSAGE_KINDS = ["hello", "health_check", "transcribe", "cancel", "shutdown"] as const;
const WORKER_MESSAGE_KINDS = [
  "ready",
  "health_result",
  "transcript",
  "cancelled",
  "error",
  "shutdown_complete",
] as const;
const ENGINE_IDS = ["qwen", "whisper"] as const;
const CAPABILITIES = [
  "health_check",
  "final_transcript",
  "cancellation",
  "graceful_shutdown",
] as const;
const HEALTH_STATUSES = ["healthy", "unhealthy", "missing", "incompatible"] as const;
const ERROR_CODES = [
  "protocol_incompatible",
  "invalid_request",
  "engine_unavailable",
  "model_missing",
  "model_incompatible",
  "inference_failed",
  "cancellation_failed",
  "internal",
] as const;

export type WorkerEngineId = (typeof ENGINE_IDS)[number];
export type WorkerCapability = (typeof CAPABILITIES)[number];
export type HealthStatus = (typeof HEALTH_STATUSES)[number];
export type WorkerErrorCode = (typeof ERROR_CODES)[number];

interface EnvelopeBase {
  contract_version: typeof WORKER_CONTRACT_VERSION;
  message_id: string;
  session_id: string | null;
  request_id: string | null;
  sent_at: string;
}

export interface CoreHello {
  supported_protocol_versions: number[];
  core_version: string;
  required_capabilities: WorkerCapability[];
}

export interface HealthCheckRequest {
  engine_id: WorkerEngineId;
  model_id: string;
}

export interface AudioFormatDto {
  sample_rate_hz: number;
  channels: number;
  bits_per_sample: number;
}

export interface TranscribeRequest {
  session_id: string;
  request_id: string;
  engine_id: WorkerEngineId;
  model_id: string;
  audio_artifact_id: string;
  audio_format: AudioFormatDto;
  language_hint: string | null;
  deadline_ms: number;
}

export interface CancelRequest {
  session_id: string;
  request_id: string;
}

export interface ShutdownRequest {
  grace_period_ms: number;
}

export interface WorkerReady {
  protocol_version: number;
  worker_version: string;
  supported_engines: WorkerEngineId[];
  runtime_id: string;
  capabilities: WorkerCapability[];
  build_signature_id: string;
}

export interface HealthResult {
  engine_id: WorkerEngineId;
  model_id: string;
  model_version: string;
  status: HealthStatus;
  device_class: string;
  safe_error_code: string | null;
}

export interface TranscriptResult {
  session_id: string;
  request_id: string;
  engine_id: WorkerEngineId;
  model_id: string;
  final_text: string;
  detected_language: string | null;
  audio_duration_ms: number;
  inference_duration_ms: number;
}

export interface CancelledResult {
  session_id: string;
  request_id: string;
}

export interface WorkerError {
  code: WorkerErrorCode;
  retryable: boolean;
  fatal: boolean;
  safe_message_key: string;
}

export interface ShutdownComplete {
  worker_version: string;
}

export type CoreToWorkerEnvelope = EnvelopeBase &
  (
    | { kind: "hello"; payload: CoreHello }
    | { kind: "health_check"; payload: HealthCheckRequest }
    | { kind: "transcribe"; payload: TranscribeRequest }
    | { kind: "cancel"; payload: CancelRequest }
    | { kind: "shutdown"; payload: ShutdownRequest }
  );

export type WorkerToCoreEnvelope = EnvelopeBase &
  (
    | { kind: "ready"; payload: WorkerReady }
    | { kind: "health_result"; payload: HealthResult }
    | { kind: "transcript"; payload: TranscriptResult }
    | { kind: "cancelled"; payload: CancelledResult }
    | { kind: "error"; payload: WorkerError }
    | { kind: "shutdown_complete"; payload: ShutdownComplete }
  );

export interface WorkerProtocolGoldenFixture {
  contract_version: typeof WORKER_CONTRACT_VERSION;
  core_to_worker: CoreToWorkerEnvelope[];
  worker_to_core: WorkerToCoreEnvelope[];
}

interface ParsedEnvelopeBase extends EnvelopeBase {
  kind: string;
  payload: unknown;
}

export function parseWorkerProtocolGoldenFixture(value: unknown): WorkerProtocolGoldenFixture {
  const record = expectRecord(value, "fixture");
  expectExactKeys(record, ["contract_version", "core_to_worker", "worker_to_core"], "fixture");
  expectContractVersion(record.contract_version);

  return {
    contract_version: WORKER_CONTRACT_VERSION,
    core_to_worker: expectArray(record.core_to_worker, "core_to_worker").map(parseCoreEnvelope),
    worker_to_core: expectArray(record.worker_to_core, "worker_to_core").map(parseWorkerEnvelope),
  };
}

function parseCoreEnvelope(value: unknown, index: number): CoreToWorkerEnvelope {
  const context = `core_to_worker[${index}]`;
  const base = parseEnvelopeBase(value, context);
  const kind = expectOneOf(base.kind, CORE_MESSAGE_KINDS, `${context}.kind`);

  switch (kind) {
    case "hello":
      expectCorrelations(base, false, false, context);
      return { ...base, kind, payload: parseCoreHello(base.payload, `${context}.payload`) };
    case "health_check":
      expectCorrelations(base, false, true, context);
      return { ...base, kind, payload: parseHealthCheck(base.payload, `${context}.payload`) };
    case "transcribe": {
      expectCorrelations(base, true, true, context);
      const payload = parseTranscribe(base.payload, `${context}.payload`);
      expectMatchingCorrelation(base, payload, context);
      return { ...base, kind, payload };
    }
    case "cancel": {
      expectCorrelations(base, true, true, context);
      const payload = parseCancel(base.payload, `${context}.payload`);
      expectMatchingCorrelation(base, payload, context);
      return { ...base, kind, payload };
    }
    case "shutdown":
      expectCorrelations(base, false, false, context);
      return { ...base, kind, payload: parseShutdown(base.payload, `${context}.payload`) };
  }
}

function parseWorkerEnvelope(value: unknown, index: number): WorkerToCoreEnvelope {
  const context = `worker_to_core[${index}]`;
  const base = parseEnvelopeBase(value, context);
  const kind = expectOneOf(base.kind, WORKER_MESSAGE_KINDS, `${context}.kind`);

  switch (kind) {
    case "ready":
      expectCorrelations(base, false, false, context);
      return { ...base, kind, payload: parseWorkerReady(base.payload, `${context}.payload`) };
    case "health_result":
      expectCorrelations(base, false, true, context);
      return { ...base, kind, payload: parseHealthResult(base.payload, `${context}.payload`) };
    case "transcript": {
      expectCorrelations(base, true, true, context);
      const payload = parseTranscript(base.payload, `${context}.payload`);
      expectMatchingCorrelation(base, payload, context);
      return { ...base, kind, payload };
    }
    case "cancelled": {
      expectCorrelations(base, true, true, context);
      const payload = parseCancel(base.payload, `${context}.payload`);
      expectMatchingCorrelation(base, payload, context);
      return { ...base, kind, payload };
    }
    case "error":
      expectErrorCorrelations(base, context);
      return { ...base, kind, payload: parseWorkerError(base.payload, `${context}.payload`) };
    case "shutdown_complete":
      expectCorrelations(base, false, false, context);
      return { ...base, kind, payload: parseShutdownComplete(base.payload, `${context}.payload`) };
  }
}

function parseEnvelopeBase(value: unknown, context: string): ParsedEnvelopeBase {
  const record = expectRecord(value, context);
  expectExactKeys(
    record,
    ["contract_version", "message_id", "session_id", "request_id", "sent_at", "kind", "payload"],
    context,
  );
  expectContractVersion(record.contract_version);

  return {
    contract_version: WORKER_CONTRACT_VERSION,
    message_id: expectUuid(record.message_id, `${context}.message_id`),
    session_id: expectNullableUuid(record.session_id, `${context}.session_id`),
    request_id: expectNullableUuid(record.request_id, `${context}.request_id`),
    sent_at: expectUtcRfc3339(record.sent_at, `${context}.sent_at`),
    kind: expectString(record.kind, `${context}.kind`),
    payload: record.payload,
  };
}

function parseCoreHello(value: unknown, context: string): CoreHello {
  const record = expectRecord(value, context);
  expectExactKeys(
    record,
    ["supported_protocol_versions", "core_version", "required_capabilities"],
    context,
  );
  const supportedVersions = expectArray(
    record.supported_protocol_versions,
    `${context}.supported_protocol_versions`,
  ).map((entry, index) => expectPositiveInteger(entry, `${context}.supported_protocol_versions[${index}]`));
  if (!supportedVersions.includes(WORKER_CONTRACT_VERSION)) {
    throw new TypeError(`${context} must offer the current protocol version`);
  }
  expectUnique(supportedVersions, `${context}.supported_protocol_versions`);
  const requiredCapabilities = parseCapabilities(
    record.required_capabilities,
    `${context}.required_capabilities`,
  );
  return {
    supported_protocol_versions: supportedVersions,
    core_version: expectNonEmptyString(record.core_version, `${context}.core_version`),
    required_capabilities: requiredCapabilities,
  };
}

function parseHealthCheck(value: unknown, context: string): HealthCheckRequest {
  const record = expectRecord(value, context);
  expectExactKeys(record, ["engine_id", "model_id"], context);
  return {
    engine_id: expectOneOf(record.engine_id, ENGINE_IDS, `${context}.engine_id`),
    model_id: expectNonEmptyString(record.model_id, `${context}.model_id`),
  };
}

function parseTranscribe(value: unknown, context: string): TranscribeRequest {
  const record = expectRecord(value, context);
  expectExactKeys(
    record,
    [
      "session_id",
      "request_id",
      "engine_id",
      "model_id",
      "audio_artifact_id",
      "audio_format",
      "language_hint",
      "deadline_ms",
    ],
    context,
  );
  return {
    session_id: expectUuid(record.session_id, `${context}.session_id`),
    request_id: expectUuid(record.request_id, `${context}.request_id`),
    engine_id: expectOneOf(record.engine_id, ENGINE_IDS, `${context}.engine_id`),
    model_id: expectNonEmptyString(record.model_id, `${context}.model_id`),
    audio_artifact_id: expectUuid(record.audio_artifact_id, `${context}.audio_artifact_id`),
    audio_format: parseAudioFormat(record.audio_format, `${context}.audio_format`),
    language_hint: expectNullableNonEmptyString(record.language_hint, `${context}.language_hint`),
    deadline_ms: expectPositiveInteger(record.deadline_ms, `${context}.deadline_ms`),
  };
}

function parseAudioFormat(value: unknown, context: string): AudioFormatDto {
  const record = expectRecord(value, context);
  expectExactKeys(record, ["sample_rate_hz", "channels", "bits_per_sample"], context);
  return {
    sample_rate_hz: expectPositiveInteger(record.sample_rate_hz, `${context}.sample_rate_hz`),
    channels: expectPositiveInteger(record.channels, `${context}.channels`),
    bits_per_sample: expectPositiveInteger(record.bits_per_sample, `${context}.bits_per_sample`),
  };
}

function parseCancel(value: unknown, context: string): CancelRequest {
  const record = expectRecord(value, context);
  expectExactKeys(record, ["session_id", "request_id"], context);
  return {
    session_id: expectUuid(record.session_id, `${context}.session_id`),
    request_id: expectUuid(record.request_id, `${context}.request_id`),
  };
}

function parseShutdown(value: unknown, context: string): ShutdownRequest {
  const record = expectRecord(value, context);
  expectExactKeys(record, ["grace_period_ms"], context);
  return { grace_period_ms: expectNonNegativeInteger(record.grace_period_ms, `${context}.grace_period_ms`) };
}

function parseWorkerReady(value: unknown, context: string): WorkerReady {
  const record = expectRecord(value, context);
  expectExactKeys(
    record,
    [
      "protocol_version",
      "worker_version",
      "supported_engines",
      "runtime_id",
      "capabilities",
      "build_signature_id",
    ],
    context,
  );
  const supportedEngines = expectArray(record.supported_engines, `${context}.supported_engines`).map(
    (entry, index) => expectOneOf(entry, ENGINE_IDS, `${context}.supported_engines[${index}]`),
  );
  if (supportedEngines.length === 0) throw new TypeError(`${context}.supported_engines is empty`);
  expectUnique(supportedEngines, `${context}.supported_engines`);
  return {
    protocol_version: expectPositiveInteger(record.protocol_version, `${context}.protocol_version`),
    worker_version: expectNonEmptyString(record.worker_version, `${context}.worker_version`),
    supported_engines: supportedEngines,
    runtime_id: expectNonEmptyString(record.runtime_id, `${context}.runtime_id`),
    capabilities: parseCapabilities(record.capabilities, `${context}.capabilities`),
    build_signature_id: expectNonEmptyString(record.build_signature_id, `${context}.build_signature_id`),
  };
}

function parseHealthResult(value: unknown, context: string): HealthResult {
  const record = expectRecord(value, context);
  expectExactKeys(
    record,
    ["engine_id", "model_id", "model_version", "status", "device_class", "safe_error_code"],
    context,
  );
  return {
    engine_id: expectOneOf(record.engine_id, ENGINE_IDS, `${context}.engine_id`),
    model_id: expectNonEmptyString(record.model_id, `${context}.model_id`),
    model_version: expectNonEmptyString(record.model_version, `${context}.model_version`),
    status: expectOneOf(record.status, HEALTH_STATUSES, `${context}.status`),
    device_class: expectNonEmptyString(record.device_class, `${context}.device_class`),
    safe_error_code: expectNullableNonEmptyString(record.safe_error_code, `${context}.safe_error_code`),
  };
}

function parseTranscript(value: unknown, context: string): TranscriptResult {
  const record = expectRecord(value, context);
  expectExactKeys(
    record,
    [
      "session_id",
      "request_id",
      "engine_id",
      "model_id",
      "final_text",
      "detected_language",
      "audio_duration_ms",
      "inference_duration_ms",
    ],
    context,
  );
  return {
    session_id: expectUuid(record.session_id, `${context}.session_id`),
    request_id: expectUuid(record.request_id, `${context}.request_id`),
    engine_id: expectOneOf(record.engine_id, ENGINE_IDS, `${context}.engine_id`),
    model_id: expectNonEmptyString(record.model_id, `${context}.model_id`),
    final_text: expectNonEmptyString(record.final_text, `${context}.final_text`),
    detected_language: expectNullableNonEmptyString(
      record.detected_language,
      `${context}.detected_language`,
    ),
    audio_duration_ms: expectNonNegativeInteger(record.audio_duration_ms, `${context}.audio_duration_ms`),
    inference_duration_ms: expectNonNegativeInteger(
      record.inference_duration_ms,
      `${context}.inference_duration_ms`,
    ),
  };
}

function parseWorkerError(value: unknown, context: string): WorkerError {
  const record = expectRecord(value, context);
  expectExactKeys(record, ["code", "retryable", "fatal", "safe_message_key"], context);
  return {
    code: expectOneOf(record.code, ERROR_CODES, `${context}.code`),
    retryable: expectBoolean(record.retryable, `${context}.retryable`),
    fatal: expectBoolean(record.fatal, `${context}.fatal`),
    safe_message_key: expectNonEmptyString(record.safe_message_key, `${context}.safe_message_key`),
  };
}

function parseShutdownComplete(value: unknown, context: string): ShutdownComplete {
  const record = expectRecord(value, context);
  expectExactKeys(record, ["worker_version"], context);
  return { worker_version: expectNonEmptyString(record.worker_version, `${context}.worker_version`) };
}

function parseCapabilities(value: unknown, context: string): WorkerCapability[] {
  const capabilities = expectArray(value, context).map((entry, index) =>
    expectOneOf(entry, CAPABILITIES, `${context}[${index}]`),
  );
  expectUnique(capabilities, context);
  return capabilities;
}

function expectMatchingCorrelation(
  envelope: EnvelopeBase,
  payload: { session_id: string; request_id: string },
  context: string,
): void {
  if (envelope.session_id !== payload.session_id || envelope.request_id !== payload.request_id) {
    throw new TypeError(`${context} correlation does not match its payload`);
  }
}

function expectCorrelations(
  envelope: EnvelopeBase,
  requiresSession: boolean,
  requiresRequest: boolean,
  context: string,
): void {
  if ((envelope.session_id !== null) !== requiresSession) {
    throw new TypeError(`${context}.session_id has invalid presence`);
  }
  if ((envelope.request_id !== null) !== requiresRequest) {
    throw new TypeError(`${context}.request_id has invalid presence`);
  }
}

function expectErrorCorrelations(envelope: EnvelopeBase, context: string): void {
  const hasSession = envelope.session_id !== null;
  const hasRequest = envelope.request_id !== null;
  if (hasSession !== hasRequest) {
    throw new TypeError(
      `${context} error correlation must contain both session_id and request_id or neither`,
    );
  }
}

function expectRecord(value: unknown, context: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new TypeError(`${context} must be an object`);
  }
  return value as Record<string, unknown>;
}

function expectExactKeys(
  record: Record<string, unknown>,
  expectedKeys: readonly string[],
  context: string,
): void {
  const actual = Object.keys(record).sort();
  const expected = [...expectedKeys].sort();
  if (actual.length !== expected.length || actual.some((key, index) => key !== expected[index])) {
    throw new TypeError(`${context} has unexpected fields: ${actual.join(",")}`);
  }
}

function expectArray(value: unknown, context: string): unknown[] {
  if (!Array.isArray(value)) throw new TypeError(`${context} must be an array`);
  return value;
}

function expectString(value: unknown, context: string): string {
  if (typeof value !== "string") throw new TypeError(`${context} must be a string`);
  return value;
}

function expectNonEmptyString(value: unknown, context: string): string {
  const parsed = expectString(value, context);
  if (parsed.trim().length === 0) throw new TypeError(`${context} must not be empty`);
  return parsed;
}

function expectNullableNonEmptyString(value: unknown, context: string): string | null {
  return value === null ? null : expectNonEmptyString(value, context);
}

function expectBoolean(value: unknown, context: string): boolean {
  if (typeof value !== "boolean") throw new TypeError(`${context} must be a boolean`);
  return value;
}

function expectNonNegativeInteger(value: unknown, context: string): number {
  if (typeof value !== "number" || !Number.isInteger(value) || value < 0) {
    throw new TypeError(`${context} must be a non-negative integer`);
  }
  return value;
}

function expectPositiveInteger(value: unknown, context: string): number {
  const parsed = expectNonNegativeInteger(value, context);
  if (parsed === 0) throw new TypeError(`${context} must be positive`);
  return parsed;
}

function expectContractVersion(value: unknown): asserts value is typeof WORKER_CONTRACT_VERSION {
  if (value !== WORKER_CONTRACT_VERSION) {
    throw new TypeError(`unsupported contract version: ${String(value)}`);
  }
}

function expectOneOf<const T extends string>(
  value: unknown,
  allowed: readonly T[],
  context: string,
): T {
  if (typeof value !== "string" || !allowed.some((entry) => entry === value)) {
    throw new TypeError(`${context} has an unknown value`);
  }
  return value as T;
}

function expectUnique<T>(values: readonly T[], context: string): void {
  if (new Set(values).size !== values.length) throw new TypeError(`${context} contains duplicates`);
}

const UUID_PATTERN = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const NIL_UUID = "00000000-0000-0000-0000-000000000000";

function expectUuid(value: unknown, context: string): string {
  const parsed = expectString(value, context);
  if (!UUID_PATTERN.test(parsed) || parsed === NIL_UUID) {
    throw new TypeError(`${context} must be a non-nil UUID`);
  }
  return parsed;
}

function expectNullableUuid(value: unknown, context: string): string | null {
  return value === null ? null : expectUuid(value, context);
}

function expectUtcRfc3339(value: unknown, context: string): string {
  const parsed = expectString(value, context);
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z$/.test(parsed)) {
    throw new TypeError(`${context} must be a UTC RFC 3339 timestamp`);
  }
  return parsed;
}
