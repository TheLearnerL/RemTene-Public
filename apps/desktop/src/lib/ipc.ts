import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export const CONTRACT_VERSION = 1 as const;

/// Rust 侧 `AppError` 的结构投影。IPC 拒绝时 `invoke` 抛出的就是这个对象，
/// 不是 `Error` 实例，所以 `String(error)` 只会得到 `[object Object]`。
export interface AppErrorView {
  contract_version: number;
  code: string;
  category: string;
  severity: string;
  retryable: boolean;
  user_message_key: string;
  correlation_id: string;
  safe_details?: Record<string, string>;
}

function isAppErrorView(value: unknown): value is AppErrorView {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as AppErrorView).code === "string" &&
    typeof (value as AppErrorView).user_message_key === "string"
  );
}

/// UI 只用稳定错误码决定可恢复动作，不读取可能含诊断信息的任意异常字符串。
export function getIpcErrorCode(error: unknown): string | null {
  return isAppErrorView(error) ? error.code : null;
}

/// 把任意 IPC 失败值转成可读文本。保留 error code 与诊断细节，
/// 否则真实故障原因会被 `String(error)` 吞成 `[object Object]`。
export function formatIpcError(error: unknown): string {
  if (isAppErrorView(error)) {
    const details = error.safe_details
      ? Object.entries(error.safe_details)
          .map(([key, value]) => `${key}=${value}`)
          .join(", ")
      : "";
    const suffix = details ? ` (${details})` : "";
    return `[${error.code}] ${error.user_message_key}${suffix}`;
  }
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}

export type LifecycleState = "starting" | "ready" | "quitting";
export type MicrophonePermission = "unknown" | "not_determined" | "granted" | "denied";
export type SystemPermission =
  | "unknown"
  | "not_determined"
  | "granted"
  | "denied"
  | "not_required"
  | "inherited_from_launcher";
export type AsrReadiness = "discovering" | "qwen_ready" | "whisper_ready" | "unavailable";
export type LocalAsrModel = "qwen" | "whisper";
export type SessionUserState = "preparing" | "recording" | "processing" | "completed";
export type SessionPhase =
  | "preparing"
  | "recording"
  | "recognizing"
  | "processing"
  | "delivering"
  | "finalizing"
  | "terminated";

export interface SessionPublicSnapshot {
  contract_version: number;
  session_id: string;
  user_state: SessionUserState;
  phase: SessionPhase;
  recording_elapsed_ms: number | null;
  recording_limit_ms: number | null;
  can_finish: boolean;
  can_cancel: boolean;
  status_code: string;
}

export interface AppSnapshot {
  contract_version: number;
  lifecycle_state: LifecycleState;
  active_session: SessionPublicSnapshot | null;
  microphone_permission: MicrophonePermission;
  accessibility_permission: SystemPermission;
  asr_readiness: AsrReadiness;
  llm_configured: boolean;
  model_summary: {
    selected_model: LocalAsrModel;
    active_model_id: string | null;
    qwen_ready: boolean;
    whisper_ready: boolean;
  };
  shortcut_configured: boolean;
  autostart_enabled: boolean;
}

export interface AutostartStatusView {
  contract_version: number;
  enabled: boolean;
}

export function validateAutostartStatus(
  value: unknown,
): AutostartStatusView {
  const keys =
    typeof value === "object" && value !== null ? Object.keys(value) : [];
  if (
    typeof value !== "object" ||
    value === null ||
    keys.length !== 2 ||
    !keys.includes("contract_version") ||
    !keys.includes("enabled") ||
    (value as AutostartStatusView).contract_version !== CONTRACT_VERSION ||
    typeof (value as AutostartStatusView).enabled !== "boolean"
  ) {
    throw new Error("Invalid autostart status");
  }
  return value as AutostartStatusView;
}

export interface SessionCommand {
  contract_version: number;
  request_id: string;
  session_id: string;
}

export interface CommandAccepted {
  contract_version: number;
  request_id: string;
}

export const SESSION_STATE_CHANGED_EVENT = "session:state-changed" as const;
export const SESSION_ENDED_EVENT = "session:ended" as const;
export const SESSION_TERMINAL_EVENT = "session:terminal" as const;
export const TEMPORARY_TEXT_DELIVERED_EVENT = "temporary-text:delivered" as const;
export const NOTIFICATION_RAISED_EVENT = "notification:raised" as const;
export const CONTROL_PANEL_NAVIGATE_EVENT = "control-panel:navigate" as const;
export const APP_SNAPSHOT_CHANGED_EVENT = "app:snapshot-changed" as const;

export type TemporaryTextStatusCode =
  | "temporary_text.not_inserted"
  | "temporary_text.indeterminate"
  | "temporary_text.llm_fallback";

export interface TemporaryTextDelivery {
  contract_version: number;
  delivery_id: string;
  status_code: TemporaryTextStatusCode;
  final_text: string;
}

export interface TemporaryTextCopyCommand {
  contract_version: number;
  delivery_id: string;
}

export interface TemporaryTextCopyResult {
  contract_version: number;
  delivery_id: string;
}

export interface HistoryQuery {
  contract_version: number;
}

export interface HistoryRecordView {
  record_id: string;
  final_text: string;
  created_at: string;
}

export interface HistoryPage {
  contract_version: number;
  records: HistoryRecordView[];
}

export interface HistoryCopyCommand {
  contract_version: number;
  request_id: string;
  record_id: string;
}

export interface HistoryCopyResult {
  contract_version: number;
  request_id: string;
  record_id: string;
}

export interface HistoryClearAllCommand {
  contract_version: number;
  request_id: string;
  acknowledge_data_loss: true;
}

export interface HistoryClearAllResult {
  contract_version: number;
  request_id: string;
  cleared_count: number;
}

export type UserNotificationCode =
  | "notification.permission_microphone"
  | "notification.asr"
  | "notification.llm"
  | "notification.delivery";

/**
 * 错误反馈窗口只接收无正文通知：稳定错误分类和 Session 标识足以选择已批准文案，
 * 转录文字、诊断详情与任意后端字符串都不得跨入此 Surface。
 */
export interface UserNotification {
  contract_version: number;
  session_id: string;
  code: UserNotificationCode;
}

