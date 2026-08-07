import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

function readSource(relativePath: string): string {
  return readFileSync(new URL(relativePath, import.meta.url), "utf8");
}

const feedbackSource = readSource("../app/SessionFeedback.tsx");
const temporaryTextSource = readSource("../app/TemporaryTextBox.tsx");
const gatewaySource = readSource("../backend/gateway.ts");
const ipcSource = readSource("./ipc.ts");
const mainSource = readSource("../main.tsx");
const shellSource = readSource("../app/AppShell.tsx");
const modelSource = readSource("../pages/ModelPage.tsx");
const previewSource = readSource(
  "../features/status/status-preview.ts",
);
const surfaceStyles = readSource("../styles/surfaces.css");
const temporaryTextRustSource = readSource(
  "../../src-tauri/src/temporary_text.rs",
);
const sessionFeedbackRustSource = readSource(
  "../../src-tauri/src/session_feedback.rs",
);

void test("session feedback owns the four concise product copies without backend prose", () => {
  for (const text of [
    "需要麦克风权限",
    "允许使用麦克风后即可开始录音。",
    "打开系统设置",
    "本地转录未完成",
    "请检查本地模型。音频仍只在本机处理。",
    "检查模型",
    "文字整理未完成",
    "原始转录已保留，可以直接复制。",
    "查看服务设置",
    "无法确认是否写入",
    "请先检查输入位置，系统不会自动重试。",
    "查看临时文字",
  ]) {
    assert.equal(feedbackSource.includes(text), true, `missing approved copy: ${text}`);
    assert.equal(
      temporaryTextSource.includes(text),
      false,
      `error copy still owned by TemporaryTextBox: ${text}`,
    );
  }

  for (const code of [
    "notification.permission_microphone",
    "notification.asr",
    "notification.llm",
    "notification.delivery",
  ]) {
    assert.equal(feedbackSource.includes(code), true, `missing ${code}`);
  }
  assert.doesNotMatch(feedbackSource, /navigator\.clipboard/);
});

void test("session feedback listens before pulling and applies the visible notification", () => {
  assert.match(
    feedbackSource,
    /listenToNotificationRaised[\s\S]*getPendingNotification/,
  );
  assert.match(
    feedbackSource,
    /applyNotificationAction\(notification\)/,
  );
  assert.match(
    ipcSource,
    /invoke<void>\("notification_apply_action", \{\s*notification: validateUserNotification\(notification\),\s*\}\)/,
  );
  assert.match(ipcSource, /"notification_get_pending"/);
  assert.match(ipcSource, /"notification:raised"/);
  assert.match(ipcSource, /"control-panel:navigate"/);
  assert.match(gatewaySource, /applyNotificationAction\(notification: UserNotification\)/);
});

void test("session feedback never degrades into a blank window", () => {
  for (const text of [
    "正在读取错误信息",
    "暂时无法显示错误详情",
    "没有执行其他操作，可以重新读取。",
    "重新读取",
  ]) {
    assert.equal(feedbackSource.includes(text), true, `missing fallback copy: ${text}`);
  }
  assert.doesNotMatch(feedbackSource, /session-feedback-shell--empty/);
  assert.doesNotMatch(feedbackSource, /\.catch\(\(\) => undefined\)/);
  assert.match(feedbackSource, /setLoadState\("unavailable"\)/);
  assert.match(feedbackSource, /gateway\.getPendingNotification\(\)/);
});

void test("session feedback has its own route, preview stubs, and compact 420 by 120 surface", () => {
  assert.match(
    mainSource,
    /case "session-feedback":\s*return <SessionFeedback \/>;/,
  );
  for (const preview of [
    "error-permission",
    "error-asr",
    "error-llm",
    "error-delivery",
  ]) {
    assert.equal(previewSource.includes(preview), true, `missing ${preview}`);
  }
  assert.match(previewSource, /getPendingNotification: \(\) => Promise\.resolve\(notification\)/);
  assert.match(previewSource, /listenToControlPanelNavigation/);
  assert.match(
    surfaceStyles,
    /html\[data-surface="session-feedback"\] \.session-feedback-shell \{[\s\S]*width: min\(420px, 100%\);[\s\S]*height: min\(120px, 100%\);/,
  );
  assert.match(
    surfaceStyles,
    /html\[data-surface="session-feedback"\] \.surface-error \{[\s\S]*display: grid;[\s\S]*grid-template-columns: minmax\(0, 1fr\) 128px;[\s\S]*grid-template-rows: 28px 40px;/,
  );
  assert.match(
    surfaceStyles,
    /html\[data-surface="session-feedback"\] \.surface-error-heading \{[\s\S]*display: flex;[\s\S]*grid-area: heading;[\s\S]*align-items: center;/,
  );
  assert.match(
    surfaceStyles,
    /html\[data-surface="session-feedback"\] \.surface-error p \{[\s\S]*grid-area: guidance;[\s\S]*max-height: 40px;[\s\S]*-webkit-line-clamp: 2;/,
  );
  assert.match(
    surfaceStyles,
    /html\[data-surface="session-feedback"\] \.surface-error button \{[\s\S]*grid-area: action;[\s\S]*width: 128px;[\s\S]*height: 40px;[\s\S]*margin: 0;/,
  );
  assert.match(
    surfaceStyles,
    /html\[data-surface="temporary-text-box"\] \.temporary-text-surface \{[\s\S]*display: grid;[\s\S]*grid-template-rows: 24px 2px 18px 8px minmax\(0, 1fr\) 8px 32px;/,
  );
});

void test("temporary popup surfaces have no title bar or opaque outer rectangle", () => {
  for (const rustSource of [temporaryTextRustSource, sessionFeedbackRustSource]) {
    assert.match(rustSource, /\.decorations\(false\)/);
    assert.match(rustSource, /\.transparent\(true\)/);
    assert.match(rustSource, /\.shadow\(false\)/);
    assert.doesNotMatch(rustSource, /\.decorations\(true\)/);
  }
  assert.match(
    surfaceStyles,
    /\.temporary-text-surface \{[\s\S]*clip-path: inset\(0 round 24px\);[\s\S]*contain: paint;/,
  );
  assert.match(
    surfaceStyles,
    /\.surface-error \{[\s\S]*clip-path: inset\(0 round 18px\);[\s\S]*contain: paint;/,
  );
});

void test("control panel navigation lands on the exact model tab", () => {
  assert.match(shellSource, /listenToControlPanelNavigation/);
  assert.match(shellSource, /window\.location\.hash = "model"/);
  assert.match(
    modelSource,
    /target === "model\.asr" \? "asr" : "text-service"/,
  );
  assert.match(
    modelSource,
    /navigationTargetToTab\(navigationTarget\)/,
  );
});
