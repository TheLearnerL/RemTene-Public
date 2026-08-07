import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import {
  recordingClockLabel,
  recordingElapsedFromAnchor,
} from "../app/recording-hud-clock.ts";
import {
  RECORDING_HUD_EXIT_DURATION_MS,
  recordingHudExitDelay,
} from "../app/recording-hud-motion.ts";

function readSource(relativePath: string): string {
  return readFileSync(new URL(relativePath, import.meta.url), "utf8");
}

void test("recording HUD advances display time from the latest backend anchor", () => {
  assert.equal(recordingElapsedFromAnchor(12_000, 1_000, 2_500, null), 13_500);
  assert.equal(recordingElapsedFromAnchor(20_000, 5_000, 5_500, null), 20_500);
});

void test("recording HUD never runs before its anchor or beyond the real limit", () => {
  assert.equal(recordingElapsedFromAnchor(12_000, 2_000, 1_000, null), 12_000);
  assert.equal(recordingElapsedFromAnchor(12_000, 1_000, 5_000, 14_000), 14_000);
});

void test("recording HUD formats real elapsed values without inventing a phase", () => {
  assert.equal(recordingClockLabel(0), "00:00");
  assert.equal(recordingClockLabel(12_999), "00:12");
  assert.equal(recordingClockLabel(600_000), "10:00");
});

void test("recording HUD fails closed for unavailable and empty IPC state", () => {
  const source = readSource("../app/RecordingHud.tsx");

  assert.match(
    source,
    /if \(hud\.status === "unavailable"\) \{\s*return <HudUnavailable \/>;/,
  );
  assert.match(
    source,
    /if \(snapshot === null\) \{\s*return <main className="hud-shell hud-shell--empty" aria-hidden="true" \/>;/,
  );
  assert.match(
    source,
    /key=\{`\$\{snapshot\.session_id\}:\$\{snapshot\.recording_elapsed_ms\}:\$\{snapshot\.recording_limit_ms\}`\}/,
  );
  assert.match(source, /data-action-error=\{failedAction \?\? undefined\}/);
  assert.match(source, /id="hud-action-error"[\s\S]*role="alert"/);
});

void test("recording HUD delays real actions until its exit feedback completes", () => {
  const source = readSource("../app/RecordingHud.tsx");

  assert.equal(RECORDING_HUD_EXIT_DURATION_MS, 210);
  assert.equal(recordingHudExitDelay(false), 210);
  assert.equal(recordingHudExitDelay(true), 0);
  assert.match(
    source,
    /setPendingAction\(action\);[\s\S]*await waitForRecordingHudExit\(reducedMotion\);[\s\S]*gateway\.(?:finishRecording|cancelRecording)/,
  );
  assert.match(source, /if \(!snapshot \|\| actionInFlight\.current\) return;/);
  assert.match(source, /data-visual-state=\{visualState\}/);
  assert.match(source, /data-action-state=/);
});

void test("completed HUD holds its result, exits, and only then hides natively", () => {
  const source = readSource("../app/RecordingHud.tsx");
  const nativeHud = readSource("../../src-tauri/src/recording_hud.rs");

  assert.match(
    source,
    /listenToSessionEnded\(\(ended\) => \{[\s\S]*setClosingSessionId\(ended\.session_id\)/,
  );
  assert.match(
    source,
    /pendingAction !== null \|\| closingSessionId === snapshot\.session_id/,
  );
  assert.match(nativeHud, /HUD_COMPLETED_HOLD_DURATION[\s\S]*from_millis\(240\)/);
  assert.match(nativeHud, /HUD_NATIVE_HIDE_DELAY[\s\S]*from_millis\(240\)/);
  assert.match(
    nativeHud,
    /emit_session_ended_to\(session_id, RECORDING_HUD_LABEL\)[\s\S]*wait_for_hud_motion\(HUD_NATIVE_HIDE_DELAY\)/,
  );
  assert.match(nativeHud, /visibility_generation/);
});

void test("recording HUD is clipped natively and transparent from its first frame", () => {
  const main = readSource("../main.tsx");
  const nativeHud = readSource("../../src-tauri/src/recording_hud.rs");
  const macPanel = readSource(
    "../../src-tauri/src/recording_hud/macos_panel.rs",
  );
  const source = readSource("../app/RecordingHud.tsx");
  const styles = readSource("../styles/surfaces.css");

  assert.match(
    main,
    /document\.documentElement\.dataset\.surface = surface;[\s\S]*document\.body\.dataset\.surface = surface;/,
  );
  assert.match(nativeHud, /HUD_CORNER_RADIUS_LOGICAL/);
  assert.match(nativeHud, /HUD_WIDTH_LOGICAL: f64 = 144\.0/);
  assert.match(nativeHud, /HUD_HEIGHT_LOGICAL: f64 = 40\.0/);
  assert.match(nativeHud, /\.decorations\(false\)[\s\S]*\.transparent\(true\)/);
  assert.match(macPanel, /panel\.set_transparent\(true\)/);
  assert.match(macPanel, /panel\.set_corner_radius\(corner_radius\)/);
  assert.match(macPanel, /setMasksToBounds: true/);
  assert.match(styles, /@keyframes hud-surface-enter/);
  assert.match(styles, /@keyframes hud-surface-exit/);
  assert.match(styles, /width: min\(144px, 100%\)/);
  assert.match(styles, /grid-template-columns: 16px minmax\(0, 1fr\) auto/);
  assert.match(styles, /@keyframes hud-content-switch/);
  assert.match(styles, /@keyframes hud-recording-pulse/);
  assert.match(styles, /@keyframes hud-processing-spin/);
  assert.match(styles, /@keyframes hud-delivering-travel/);
  assert.match(styles, /@keyframes hud-completed-pop/);
  assert.match(styles, /\.hud-button:not\(:disabled\):active/);
  assert.match(source, /data-phase=\{snapshot\.phase\}/);
  assert.match(source, /className="hud-actions-slot"/);
});

void test("temporary text copies through its gateway without faking success", () => {
  const source = readSource("../app/TemporaryTextBox.tsx");

  assert.match(
    source,
    /disabled=\{delivery === null \|\| copying\}/,
  );
  assert.match(
    source,
    /await gateway\.copyTemporaryText\(deliveryId\);\s*setCopiedDeliveryId\(deliveryId\);/,
  );
  assert.match(source, /const \[copyingDeliveryId, setCopyingDeliveryId\]/);
  assert.match(source, /current === deliveryId \? null : current/);
  assert.doesNotMatch(source, /navigator\.clipboard/);
  assert.doesNotMatch(source, /previewCopyEnabled|复制暂不可用/);
});