export type ControlPanelNavigationTarget =
  | "model.asr"
  | "model.text_service";

export interface ControlPanelNavigation {
  contract_version: number;
  target: ControlPanelNavigationTarget;
}

const USER_NOTIFICATION_CODES = new Set<UserNotificationCode>([
  "notification.permission_microphone",
  "notification.asr",
  "notification.llm",
  "notification.delivery",
]);

const CONTROL_PANEL_NAVIGATION_TARGETS =
  new Set<ControlPanelNavigationTarget>([
    "model.asr",
    "model.text_service",
  ]);

function hasExactKeys(value: object, expected: readonly string[]): boolean {
  const actual = Object.keys(value).sort();
  return actual.join(",") === [...expected].sort().join(",");
}

function isUuid(value: string): boolean {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(
    value,
  );
}

export function validateUserNotification(
  value: unknown,
): UserNotification {
  if (
    typeof value !== "object" ||
    value === null ||
    !hasExactKeys(value, ["contract_version", "session_id", "code"])
  ) {
    throw new Error("Invalid user notification projection");
  }
  const notification = value as UserNotification;
  if (notification.contract_version !== CONTRACT_VERSION) {
    throw new Error(
      `IPC contract mismatch: ${notification.contract_version}`,
    );
  }
  if (
    typeof notification.session_id !== "string" ||
    !isUuid(notification.session_id)
  ) {
    throw new Error("Invalid user notification session ID");
  }
  if (
    typeof notification.code !== "string" ||
    !USER_NOTIFICATION_CODES.has(notification.code)
  ) {
    throw new Error("Invalid user notification code");
  }
  return notification;
}

export function validateControlPanelNavigation(
  value: unknown,
): ControlPanelNavigation {
  if (
    typeof value !== "object" ||
    value === null ||
    !hasExactKeys(value, ["contract_version", "target"])
  ) {
    throw new Error("Invalid control panel navigation projection");
  }
  const navigation = value as ControlPanelNavigation;
  if (navigation.contract_version !== CONTRACT_VERSION) {
    throw new Error(`IPC contract mismatch: ${navigation.contract_version}`);
  }
  if (
    typeof navigation.target !== "string" ||
    !CONTROL_PANEL_NAVIGATION_TARGETS.has(navigation.target)
  ) {
    throw new Error("Invalid control panel navigation target");
  }
  return navigation;
}

function validateTemporaryTextCopyProjection(
  value: unknown,
  projection: "command" | "result",
): TemporaryTextCopyCommand {
  if (
    typeof value !== "object" ||
    value === null ||
    !hasExactKeys(value, ["contract_version", "delivery_id"])
  ) {
    throw new Error(`Invalid temporary text copy ${projection}`);
  }
  const dto = value as TemporaryTextCopyCommand;
  if (dto.contract_version !== CONTRACT_VERSION) {
    throw new Error(`IPC contract mismatch: ${dto.contract_version}`);
  }
  if (typeof dto.delivery_id !== "string" || !isUuid(dto.delivery_id)) {
    throw new Error("Invalid temporary text delivery ID");
  }
  return dto;
}

export function createTemporaryTextCopyCommand(
  deliveryId: string,
): TemporaryTextCopyCommand {
  return validateTemporaryTextCopyCommand(
    {
      contract_version: CONTRACT_VERSION,
      delivery_id: deliveryId,
    },
  );
}

export function validateTemporaryTextCopyCommand(
  value: unknown,
): TemporaryTextCopyCommand {
  return validateTemporaryTextCopyProjection(value, "command");
}

export function validateTemporaryTextCopyResult(
  value: unknown,
  expectedDeliveryId: string,
): TemporaryTextCopyResult {
  const result = validateTemporaryTextCopyProjection(value, "result");
  if (result.delivery_id !== expectedDeliveryId) {
    throw new Error("Temporary text copy correlation mismatch");
  }
  return result;
}

export function validateHistoryQuery(value: unknown): HistoryQuery {
  if (
    typeof value !== "object" ||
    value === null ||
    !hasExactKeys(value, ["contract_version"])
  ) {
    throw new Error("Invalid history query");
  }
  const query = value as HistoryQuery;
  if (query.contract_version !== CONTRACT_VERSION) {
    throw new Error(`IPC contract mismatch: ${query.contract_version}`);
  }
  return query;
}

export function createHistoryQuery(): HistoryQuery {
  return validateHistoryQuery({ contract_version: CONTRACT_VERSION });
}

export function isUtcRfc3339(value: string): boolean {
  const match =
    /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.\d{1,9})?Z$/.exec(
      value,
    );
  if (match === null) return false;
  const parsed = Date.parse(value);
  if (!Number.isFinite(parsed)) return false;
  const date = new Date(parsed);
  return (
    date.getUTCFullYear() === Number(match[1]) &&
    date.getUTCMonth() + 1 === Number(match[2]) &&
    date.getUTCDate() === Number(match[3]) &&
    date.getUTCHours() === Number(match[4]) &&
    date.getUTCMinutes() === Number(match[5]) &&
    date.getUTCSeconds() === Number(match[6])
  );
}

export function validateHistoryRecord(
  value: unknown,
): HistoryRecordView {
  if (
    typeof value !== "object" ||
    value === null ||
    !hasExactKeys(value, ["record_id", "final_text", "created_at"])
  ) {
    throw new Error("Invalid history record projection");
  }
  const record = value as HistoryRecordView;
  if (typeof record.record_id !== "string" || !isUuid(record.record_id)) {
    throw new Error("Invalid history record ID");
  }
  if (
    typeof record.final_text !== "string" ||
    record.final_text.trim().length === 0
  ) {
    throw new Error("Invalid history record text");
  }
  if (
    typeof record.created_at !== "string" ||
    !isUtcRfc3339(record.created_at)
  ) {
    throw new Error("Invalid history record timestamp");
  }
  return record;
}

