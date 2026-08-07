import {
  type BackendGateway,
  tauriBackendGateway,
} from "@/backend/gateway";
import {
  CONTRACT_VERSION,
  type AppSnapshot,
  type HistoryClearAllResult,
  type HistoryCopyResult,
  type HistoryPage,
  type HistoryRecordView,
  type LlmApiKeyState,
  type LlmApiKeyStatusView,
  type LlmConnectionTestResult,
  type LocalAsrModel,
  type PermissionStatusView,
  type ProcessingMode,
  type RevealLlmApiKeyResult,
  type SettingsView,
  type TemporaryTextCopyResult,
  type TemporaryTextDelivery,
  type UserNotification,
  type UserNotificationCode,
} from "@/lib/ipc";

type StatusPreview =
  | "partial"
  | "ready"
  | "busy"
  | "error"
  | "empty";

type ModelPreview =
  | "model-local-ready"
  | "model-local-failed"
  | "model-configured"
  | "model-not-configured"
  | "model-endpoint-saved"
  | "model-secret-recovery"
  | "model-secret-unavailable"
  | "model-connection-failed";

type NotificationPreview =
  | "error-permission"
  | "error-asr"
  | "error-llm"
  | "error-delivery";

type TemporaryTextPreview =
  | "not-inserted"
  | "temporary-not-inserted"
  | "temporary-indeterminate"
  | "temporary-llm-fallback";

type OutputPreview =
  | "output-populated"
  | "output-off"
  | "output-unavailable"
  | "output-clear"
  | "output-empty"
  | "output-copy-success"
  | "output-copy-failure";

type Preview =
  | StatusPreview
  | ModelPreview
  | NotificationPreview
  | TemporaryTextPreview
  | OutputPreview;

function previewName(): Preview | null {
  if (!import.meta.env.DEV) return null;
  const value = new URLSearchParams(window.location.search).get("preview");
  switch (value) {
    case "status-partial":
      return "partial";
    case "status-ready":
      return "ready";
    case "status-busy":
      return "busy";
    case "status-error":
      return "error";
    case "status-empty":
      return "empty";
    case "model-local-ready":
    case "model-local-failed":
    case "model-configured":
    case "model-not-configured":
    case "model-endpoint-saved":
    case "model-secret-recovery":
    case "model-secret-unavailable":
    case "model-connection-failed":
      return value;
    case "error-permission":
    case "error-asr":
    case "error-llm":
    case "error-delivery":
      return value;
    case "not-inserted":
    case "temporary-not-inserted":
    case "temporary-indeterminate":
    case "temporary-llm-fallback":
      return value;
    case "output-populated":
    case "output-off":
    case "output-unavailable":
    case "output-clear":
    case "output-empty":
    case "output-copy-success":
    case "output-copy-failure":
      return value;
    default:
      return null;
  }
}

function isModelPreview(preview: Preview): preview is ModelPreview {
  return preview.startsWith("model-");
}

function isNotificationPreview(
  preview: Preview,
): preview is NotificationPreview {
  return preview.startsWith("error-");
}

function isTemporaryTextPreview(
  preview: Preview,
): preview is TemporaryTextPreview {
  return (
    preview === "not-inserted" ||
    preview.startsWith("temporary-")
  );
}

function isOutputPreview(preview: Preview): preview is OutputPreview {
  return preview.startsWith("output-");
}

function previewNotification(
  preview: NotificationPreview,
): UserNotification {
  const codeByPreview: Record<NotificationPreview, UserNotificationCode> = {
    "error-permission": "notification.permission_microphone",
    "error-asr": "notification.asr",
    "error-llm": "notification.llm",
    "error-delivery": "notification.delivery",
  };
  return {
    contract_version: CONTRACT_VERSION,
    session_id: "00000000-0000-4000-8000-000000000001",
    code: codeByPreview[preview],
  };
}

function previewTemporaryText(
  preview: TemporaryTextPreview,
): TemporaryTextDelivery {
  const common = {
    contract_version: CONTRACT_VERSION,
    delivery_id: "00000000-0000-4000-8000-000000000001",
  };
  switch (preview) {
    case "not-inserted":
    case "temporary-not-inserted":
      return {
        ...common,
        status_code: "temporary_text.not_inserted",
        final_text: "请把明天的评审改到下午三点，并保留原来的议程。",
      };
    case "temporary-indeterminate":
      return {
        ...common,
        status_code: "temporary_text.indeterminate",
        final_text: "请把明天的评审改到下午三点，并保留原来的议程。",
      };
    case "temporary-llm-fallback":
      return {
        ...common,
        status_code: "temporary_text.llm_fallback",
        final_text: "明天下午三点评审保留原议程",
      };
  }
}

