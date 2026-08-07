import {
  type AppSnapshot,
  type PermissionStatusView,
  type ProcessingMode,
  type SettingsView,
} from "../../lib/ipc.ts";

export type StatusPageState =
  | "loading"
  | "ready"
  | "partial"
  | "empty"
  | "error"
  | "busy";

export function deriveStatusPageState(
  snapshot: AppSnapshot | null,
  permissions: PermissionStatusView | null,
  settings: SettingsView | null,
  loading: boolean,
  failed: boolean,
): StatusPageState {
  if (
    snapshot?.active_session !== null &&
    snapshot?.active_session !== undefined
  ) {
    const matchesApprovedBusyState =
      !loading &&
      !failed &&
      settings !== null &&
      snapshot.lifecycle_state === "ready" &&
      snapshot.active_session.user_state === "processing" &&
      snapshot.active_session.phase === "processing" &&
      snapshot.llm_configured &&
      settings.processing_mode === "faithful";
    if (matchesApprovedBusyState) return "busy";
  }
  if (failed) return "error";
  if (
    loading ||
    snapshot === null ||
    permissions === null ||
    settings === null ||
    snapshot.lifecycle_state === "starting"
  ) {
    return "loading";
  }
  if (snapshot.lifecycle_state === "quitting") {
    return "error";
  }

  if (snapshot.asr_readiness === "unavailable") {
    return "error";
  }

  if (
    permissions.microphone === "not_determined" &&
    snapshot.microphone_permission === "not_determined"
  ) {
    return "empty";
  }

  const asrReady =
    snapshot.asr_readiness === "qwen_ready" ||
    snapshot.asr_readiness === "whisper_ready";
  if (snapshot.asr_readiness === "discovering") {
    return "error";
  }
  if (!asrReady) {
    return "error";
  }
  const microphoneReady = permissions.microphone === "granted";
  if (!microphoneReady) {
    return "error";
  }

  const directDeliveryReady =
    permissions.accessibility === "granted" ||
    permissions.accessibility === "not_required";
  if (
    directDeliveryReady &&
    (!snapshot.shortcut_configured ||
      !snapshot.llm_configured ||
      settings.llm === null)
  ) {
    return "partial";
  }

  if (
    directDeliveryReady &&
    snapshot.shortcut_configured &&
    snapshot.llm_configured &&
    settings.llm !== null
  ) {
    return "ready";
  }

  return "error";
}

export function effectiveProcessingMode(
  _snapshot: AppSnapshot | null,
  settings: SettingsView | null,
): ProcessingMode {
  return settings?.processing_mode ?? "raw";
}

export function selectionDetail(
  mode: ProcessingMode,
  readSelectedText: boolean,
): string {
  if (mode === "raw") {
    return "原始转录不会读取选中的文字，也不会使用第三方服务。";
  }
  if (readSelectedText) {
    return "只读取当前选中的文字，用完即清除。";
  }
  if (mode === "structured") {
    return "开启后，选中的文字只用于本次整理。";
  }
  return "开启后，选中的文字只用于本次整理。";
}