export function validateHistoryPage(value: unknown): HistoryPage {
  if (
    typeof value !== "object" ||
    value === null ||
    !hasExactKeys(value, ["contract_version", "records"])
  ) {
    throw new Error("Invalid history page projection");
  }
  const page = value as HistoryPage;
  if (page.contract_version !== CONTRACT_VERSION) {
    throw new Error(`IPC contract mismatch: ${page.contract_version}`);
  }
  if (!Array.isArray(page.records)) {
    throw new Error("Invalid history records");
  }
  const recordIds = new Set<string>();
  const records = page.records.map((record) => validateHistoryRecord(record));
  records.forEach((record, index) => {
    const normalizedId = record.record_id.toLowerCase();
    if (recordIds.has(normalizedId)) {
      throw new Error("Duplicate history record ID");
    }
    recordIds.add(normalizedId);

    const previous = records[index - 1];
    if (previous === undefined) return;
    const previousTime = Date.parse(previous.created_at);
    const currentTime = Date.parse(record.created_at);
    if (
      currentTime > previousTime ||
      (currentTime === previousTime &&
        normalizedId > previous.record_id.toLowerCase())
    ) {
      throw new Error("Invalid history record order");
    }
  });
  return page;
}

function validateHistoryCopyProjection(
  value: unknown,
  projection: "command" | "result",
): HistoryCopyCommand {
  if (
    typeof value !== "object" ||
    value === null ||
    !hasExactKeys(value, ["contract_version", "request_id", "record_id"])
  ) {
    throw new Error(`Invalid history copy ${projection}`);
  }
  const dto = value as HistoryCopyCommand;
  if (dto.contract_version !== CONTRACT_VERSION) {
    throw new Error(`IPC contract mismatch: ${dto.contract_version}`);
  }
  if (typeof dto.request_id !== "string" || !isUuid(dto.request_id)) {
    throw new Error("Invalid history copy request ID");
  }
  if (typeof dto.record_id !== "string" || !isUuid(dto.record_id)) {
    throw new Error("Invalid history record ID");
  }
  return dto;
}

export function createHistoryCopyCommand(
  recordId: string,
  requestId = crypto.randomUUID(),
): HistoryCopyCommand {
  return validateHistoryCopyProjection(
    {
      contract_version: CONTRACT_VERSION,
      request_id: requestId,
      record_id: recordId,
    },
    "command",
  );
}

export function validateHistoryCopyResult(
  value: unknown,
  expected: HistoryCopyCommand,
): HistoryCopyResult {
  const result = validateHistoryCopyProjection(value, "result");
  if (
    result.request_id !== expected.request_id ||
    result.record_id !== expected.record_id
  ) {
    throw new Error("History copy correlation mismatch");
  }
  return result;
}

export function createHistoryClearAllCommand(
  requestId = crypto.randomUUID(),
): HistoryClearAllCommand {
  return {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
    acknowledge_data_loss: true,
  };
}

export function validateHistoryClearAllResult(
  value: unknown,
  expectedRequestId: string,
): HistoryClearAllResult {
  if (
    typeof value !== "object" ||
    value === null ||
    !hasExactKeys(value, ["contract_version", "request_id", "cleared_count"])
  ) {
    throw new Error("Invalid history clear result");
  }
  const result = value as HistoryClearAllResult;
  if (result.contract_version !== CONTRACT_VERSION) {
    throw new Error(`IPC contract mismatch: ${result.contract_version}`);
  }
  if (
    typeof result.request_id !== "string" ||
    !isUuid(result.request_id) ||
    result.request_id !== expectedRequestId
  ) {
    throw new Error("History clear correlation mismatch");
  }
  if (
    typeof result.cleared_count !== "number" ||
    !Number.isSafeInteger(result.cleared_count) ||
    result.cleared_count < 0
  ) {
    throw new Error("Invalid cleared history count");
  }
  return result;
}

export async function getAppSnapshot(): Promise<AppSnapshot> {
  const snapshot = await invoke<AppSnapshot>("app_get_snapshot");
  if (snapshot.contract_version !== CONTRACT_VERSION) {
    throw new Error(`IPC contract mismatch: ${snapshot.contract_version}`);
  }
  return snapshot;
}

/**
 * Runs one explicit local model health check, then returns the refreshed public snapshot.
 * No engine or path is accepted from Renderer; Core applies the persisted ASR preference.
 */
export async function checkAsrHealth(): Promise<AppSnapshot> {
    const snapshot = await invoke<AppSnapshot>("model_check_health");
  if (snapshot.contract_version !== CONTRACT_VERSION) {
    throw new Error(`IPC contract mismatch: ${snapshot.contract_version}`);
  }
    return snapshot;
}

/** Verifies, prewarms and atomically selects one fixed local ASR model. */
export async function switchAsrModel(engine: LocalAsrModel): Promise<AppSnapshot> {
  const snapshot = await invoke<AppSnapshot>("model_switch_engine", { engine });
  if (snapshot.contract_version !== CONTRACT_VERSION) {
    throw new Error(`IPC contract mismatch: ${snapshot.contract_version}`);
  }
  return snapshot;
}

/** Receives the content-free public snapshot after an automatic Core-side refresh. */
export async function listenToAppSnapshotChanged(
  onSnapshot: (snapshot: AppSnapshot) => void,
): Promise<UnlistenFn> {
  return listen<AppSnapshot>(APP_SNAPSHOT_CHANGED_EVENT, ({ payload }) => {
    if (payload.contract_version !== CONTRACT_VERSION) {
      throw new Error(`IPC contract mismatch: ${payload.contract_version}`);
    }
    onSnapshot(payload);
  });
}

/** Opens the application-owned local model directory without exposing a path to Renderer. */
export async function openModelDirectory(): Promise<void> {
  return invoke<void>("model_open_directory");
}

export async function getRecordingHudState(): Promise<SessionPublicSnapshot | null> {
  const snapshot = await invoke<SessionPublicSnapshot | null>("recording_hud_get_state");
  if (snapshot !== null) validateSessionSnapshot(snapshot);
  return snapshot;
}

export async function listenToRecordingHudState(
  onState: (snapshot: SessionPublicSnapshot) => void,
): Promise<UnlistenFn> {
  return listen<SessionPublicSnapshot>(SESSION_STATE_CHANGED_EVENT, ({ payload }) => {
    validateSessionSnapshot(payload);
    onState(payload);
  });
}

export interface SessionEnded {
  contract_version: number;
  session_id: string;
}

export type SessionTerminalOutcome =
  | "completed"
  | "cancelled"
  | "rejected"
  | "failed";

