import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

function readSource(relativePath: string): string {
  return readFileSync(new URL(relativePath, import.meta.url), "utf8");
}

void test("output production reads history and retries through the gateway", () => {
  const source = readSource("../pages/OutputPage.tsx");

  assert.match(source, /useState<OutputState>\("loading"\)/);
  assert.match(source, /gateway\s*\.listHistory\(\)/);
  assert.match(source, /Promise\.allSettled\(\[[\s\S]*gateway\.listHistory\(\)[\s\S]*gateway\.getSettings\(\)/);
  assert.match(source, /if \(recordCount > 0\) return "populated"/);
  assert.match(source, /onRetry=\{retry\}/);
  assert.match(source, /const historyRequestGeneration = useRef\(0\)/);
  assert.match(
    source,
    /if \(historyRequestGeneration\.current !== generation\) return;/,
  );
});

void test("output production keeps one copy outcome and persists history policy", () => {
  const source = readSource("../pages/OutputPage.tsx");

  assert.match(source, /historyPolicy \? \([\s\S]*<HistoryPolicy/);
  assert.match(
    source,
    /detail="当前设置暂时无法读取"[\s\S]*<OutputSwitch checked=\{false\} disabled \/>/,
  );
  assert.match(source, /disabled=\{!interactive \|\| saving\}/);
  assert.match(source, /gateway\.setHistoryEnabled\(settings\.version, enabled\)/);
  assert.match(
    source,
    /gateway\.setHistoryRetention\([\s\S]*settings\.version,[\s\S]*nextRetentionDays,[\s\S]*acknowledgeDataLoss/,
  );
  assert.match(source, /setState\("retention-confirm"\)/);
  assert.match(source, /estimatedExpiredCount/);
  assert.match(source, /gateway\.copyHistoryRecord\(recordId\)/);
  assert.match(source, /const canCopy = actionsEnabled/);
  assert.match(source, /const showCopyOutcome = copied \|\| failed/);
  assert.match(source, /copyOutcome === null &&[\s\S]*state === "copy-success"/);
  assert.doesNotMatch(source, /navigator\.clipboard|invoke\(/);
});

void test("history policy editors apply without save buttons and keep destructive confirmation", () => {
  const source = readSource("../pages/OutputPage.tsx");
  const limitEditor = source.slice(
    source.indexOf("function HistoryLimitEditor"),
    source.indexOf("function HistoryRetentionEditor"),
  );
  const retentionEditor = source.slice(
    source.indexOf("function HistoryRetentionEditor"),
    source.indexOf("function HistoryPolicy"),
  );

  assert.match(source, /HISTORY_RETENTION_OPTIONS = \[3, 10, 20, 30\] as const/);
  assert.match(retentionEditor, /<select/);
  assert.match(retentionEditor, /aria-label="历史保存期限（天）"/);
  assert.match(limitEditor, /onBlur=\{onApply\}/);
  assert.match(
    limitEditor,
    /if \(event\.key !== "Enter"\) return;[\s\S]*event\.currentTarget\.blur\(\)/,
  );
  assert.match(
    source,
    /setRetentionInput\(value\);[\s\S]*requestHistoryRetentionChange\(value\)/,
  );
  assert.doesNotMatch(limitEditor, /<button/);
  assert.doesNotMatch(retentionEditor, /<button/);
  assert.match(source, /className="output-rows" aria-busy=\{saving\}/);
  assert.match(source, /setState\("limit-confirm"\)/);
  assert.match(source, /setState\("retention-confirm"\)/);
});

void test("system auto-copy and local logs use real independent gateway actions", () => {
  const source = readSource("../pages/SystemPage.tsx");

  assert.match(
    source,
    /gateway\.setAutoCopyResult\([\s\S]*data\.settings\.version,[\s\S]*checked/,
  );
  assert.match(
    source,
    /gateway\.setLocalDiagnosticsEnabled\([\s\S]*data\.settings\.version,[\s\S]*checked/,
  );
  assert.match(source, /gateway\.openDiagnosticsDirectory\(\)/);
  assert.match(source, /checked=\{copyEnabled\}/);
  assert.match(source, /checked=\{diagnosticsEnabled\}/);
  assert.match(source, /默认保留最近 3 天/);
  assert.doesNotMatch(source, /自动复制当前未接入|桌面.*诊断\.log/);
});

void test("five-domain pages share one header, card, trailing-rail, and bottom baseline", () => {
  const entry = readSource("../main.tsx");
  const styles = readSource("../styles/page-layout.css");
  const modelStyles = readSource("../styles/model-page.css");

  assert.match(entry, /import "\.\/styles\/page-layout\.css";/);
  for (const className of [
    "status-spec-header",
    "recording-spec-header",
    "model-header",
    "output-header",
    "system-spec-header",
  ]) {
    assert.equal(styles.includes(className), true, `missing ${className}`);
  }
  assert.match(styles, /min-height: 100px;/);
  assert.match(styles, /border: 1px solid var\(--remtene-border\);/);
  assert.match(styles, /width: 112px;/);
  assert.match(styles, /--compact-row-rail: 72px;/);
  assert.match(styles, /--compact-switch-rail: 44px;/);
  assert.match(styles, /recording-row\[data-has-trailing="true"\]/);
  assert.match(
    styles,
    /output-row:not\(\[data-end-kind="action"\]\)/,
  );
  assert.match(
    styles,
    /\.output-row\[data-end-kind="action"\][\s\S]*grid-template-columns:[^;]*128px;/,
  );
  for (const compactColumns of [
    "status-spec-columns",
    "recording-columns",
    "model-asr-columns",
    "output-columns",
    "system-spec-card--compact",
  ]) {
    assert.equal(
      styles.includes(compactColumns),
      true,
      `missing compact rail for ${compactColumns}`,
    );
  }
  assert.match(styles, /padding-bottom: 48px;/);
  assert.match(
    modelStyles,
    /\.model-directory-trailing \{[\s\S]*gap: 4px;[\s\S]*white-space: nowrap;/,
  );
  assert.match(
    modelStyles,
    /\.model-directory-link \{[\s\S]*white-space: nowrap;/,
  );
});

void test("recording shortcut uses a focusable field and a window-level WebKit fallback", () => {
  const source = readSource("../pages/RecordingPage.tsx");

  assert.match(source, /<input[\s\S]*readOnly[\s\S]*aria-label="录入新的全局快捷键"/);
  assert.match(source, /window\.addEventListener\("keydown", captureKeyDown, true\)/);
  assert.match(source, /window\.addEventListener\("keyup", captureKeyUp, true\)/);
  assert.match(source, /pendingModifierRef\.current !== event\.code/);
  assert.match(source, /if \(event\.key === "Escape"\)[\s\S]*setDraft\(current\)/);
  assert.match(source, /window\.requestAnimationFrame\(\(\) => fieldRef\.current\?\.focus\(\)\)/);
  assert.match(
    source,
    /const saveRecordingShortcut = async[\s\S]*await recordingSettings\.updateShortcut\(binding\);[\s\S]*await dashboard\.refresh\(\);/,
  );
});

void test("system operation failures promote the status surface to an alert", () => {
  const source = readSource("../pages/SystemPage.tsx");

  assert.match(
    source,
    /setOperationTitle\("兼容粘贴设置未更新"\);[\s\S]*if \(operationMessage\) \{\s*statusTitle = operationTitle \?\? "系统设置未更新";\s*statusDetail = operationMessage;\s*statusTone = "error";\s*\}/,
  );
  assert.match(
    source,
    /role=\{tone === "error" \? "alert" : "status"\}/,
  );
});

void test("system permission rows expose only user-triggered repair actions", () => {
  const source = readSource("../pages/SystemPage.tsx");

  assert.match(source, /const runPermissionRepair = async/);
  assert.match(source, /gateway\.requestMicrophonePermission\(\)/);
  assert.match(source, /gateway\.requestAccessibilityPermission\(\)/);
  assert.match(source, /gateway\.openMicrophoneSettings\(\)/);
  assert.match(source, /gateway\.openAccessibilitySettings\(\)/);
  assert.match(source, /onClick=\{\(\) => void runPermissionRepair\(kind\)\}/);
  assert.match(source, /action === "request" \? "请求授权" : "打开设置"/);
  assert.match(source, /`请求\$\{permissionLabel\}授权`/);
  assert.match(source, /`打开\$\{permissionLabel\}设置`/);
});

void test("system permissions refresh when the native Tauri window regains focus", () => {
  const source = readSource("../pages/SystemPage.tsx");
  const dashboardSource = readSource("../features/status/useStatusDashboard.ts");

  assert.match(source, /getCurrentWindow\(\)\s*\.onFocusChanged/);
  assert.match(source, /if \(active && focused\) reload\(\)/);
  assert.match(source, /stopTauriFocusListener\?\.\(\)/);
  assert.match(source, /visibilitychange/);
  assert.match(dashboardSource, /getCurrentWindow\(\)\.onFocusChanged/);
  assert.match(dashboardSource, /if \(focused\) safeRefresh\(\)/);
  assert.match(dashboardSource, /visibilitychange/);
});

void test("status processing modes persist in place without model navigation", () => {
  const source = readSource("../pages/StatusPage.tsx");
  const selectMode = source.match(
    /const selectMode = \(mode: ProcessingMode\) => \{[\s\S]*?\n {2}\};/,
  )?.[0];

  assert.ok(selectMode);
  assert.match(selectMode, /textSettings\.update\(mode, settings\.read_selected_text\)/);
  assert.doesNotMatch(selectMode, /onNavigate|llm_configured/);
});

void test("system cards expose one custom internal scrollbar", () => {
  const source = readSource("../pages/SystemPage.tsx");
  const styles = readSource("../styles/system-page.css");

  assert.match(source, /className="system-spec-list"/);
  assert.doesNotMatch(source, /className="system-spec-list remtene-scroll"/);
  assert.match(source, /className="system-spec-scroll-track"/);
  assert.match(
    styles,
    /\.system-spec-list \{[\s\S]*overflow-y: scroll;[\s\S]*scrollbar-width: none;/,
  );
  assert.match(
    styles,
    /\.system-spec-list::-webkit-scrollbar \{[\s\S]*display: none;/,
  );
});

void test("macOS overlay chrome uses a scoped native drag region", () => {
  const shell = readSource("../app/AppShell.tsx");
  const entry = readSource("../main.tsx");
  const styles = readSource("../styles/globals.css");

  assert.match(
    shell,
    /className="app-window-drag-region"[\s\S]*data-tauri-drag-region/,
  );
  assert.match(
    entry,
    /isTauri\(\)[\s\S]*document\.documentElement\.dataset\.windowChrome = "macos-overlay"/,
  );
  assert.match(
    styles,
    /html\[data-window-chrome="macos-overlay"\] \.app-window-drag-region \{[\s\S]*app-region: drag;/,
  );
  assert.match(
    styles,
    /html\[data-window-chrome="macos-overlay"\] \.app-brand \{[\s\S]*padding-top: 44px;/,
  );
});

void test("compact navigation and recording copy omit the standby filler", () => {
  const shell = readSource("../app/AppShell.tsx");
  const recording = readSource("../pages/RecordingPage.tsx");
  const styles = readSource("../styles/globals.css");

  assert.doesNotMatch(shell, /app-sidebar-note|>\s*待命\s*</);
  assert.doesNotMatch(styles, /\.app-sidebar-note/);
  assert.match(
    recording,
    /label="音频处理"[\s\S]*detail="音频只在本机处理"/,
  );
  assert.doesNotMatch(recording, /待命时麦克风保持关闭/);
});
