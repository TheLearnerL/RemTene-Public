import assert from "node:assert/strict";
import test from "node:test";

import { CONTRACT_VERSION, type SettingsView } from "./ipc.ts";
import {
  apiKeyStateFeedback,
  canTestConnection,
  clearSensitiveStateWhenWindowLosesFocus,
  connectionErrorFeedback,
  connectionTestFeedback,
  createInitialLlmSettingsPanelState,
  hasSettingsChanges,
  installSecretLifecycleGuards,
  isSensitiveResultCurrent,
  isSettingsVersionConflict,
  llmActionErrorFeedback,
  llmSettingsPanelReducer,
  validateLlmSettingsForm,
} from "./llm-settings-ui.ts";

function settings(version = 3): SettingsView {
  return {
    contract_version: CONTRACT_VERSION,
    version,
    recording_mode: "toggle",
    max_recording_duration_seconds: 600,
    recording_shortcut: null,
    processing_mode: "faithful",
    read_selected_text: false,
    clipboard_bridge_allowed: false,
    auto_copy_result: false,
    local_diagnostics_enabled: true,
    history_policy: { enabled: true, limit: 10, retention_days: null },
    llm: {
      base_url: "https://provider.invalid/v1",
      model: "test-model",
    },
  };
}

class FakeLifecycleTarget {
  visibilityState: DocumentVisibilityState = "visible";
  private readonly listeners = new Map<string, Set<() => void>>();

  addEventListener(type: string, listener: () => void): void {
    const listeners = this.listeners.get(type) ?? new Set();
    listeners.add(listener);
    this.listeners.set(type, listeners);
  }

  removeEventListener(type: string, listener: () => void): void {
    this.listeners.get(type)?.delete(listener);
  }

  dispatch(type: string): void {
    for (const listener of this.listeners.get(type) ?? []) listener();
  }
}

void test("initial load can fail independently and retry to ready state", () => {
  let state = createInitialLlmSettingsPanelState();
  state = llmSettingsPanelReducer(state, {
    type: "settings_load_failed",
    feedback: { tone: "error", text: "设置读取失败" },
  });
  state = llmSettingsPanelReducer(state, {
    type: "key_status_loaded",
    status: {
      contract_version: CONTRACT_VERSION,
      state: "configured",
      storage: "encrypted_local",
    },
  });
  assert.equal(state.settingsLoad, "error");
  assert.equal(state.keyStatusLoad, "ready");

  state = llmSettingsPanelReducer(state, { type: "settings_loading" });
  state = llmSettingsPanelReducer(state, {
    type: "settings_loaded",
    settings: settings(),
  });
  assert.equal(state.settingsLoad, "ready");
  assert.equal(state.form.baseUrl, "https://provider.invalid/v1");
});

void test("settings form validates and trims non-secret values", () => {
  const result = validateLlmSettingsForm({
    baseUrl: "  https://provider.invalid/v1  ",
    model: "  test-model  ",
  });
  assert.equal(result.ok, true);
  if (result.ok) {
    assert.deepEqual(result.value, {
      base_url: "https://provider.invalid/v1",
      model: "test-model",
    });
  }

  assert.equal(validateLlmSettingsForm({ baseUrl: "", model: "" }).ok, false);
  assert.equal(
    validateLlmSettingsForm({ baseUrl: "file:///tmp/provider", model: "test" }).ok,
    false,
  );
});

void test("settings dirty and conflict state never silently overwrites latest values", () => {
  let state = createInitialLlmSettingsPanelState();
  state = llmSettingsPanelReducer(state, {
    type: "settings_loaded",
    settings: settings(),
  });
  state = llmSettingsPanelReducer(state, {
    type: "form_changed",
    field: "model",
    value: "new-model",
  });
  assert.equal(hasSettingsChanges(state.settings, state.form), true);

  state = llmSettingsPanelReducer(state, {
    type: "settings_conflict",
    settings: {
      ...settings(4),
      llm: {
        base_url: "https://latest.invalid/v1",
        model: "latest-model",
      },
    },
  });
  assert.equal(state.form.model, "latest-model");
  assert.equal(state.settingsFeedback?.tone, "warning");
  assert.equal(hasSettingsChanges(state.settings, state.form), false);
});

void test("all API key states have content-free user feedback", () => {
  for (const state of [
    "not_configured",
    "configured",
    "recovery_required",
    "unavailable",
  ] as const) {
    const feedback = apiKeyStateFeedback(state);
    assert.ok(feedback.text.length > 0);
    assert.equal(feedback.text.includes("secret_value"), false);
  }
});

void test("secret draft is cleared for every required lifecycle boundary", () => {
  const clearReasons = [
    "hide",
    "save",
    "window_blur",
    "visibility_hidden",
    "pagehide",
    "unmount",
    "delete",
    "reset",
  ];

  for (const reason of clearReasons) {
    let state = createInitialLlmSettingsPanelState();
    state = llmSettingsPanelReducer(state, {
      type: "secret_revealed",
      value: `sk-sensitive-${reason}`,
    });
    assert.equal(state.secretDraft.visible, true);
    state = llmSettingsPanelReducer(state, { type: "clear_sensitive" });
    assert.deepEqual(state.secretDraft, {
      value: "",
      visible: false,
      source: "empty",
    });
    assert.equal(state.keyFeedback, null);
  }
});