export type SessionTerminalErrorCode =
  | "session.failed.audio"
  | "session.failed.asr"
  | "session.failed.llm"
  | "session.failed.delivery"
  | "session.failed.storage"
  | "session.failed.lifecycle"
  | "session.rejected.secure_input"
  | "session.rejected.selection_too_long"
  | "session.rejected.permission_unavailable"
  | "session.rejected.asr_unavailable"
  | "session.rejected.recording_hud_unavailable";

export interface SessionTerminalEvent {
  contract_version: number;
  session_id: string;
  outcome: SessionTerminalOutcome;
  error_code: SessionTerminalErrorCode | null;
}

/// 会话逻辑结束通知。载荷只含会话标识；不承诺原生 HUD 已完成视觉退场。
export async function listenToSessionEnded(
  onEnded: (event: SessionEnded) => void,
): Promise<UnlistenFn> {
  return listen<SessionEnded>(SESSION_ENDED_EVENT, ({ payload }) => {
    if (payload.contract_version !== CONTRACT_VERSION) {
      throw new Error(`IPC contract mismatch: ${payload.contract_version}`);
    }
    onEnded(payload);
  });
}

/// Domain 已确认终态后的无内容业务结果。
export async function listenToSessionTerminal(
  onTerminal: (event: SessionTerminalEvent) => void,
): Promise<UnlistenFn> {
  return listen<SessionTerminalEvent>(SESSION_TERMINAL_EVENT, ({ payload }) => {
    onTerminal(validateSessionTerminalEvent(payload));
  });
}

export async function finishRecording(sessionId: string): Promise<CommandAccepted> {
  return submitRecordingCommand("recording_finish", sessionId);
}

export async function cancelRecording(sessionId: string): Promise<CommandAccepted> {
  return submitRecordingCommand("recording_cancel", sessionId);
}

async function submitRecordingCommand(
  commandName: "recording_finish" | "recording_cancel",
  sessionId: string,
): Promise<CommandAccepted> {
  const command: SessionCommand = {
    contract_version: CONTRACT_VERSION,
    request_id: crypto.randomUUID(),
    session_id: sessionId,
  };
  const accepted = await invoke<CommandAccepted>(commandName, { command });
  if (
    accepted.contract_version !== CONTRACT_VERSION ||
    accepted.request_id !== command.request_id
  ) {
    throw new Error("Recording command correlation mismatch");
  }
  return accepted;
}

export async function startSession(): Promise<string> {
  return invoke<string>("session_start");
}

export interface SessionFinishView {
  contract_version: number;
  status: "delivered" | "failed" | "discarded" | "not_recording";
  delivery: "inserted" | "clipboard" | "temporary_text" | null;
  notice: "llm_not_configured" | "llm_unavailable" | null;
  failure: string | null;
}

export function validateSessionFinishView(view: SessionFinishView): SessionFinishView {
  if (view.contract_version !== CONTRACT_VERSION) {
    throw new Error(`IPC contract mismatch: ${view.contract_version}`);
  }
  return view;
}

export async function finishSession(sessionId: string): Promise<SessionFinishView> {
  return validateSessionFinishView(
    await invoke<SessionFinishView>("session_finish", { sessionId }),
  );
}

export async function cancelSession(sessionId: string): Promise<void> {
  return invoke<void>("session_cancel", { sessionId });
}

export interface PermissionStatusView {
  contract_version: number;
  microphone: MicrophonePermission;
  accessibility: SystemPermission;
  app_display_name: string;
  process_name: string;
}

export async function getPermissionStatus(): Promise<PermissionStatusView> {
  return invoke<PermissionStatusView>("permission_get_status");
}

export interface SettingsView {
  contract_version: number;
  version: number;
  recording_mode: RecordingMode;
  max_recording_duration_seconds: number;
  recording_shortcut: string | null;
  processing_mode: ProcessingMode;
  read_selected_text: boolean;
  clipboard_bridge_allowed: boolean;
  auto_copy_result: boolean;
  local_diagnostics_enabled: boolean;
  history_policy: HistoryPolicyView;
  llm: LlmSettingsView | null;
}

export interface HistoryPolicyView {
  enabled: boolean;
  limit: number;
  retention_days: number | null;
}

export type RecordingMode = "toggle" | "push_to_talk";
export type ProcessingMode = "raw" | "faithful" | "structured";

export interface LlmSettingsView {
  base_url: string;
  model: string;
}

function assertSettingsContract(view: SettingsView): SettingsView {
  if (
    view.contract_version !== CONTRACT_VERSION ||
    typeof view.auto_copy_result !== "boolean" ||
    typeof view.local_diagnostics_enabled !== "boolean"
  ) {
    throw new Error(`IPC contract mismatch: ${view.contract_version}`);
  }
  return view;
}

export async function getSettings(): Promise<SettingsView> {
  return assertSettingsContract(await invoke<SettingsView>("settings_get"));
}

/// 打开后，原生插入被证明未发生时才会改走剪贴板；它会覆盖剪贴板内容并模拟 ⌘V。
export async function setClipboardBridgeAllowed(allowed: boolean): Promise<SettingsView> {
  return assertSettingsContract(
    await invoke<SettingsView>("settings_set_clipboard_bridge", { allowed }),
  );
}

interface CorrelatedCommand {
  contract_version: number;
  request_id: string;
}

interface CorrelatedResult {
  contract_version: number;
  request_id: string;
}

export interface SetRecordingPreferencesCommand extends CorrelatedCommand {
  expected_version: number;
  recording_mode: RecordingMode;
  max_recording_duration_seconds: number;
}

export interface SetRecordingPreferencesResult extends CorrelatedResult {
  settings: SettingsView;
}

export function createSetRecordingPreferencesCommand(
  expectedVersion: number,
  recordingMode: RecordingMode,
  maxRecordingDurationSeconds: number,
  requestId = crypto.randomUUID(),
): SetRecordingPreferencesCommand {
  return {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
    expected_version: expectedVersion,
    recording_mode: recordingMode,
    max_recording_duration_seconds: maxRecordingDurationSeconds,
  };
}

