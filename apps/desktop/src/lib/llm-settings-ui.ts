import {
  type LlmApiKeyState,
  type LlmApiKeyStatusView,
  type LlmConnectionTestErrorCode,
  type LlmConnectionTestResult,
  type LlmSettingsView,
  type LlmUpstreamErrorView,
  type SettingsView,
  getIpcErrorCode,
} from "./ipc.ts";

export type FeedbackTone = "neutral" | "success" | "warning" | "error";

export interface Feedback {
  tone: FeedbackTone;
  text: string;
}

export interface LlmSettingsForm {
  baseUrl: string;
  model: string;
}

export type PanelOperation =
  | "save_settings"
  | "reveal_key"
  | "save_key"
  | "delete_key"
  | "reset_secrets"
  | "test_connection";

export type ConfirmationAction = "delete_key" | "reset_secrets";

export interface SecretDraftState {
  value: string;
  visible: boolean;
  source: "empty" | "input" | "revealed";
}

export interface SecretLifecycleWindowTarget {
  addEventListener(type: "blur" | "pagehide", listener: () => void): void;
  removeEventListener(type: "blur" | "pagehide", listener: () => void): void;
}

export interface SecretLifecycleDocumentTarget {
  readonly visibilityState: DocumentVisibilityState;
  addEventListener(type: "visibilitychange", listener: () => void): void;
  removeEventListener(type: "visibilitychange", listener: () => void): void;
}

export interface LlmSettingsPanelState {
  settingsLoad: "loading" | "ready" | "error";
  keyStatusLoad: "loading" | "ready" | "error";
  settings: SettingsView | null;
  form: LlmSettingsForm;
  apiKeyStatus: LlmApiKeyStatusView | null;
  secretDraft: SecretDraftState;
  operation: PanelOperation | null;
  confirmation: ConfirmationAction | null;
  settingsFeedback: Feedback | null;
  keyFeedback: Feedback | null;
  connectionFeedback: Feedback | null;
  upstreamError: LlmUpstreamErrorView | null;
}

export type LlmSettingsPanelAction =
  | { type: "settings_loading" }
  | { type: "settings_loaded"; settings: SettingsView }
  | { type: "settings_load_failed"; feedback: Feedback }
  | { type: "key_status_loading" }
  | { type: "key_status_loaded"; status: LlmApiKeyStatusView }
  | { type: "key_status_failed"; feedback: Feedback }
  | { type: "form_changed"; field: keyof LlmSettingsForm; value: string }
  | { type: "operation_started"; operation: PanelOperation }
  | { type: "operation_finished"; operation: PanelOperation }
  | { type: "settings_saved"; settings: SettingsView }
  | { type: "settings_conflict"; settings: SettingsView }
  | { type: "settings_action_failed"; feedback: Feedback }
  | { type: "secret_edited"; value: string }
  | { type: "secret_revealed"; value: string }
  | { type: "clear_sensitive" }
  | { type: "confirmation_changed"; confirmation: ConfirmationAction | null }
  | { type: "key_action_succeeded"; status: LlmApiKeyStatusView; feedback: Feedback }
  | { type: "key_action_failed"; feedback: Feedback }
  | {
      type: "connection_completed";
      feedback: Feedback;
      upstreamError: LlmUpstreamErrorView | null;
    };

const EMPTY_FORM: LlmSettingsForm = {
  baseUrl: "",
  model: "",
};

const EMPTY_SECRET_DRAFT: SecretDraftState = {
  value: "",
  visible: false,
  source: "empty",
};

export function createInitialLlmSettingsPanelState(): LlmSettingsPanelState {
  return {
    settingsLoad: "loading",
    keyStatusLoad: "loading",
    settings: null,
    form: { ...EMPTY_FORM },
    apiKeyStatus: null,
    secretDraft: { ...EMPTY_SECRET_DRAFT },
    operation: null,
    confirmation: null,
    settingsFeedback: null,
    keyFeedback: null,
    connectionFeedback: null,
    upstreamError: null,
  };
}

