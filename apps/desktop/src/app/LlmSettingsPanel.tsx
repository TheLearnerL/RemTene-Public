import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  useCallback,
  useEffect,
  useReducer,
  useRef,
  useState,
} from "react";

import { useBackendGateway } from "@/backend/useBackendGateway";
import { Button } from "@/components/ui/button";
import { type LlmUpstreamErrorView } from "@/lib/ipc";
import {
  type ConfirmationAction,
  type Feedback,
  type PanelOperation,
  apiKeyStateFeedback,
  canTestConnection,
  clearSensitiveStateWhenWindowLosesFocus,
  connectionTestFeedback,
  createInitialLlmSettingsPanelState,
  hasSettingsChanges,
  installSecretLifecycleGuards,
  isSensitiveResultCurrent,
  isSettingsVersionConflict,
  llmActionErrorFeedback,
  llmSettingsPanelReducer,
  validateLlmSettingsForm,
} from "@/lib/llm-settings-ui";

export type LlmSettingsVisualState =
  | "configured"
  | "not-configured"
  | "endpoint-saved"
  | "secret-recovery"
  | "secret-unavailable"
  | "connection-failed";

type LlmSettingsDisplayState =
  | LlmSettingsVisualState
  | "loading"
  | "settings-unavailable"
  | "key-status-unavailable";

interface LlmSettingsPanelProps {
  onConfigurationChanged?: () => void | Promise<void>;
  visualState?: LlmSettingsVisualState;
}

const feedbackClasses: Record<Feedback["tone"], string> = {
  neutral: "border-border bg-background text-muted-foreground",
  success: "border-emerald-700/25 bg-emerald-500/8 text-emerald-800 dark:text-emerald-300",
  warning: "border-amber-700/25 bg-amber-500/8 text-amber-900 dark:text-amber-200",
  error: "border-red-700/25 bg-red-500/8 text-red-800 dark:text-red-300",
};

function FeedbackMessage({
  feedback,
  className = "",
}: {
  feedback: Feedback | null;
  className?: string;
}) {
  if (feedback === null) return null;
  return (
    <p
      className={`model-inline-feedback border ${feedbackClasses[feedback.tone]} ${className}`}
      role={feedback.tone === "error" ? "alert" : "status"}
      aria-live="polite"
    >
      {feedback.text}
    </p>
  );
}

function UpstreamErrorMessage({
  upstream,
}: {
  upstream: LlmUpstreamErrorView | null;
}) {
  if (upstream === null) return null;
  return (
    <section
      className="model-upstream-error"
      aria-labelledby="llm-upstream-error-title"
    >
      <div className="model-upstream-error-heading">
        <strong id="llm-upstream-error-title">
          服务返回信息（已隐藏敏感内容） · HTTP {upstream.http_status}
        </strong>
        {upstream.truncated ? <span>响应未完整显示</span> : null}
      </div>
      <pre tabIndex={0}>
        {upstream.response_body.length > 0
          ? upstream.response_body
          : "（服务没有返回详细信息）"}
      </pre>
      <p>API Key 等敏感信息已隐藏，也不会写入日志或历史。</p>
    </section>
  );
}