export async function setRecordingPreferences(
  expectedVersion: number,
  recordingMode: RecordingMode,
  maxRecordingDurationSeconds: number,
): Promise<SettingsView> {
  const command = createSetRecordingPreferencesCommand(
    expectedVersion,
    recordingMode,
    maxRecordingDurationSeconds,
  );
  const result = assertCorrelatedContract(
    await invoke<SetRecordingPreferencesResult>(
      "settings_set_recording_preferences",
      { command },
    ),
    command.request_id,
    "recording preferences mutation",
  );
  return assertSettingsContract(result.settings);
}

export interface SetRecordingShortcutCommand extends CorrelatedCommand {
  expected_version: number;
  recording_shortcut: string | null;
}

export interface SetRecordingShortcutResult extends CorrelatedResult {
  settings: SettingsView;
}

export function createSetRecordingShortcutCommand(
  expectedVersion: number,
  recordingShortcut: string | null,
  requestId = crypto.randomUUID(),
): SetRecordingShortcutCommand {
  return {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
    expected_version: expectedVersion,
    recording_shortcut: recordingShortcut,
  };
}

export async function setRecordingShortcut(
  expectedVersion: number,
  recordingShortcut: string | null,
): Promise<SettingsView> {
  const command = createSetRecordingShortcutCommand(
    expectedVersion,
    recordingShortcut,
  );
  const result = assertCorrelatedContract(
    await invoke<SetRecordingShortcutResult>("settings_set_recording_shortcut", {
      command,
    }),
    command.request_id,
    "recording shortcut mutation",
  );
  return assertSettingsContract(result.settings);
}

export interface SetHistoryEnabledCommand extends CorrelatedCommand {
  expected_version: number;
  enabled: boolean;
}

export interface SetHistoryEnabledResult extends CorrelatedResult {
  settings: SettingsView;
}

export function createSetHistoryEnabledCommand(
  expectedVersion: number,
  enabled: boolean,
  requestId = crypto.randomUUID(),
): SetHistoryEnabledCommand {
  return {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
    expected_version: expectedVersion,
    enabled,
  };
}

export async function setHistoryEnabled(
  expectedVersion: number,
  enabled: boolean,
): Promise<SettingsView> {
  const command = createSetHistoryEnabledCommand(expectedVersion, enabled);
  const result = assertCorrelatedContract(
    await invoke<SetHistoryEnabledResult>("settings_set_history_enabled", {
      command,
    }),
    command.request_id,
    "history settings mutation",
  );
  return assertSettingsContract(result.settings);
}

export interface SetHistoryLimitCommand extends CorrelatedCommand {
  expected_version: number;
  limit: number;
  acknowledge_data_loss: boolean;
}

export interface SetHistoryLimitResult extends CorrelatedResult {
  settings: SettingsView;
}

export function createSetHistoryLimitCommand(
  expectedVersion: number,
  limit: number,
  acknowledgeDataLoss: boolean,
  requestId = crypto.randomUUID(),
): SetHistoryLimitCommand {
  return {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
    expected_version: expectedVersion,
    limit,
    acknowledge_data_loss: acknowledgeDataLoss,
  };
}

export async function setHistoryLimit(
  expectedVersion: number,
  limit: number,
  acknowledgeDataLoss: boolean,
): Promise<SettingsView> {
  const command = createSetHistoryLimitCommand(
    expectedVersion,
    limit,
    acknowledgeDataLoss,
  );
  const result = assertCorrelatedContract(
    await invoke<SetHistoryLimitResult>("settings_set_history_limit", {
      command,
    }),
    command.request_id,
    "history limit mutation",
  );
  return assertSettingsContract(result.settings);
}

export interface SetHistoryRetentionCommand extends CorrelatedCommand {
  expected_version: number;
  retention_days: number | null;
  acknowledge_data_loss: boolean;
}

export interface SetHistoryRetentionResult extends CorrelatedResult {
  settings: SettingsView;
}

export function createSetHistoryRetentionCommand(
  expectedVersion: number,
  retentionDays: number | null,
  acknowledgeDataLoss: boolean,
  requestId = crypto.randomUUID(),
): SetHistoryRetentionCommand {
  return {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
    expected_version: expectedVersion,
    retention_days: retentionDays,
    acknowledge_data_loss: acknowledgeDataLoss,
  };
}

export async function setHistoryRetention(
  expectedVersion: number,
  retentionDays: number | null,
  acknowledgeDataLoss: boolean,
): Promise<SettingsView> {
  const command = createSetHistoryRetentionCommand(
    expectedVersion,
    retentionDays,
    acknowledgeDataLoss,
  );
  const result = assertCorrelatedContract(
    await invoke<SetHistoryRetentionResult>(
      "settings_set_history_retention",
      { command },
    ),
    command.request_id,
    "history retention mutation",
  );
  return assertSettingsContract(result.settings);
}

export interface SetAutoCopyResultCommand extends CorrelatedCommand {
  expected_version: number;
  enabled: boolean;
}

export interface SetAutoCopyResultResult extends CorrelatedResult {
  settings: SettingsView;
}

export function createSetAutoCopyResultCommand(
  expectedVersion: number,
  enabled: boolean,
  requestId = crypto.randomUUID(),
): SetAutoCopyResultCommand {
  return {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
    expected_version: expectedVersion,
    enabled,
  };
}

export async function setAutoCopyResult(
  expectedVersion: number,
  enabled: boolean,
): Promise<SettingsView> {
  const command = createSetAutoCopyResultCommand(expectedVersion, enabled);
  const result = assertCorrelatedContract(
    await invoke<SetAutoCopyResultResult>("settings_set_auto_copy_result", {
      command,
    }),
    command.request_id,
    "auto-copy mutation",
  );
  return assertSettingsContract(result.settings);
}

export interface SetLocalDiagnosticsCommand extends CorrelatedCommand {
  expected_version: number;
  enabled: boolean;
}

export interface SetLocalDiagnosticsResult extends CorrelatedResult {
  settings: SettingsView;
}

export function createSetLocalDiagnosticsCommand(
  expectedVersion: number,
  enabled: boolean,
  requestId = crypto.randomUUID(),
): SetLocalDiagnosticsCommand {
  return {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
    expected_version: expectedVersion,
    enabled,
  };
}