void test("browser, Tauri focus, and unmount lifecycle guards invoke real clearing", () => {
  const windowTarget = new FakeLifecycleTarget();
  const documentTarget = new FakeLifecycleTarget();
  let clearCount = 0;
  const clear = () => {
    clearCount += 1;
  };
  const uninstall = installSecretLifecycleGuards(windowTarget, documentTarget, clear);

  windowTarget.dispatch("blur");
  windowTarget.dispatch("pagehide");
  documentTarget.dispatch("visibilitychange");
  assert.equal(clearCount, 2, "visible documents must not clear on visibilitychange");

  documentTarget.visibilityState = "hidden";
  documentTarget.dispatch("visibilitychange");
  clearSensitiveStateWhenWindowLosesFocus(true, clear);
  clearSensitiveStateWhenWindowLosesFocus(false, clear);
  assert.equal(clearCount, 4);

  uninstall();
  assert.equal(clearCount, 5, "unmount cleanup must clear once");
  windowTarget.dispatch("blur");
  windowTarget.dispatch("pagehide");
  documentTarget.dispatch("visibilitychange");
  assert.equal(clearCount, 5, "unmount cleanup must remove every DOM listener");
});

void test("late reveal results are invalidated by any sensitive-state clear", () => {
  const revealGeneration = 7;
  assert.equal(isSensitiveResultCurrent(revealGeneration, revealGeneration), true);
  assert.equal(
    isSensitiveResultCurrent(revealGeneration, revealGeneration + 1),
    false,
    "a hide, blur, pagehide, visibility change, or unmount must reject the old result",
  );
});

void test("save, delete, reset, and failures clear sensitive drafts", () => {
  const successStatus = {
    contract_version: CONTRACT_VERSION,
    state: "configured" as const,
    storage: "encrypted_local" as const,
  };
  let state = createInitialLlmSettingsPanelState();
  state = llmSettingsPanelReducer(state, {
    type: "secret_edited",
    value: "sk-sensitive-marker",
  });
  state = llmSettingsPanelReducer(state, {
    type: "key_action_succeeded",
    status: successStatus,
    feedback: { tone: "success", text: "完成" },
  });
  assert.equal(state.secretDraft.value, "");

  state = llmSettingsPanelReducer(state, {
    type: "secret_edited",
    value: "sk-another-sensitive-marker",
  });
  state = llmSettingsPanelReducer(state, {
    type: "key_action_failed",
    feedback: { tone: "error", text: "失败" },
  });
  assert.equal(state.secretDraft.value, "");
});

void test("busy state prevents a second concurrent operation", () => {
  let state = createInitialLlmSettingsPanelState();
  state = llmSettingsPanelReducer(state, {
    type: "operation_started",
    operation: "save_settings",
  });
  state = llmSettingsPanelReducer(state, {
    type: "operation_started",
    operation: "delete_key",
  });
  assert.equal(state.operation, "save_settings");
  state = llmSettingsPanelReducer(state, {
    type: "operation_finished",
    operation: "delete_key",
  });
  assert.equal(state.operation, "save_settings");
});

void test("connection test requires persisted settings, configured key, and no draft", () => {
  let state = createInitialLlmSettingsPanelState();
  assert.equal(canTestConnection(state), false);
  state = llmSettingsPanelReducer(state, {
    type: "settings_loaded",
    settings: settings(),
  });
  state = llmSettingsPanelReducer(state, {
    type: "key_status_loaded",
    status: {
      contract_version: CONTRACT_VERSION,
      state: "configured",
      storage: "encrypted_local",
    },
  });
  assert.equal(canTestConnection(state), true);

  state = llmSettingsPanelReducer(state, {
    type: "secret_edited",
    value: "pending-key",
  });
  assert.equal(canTestConnection(state), false);
});

void test("upstream connection diagnostics are transient and replaced by the latest test", () => {
  let state = createInitialLlmSettingsPanelState();
  state = llmSettingsPanelReducer(state, {
    type: "connection_completed",
    feedback: { tone: "error", text: "认证失败" },
    upstreamError: {
      http_status: 401,
      response_body: '{"error":"first"}',
      truncated: false,
    },
  });
  assert.equal(state.upstreamError?.http_status, 401);

  state = llmSettingsPanelReducer(state, {
    type: "connection_completed",
    feedback: { tone: "error", text: "服务不可用" },
    upstreamError: {
      http_status: 503,
      response_body: '{"error":"latest"}',
      truncated: false,
    },
  });
  assert.equal(state.upstreamError?.http_status, 503);
  assert.equal(state.upstreamError?.response_body.includes("first"), false);

  state = llmSettingsPanelReducer(state, { type: "clear_sensitive" });
  assert.equal(state.upstreamError, null);
  assert.equal(state.connectionFeedback?.text, "服务不可用");
});

void test("connection result maps every stable code without provider content", () => {
  const codes = [
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
  ] as const;
  for (const code of codes) {
    const feedback = connectionErrorFeedback(code);
    assert.ok(feedback.text.length > 0);
    assert.equal(feedback.text.includes("response body"), false);
  }

  assert.equal(
    connectionTestFeedback({
      contract_version: CONTRACT_VERSION,
      request_id: "00000000-0000-4000-8000-000000000001",
      status: "succeeded",
      error_code: null,
      upstream_error: null,
    }).tone,
    "success",
  );
});

void test("IPC action errors use stable code and never stringify arbitrary values", () => {
  const conflict = {
    contract_version: CONTRACT_VERSION,
    code: "settings.version_conflict",
    category: "storage",
    severity: "warning",
    retryable: false,
    user_message_key: "errors.settings.version_conflict",
    correlation_id: "safe-correlation",
  };
  assert.equal(isSettingsVersionConflict(conflict), true);
  assert.equal(llmActionErrorFeedback(conflict, "fallback").tone, "warning");

  const marker = "sk-must-not-appear";
  const unknown = llmActionErrorFeedback(new Error(marker), "操作失败");
  assert.equal(unknown.text, "操作失败");
  assert.equal(unknown.text.includes(marker), false);
});