export function llmSettingsPanelReducer(
  state: LlmSettingsPanelState,
  action: LlmSettingsPanelAction,
): LlmSettingsPanelState {
  switch (action.type) {
    case "settings_loading":
      return {
        ...state,
        settingsLoad: "loading",
        settingsFeedback: null,
        connectionFeedback: null,
        upstreamError: null,
      };
    case "settings_loaded":
      return {
        ...state,
        settingsLoad: "ready",
        settings: action.settings,
        form: formFromSettings(action.settings),
        settingsFeedback: null,
        connectionFeedback: null,
        upstreamError: null,
      };
    case "settings_load_failed":
      return {
        ...state,
        settingsLoad: "error",
        settingsFeedback: action.feedback,
        connectionFeedback: null,
        upstreamError: null,
      };
    case "key_status_loading":
      return {
        ...state,
        keyStatusLoad: "loading",
        keyFeedback: null,
        upstreamError: null,
      };
    case "key_status_loaded":
      return {
        ...state,
        keyStatusLoad: "ready",
        apiKeyStatus: action.status,
        keyFeedback: null,
      };
    case "key_status_failed":
      return {
        ...state,
        keyStatusLoad: "error",
        keyFeedback: action.feedback,
        upstreamError: null,
      };
    case "form_changed":
      return {
        ...state,
        form: {
          ...state.form,
          [action.field]: action.value,
        },
        settingsFeedback: null,
        connectionFeedback: null,
        upstreamError: null,
      };
    case "operation_started":
      if (state.operation !== null) return state;
      return {
        ...state,
        operation: action.operation,
        confirmation: null,
        connectionFeedback:
          action.operation === "test_connection" ? null : state.connectionFeedback,
        upstreamError: null,
      };
    case "operation_finished":
      return state.operation === action.operation ? { ...state, operation: null } : state;
    case "settings_saved":
      return {
        ...state,
        settingsLoad: "ready",
        settings: action.settings,
        form: formFromSettings(action.settings),
        secretDraft: { ...EMPTY_SECRET_DRAFT },
        settingsFeedback: {
          tone: "success",
          text: "服务设置已保存。",
        },
        connectionFeedback: null,
        upstreamError: null,
      };
    case "settings_conflict":
      return {
        ...state,
        settingsLoad: "ready",
        settings: action.settings,
        form: formFromSettings(action.settings),
        secretDraft: { ...EMPTY_SECRET_DRAFT },
        settingsFeedback: {
          tone: "warning",
          text: "设置已在其他窗口更新。请确认最新内容后重新保存。",
        },
        connectionFeedback: null,
        upstreamError: null,
      };
    case "settings_action_failed":
      return {
        ...state,
        secretDraft: { ...EMPTY_SECRET_DRAFT },
        settingsFeedback: action.feedback,
        upstreamError: null,
      };
    case "secret_edited":
      return {
        ...state,
        secretDraft: {
          value: action.value,
          visible: state.secretDraft.visible,
          source: action.value.length === 0 ? "empty" : "input",
        },
        keyFeedback: null,
        connectionFeedback: null,
        upstreamError: null,
      };
    case "secret_revealed":
      return {
        ...state,
        secretDraft: {
          value: action.value,
          visible: true,
          source: "revealed",
        },
        keyFeedback: {
          tone: "warning",
          text: "API Key 正在当前窗口短暂显示，离开窗口后会立即隐藏。",
        },
      };
    case "clear_sensitive":
      return {
        ...state,
        secretDraft: { ...EMPTY_SECRET_DRAFT },
        confirmation: null,
        keyFeedback: state.secretDraft.source === "revealed" ? null : state.keyFeedback,
        upstreamError: null,
      };
    case "confirmation_changed":
      return {
        ...state,
        confirmation: action.confirmation,
      };
    case "key_action_succeeded":
      return {
        ...state,
        keyStatusLoad: "ready",
        apiKeyStatus: action.status,
        secretDraft: { ...EMPTY_SECRET_DRAFT },
        confirmation: null,
        keyFeedback: action.feedback,
        connectionFeedback: null,
        upstreamError: null,
      };
    case "key_action_failed":
      return {
        ...state,
        secretDraft: { ...EMPTY_SECRET_DRAFT },
        confirmation: null,
        keyFeedback: action.feedback,
        upstreamError: null,
      };
    case "connection_completed":
      return {
        ...state,
        connectionFeedback: action.feedback,
        upstreamError: action.upstreamError,
      };
  }
}