export async function setLocalDiagnosticsEnabled(
  expectedVersion: number,
  enabled: boolean,
): Promise<SettingsView> {
  const command = createSetLocalDiagnosticsCommand(expectedVersion, enabled);
  const result = assertCorrelatedContract(
    await invoke<SetLocalDiagnosticsResult>("settings_set_local_diagnostics", {
      command,
    }),
    command.request_id,
    "local diagnostics mutation",
  );
  return assertSettingsContract(result.settings);
}

/** Opens the app-owned cache log directory; Renderer never receives its path. */
export async function openDiagnosticsDirectory(): Promise<void> {
  return invoke<void>("diagnostics_open_directory");
}

export interface SetAutostartCommand extends CorrelatedCommand {
  enabled: boolean;
}

export interface SetAutostartResult extends CorrelatedResult {
  status: AutostartStatusView;
}

export function createSetAutostartCommand(
  enabled: boolean,
  requestId = crypto.randomUUID(),
): SetAutostartCommand {
  return {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
    enabled,
  };
}

export async function getAutostartStatus(): Promise<AutostartStatusView> {
  return validateAutostartStatus(
    await invoke<unknown>("autostart_get_status"),
  );
}

export async function setAutostartEnabled(
  enabled: boolean,
): Promise<AutostartStatusView> {
  const command = createSetAutostartCommand(enabled);
  const result = assertCorrelatedContract(
    await invoke<SetAutostartResult>("autostart_set_enabled", { command }),
    command.request_id,
    "autostart mutation",
  );
  return validateAutostartStatus(result.status);
}

export interface SetTextProcessingSettingsCommand extends CorrelatedCommand {
  expected_version: number;
  processing_mode: ProcessingMode;
  read_selected_text: boolean;
}

export interface SetTextProcessingSettingsResult extends CorrelatedResult {
  settings: SettingsView;
}

export function createSetTextProcessingSettingsCommand(
  expectedVersion: number,
  processingMode: ProcessingMode,
  readSelectedText: boolean,
  requestId = crypto.randomUUID(),
): SetTextProcessingSettingsCommand {
  return {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
    expected_version: expectedVersion,
    processing_mode: processingMode,
    read_selected_text: readSelectedText,
  };
}

export async function setTextProcessingSettings(
  expectedVersion: number,
  processingMode: ProcessingMode,
  readSelectedText: boolean,
): Promise<SettingsView> {
  const command = createSetTextProcessingSettingsCommand(
    expectedVersion,
    processingMode,
    readSelectedText,
  );
  const result = assertCorrelatedContract(
    await invoke<SetTextProcessingSettingsResult>(
      "settings_set_text_processing",
      { command },
    ),
    command.request_id,
    "text processing settings mutation",
  );
  return assertSettingsContract(result.settings);
}

export interface SetLlmSettingsCommand extends CorrelatedCommand {
  expected_version: number;
  llm: LlmSettingsView | null;
}

export interface SetLlmSettingsResult extends CorrelatedResult {
  settings: SettingsView;
}

export function createSetLlmSettingsCommand(
  expectedVersion: number,
  llm: LlmSettingsView | null,
  requestId = crypto.randomUUID(),
): SetLlmSettingsCommand {
  return {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
    expected_version: expectedVersion,
    llm,
  };
}

export async function setLlmSettings(
  expectedVersion: number,
  llm: LlmSettingsView | null,
): Promise<SettingsView> {
  const command = createSetLlmSettingsCommand(expectedVersion, llm);
  const result = assertCorrelatedContract(
    await invoke<SetLlmSettingsResult>("settings_set_llm", { command }),
    command.request_id,
    "LLM settings mutation",
  );
  return assertSettingsContract(result.settings);
}

export type LlmApiKeyState =
  | "not_configured"
  | "configured"
  | "recovery_required"
  | "unavailable";
export type SecretStorageKind = "encrypted_local";

export interface LlmApiKeyStatusView {
  contract_version: number;
  state: LlmApiKeyState;
  storage: SecretStorageKind;
}

export interface SetLlmApiKeyCommand extends CorrelatedCommand {
  secret_value: string;
}

export type RevealLlmApiKeyCommand = CorrelatedCommand;

export type DeleteLlmApiKeyCommand = CorrelatedCommand;

export interface ResetUnrecoverableLlmSecretsCommand extends CorrelatedCommand {
  acknowledge_data_loss: boolean;
}

export interface LlmApiKeyMutationResult extends CorrelatedResult {
  status: LlmApiKeyStatusView;
}

export interface RevealLlmApiKeyResult extends CorrelatedResult {
  secret_value: string;
}

function createCorrelatedCommand(requestId = crypto.randomUUID()): CorrelatedCommand {
  return {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
  };
}

export function createSetLlmApiKeyCommand(
  secretValue: string,
  requestId = crypto.randomUUID(),
): SetLlmApiKeyCommand {
  return {
    ...createCorrelatedCommand(requestId),
    secret_value: secretValue,
  };
}

export function createRevealLlmApiKeyCommand(
  requestId = crypto.randomUUID(),
): RevealLlmApiKeyCommand {
  return createCorrelatedCommand(requestId);
}

export function createDeleteLlmApiKeyCommand(
  requestId = crypto.randomUUID(),
): DeleteLlmApiKeyCommand {
  return createCorrelatedCommand(requestId);
}

export function createResetUnrecoverableLlmSecretsCommand(
  acknowledgeDataLoss: boolean,
  requestId = crypto.randomUUID(),
): ResetUnrecoverableLlmSecretsCommand {
  return {
    ...createCorrelatedCommand(requestId),
    acknowledge_data_loss: acknowledgeDataLoss,
  };
}

export async function getLlmApiKeyStatus(): Promise<LlmApiKeyStatusView> {
  return assertLlmApiKeyStatus(
    await invoke<LlmApiKeyStatusView>("secret_get_llm_api_key_status"),
  );
}

export async function setLlmApiKey(secretValue: string): Promise<LlmApiKeyStatusView> {
  const command = createSetLlmApiKeyCommand(secretValue);
  const result = assertCorrelatedContract(
    await invoke<LlmApiKeyMutationResult>("secret_set_llm_api_key", { command }),
    command.request_id,
    "LLM API key mutation",
  );
  return assertLlmApiKeyStatus(result.status);
}