function previewHistoryTimestamp(
  daysAgo: number,
  hour: number,
  minute: number,
): string {
  const date = new Date();
  date.setDate(date.getDate() - daysAgo);
  date.setHours(hour, minute, 0, 0);
  return date.toISOString();
}

function previewHistoryPage(preview: OutputPreview): HistoryPage {
  const empty = preview === "output-empty" || preview === "output-off";
  const rows: Array<[string, number, number, number]> = [
    ["明天下午三点继续评审。", 0, 10, 24],
    ["整理发布清单，并补充风险说明。", 0, 9, 52],
    ["请把这一段改成更清晰的列表。", 1, 18, 40],
    ["确认下周发布前的验收范围。", 1, 16, 18],
    ["把风险项按优先级重新排列。", 4, 14, 5],
    ["补充本地数据边界说明。", 4, 11, 32],
  ];
  const records: HistoryRecordView[] = empty
    ? []
    : rows.map(([finalText, daysAgo, hour, minute], index) => ({
        record_id: `00000000-0000-4000-8000-${String(index + 10).padStart(12, "0")}`,
        final_text: finalText,
        created_at: previewHistoryTimestamp(daysAgo, hour, minute),
      }));
  return {
    contract_version: CONTRACT_VERSION,
    records,
  };
}

function previewSnapshot(
  preview: Exclude<StatusPreview, "error"> | ModelPreview,
): AppSnapshot {
  if (isModelPreview(preview)) {
    const asrUnavailable = preview === "model-local-failed";
    const llmConfigured =
      preview === "model-configured" ||
      preview === "model-secret-recovery" ||
      preview === "model-secret-unavailable" ||
      preview === "model-connection-failed";
    return {
      contract_version: CONTRACT_VERSION,
      lifecycle_state: "ready",
      active_session: null,
      microphone_permission: "granted",
      accessibility_permission: "granted",
      asr_readiness: asrUnavailable ? "unavailable" : "qwen_ready",
      llm_configured: llmConfigured,
      model_summary: {
        selected_model: "qwen",
        active_model_id: asrUnavailable ? null : "qwen3-asr",
        qwen_ready: !asrUnavailable,
        whisper_ready: false,
      },
      shortcut_configured: true,
      autostart_enabled: false,
    };
  }

  const ready = preview === "ready" || preview === "busy";
  return {
    contract_version: CONTRACT_VERSION,
    lifecycle_state: "ready",
    active_session:
      preview === "busy"
        ? {
            contract_version: CONTRACT_VERSION,
            session_id: "00000000-0000-4000-8000-000000000001",
            user_state: "processing",
            phase: "processing",
            recording_elapsed_ms: null,
            recording_limit_ms: null,
            can_finish: false,
            can_cancel: true,
            status_code: "session.processing",
          }
        : null,
    microphone_permission: preview === "empty" ? "denied" : "granted",
    accessibility_permission:
      preview === "empty" ? "not_determined" : "granted",
    asr_readiness: "qwen_ready",
    llm_configured: ready,
    model_summary: {
      selected_model: "qwen",
      active_model_id: "qwen3-asr",
      qwen_ready: true,
      whisper_ready: false,
    },
    shortcut_configured: ready,
    autostart_enabled: false,
  };
}

function previewPermissions(
  preview: Exclude<StatusPreview, "error"> | ModelPreview,
): PermissionStatusView {
  return {
    contract_version: CONTRACT_VERSION,
    microphone: preview === "empty" ? "denied" : "granted",
    accessibility:
      preview === "empty" ? "not_determined" : "granted",
    app_display_name: "辑语",
    process_name: "remtene-desktop",
  };
}

function previewSettings(
  preview: Exclude<StatusPreview, "error"> | ModelPreview,
  processingMode: ProcessingMode = "faithful",
  readSelectedText = false,
): SettingsView {
  const configured =
    preview === "ready" ||
    preview === "busy" ||
    preview === "model-configured" ||
    preview === "model-endpoint-saved" ||
    preview === "model-secret-recovery" ||
    preview === "model-secret-unavailable" ||
    preview === "model-connection-failed";
  return {
    contract_version: CONTRACT_VERSION,
    version: 1,
    recording_mode: "toggle",
    max_recording_duration_seconds: 600,
    recording_shortcut: null,
    processing_mode: processingMode,
    read_selected_text: readSelectedText,
    clipboard_bridge_allowed: false,
    auto_copy_result: false,
    local_diagnostics_enabled: true,
    history_policy: {
      enabled: true,
      limit: 10,
      retention_days: null,
    },
    llm: configured
      ? {
          base_url: "https://api.example.com/v1",
          model: "gpt-4.1-mini",
        }
      : null,
  };
}