export function installSecretLifecycleGuards(
  windowTarget: SecretLifecycleWindowTarget,
  documentTarget: SecretLifecycleDocumentTarget,
  clearSensitiveState: () => void,
): () => void {
  const handleWindowBlur = () => clearSensitiveState();
  const handlePageHide = () => clearSensitiveState();
  const handleVisibilityChange = () => {
    if (documentTarget.visibilityState !== "visible") clearSensitiveState();
  };

  windowTarget.addEventListener("blur", handleWindowBlur);
  windowTarget.addEventListener("pagehide", handlePageHide);
  documentTarget.addEventListener("visibilitychange", handleVisibilityChange);

  return () => {
    clearSensitiveState();
    windowTarget.removeEventListener("blur", handleWindowBlur);
    windowTarget.removeEventListener("pagehide", handlePageHide);
    documentTarget.removeEventListener("visibilitychange", handleVisibilityChange);
  };
}

export function clearSensitiveStateWhenWindowLosesFocus(
  focused: boolean,
  clearSensitiveState: () => void,
): void {
  if (!focused) clearSensitiveState();
}

export function isSensitiveResultCurrent(
  requestGeneration: number,
  currentGeneration: number,
): boolean {
  return requestGeneration === currentGeneration;
}

export function formFromSettings(settings: SettingsView): LlmSettingsForm {
  return settings.llm
    ? {
        baseUrl: settings.llm.base_url,
        model: settings.llm.model,
      }
    : { ...EMPTY_FORM };
}

export function validateLlmSettingsForm(
  form: LlmSettingsForm,
): { ok: true; value: LlmSettingsView } | { ok: false; feedback: Feedback } {
  const baseUrl = form.baseUrl.trim();
  const model = form.model.trim();
  if (baseUrl.length === 0 || model.length === 0) {
    return {
      ok: false,
      feedback: {
        tone: "error",
        text: "请填写服务地址和模型名称。",
      },
    };
  }

  let parsed: URL;
  try {
    parsed = new URL(baseUrl);
  } catch {
    return {
      ok: false,
      feedback: {
        tone: "error",
        text: "服务地址格式不正确，请填写完整的 http:// 或 https:// 地址。",
      },
    };
  }
  if (parsed.protocol !== "https:" && parsed.protocol !== "http:") {
    return {
      ok: false,
      feedback: {
        tone: "error",
        text: "服务地址必须以 http:// 或 https:// 开头。",
      },
    };
  }

  return {
    ok: true,
    value: {
      base_url: baseUrl,
      model,
    },
  };
}

export function hasSettingsChanges(
  settings: SettingsView | null,
  form: LlmSettingsForm,
): boolean {
  if (settings === null) return false;
  const current = formFromSettings(settings);
  return current.baseUrl !== form.baseUrl || current.model !== form.model;
}

export function apiKeyStateFeedback(state: LlmApiKeyState): Feedback {
  switch (state) {
    case "not_configured":
      return {
        tone: "neutral",
        text: "尚未保存 API Key。本地原始转录仍可使用。",
      };
    case "configured":
      return {
        tone: "success",
        text: "API Key 已安全保存在本机。",
      };
    case "recovery_required":
      return {
        tone: "error",
        text: "原 API Key 无法读取。清除失效记录后才能重新配置。",
      };
    case "unavailable":
      return {
        tone: "error",
        text: "暂时无法读取 API Key，目前不能清除或替换。",
      };
  }
}