export async function revealLlmApiKey(): Promise<RevealLlmApiKeyResult> {
  const command = createRevealLlmApiKeyCommand();
  return assertCorrelatedContract(
    await invoke<RevealLlmApiKeyResult>("secret_reveal_llm_api_key", { command }),
    command.request_id,
    "LLM API key reveal",
  );
}

export async function deleteLlmApiKey(): Promise<LlmApiKeyStatusView> {
  const command = createDeleteLlmApiKeyCommand();
  const result = assertCorrelatedContract(
    await invoke<LlmApiKeyMutationResult>("secret_delete_llm_api_key", { command }),
    command.request_id,
    "LLM API key deletion",
  );
  return assertLlmApiKeyStatus(result.status);
}

export async function resetUnrecoverableLlmSecrets(
  acknowledgeDataLoss: true,
): Promise<LlmApiKeyStatusView> {
  const command = createResetUnrecoverableLlmSecretsCommand(acknowledgeDataLoss);
  const result = assertCorrelatedContract(
    await invoke<LlmApiKeyMutationResult>("secret_reset_unrecoverable_llm_secrets", { command }),
    command.request_id,
    "LLM secret recovery reset",
  );
  return assertLlmApiKeyStatus(result.status);
}

export type LlmTestConnectionCommand = CorrelatedCommand;

export type LlmConnectionTestStatus = "succeeded" | "failed";
export type LlmConnectionTestErrorCode =
  | "busy"
  | "runtime_unavailable"
  | "settings_unavailable"
  | "not_configured"
  | "recovery_required"
  | "secret_unavailable"
  | "invalid_configuration"
  | "authentication_failed"
  | "permission_denied"
  | "rate_limited"
  | "timeout"
  | "network"
  | "provider_unavailable"
  | "request_rejected"
  | "invalid_response"
  | "response_too_large"
  | "cancelled"
  | "internal";

export interface LlmUpstreamErrorView {
  http_status: number;
  response_body: string;
  truncated: boolean;
}

export interface LlmConnectionTestResult extends CorrelatedResult {
  status: LlmConnectionTestStatus;
  error_code: LlmConnectionTestErrorCode | null;
  upstream_error: LlmUpstreamErrorView | null;
}

const MAX_LLM_UPSTREAM_ERROR_BODY_LENGTH = 16 * 1024;

const LLM_CONNECTION_TEST_ERROR_CODES = new Set<LlmConnectionTestErrorCode>([
  "busy",
  "runtime_unavailable",
  "settings_unavailable",
  "not_configured",
  "recovery_required",
  "secret_unavailable",
  "invalid_configuration",
  "authentication_failed",
  "permission_denied",
  "rate_limited",
  "timeout",
  "network",
  "provider_unavailable",
  "request_rejected",
  "invalid_response",
  "response_too_large",
  "cancelled",
  "internal",
]);

export function createLlmTestConnectionCommand(
  requestId = crypto.randomUUID(),
): LlmTestConnectionCommand {
  return createCorrelatedCommand(requestId);
}

export function validateLlmConnectionTestResult(
  result: LlmConnectionTestResult,
  requestId: string,
): LlmConnectionTestResult {
  assertCorrelatedContract(result, requestId, "LLM connection test");
  if (result.status !== "succeeded" && result.status !== "failed") {
    throw new Error("Invalid LLM connection test result");
  }
  if (result.status === "succeeded" && result.error_code !== null) {
    throw new Error("Invalid LLM connection test result");
  }
  if (result.status === "succeeded" && result.upstream_error !== null) {
    throw new Error("Invalid LLM connection test result");
  }
  if (
    result.status === "failed" &&
    (result.error_code === null || !LLM_CONNECTION_TEST_ERROR_CODES.has(result.error_code))
  ) {
    throw new Error("Invalid LLM connection test result");
  }
  if (result.status === "failed" && result.upstream_error !== null) {
    const upstream = result.upstream_error;
    if (
      typeof upstream !== "object" ||
      !Number.isInteger(upstream.http_status) ||
      upstream.http_status < 100 ||
      upstream.http_status > 599 ||
      typeof upstream.response_body !== "string" ||
      upstream.response_body.length > MAX_LLM_UPSTREAM_ERROR_BODY_LENGTH ||
      typeof upstream.truncated !== "boolean"
    ) {
      throw new Error("Invalid LLM upstream error response");
    }
  }
  return result;
}

export async function testLlmConnection(): Promise<LlmConnectionTestResult> {
  const command = createLlmTestConnectionCommand();
  return validateLlmConnectionTestResult(
    await invoke<LlmConnectionTestResult>("llm_test_connection", { command }),
    command.request_id,
  );
}

export function assertCorrelatedContract<T extends CorrelatedResult>(
  result: T,
  requestId: string,
  operation: string,
): T {
  if (result.contract_version !== CONTRACT_VERSION) {
    throw new Error(`IPC contract mismatch: ${result.contract_version}`);
  }
  if (result.request_id !== requestId) {
    throw new Error(`${operation} correlation mismatch`);
  }
  return result;
}

function assertLlmApiKeyStatus(status: LlmApiKeyStatusView): LlmApiKeyStatusView {
  if (status.contract_version !== CONTRACT_VERSION) {
    throw new Error(`IPC contract mismatch: ${status.contract_version}`);
  }
  if (status.storage !== "encrypted_local") {
    throw new Error("Invalid LLM API key storage kind");
  }
  if (
    status.state !== "not_configured" &&
    status.state !== "configured" &&
    status.state !== "recovery_required" &&
    status.state !== "unavailable"
  ) {
    throw new Error("Invalid LLM API key state");
  }
  return status;
}

export async function requestMicrophonePermission(): Promise<PermissionStatusView> {
  return invoke<PermissionStatusView>("permission_request_microphone");
}

export async function requestAccessibilityPermission(): Promise<PermissionStatusView> {
  return invoke<PermissionStatusView>("permission_request_accessibility");
}

export async function openAccessibilitySettings(): Promise<void> {
  return invoke<void>("permission_open_accessibility_settings");
}

export async function openMicrophoneSettings(): Promise<void> {
  return invoke<void>("permission_open_microphone_settings");
}

