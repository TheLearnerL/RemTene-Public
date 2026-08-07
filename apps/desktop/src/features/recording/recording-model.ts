import {
  type AppSnapshot,
  type PermissionStatusView,
} from "../../lib/ipc.ts";

export type RecordingPageState =
  | "loading"
  | "error"
  | "idle"
  | "permission-required"
  | "asr-unavailable"
  | "active";

function hasReadyLocalAsr(snapshot: AppSnapshot): boolean {
  return (
    snapshot.asr_readiness === "qwen_ready" ||
    snapshot.asr_readiness === "whisper_ready"
  );
}

export function recordingLimitLabel(seconds: number): string {
  const safeSeconds = Math.max(1, Math.round(seconds));
  const minutes = Math.floor(safeSeconds / 60);
  const remainingSeconds = safeSeconds % 60;
  if (remainingSeconds === 0) return `${minutes} 分钟`;
  if (minutes === 0) return `${remainingSeconds} 秒`;
  return `${minutes} 分 ${remainingSeconds} 秒`;
}

/**
 * 只把实时公开状态映射到能够逐字证明的 Penpot 画板。
 *
 * Shortcut Conflict／Ready、Hold Selected 和 Busy Processing 都需要当前
 * IPC 尚未公开的候选快捷键或 Session 冻结设置，因此不能从粗粒度快照推断。
 */
export function deriveRecordingPageState(
  snapshot: AppSnapshot | null,
  permissions: PermissionStatusView | null,
  loading: boolean,
  failed: boolean,
): RecordingPageState {
  if (snapshot?.active_session) return "active";
  if (failed) return "error";
  if (
    loading ||
    snapshot === null ||
    permissions === null ||
    snapshot.lifecycle_state === "starting"
  ) {
    return "loading";
  }
  if (snapshot.lifecycle_state !== "ready") {
    return "error";
  }

  const permissionsMatch =
    snapshot.microphone_permission === permissions.microphone &&
    snapshot.accessibility_permission === permissions.accessibility;
  if (!permissionsMatch) {
    return "error";
  }

  // Permission repair is independently actionable and must not be hidden behind an
  // ASR result that the user cannot obtain from this page yet. The rendered view
  // still exposes an unavailable ASR result separately when both blockers exist.
  if (permissions.microphone !== "granted") {
    return "permission-required";
  }

  if (snapshot.asr_readiness === "unavailable") {
    return "asr-unavailable";
  }

  if (
    hasReadyLocalAsr(snapshot) ||
    snapshot.asr_readiness === "discovering"
  ) {
    return "idle";
  }

  return "error";
}