export function connectionTestFeedback(result: LlmConnectionTestResult): Feedback {
  if (result.status === "succeeded") {
    return {
      tone: "success",
      text: "连接成功，当前文字服务可以使用。",
    };
  }
  return connectionErrorFeedback(result.error_code ?? "internal");
}

export function connectionErrorFeedback(code: LlmConnectionTestErrorCode): Feedback {
  const messages: Record<LlmConnectionTestErrorCode, string> = {
    busy: "当前有输入任务正在进行，请结束后再测试连接。",
    runtime_unavailable: "暂时无法读取应用状态，请稍后重试。",
    settings_unavailable: "服务设置暂时无法读取，请重新加载。",
    not_configured: "请先保存服务设置和 API Key。",
    recovery_required: "原 API Key 无法读取，请先清除失效记录并重新输入。",
    secret_unavailable: "暂时无法读取 API Key，请稍后重试。",
    invalid_configuration: "服务设置不正确，请检查服务地址和模型名称。",
    authentication_failed: "认证失败，请检查 API Key 是否正确或已失效。",
    permission_denied:
      "服务已识别 API Key，但没有权限使用当前模型。请检查模型权限、账户状态或所在地区。",
    rate_limited: "请求过于频繁或额度不足，请稍后重试或检查账户状态。",
    timeout: "连接超时，请检查网络或稍后重试。",
    network: "无法连接服务，请检查网络和服务地址。",
    provider_unavailable: "文字服务暂时不可用，请稍后重试。",
    request_rejected: "服务拒绝了测试请求，请检查服务地址和模型。",
    invalid_response: "服务返回的内容格式不兼容。",
    response_too_large: "服务返回的内容过多，无法显示。",
    cancelled: "连接测试已取消。",
    internal: "连接测试失败，请稍后重试。",
  };
  return {
    tone: code === "cancelled" ? "warning" : "error",
    text: messages[code],
  };
}

export function llmActionErrorFeedback(error: unknown, fallback: string): Feedback {
  const code = getIpcErrorCode(error);
  const messages: Record<string, string> = {
    "settings.version_conflict": "设置已在其他位置更新，请重新加载后再保存。",
    "settings.llm_invalid": "服务设置不正确，请检查服务地址和模型名称。",
    "settings.invalid": "设置内容无效，请检查后重试。",
    "settings.store_unavailable": "设置暂时无法读取或保存，请稍后重试。",
    "llm.configuration_busy": "当前有输入任务正在进行，请结束后再修改 AI 配置。",
    "llm.not_configured": "请先保存服务设置。",
    "llm.runtime_unavailable": "暂时无法读取应用状态，请稍后重试。",
    "secret.value_invalid": "请输入有效的 API Key。",
    "secret.recovery_required": "原 API Key 无法读取，请先清除失效记录。",
    "secret.verification_failed": "密钥保存后未能通过验证，请重新输入。",
    "secret.reset_confirmation_required": "必须明确确认数据损失后才能清除失效秘密。",
    "secret.reset_not_required": "当前不需要清除失效记录，请重新加载。",
  };
  return {
    tone: code === "settings.version_conflict" ? "warning" : "error",
    text: (code && messages[code]) || fallback,
  };
}

export function isSettingsVersionConflict(error: unknown): boolean {
  return getIpcErrorCode(error) === "settings.version_conflict";
}

export function canTestConnection(state: LlmSettingsPanelState): boolean {
  return (
    state.operation === null &&
    state.settingsLoad === "ready" &&
    state.keyStatusLoad === "ready" &&
    state.settings !== null &&
    state.settings.llm !== null &&
    state.apiKeyStatus?.state === "configured" &&
    !hasSettingsChanges(state.settings, state.form) &&
    state.secretDraft.value.length === 0
  );
}