/// 窗口挂载后拉取本次内容。窗口是按需新建的，可能晚于事件送达，因此必须能主动拉。
export async function getPendingTemporaryText(): Promise<TemporaryTextDelivery | null> {
  return invoke<TemporaryTextDelivery | null>("temporary_text_get_pending");
}

export async function listHistory(): Promise<HistoryPage> {
  const query = createHistoryQuery();
  return validateHistoryPage(
    await invoke<unknown>("history_list", { query }),
  );
}

export async function copyHistoryRecord(
  recordId: string,
): Promise<HistoryCopyResult> {
  const command = createHistoryCopyCommand(recordId);
  return validateHistoryCopyResult(
    await invoke<unknown>("history_copy", { command }),
    command,
  );
}

export async function clearAllHistory(): Promise<HistoryClearAllResult> {
  const command = createHistoryClearAllCommand();
  return validateHistoryClearAllResult(
    await invoke<unknown>("history_clear_all", { command }),
    command.request_id,
  );
}

export async function copyTemporaryText(
  deliveryId: string,
): Promise<TemporaryTextCopyResult> {
  const command = createTemporaryTextCopyCommand(deliveryId);
  return validateTemporaryTextCopyResult(
    await invoke<unknown>("temporary_text_copy_all", { command }),
    command.delivery_id,
  );
}

/// 关闭临时文本框：窗口连同本次文本一起销毁，内容只留在历史记录里。
export async function dismissTemporaryText(): Promise<void> {
  return invoke<void>("temporary_text_dismiss");
}

export async function listenToTemporaryText(
  onDelivery: (delivery: TemporaryTextDelivery) => void,
): Promise<UnlistenFn> {
  return listen<TemporaryTextDelivery>(TEMPORARY_TEXT_DELIVERED_EVENT, ({ payload }) => {
    if (payload.contract_version !== CONTRACT_VERSION) {
      throw new Error(`IPC contract mismatch: ${payload.contract_version}`);
    }
    onDelivery(payload);
  });
}

export async function getPendingNotification(): Promise<UserNotification | null> {
  const pending = await invoke<unknown>("notification_get_pending");
  return pending === null ? null : validateUserNotification(pending);
}

export async function listenToNotificationRaised(
  onNotification: (notification: UserNotification) => void,
): Promise<UnlistenFn> {
  return listen<unknown>(NOTIFICATION_RAISED_EVENT, ({ payload }) => {
    onNotification(validateUserNotification(payload));
  });
}

export async function applyNotificationAction(
  notification: UserNotification,
): Promise<void> {
  return invoke<void>("notification_apply_action", {
    notification: validateUserNotification(notification),
  });
}

export async function listenToControlPanelNavigation(
  onNavigation: (navigation: ControlPanelNavigation) => void,
): Promise<UnlistenFn> {
  return listen<unknown>(CONTROL_PANEL_NAVIGATE_EVENT, ({ payload }) => {
    onNavigation(validateControlPanelNavigation(payload));
  });
}

export function validateSessionSnapshot(snapshot: SessionPublicSnapshot): void {
  if (snapshot.contract_version !== CONTRACT_VERSION) {
    throw new Error(`IPC contract mismatch: ${snapshot.contract_version}`);
  }
  const expectedUserState: Record<SessionPhase, SessionUserState> = {
    preparing: "preparing",
    recording: "recording",
    recognizing: "processing",
    processing: "processing",
    delivering: "processing",
    finalizing: "processing",
    terminated: "completed",
  };
  const expectedStatusCode: Record<SessionPhase, string> = {
    preparing: "session.preparing",
    recording: "session.recording",
    recognizing: "session.recognizing",
    processing: "session.processing",
    delivering: "session.delivering",
    finalizing: "session.finalizing",
    terminated: "session.completed",
  };
  if (
    expectedUserState[snapshot.phase] !== snapshot.user_state ||
    expectedStatusCode[snapshot.phase] !== snapshot.status_code
  ) {
    throw new Error("Invalid Recording HUD phase projection");
  }
  const isPreparing = snapshot.user_state === "preparing" && snapshot.phase === "preparing";
  const isRecording = snapshot.user_state === "recording" && snapshot.phase === "recording";
  if (snapshot.can_finish !== isRecording || snapshot.can_cancel !== (isPreparing || isRecording)) {
    throw new Error("Invalid Recording HUD control projection");
  }
}

export function validateSessionTerminalEvent(
  event: SessionTerminalEvent,
): SessionTerminalEvent {
  if (event.contract_version !== CONTRACT_VERSION) {
    throw new Error(`IPC contract mismatch: ${event.contract_version}`);
  }
  const allowedKeys = [
    "contract_version",
    "error_code",
    "outcome",
    "session_id",
  ];
  if (
    Object.keys(event).sort().join(",") !== allowedKeys.sort().join(",")
  ) {
    throw new Error("Invalid Session terminal projection");
  }
  if (
    !/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(
      event.session_id,
    )
  ) {
    throw new Error("Invalid Session terminal ID");
  }
  if (!["completed", "cancelled", "rejected", "failed"].includes(event.outcome)) {
    throw new Error("Invalid Session terminal outcome");
  }
  const failureCodes: SessionTerminalErrorCode[] = [
    "session.failed.audio",
    "session.failed.asr",
    "session.failed.llm",
    "session.failed.delivery",
    "session.failed.storage",
    "session.failed.lifecycle",
  ];
  const rejectionCodes: SessionTerminalErrorCode[] = [
    "session.rejected.secure_input",
    "session.rejected.selection_too_long",
    "session.rejected.permission_unavailable",
    "session.rejected.asr_unavailable",
    "session.rejected.recording_hud_unavailable",
  ];
  const hasStableError =
    (event.outcome === "failed" &&
      event.error_code !== null &&
      failureCodes.includes(event.error_code)) ||
    (event.outcome === "rejected" &&
      event.error_code !== null &&
      rejectionCodes.includes(event.error_code));
  const expectsError = event.outcome === "failed" || event.outcome === "rejected";
  if (expectsError ? !hasStableError : event.error_code !== null) {
    throw new Error("Invalid Session terminal error projection");
  }
  return event;
}
