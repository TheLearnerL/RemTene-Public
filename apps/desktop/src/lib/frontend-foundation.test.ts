import assert from "node:assert/strict";
import test from "node:test";

import {
  domainHref,
  parseAppDomain,
} from "../app/navigation.ts";
import {
  deriveRecordingPageState,
  recordingLimitLabel,
} from "../features/recording/recording-model.ts";
import {
  deriveStatusPageState,
  effectiveProcessingMode,
  selectionDetail,
} from "../features/status/status-model.ts";
import { permissionRepairAction } from "../features/permissions/permission-actions.ts";
import {
  CONTRACT_VERSION,
  type AppSnapshot,
  type PermissionStatusView,
  type SettingsView,
} from "./ipc.ts";

function appSnapshot(
  overrides: Partial<AppSnapshot> = {},
): AppSnapshot {
  return {
    contract_version: CONTRACT_VERSION,
    lifecycle_state: "ready",
    active_session: null,
    microphone_permission: "granted",
    accessibility_permission: "granted",
    asr_readiness: "qwen_ready",
    llm_configured: false,
    model_summary: {
      selected_model: "qwen",
      active_model_id: "qwen-local",
      qwen_ready: true,
      whisper_ready: false,
    },
    shortcut_configured: true,
    autostart_enabled: false,
    ...overrides,
  };
}

function permissions(
  overrides: Partial<PermissionStatusView> = {},
): PermissionStatusView {
  return {
    contract_version: CONTRACT_VERSION,
    microphone: "granted",
    accessibility: "granted",
    app_display_name: "辑语",
    process_name: "remtene-desktop",
    ...overrides,
  };
}

function settings(
  overrides: Partial<SettingsView> = {},
): SettingsView {
  return {
    contract_version: CONTRACT_VERSION,
    version: 1,
    recording_mode: "toggle",
    max_recording_duration_seconds: 600,
    recording_shortcut: null,
    processing_mode: "faithful",
    read_selected_text: false,
    clipboard_bridge_allowed: false,
    auto_copy_result: false,
    local_diagnostics_enabled: true,
    history_policy: { enabled: true, limit: 10, retention_days: null },
    llm: null,
    ...overrides,
  };
}

void test("five-domain navigation rejects unknown hashes", () => {
  assert.equal(parseAppDomain(""), "status");
  assert.equal(parseAppDomain("#status"), "status");
  assert.equal(parseAppDomain("#/recording"), "recording");
  assert.equal(parseAppDomain("#model"), "model");
  assert.equal(parseAppDomain("#output"), "output");
  assert.equal(parseAppDomain("#system"), "system");
  assert.equal(parseAppDomain("#organize"), "status");
  assert.equal(domainHref("output"), "#output");
});

void test("recording page maps ready and discovering ASR to idle for either shortcut state", () => {
  assert.equal(
    deriveRecordingPageState(
      appSnapshot({ shortcut_configured: false }),
      permissions(),
      false,
      false,
    ),
    "idle",
  );
  assert.equal(
    deriveRecordingPageState(
      appSnapshot({
        asr_readiness: "discovering",
        shortcut_configured: false,
      }),
      permissions(),
      false,
      false,
    ),
    "idle",
  );
  assert.equal(
    deriveRecordingPageState(
      appSnapshot({ shortcut_configured: true }),
      permissions(),
      false,
      false,
    ),
    "idle",
  );
  assert.equal(
    deriveRecordingPageState(
      appSnapshot({
        asr_readiness: "discovering",
        shortcut_configured: true,
      }),
      permissions(),
      false,
      false,
    ),
    "idle",
  );
});

void test("recording duration keeps the approved default copy and formats real values", () => {
  assert.equal(recordingLimitLabel(600), "10 分钟");
  assert.equal(recordingLimitLabel(45), "45 秒");
  assert.equal(recordingLimitLabel(630), "10 分 30 秒");
});

void test("recording page exposes permission repair before model repair", () => {
  assert.equal(
    deriveRecordingPageState(
      appSnapshot({
        microphone_permission: "not_determined",
        accessibility_permission: "not_determined",
        asr_readiness: "unavailable",
        shortcut_configured: false,
      }),
      permissions({
        microphone: "not_determined",
        accessibility: "not_determined",
      }),
      false,
      false,
    ),
    "permission-required",
  );
  assert.equal(
    deriveRecordingPageState(
      appSnapshot({
        microphone_permission: "denied",
        accessibility_permission: "not_determined",
      }),
      permissions({
        microphone: "denied",
        accessibility: "not_determined",
      }),
      false,
      false,
    ),
    "permission-required",
  );
  assert.equal(
    deriveRecordingPageState(
      appSnapshot({
        microphone_permission: "unknown",
        accessibility_permission: "not_required",
      }),
      permissions({
        microphone: "unknown",
        accessibility: "not_required",
      }),
      false,
      false,
    ),
    "permission-required",
  );
});