function previewApiKeyState(preview: Preview): LlmApiKeyState {
  if (preview === "model-secret-unavailable") return "unavailable";
  if (
    preview === "model-configured" ||
    preview === "model-secret-recovery" ||
    preview === "model-connection-failed" ||
    preview === "ready" ||
    preview === "busy"
  ) {
    return "configured";
  }
  return "not_configured";
}

function previewApiKeyStatus(state: LlmApiKeyState): LlmApiKeyStatusView {
  return {
    contract_version: CONTRACT_VERSION,
    state,
    storage: "encrypted_local",
  };
}

export function createStatusPreviewGateway(): BackendGateway | undefined {
  const preview = previewName();
  if (preview === null) return undefined;

  const inertSurfaceMethods = {
    checkAsrHealth: () => Promise.resolve(previewSnapshot("partial")),
    switchAsrModel: (engine: LocalAsrModel) => {
      const snapshot = previewSnapshot("partial");
      const whisper = engine === "whisper";
      return Promise.resolve<AppSnapshot>({
        ...snapshot,
        asr_readiness: whisper ? "whisper_ready" : "qwen_ready",
        model_summary: {
          selected_model: engine,
          active_model_id: whisper
            ? "whisper-large-v3-turbo-q5_0-v1"
            : "qwen3-asr-0.6b-v1",
          qwen_ready: !whisper,
          whisper_ready: whisper,
        },
      });
    },
    listenToAppSnapshotChanged: () =>
      Promise.resolve(() => undefined),
    openModelDirectory: () => Promise.resolve(),
    listHistory: () =>
      Promise.resolve<HistoryPage>({
        contract_version: CONTRACT_VERSION,
        records: [],
      }),
    copyHistoryRecord: (
      recordId: string,
    ): Promise<HistoryCopyResult> =>
      Promise.resolve({
        contract_version: CONTRACT_VERSION,
        request_id: crypto.randomUUID(),
        record_id: recordId,
      }),
    clearAllHistory: (): Promise<HistoryClearAllResult> =>
      Promise.resolve({
        contract_version: CONTRACT_VERSION,
        request_id: crypto.randomUUID(),
        cleared_count: 0,
      }),
    getPendingTemporaryText: () =>
      Promise.resolve<TemporaryTextDelivery | null>(null),
    listenToTemporaryText: () =>
      Promise.resolve(() => undefined),
    dismissTemporaryText: () => Promise.resolve(),
    copyTemporaryText: (
      deliveryId: string,
    ): Promise<TemporaryTextCopyResult> =>
      Promise.resolve({
        contract_version: CONTRACT_VERSION,
        delivery_id: deliveryId,
      }),
    getPendingNotification: () =>
      Promise.resolve<UserNotification | null>(null),
    listenToNotificationRaised: () =>
      Promise.resolve(() => undefined),
    applyNotificationAction: () => Promise.resolve(),
    listenToControlPanelNavigation: () =>
      Promise.resolve(() => undefined),
  };

  if (isOutputPreview(preview)) {
    const historyPage = previewHistoryPage(preview);
    const outputSettings = previewSettings("partial");
    outputSettings.history_policy.enabled = preview !== "output-off";
    return {
      ...tauriBackendGateway,
      ...inertSurfaceMethods,
      getAppSnapshot: () => Promise.resolve(previewSnapshot("partial")),
      getPermissionStatus: () =>
        Promise.resolve(previewPermissions("partial")),
      getSettings: () => Promise.resolve(outputSettings),
      setHistoryEnabled: (_expectedVersion, enabled) =>
        Promise.resolve({
          ...outputSettings,
          version: outputSettings.version + 1,
          history_policy: {
            ...outputSettings.history_policy,
            enabled,
          },
        }),
      setHistoryLimit: (_expectedVersion, limit) =>
        Promise.resolve({
          ...outputSettings,
          version: outputSettings.version + 1,
          history_policy: {
            ...outputSettings.history_policy,
            limit,
          },
        }),
      setHistoryRetention: (_expectedVersion, retentionDays) =>
        Promise.resolve({
          ...outputSettings,
          version: outputSettings.version + 1,
          history_policy: {
            ...outputSettings.history_policy,
            retention_days: retentionDays,
          },
        }),
      listHistory:
        preview === "output-unavailable"
          ? () => Promise.reject(new Error("output preview unavailable"))
          : () => Promise.resolve(historyPage),
      listenToRecordingState: () =>
        Promise.resolve(() => undefined),
      listenToSessionEnded: () =>
        Promise.resolve(() => undefined),
      listenToSessionTerminal: () =>
        Promise.resolve(() => undefined),
    };
  }

  if (isNotificationPreview(preview)) {
    const notification = previewNotification(preview);
    return {
      ...tauriBackendGateway,
      ...inertSurfaceMethods,
      getAppSnapshot: () => Promise.resolve(previewSnapshot("partial")),
      getPermissionStatus: () =>
        Promise.resolve(previewPermissions("partial")),
      getSettings: () => Promise.resolve(previewSettings("partial")),
      getPendingNotification: () => Promise.resolve(notification),
      listenToRecordingState: () =>
        Promise.resolve(() => undefined),
      listenToSessionEnded: () =>
        Promise.resolve(() => undefined),
      listenToSessionTerminal: () =>
        Promise.resolve(() => undefined),
    };
  }

  if (isTemporaryTextPreview(preview)) {
    const delivery = previewTemporaryText(preview);
    return {
      ...tauriBackendGateway,
      ...inertSurfaceMethods,
      getAppSnapshot: () => Promise.resolve(previewSnapshot("partial")),
      getPermissionStatus: () =>
        Promise.resolve(previewPermissions("partial")),
      getSettings: () => Promise.resolve(previewSettings("partial")),
      getPendingTemporaryText: () => Promise.resolve(delivery),
      listenToRecordingState: () =>
        Promise.resolve(() => undefined),
      listenToSessionEnded: () =>
        Promise.resolve(() => undefined),
      listenToSessionTerminal: () =>
        Promise.resolve(() => undefined),
    };
  }

  if (preview === "error") {
    const unavailable = () =>
      Promise.reject(new Error("status preview unavailable"));
    return {
      ...tauriBackendGateway,
      ...inertSurfaceMethods,
      getAppSnapshot: unavailable,
      getPermissionStatus: unavailable,
      getSettings: unavailable,
      listenToRecordingState: () =>
        Promise.resolve(() => undefined),
      listenToSessionEnded: () =>
        Promise.resolve(() => undefined),
      listenToSessionTerminal: () =>
        Promise.resolve(() => undefined),
    };
  }

  let settings = previewSettings(preview);
  let apiKeyState = previewApiKeyState(preview);
  return {
    ...tauriBackendGateway,
    ...inertSurfaceMethods,
    getAppSnapshot: () => Promise.resolve(previewSnapshot(preview)),
    getPermissionStatus: () =>
      Promise.resolve(previewPermissions(preview)),
    getSettings: () => Promise.resolve(settings),
    setTextProcessingSettings: (
      _expectedVersion,
      processingMode,
      readSelectedText,
    ) => {
      settings = {
        ...settings,
        version: settings.version + 1,
        processing_mode: processingMode,
        read_selected_text: readSelectedText,
      };
      return Promise.resolve(settings);
    },
    setLlmSettings: (_expectedVersion, llm) => {
      settings = {
        ...settings,
        version: settings.version + 1,
        llm,
      };
      return Promise.resolve(settings);
    },
    getLlmApiKeyStatus: () =>
      Promise.resolve(previewApiKeyStatus(apiKeyState)),
    setLlmApiKey: () => {
      apiKeyState = "configured";
      return Promise.resolve(previewApiKeyStatus(apiKeyState));
    },
    revealLlmApiKey: () => {
      const result: RevealLlmApiKeyResult = {
        contract_version: CONTRACT_VERSION,
        request_id: crypto.randomUUID(),
        secret_value: "sk-preview-only",
      };
      return Promise.resolve(result);
    },
    deleteLlmApiKey: () => {
      apiKeyState = "not_configured";
      return Promise.resolve(previewApiKeyStatus(apiKeyState));
    },
    resetUnrecoverableLlmSecrets: () => {
      apiKeyState = "not_configured";
      return Promise.resolve(previewApiKeyStatus(apiKeyState));
    },
    testLlmConnection: () => {
      const failed = preview === "model-connection-failed";
      const result: LlmConnectionTestResult = {
        contract_version: CONTRACT_VERSION,
        request_id: crypto.randomUUID(),
        status: failed ? "failed" : "succeeded",
        error_code: failed ? "network" : null,
        upstream_error: failed
          ? {
              http_status: 502,
              response_body: '{"error":"preview upstream unavailable"}',
              truncated: false,
            }
          : null,
      };
      return Promise.resolve(result);
    },
    listenToRecordingState: () =>
      Promise.resolve(() => undefined),
    listenToSessionEnded: () =>
      Promise.resolve(() => undefined),
    listenToSessionTerminal: () =>
      Promise.resolve(() => undefined),
    startSession: () =>
      Promise.resolve("00000000-0000-4000-8000-000000000001"),
  };
}