function Confirmation({
  action,
  busy,
  onCancel,
  onConfirm,
}: {
  action: ConfirmationAction;
  busy: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const reset = action === "reset_secrets";
  return (
    <div
      className="model-confirmation"
      role="alertdialog"
      aria-labelledby={`llm-${action}-title`}
      aria-describedby={`llm-${action}-description`}
    >
      <p id={`llm-${action}-title`} className="model-confirmation-title">
        {reset ? "确认清除失效密钥记录？" : "确认删除 API Key？"}
      </p>
      <p
        id={`llm-${action}-description`}
        className="model-confirmation-description"
      >
        {reset
          ? "此操作会清除系统中无法恢复的密钥记录，且无法撤销。完成后需要重新输入 API Key。"
          : "删除后，软件不再整理语音内容，但本地原始语音转录仍然可用。"}
      </p>
      <div className="model-confirmation-actions">
        <Button
          variant="destructive"
          disabled={busy}
          onClick={onConfirm}
        >
          {reset ? "确认清除" : "确认删除"}
        </Button>
        <Button variant="outline" disabled={busy} onClick={onCancel}>
          取消
        </Button>
      </div>
    </div>
  );
}

export default function LlmSettingsPanel({
  onConfigurationChanged,
  visualState,
}: LlmSettingsPanelProps) {
  const gateway = useBackendGateway();
  const [state, dispatch] = useReducer(
    llmSettingsPanelReducer,
    undefined,
    createInitialLlmSettingsPanelState,
  );
  const mountedRef = useRef(false);
  const settingsRequestRef = useRef(0);
  const keyStatusRequestRef = useRef(0);
  const operationRef = useRef<PanelOperation | null>(null);
  const sensitiveGenerationRef = useRef(0);
  const [editingSettings, setEditingSettings] = useState(false);

  const clearSensitiveState = useCallback(() => {
    sensitiveGenerationRef.current += 1;
    dispatch({ type: "clear_sensitive" });
  }, []);

  const loadSettings = useCallback(async () => {
    const request = ++settingsRequestRef.current;
    dispatch({ type: "settings_loading" });
    try {
      const settings = await gateway.getSettings();
      if (mountedRef.current && request === settingsRequestRef.current) {
        dispatch({ type: "settings_loaded", settings });
      }
    } catch (error) {
      if (mountedRef.current && request === settingsRequestRef.current) {
        dispatch({
          type: "settings_load_failed",
          feedback: llmActionErrorFeedback(error, "服务设置加载失败，请重试。"),
        });
      }
    }
  }, [gateway]);

  const loadKeyStatus = useCallback(async () => {
    const request = ++keyStatusRequestRef.current;
    dispatch({ type: "key_status_loading" });
    try {
      const status = await gateway.getLlmApiKeyStatus();
      if (mountedRef.current && request === keyStatusRequestRef.current) {
        dispatch({ type: "key_status_loaded", status });
      }
    } catch (error) {
      if (mountedRef.current && request === keyStatusRequestRef.current) {
        dispatch({
          type: "key_status_failed",
          feedback: llmActionErrorFeedback(error, "API Key 状态加载失败，请重试。"),
        });
      }
    }
  }, [gateway]);

  const notifyConfigurationChanged = useCallback(async () => {
    try {
      await onConfigurationChanged?.();
    } catch {
      // 公开快照刷新失败不改变已经持久化的设置；App 下次聚焦时仍会重新读取。
    }
  }, [onConfigurationChanged]);

  const beginOperation = useCallback((operation: PanelOperation): boolean => {
    if (operationRef.current !== null) return false;
    operationRef.current = operation;
    dispatch({ type: "operation_started", operation });
    return true;
  }, []);

  const finishOperation = useCallback((operation: PanelOperation) => {
    if (operationRef.current === operation) {
      operationRef.current = null;
    }
    if (mountedRef.current) {
      dispatch({ type: "operation_finished", operation });
    }
  }, []);

  useEffect(() => {
    mountedRef.current = true;
    void loadSettings();
    void loadKeyStatus();

    const stopDomLifecycleGuards = installSecretLifecycleGuards(
      window,
      document,
      clearSensitiveState,
    );

    let active = true;
    let stopFocusListener: (() => void) | null = null;
    try {
      void getCurrentWindow()
        .onFocusChanged(({ payload: focused }) => {
          if (active) {
            clearSensitiveStateWhenWindowLosesFocus(focused, clearSensitiveState);
          }
        })
        .then((stop) => {
          if (active) stopFocusListener = stop;
          else stop();
        })
        .catch(() => {
          // 普通浏览器预览没有 Tauri window API；DOM 生命周期监听仍然生效。
        });
    } catch {
      // getCurrentWindow 本身在无 Tauri globals 的浏览器里会同步失败。
      // Secret 草稿仍受 visibility/pagehide/beforeunload 与卸载清理保护。
    }

    return () => {
      stopDomLifecycleGuards();
      mountedRef.current = false;
      operationRef.current = null;
      active = false;
      stopFocusListener?.();
    };
  }, [clearSensitiveState, loadKeyStatus, loadSettings]);

  useEffect(() => {
    if (
      state.secretDraft.source !== "revealed" ||
      !state.secretDraft.visible
    ) {
      return;
    }
    const timeout = window.setTimeout(clearSensitiveState, 30_000);
    return () => window.clearTimeout(timeout);
  }, [
    clearSensitiveState,
    state.secretDraft.source,
    state.secretDraft.visible,
  ]);

  const saveSettings = async () => {
    if (state.settings === null || !beginOperation("save_settings")) return;
    const validation = validateLlmSettingsForm(state.form);
    if (!validation.ok) {
      dispatch({ type: "settings_action_failed", feedback: validation.feedback });
      finishOperation("save_settings");
      return;
    }

    clearSensitiveState();
    try {
      const stored = await gateway.setLlmSettings(
        state.settings.version,
        validation.value,
      );
      if (!mountedRef.current) return;
      dispatch({ type: "settings_saved", settings: stored });
      setEditingSettings(false);
      await loadKeyStatus();
      await notifyConfigurationChanged();
    } catch (error) {
      if (!mountedRef.current) return;
      if (isSettingsVersionConflict(error)) {
        try {
          const latest = await gateway.getSettings();
          if (!mountedRef.current) return;
          dispatch({ type: "settings_conflict", settings: latest });
          await loadKeyStatus();
        } catch (reloadError) {
          if (mountedRef.current) {
            dispatch({
              type: "settings_action_failed",
              feedback: llmActionErrorFeedback(
                reloadError,
                "检测到设置冲突，但最新设置加载失败。请稍后重试。",
              ),
            });
          }
        }
      } else {
        dispatch({
          type: "settings_action_failed",
          feedback: llmActionErrorFeedback(error, "服务设置保存失败，请重试。"),
        });
      }
    } finally {
      clearSensitiveState();
      finishOperation("save_settings");
    }
  };

  const saveInitialConfiguration = async () => {
    if (
      state.settings === null ||
      state.settings.llm !== null ||
      !beginOperation("save_settings")
    ) {
      return;
    }
    const validation = validateLlmSettingsForm(state.form);
    if (!validation.ok) {
      dispatch({ type: "settings_action_failed", feedback: validation.feedback });
      finishOperation("save_settings");
      return;
    }
    if (state.secretDraft.value.trim().length === 0) {
      dispatch({
        type: "key_action_failed",
        feedback: { tone: "error", text: "请输入 API Key。" },
      });
      finishOperation("save_settings");
      return;
    }

    const secretValue = state.secretDraft.value;
    let settingsSaved = false;
    clearSensitiveState();
    try {
      const stored = await gateway.setLlmSettings(
        state.settings.version,
        validation.value,
      );
      settingsSaved = true;
      if (!mountedRef.current) return;
      dispatch({ type: "settings_saved", settings: stored });

      const status = await gateway.setLlmApiKey(secretValue);
      if (!mountedRef.current) return;
      dispatch({
        type: "key_action_succeeded",
        status,
        feedback: {
          tone: "success",
          text: "服务地址、模型与 API Key 已保存。",
        },
      });
      setEditingSettings(false);
      await notifyConfigurationChanged();
    } catch (error) {
      if (!mountedRef.current) return;
      if (settingsSaved) {
        const keyFailure = llmActionErrorFeedback(
          error,
          "API Key 保存失败，请重新输入后重试。",
        );
        dispatch({
          type: "key_action_failed",
          feedback: {
            ...keyFailure,
            text: `服务地址与模型已保存，但 API Key 未保存。${keyFailure.text}`,
          },
        });
        await loadKeyStatus();
        await notifyConfigurationChanged();
      } else if (isSettingsVersionConflict(error)) {
        try {
          const latest = await gateway.getSettings();
          if (!mountedRef.current) return;
          dispatch({ type: "settings_conflict", settings: latest });
          await loadKeyStatus();
        } catch (reloadError) {
          if (mountedRef.current) {
            dispatch({
              type: "settings_action_failed",
              feedback: llmActionErrorFeedback(
                reloadError,
                "检测到设置冲突，但最新设置加载失败。请稍后重试。",
              ),
            });
          }
        }
      } else {
        dispatch({
          type: "settings_action_failed",
          feedback: llmActionErrorFeedback(error, "服务设置保存失败，请重试。"),
        });
      }
    } finally {
      clearSensitiveState();
      finishOperation("save_settings");
    }
  };

  const revealKey = async () => {
    if (!beginOperation("reveal_key")) return;
    const requestGeneration = sensitiveGenerationRef.current;
    try {
      const result = await gateway.revealLlmApiKey();
      if (
        mountedRef.current &&
        isSensitiveResultCurrent(requestGeneration, sensitiveGenerationRef.current)
      ) {
        dispatch({ type: "secret_revealed", value: result.secret_value });
      }
    } catch (error) {
      if (mountedRef.current) {
        dispatch({
          type: "key_action_failed",
          feedback: llmActionErrorFeedback(error, "API Key 查看失败，请重试。"),
        });
        await loadKeyStatus();
      }
    } finally {
      finishOperation("reveal_key");
    }
  };

  const saveKey = async () => {
    if (state.secretDraft.value.trim().length === 0) {
      dispatch({
        type: "key_action_failed",
        feedback: { tone: "error", text: "请输入 API Key。" },
      });
      return;
    }
    if (
      state.settings?.llm === null ||
      state.settings === null ||
      hasSettingsChanges(state.settings, state.form)
    ) {
      dispatch({
        type: "key_action_failed",
        feedback: { tone: "warning", text: "请先保存服务地址和模型，再保存 API Key。" },
      });
      return;
    }
    if (!beginOperation("save_key")) return;

    const secretValue = state.secretDraft.value;
    clearSensitiveState();
    try {
      const status = await gateway.setLlmApiKey(secretValue);
      if (!mountedRef.current) return;
      dispatch({
        type: "key_action_succeeded",
        status,
        feedback: { tone: "success", text: "API Key 已安全保存。" },
      });
      await notifyConfigurationChanged();
    } catch (error) {
      if (mountedRef.current) {
        dispatch({
          type: "key_action_failed",
          feedback: llmActionErrorFeedback(error, "API Key 保存失败，请重新输入后重试。"),
        });
        await loadKeyStatus();
      }
    } finally {
      clearSensitiveState();
      finishOperation("save_key");
    }
  };

  const deleteKey = async () => {
    if (!beginOperation("delete_key")) return;
    clearSensitiveState();
    try {
      const status = await gateway.deleteLlmApiKey();
      if (!mountedRef.current) return;
      dispatch({
        type: "key_action_succeeded",
        status,
        feedback: { tone: "success", text: "API Key 已删除，本地原始转录不受影响。" },
      });
      await notifyConfigurationChanged();
    } catch (error) {
      if (mountedRef.current) {
        dispatch({
          type: "key_action_failed",
          feedback: llmActionErrorFeedback(error, "API Key 删除失败，请重试。"),
        });
        await loadKeyStatus();
      }
    } finally {
      clearSensitiveState();
      finishOperation("delete_key");
    }
  };

  const resetSecrets = async () => {
    if (!beginOperation("reset_secrets")) return;
    clearSensitiveState();
    try {
      const status = await gateway.resetUnrecoverableLlmSecrets(true);
      if (!mountedRef.current) return;
      dispatch({
        type: "key_action_succeeded",
        status,
        feedback: { tone: "success", text: "失效记录已清除，请重新输入 API Key。" },
      });
      await notifyConfigurationChanged();
    } catch (error) {
      if (mountedRef.current) {
        dispatch({
          type: "key_action_failed",
          feedback: llmActionErrorFeedback(error, "失效记录清除失败，请重试。"),
        });
        await loadKeyStatus();
      }
    } finally {
      clearSensitiveState();
      finishOperation("reset_secrets");
    }
  };

  const testConnection = async () => {
    if (!canTestConnection(state) || !beginOperation("test_connection")) return;
    clearSensitiveState();
    try {
      const result = await gateway.testLlmConnection();
      if (mountedRef.current) {
        dispatch({
          type: "connection_completed",
          feedback: connectionTestFeedback(result),
          upstreamError: result.upstream_error,
        });
      }
    } catch (error) {
      if (mountedRef.current) {
        dispatch({
          type: "connection_completed",
          feedback: llmActionErrorFeedback(error, "连接测试失败，请稍后重试。"),
          upstreamError: null,
        });
      }
    } finally {
      finishOperation("test_connection");
    }
  };

  const busy = state.operation !== null;
  const keyState = state.apiKeyStatus?.state;
  const keyUnavailable = keyState === "unavailable";
  const keyNeedsRecovery = keyState === "recovery_required";
  const keyConfigured = keyState === "configured";
  const inferredVisualState: LlmSettingsDisplayState =
    state.settingsLoad === "error"
      ? "settings-unavailable"
      : state.settingsLoad === "loading" ||
          state.keyStatusLoad === "loading"
        ? "loading"
        : state.keyStatusLoad === "error"
          ? "key-status-unavailable"
          : keyUnavailable
            ? "secret-unavailable"
            : state.secretDraft.source === "revealed" &&
                state.secretDraft.visible
              ? "secret-recovery"
              : keyNeedsRecovery
                ? "secret-unavailable"
                : state.connectionFeedback?.tone === "error"
                  ? "connection-failed"
                  : state.settings?.llm === null
                    ? "not-configured"
                    : keyConfigured
                      ? "configured"
                      : "endpoint-saved";
  const displayState = visualState ?? inferredVisualState;
  const approvedVisualPreview = visualState !== undefined;
  const settingsReady =
    state.settingsLoad === "ready" && state.settings !== null;
  const settingsConfigured = state.settings?.llm != null;
  const showSettingsEditor =
    editingSettings || displayState === "not-configured";
  const showSecretEditor = displayState === "endpoint-saved";

  const bannerCopy: Record<
    LlmSettingsDisplayState,
    { title: string; detail: string; tone: "success" | "warning" | "error" }
  > = {
    loading: {
      title: "正在读取第三方文字服务",
      detail: "正在读取已保存的服务设置和 API Key 状态。",
      tone: "warning",
    },
    "settings-unavailable": {
      title: "第三方文字服务设置暂不可读",
      detail: "已保存的设置没有改变，请稍后重新加载。",
      tone: "error",
    },
    "key-status-unavailable": {
      title: "API Key 状态暂不可读",
      detail: "服务地址仍可查看，请稍后重新检查 API Key。",
      tone: "error",
    },
    configured: {
      title: "第三方文字服务已配置",
      detail: "只有文字整理会发送转录文字；原始转录不会发送任何内容。",
      tone: "success",
    },
    "not-configured": {
      title: "填写第三方文字服务",
      detail: "填写服务地址、模型名称和 API Key 后保存。",
      tone: "warning",
    },
    "endpoint-saved": {
      title: "服务地址已保存",
      detail: "继续填写 API Key。密钥会安全保存在系统中。",
      tone: "success",
    },
    "secret-recovery": {
      title: "API Key 正在短暂查看",
      detail: "完整内容只在当前窗口显示，离开页面或倒计时结束后会隐藏。",
      tone: "warning",
    },
    "secret-unavailable": {
      title: "暂时无法读取 API Key",
      detail: "服务地址仍可查看，但文字整理暂时无法使用。",
      tone: "error",
    },
    "connection-failed": {
      title: "第三方文字服务连接失败",
      detail: "检查服务地址、模型名称和 API Key 后重试。已保存设置不会改变。",
      tone: "error",
    },
  };

  const stepOneStatus =
    approvedVisualPreview
      ? displayState === "not-configured"
        ? "尚未保存"
        : "已保存"
      : displayState === "settings-unavailable"
        ? "状态未知"
        : !settingsReady
          ? "正在读取"
          : state.settings?.llm === null
            ? "尚未保存"
            : "已保存";
  const stepTwoStatus =
    displayState === "loading"
      ? "正在读取"
      : displayState === "settings-unavailable"
        ? "等待第 1 步"
        : displayState === "key-status-unavailable"
          ? "状态未知"
          : displayState === "not-configured"
            ? "待保存"
            : displayState === "endpoint-saved"
              ? "等待密钥"
              : displayState === "secret-unavailable"
                ? keyNeedsRecovery
                  ? "需要恢复"
                  : "安全存储不可用"
                : displayState === "secret-recovery"
                  ? "短暂查看中"
                  : "已安全保存";
  const stepThreeStatus =
    displayState === "loading"
      ? "正在读取"
      : displayState === "settings-unavailable" ||
          displayState === "key-status-unavailable"
        ? "暂不可用"
        : displayState === "not-configured"
          ? "等待保存"
          : displayState === "endpoint-saved"
            ? "等待第 2 步"
            : displayState === "secret-unavailable"
              ? "暂不可用"
              : displayState === "connection-failed"
                ? "测试失败"
                : "可测试";

  const footerRight =
    displayState === "loading"
      ? "正在读取"
      : displayState === "settings-unavailable"
        ? "配置：状态未知"
        : displayState === "key-status-unavailable"
          ? "密钥：状态未知"
          : displayState === "configured"
            ? approvedVisualPreview
              ? "连接：可用"
              : state.connectionFeedback?.tone === "success"
                ? "连接：测试通过"
                : "连接：待测试"
            : displayState === "not-configured"
              ? "尚未完成配置"
              : displayState === "endpoint-saved"
                ? "等待 API Key"
                : displayState === "secret-recovery"
                  ? "密钥：短暂显示"
                  : displayState === "secret-unavailable"
                    ? keyNeedsRecovery
                      ? "密钥：需要恢复"
                      : "文字整理：不可用"
                    : "连接：失败";

  const requestDelete = () => {
    clearSensitiveState();
    dispatch({
      type: "confirmation_changed",
      confirmation: "delete_key",
    });
  };

  const readonlyBaseUrl =
    displayState === "settings-unavailable"
      ? "当前无法读取"
      : displayState === "loading" && !settingsReady
        ? "正在读取…"
        : approvedVisualPreview && displayState === "endpoint-saved"
          ? "例如：https://api.example.com/v1"
          : state.form.baseUrl ||
            (approvedVisualPreview
              ? "https://api.example.com/v1"
              : "尚未填写");
  const readonlyModel =
    displayState === "settings-unavailable"
      ? "当前无法读取"
      : displayState === "loading" && !settingsReady
        ? "正在读取…"
        : approvedVisualPreview && displayState === "endpoint-saved"
          ? "输入模型名称"
          : state.form.model ||
            (approvedVisualPreview ? "gpt-4.1-mini" : "尚未填写");
  const connectionHint =
    displayState === "loading"
      ? "正在读取已保存配置与 API Key 状态"
      : displayState === "settings-unavailable"
        ? "重新加载服务设置后即可测试连接"
        : displayState === "key-status-unavailable"
          ? "重新检查 API Key 后即可测试连接"
          : displayState === "not-configured"
            ? "填写并保存服务地址、模型名称和 API Key 后即可测试"
            : displayState === "endpoint-saved"
              ? "保存 API Key 后才能测试已保存配置"
              : displayState === "secret-unavailable"
                ? keyNeedsRecovery
                  ? "清除失效记录并重新输入 API Key 后即可测试"
                  : "恢复读取 API Key 后即可测试"
                : displayState === "secret-recovery"
                  ? "API Key 正在显示；测试仍会使用已保存的设置"
                  : displayState === "connection-failed"
                    ? "请检查已保存的服务地址、模型和 API Key"
                    : "测试只使用已保存的设置，不会保存当前输入";
  const connectionSummary =
    displayState === "loading"
      ? "正在读取可测试状态"
      : displayState === "settings-unavailable"
        ? "无法读取已保存服务设置"
        : displayState === "key-status-unavailable"
          ? "无法读取已保存 API Key 状态"
          : displayState === "not-configured"
            ? "当前没有可测试的已保存配置"
            : displayState === "endpoint-saved"
              ? "服务地址与模型已保存，尚缺 API Key"
              : displayState === "secret-unavailable"
                ? keyNeedsRecovery
                  ? "原 API Key 无法读取，暂时不能测试"
                  : "无法读取已保存 API Key"
                : displayState === "connection-failed"
                  ? "上次测试未通过，可修正后重新测试"
                  : "已保存服务地址、模型与 API Key";
  const connectionStateAvailable =
    displayState === "configured" ||
    displayState === "connection-failed" ||
    displayState === "secret-recovery";
  const connectionPreviouslyTested =
    state.connectionFeedback?.tone === "success" ||
    state.connectionFeedback?.tone === "error";

  return (
    <>
      <section
        className="model-banner"
        data-tone={bannerCopy[displayState].tone}
        aria-live="polite"
      >
        <span className="model-banner-dot" aria-hidden="true" />
        <div className="model-banner-copy">
          <strong>{bannerCopy[displayState].title}</strong>
          <span>{bannerCopy[displayState].detail}</span>
        </div>
      </section>

      <section
        className="model-service-card"
        aria-label="第三方文字服务"
      >
        <div
          className="model-service-scroll remtene-scroll"
          tabIndex={0}
          aria-label="第三方文字服务配置步骤"
        >
          <section className="model-step" aria-labelledby="llm-step-one-title">
            <div className="model-step-heading">
              <span className="model-step-number" aria-hidden="true">
                1
              </span>
              <h3 id="llm-step-one-title">服务地址与模型</h3>
              <span className="model-step-status">{stepOneStatus}</span>
            </div>

            {showSettingsEditor ? (
              <fieldset
                className={`model-settings-grid${
                  displayState === "not-configured"
                    ? " model-settings-grid--initial"
                    : ""
                }`}
                disabled={busy || !settingsReady}
              >
                <label>
                  <span>服务地址（Base URL）</span>
                  <input
                    id="llm-base-url"
                    type="url"
                    inputMode="url"
                    autoComplete="off"
                    spellCheck={false}
                    placeholder="例如：https://api.example.com/v1"
                    value={state.form.baseUrl}
                    onChange={(event) =>
                      dispatch({
                        type: "form_changed",
                        field: "baseUrl",
                        value: event.target.value,
                      })
                    }
                  />
                </label>
                <label>
                  <span>模型名称</span>
                  <input
                    id="llm-model"
                    type="text"
                    autoComplete="off"
                    spellCheck={false}
                    placeholder="输入模型名称"
                    value={state.form.model}
                    onChange={(event) =>
                      dispatch({
                        type: "form_changed",
                        field: "model",
                        value: event.target.value,
                      })
                    }
                  />
                </label>
                {displayState !== "not-configured" ? (
                  <Button
                    variant="outline"
                    disabled={busy || !settingsReady}
                    onClick={() => void saveSettings()}
                  >
                    {state.operation === "save_settings" ? "正在保存…" : "保存"}
                  </Button>
                ) : null}
              </fieldset>
            ) : (
              <div className="model-settings-grid">
                <label>
                  <span>服务地址（Base URL）</span>
                  <span className="model-readonly-value">
                    {readonlyBaseUrl}
                  </span>
                </label>
                <label>
                  <span>模型名称</span>
                  <span className="model-readonly-value">
                    {readonlyModel}
                  </span>
                </label>
                <Button
                  variant="outline"
                  disabled={busy || !settingsReady}
                  onClick={() => setEditingSettings(true)}
                >
                  {displayState === "loading"
                    ? "正在读取…"
                    : displayState === "settings-unavailable"
                      ? "暂不可编辑"
                      : "编辑"}
                </Button>
              </div>
            )}
            <FeedbackMessage feedback={state.settingsFeedback} />
            {state.settingsLoad === "error" ? (
              <Button
                className="model-retry-button"
                variant="outline"
                disabled={busy}
                onClick={() => void loadSettings()}
              >
                重新加载
              </Button>
            ) : null}
          </section>

          <section className="model-step" aria-labelledby="llm-step-two-title">
            <div className="model-step-heading">
              <span className="model-step-number" aria-hidden="true">
                2
              </span>
              <h3 id="llm-step-two-title">API Key</h3>
              <span className="model-step-status">{stepTwoStatus}</span>
            </div>

            {displayState === "loading" ? (
              <div className="model-key-grid">
                <label>
                  <span>正在读取 API Key 状态</span>
                  <span className="model-readonly-value is-disabled">
                    正在读取…
                  </span>
                </label>
                <Button variant="outline" disabled>
                  正在读取…
                </Button>
              </div>
            ) : displayState === "settings-unavailable" ? (
              <div className="model-key-grid">
                <label>
                  <span>服务设置状态未知</span>
                  <span className="model-readonly-value is-disabled">
                    重新加载服务设置后再管理 API Key
                  </span>
                </label>
                <Button variant="outline" disabled>
                  暂不可操作
                </Button>
              </div>
            ) : displayState === "key-status-unavailable" ? (
              <div className="model-key-grid">
                <label>
                  <span>API Key 状态暂不可读</span>
                  <span className="model-readonly-value is-disabled">
                    请稍后重新检查
                  </span>
                </label>
                <Button variant="outline" disabled>
                  状态未知
                </Button>
              </div>
            ) : displayState === "not-configured" ? (
              <div className="model-key-grid">
                <label>
                  <span>API Key</span>
                  <input
                    id="llm-api-key"
                    name="remtene-llm-api-key"
                    type="password"
                    autoComplete="off"
                    autoCapitalize="none"
                    spellCheck={false}
                    disabled={busy || !settingsReady}
                    placeholder="输入期间仅当前设置窗口可访问"
                    value={state.secretDraft.value}
                    onChange={(event) =>
                      dispatch({
                        type: "secret_edited",
                        value: event.target.value,
                      })
                    }
                  />
                </label>
                <Button
                  variant="outline"
                  disabled={
                    busy ||
                    !settingsReady ||
                    state.form.baseUrl.trim().length === 0 ||
                    state.form.model.trim().length === 0 ||
                    state.secretDraft.value.trim().length === 0
                  }
                  onClick={() => void saveInitialConfiguration()}
                >
                  {state.operation === "save_settings" ? "正在保存…" : "保存"}
                </Button>
              </div>
            ) : displayState === "secret-unavailable" ? (
              keyNeedsRecovery ? (
                <div className="model-key-grid">
                  <label>
                    <span>原 API Key 无法读取</span>
                    <span className="model-readonly-value is-disabled">
                      清除失效记录后需要重新输入
                    </span>
                  </label>
                  <Button
                    variant="outline"
                    disabled={busy}
                    onClick={() =>
                      dispatch({
                        type: "confirmation_changed",
                        confirmation: "reset_secrets",
                      })
                    }
                  >
                    清除失效记录
                  </Button>
                </div>
              ) : (
                <div className="model-key-grid">
                  <label>
                    <span>暂时无法读取 API Key</span>
                    <span className="model-readonly-value is-disabled">
                      API Key 不会显示在界面中
                    </span>
                  </label>
                  <Button variant="outline" disabled>
                    暂不可操作
                  </Button>
                </div>
              )
            ) : displayState === "secret-recovery" ? (
              <div className="model-key-recovery-grid">
                <label>
                  <span>仅在当前窗口短暂可见</span>
                  <input
                    className="model-revealed-secret"
                    type="text"
                    readOnly
                    aria-label="已显示的 API Key"
                    value={
                      state.secretDraft.source === "revealed"
                        ? state.secretDraft.value
                        : "已显示 · 30 秒后自动隐藏"
                    }
                    onFocus={(event) => event.currentTarget.select()}
                  />
                </label>
                <Button
                  variant="outline"
                  disabled={busy}
                  onClick={clearSensitiveState}
                >
                  立即隐藏
                </Button>
                <Button
                  variant="outline"
                  disabled={busy}
                  onClick={requestDelete}
                >
                  删除
                </Button>
              </div>
            ) : showSecretEditor ? (
              <div className="model-key-grid">
                <label>
                  <span>输入新的 API Key</span>
                  <input
                    id="llm-api-key"
                    name="remtene-llm-api-key"
                    type={state.secretDraft.visible ? "text" : "password"}
                    autoComplete="off"
                    autoCapitalize="none"
                    spellCheck={false}
                    disabled={busy || !settingsConfigured}
                    placeholder="输入期间仅当前设置窗口可访问"
                    value={state.secretDraft.value}
                    onChange={(event) =>
                      dispatch({
                        type: "secret_edited",
                        value: event.target.value,
                      })
                    }
                  />
                </label>
                <Button
                  variant="outline"
                  disabled={
                    busy ||
                    !settingsConfigured ||
                    state.secretDraft.value.trim().length === 0
                  }
                  onClick={() => void saveKey()}
                >
                  {state.operation === "save_key"
                    ? "正在保存…"
                    : "保存"}
                </Button>
              </div>
            ) : (
              <div className="model-key-actions">
                <label>
                  <span>已保存的 API Key 默认隐藏</span>
                  <span className="model-readonly-value">
                    已安全保存
                  </span>
                </label>
                <Button
                  variant="outline"
                  disabled={busy}
                  onClick={() => void revealKey()}
                >
                  {state.operation === "reveal_key" ? "正在读取…" : "查看"}
                </Button>
                <Button
                  variant="outline"
                  disabled={busy}
                  onClick={requestDelete}
                >
                  删除
                </Button>
              </div>
            )}

            {state.keyStatusLoad === "error" ? (
              <Button
                className="model-retry-button"
                variant="outline"
                disabled={busy}
                onClick={() => void loadKeyStatus()}
              >
                重新检查
              </Button>
            ) : null}
            {state.confirmation ? (
              <Confirmation
                action={state.confirmation}
                busy={busy}
                onCancel={() =>
                  dispatch({
                    type: "confirmation_changed",
                    confirmation: null,
                  })
                }
                onConfirm={() =>
                  void (state.confirmation === "delete_key"
                    ? deleteKey()
                    : resetSecrets())
                }
              />
            ) : null}
            {state.keyFeedback &&
            (keyState === undefined ||
              state.keyFeedback.text !== apiKeyStateFeedback(keyState).text) ? (
              <FeedbackMessage feedback={state.keyFeedback} />
            ) : null}
          </section>

          <section className="model-step" aria-labelledby="llm-step-three-title">
            <div className="model-step-heading">
              <span className="model-step-number" aria-hidden="true">
                3
              </span>
              <h3 id="llm-step-three-title">测试连接</h3>
              <span className="model-step-status">{stepThreeStatus}</span>
            </div>
            <p className="model-step-hint">
              {connectionHint}
            </p>
            <div className="model-test-grid">
              <span className="model-readonly-value">
                {connectionSummary}
              </span>
              <Button
                variant="outline"
                disabled={
                  !connectionStateAvailable ||
                  !canTestConnection(state)
                }
                onClick={() => void testConnection()}
              >
                {state.operation === "test_connection"
                  ? "正在测试…"
                  : displayState === "connection-failed" ||
                      connectionPreviouslyTested ||
                      (approvedVisualPreview &&
                        displayState === "configured")
                    ? "重新测试"
                    : "测试连接"}
              </Button>
            </div>
            <FeedbackMessage feedback={state.connectionFeedback} />
            <UpstreamErrorMessage upstream={state.upstreamError} />
          </section>
        </div>
      </section>

      <footer
        className="model-footer"
        data-tone={
          displayState === "configured"
            ? "success"
            : displayState === "connection-failed" ||
                displayState === "secret-unavailable" ||
                displayState === "settings-unavailable" ||
                displayState === "key-status-unavailable"
              ? "error"
              : "warning"
        }
      >
        <span>第三方服务只接收文字整理所需的内容</span>
        <span>{footerRight}</span>
      </footer>
    </>
  );
}