void test("recording page exposes unavailable ASR after microphone permission is granted", () => {
  assert.equal(
    deriveRecordingPageState(
      appSnapshot({
        asr_readiness: "unavailable",
      }),
      permissions(),
      false,
      false,
    ),
    "asr-unavailable",
  );
});

void test("recording page preserves active-session controls before readiness checks", () => {
  assert.equal(
    deriveRecordingPageState(
      appSnapshot({
        lifecycle_state: "quitting",
        asr_readiness: "unavailable",
        active_session: {
          contract_version: CONTRACT_VERSION,
          session_id: "00000000-0000-4000-8000-000000000001",
          user_state: "preparing",
          phase: "preparing",
          recording_elapsed_ms: null,
          recording_limit_ms: 600_000,
          can_finish: false,
          can_cancel: true,
          status_code: "session.preparing",
        },
      }),
      permissions(),
      false,
      true,
    ),
    "active",
  );
});

void test("recording page distinguishes real loading from explicit failures", () => {
  assert.equal(
    deriveRecordingPageState(null, null, true, false),
    "loading",
  );
  assert.equal(
    deriveRecordingPageState(
      appSnapshot({ lifecycle_state: "starting" }),
      permissions(),
      false,
      false,
    ),
    "loading",
  );
  assert.equal(
    deriveRecordingPageState(
      appSnapshot({ lifecycle_state: "quitting" }),
      permissions(),
      false,
      false,
    ),
    "error",
  );
  assert.equal(
    deriveRecordingPageState(appSnapshot(), permissions(), false, true),
    "error",
  );
  assert.equal(
    deriveRecordingPageState(
      appSnapshot({ microphone_permission: "granted" }),
      permissions({ microphone: "denied" }),
      false,
      false,
    ),
    "error",
  );
});

void test("status page derives the Penpot loading and error states", () => {
  assert.equal(
    deriveStatusPageState(
      appSnapshot(),
      permissions(),
      settings(),
      true,
      false,
    ),
    "loading",
  );
  assert.equal(
    deriveStatusPageState(null, null, null, false, true),
    "error",
  );
  assert.equal(
    deriveStatusPageState(
      appSnapshot(),
      permissions(),
      null,
      false,
      true,
    ),
    "error",
  );
  assert.equal(
    deriveStatusPageState(
      appSnapshot({ lifecycle_state: "quitting" }),
      permissions(),
      settings(),
      false,
      false,
    ),
    "error",
  );
});

void test("status page derives the Penpot ready and partial states", () => {
  assert.equal(
    deriveStatusPageState(
      appSnapshot({ llm_configured: true }),
      permissions(),
      settings({
        llm: {
          base_url: "https://llm.example.test/v1",
          model: "text-model",
        },
      }),
      false,
      false,
    ),
    "ready",
  );
  assert.equal(
    deriveStatusPageState(
      appSnapshot({ shortcut_configured: false }),
      permissions(),
      settings(),
      false,
      false,
    ),
    "partial",
  );
});

void test("status page does not infer first use from microphone permission", () => {
  assert.equal(
    deriveStatusPageState(
      appSnapshot(),
      permissions({ microphone: "denied" }),
      settings(),
      false,
      false,
    ),
    "error",
  );
});

void test("status page uses the approved first-setup state for an unrequested microphone", () => {
  assert.equal(
    deriveStatusPageState(
      appSnapshot({ microphone_permission: "not_determined" }),
      permissions({ microphone: "not_determined" }),
      settings(),
      false,
      false,
    ),
    "empty",
  );
});

void test("permission repair actions follow verified OS state", () => {
  assert.equal(permissionRepairAction("not_determined"), "request");
  assert.equal(permissionRepairAction("denied"), "open-settings");
  assert.equal(permissionRepairAction("granted"), null);
  assert.equal(permissionRepairAction("not_required"), null);
  assert.equal(permissionRepairAction("inherited_from_launcher"), null);
  assert.equal(permissionRepairAction("unknown"), null);
});

void test("status page does not present first-use copy for ASR discovery or failure", () => {
  assert.equal(
    deriveStatusPageState(
      appSnapshot({ asr_readiness: "discovering" }),
      permissions({ microphone: "denied" }),
      settings(),
      false,
      false,
    ),
    "error",
  );
  assert.equal(
    deriveStatusPageState(
      appSnapshot({ asr_readiness: "unavailable" }),
      permissions(),
      settings(),
      false,
      false,
    ),
    "error",
  );
  assert.equal(
    deriveStatusPageState(
      appSnapshot({
        asr_readiness: "unavailable",
        microphone_permission: "not_determined",
      }),
      permissions({ microphone: "not_determined" }),
      settings(),
      false,
      false,
    ),
    "error",
  );
});

void test("status page treats an optional text-service gap as partial readiness", () => {
  assert.equal(
    deriveStatusPageState(
      appSnapshot({ llm_configured: false }),
      permissions(),
      settings(),
      false,
      false,
    ),
    "partial",
  );
  assert.equal(
    deriveStatusPageState(
      appSnapshot({ shortcut_configured: true }),
      permissions({ accessibility: "denied" }),
      settings(),
      false,
      false,
    ),
    "error",
  );
  assert.equal(
    deriveStatusPageState(
      appSnapshot({
        lifecycle_state: "quitting",
        llm_configured: true,
      }),
      permissions(),
      settings({
        llm: {
          base_url: "https://llm.example.test/v1",
          model: "text-model",
        },
      }),
      false,
      false,
    ),
    "error",
  );
});

void test("status page derives the Penpot busy state before readiness checks", () => {
  const state = deriveStatusPageState(
    appSnapshot({
      asr_readiness: "unavailable",
      llm_configured: true,
      active_session: {
        contract_version: CONTRACT_VERSION,
        session_id: "00000000-0000-4000-8000-000000000001",
        user_state: "processing",
        phase: "processing",
        recording_elapsed_ms: null,
        recording_limit_ms: null,
        can_finish: false,
        can_cancel: false,
        status_code: "session.processing",
      },
    }),
    permissions(),
    settings(),
    false,
    false,
  );
  assert.equal(state, "busy");
});

void test("active sessions preserve the underlying page instead of replacing it with loading", () => {
  const recording = {
    contract_version: CONTRACT_VERSION,
    session_id: "00000000-0000-4000-8000-000000000001",
    user_state: "recording" as const,
    phase: "recording" as const,
    recording_elapsed_ms: 1_000,
    recording_limit_ms: 60_000,
    can_finish: true,
    can_cancel: true,
    status_code: "session.recording",
  };
  assert.equal(
    deriveStatusPageState(
      appSnapshot({
        llm_configured: true,
        active_session: recording,
      }),
      permissions(),
      settings(),
      false,
      false,
    ),
    "partial",
  );
  assert.equal(
    deriveStatusPageState(
      appSnapshot({
        llm_configured: true,
        active_session: recording,
      }),
      permissions(),
      null,
      false,
      true,
    ),
    "error",
  );
  assert.equal(
    deriveStatusPageState(
      appSnapshot({
        llm_configured: true,
        active_session: {
          ...recording,
          user_state: "processing",
          phase: "processing",
        },
      }),
      permissions(),
      settings({ processing_mode: "structured" }),
      false,
      false,
    ),
    "partial",
  );
});

void test("effective processing mode preserves the saved selection without text service", () => {
  assert.equal(
    effectiveProcessingMode(
      appSnapshot({ llm_configured: false }),
      settings({ processing_mode: "structured" }),
    ),
    "structured",
  );
  assert.equal(effectiveProcessingMode(null, settings()), "faithful");
  assert.equal(effectiveProcessingMode(appSnapshot(), null), "raw");
});

void test("effective processing mode preserves the stored AI mode when configured", () => {
  assert.equal(
    effectiveProcessingMode(
      appSnapshot({ llm_configured: true }),
      settings({ processing_mode: "structured" }),
    ),
    "structured",
  );
});

void test("selection detail uses concise product copy", () => {
  assert.equal(
    selectionDetail("raw", true),
    "原始转录不会读取选中的文字，也不会使用第三方服务。",
  );
  assert.equal(
    selectionDetail("faithful", false),
    "开启后，选中的文字只用于本次整理。",
  );
  assert.equal(
    selectionDetail("structured", false),
    "开启后，选中的文字只用于本次整理。",
  );
  assert.equal(
    selectionDetail("faithful", true),
    "只读取当前选中的文字，用完即清除。",
  );
});
