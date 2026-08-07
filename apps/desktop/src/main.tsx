import React from "react";
import ReactDOM from "react-dom/client";
import { isTauri } from "@tauri-apps/api/core";
import App from "./app/App";
import RecordingHud from "./app/RecordingHud";
import SessionFeedback from "./app/SessionFeedback";
import TemporaryTextBox from "./app/TemporaryTextBox";
import { GatewayProvider } from "./backend/GatewayProvider";
import { createStatusPreviewGateway } from "./features/status/status-preview";
import "./styles/globals.css";
import "./styles/page-layout.css";

const search = new URLSearchParams(window.location.search);
const surface = search.get("surface");

// 辅助窗口必须在 React 首次绘制前就切换到透明根背景；若等组件 Effect，
// macOS WebView 会先绘制一帧控制面板的矩形背景，破坏 HUD 圆角。
if (
  surface === "recording-hud" ||
  surface === "temporary-text-box" ||
  surface === "session-feedback"
) {
  document.documentElement.dataset.surface = surface;
  document.body.dataset.surface = surface;
}

const macOSDesktopControlPanel =
  surface === null &&
  isTauri() &&
  /Macintosh|Mac OS X/.test(navigator.userAgent);
const macOSWindowChromePreview =
  import.meta.env.DEV &&
  surface === null &&
  search.get("window-chrome") === "macos-overlay";
const statusPreviewGateway = import.meta.env.DEV
  ? createStatusPreviewGateway()
  : undefined;

if (macOSDesktopControlPanel || macOSWindowChromePreview) {
  document.documentElement.dataset.windowChrome = "macos-overlay";
}

if (import.meta.env.DEV) {
  const theme = search.get("theme");
  if (theme === "light" || theme === "dark") {
    document.documentElement.dataset.theme = theme;
  }
}

function surfaceElement() {
  switch (surface) {
    case "recording-hud":
      return <RecordingHud />;
    case "temporary-text-box":
      return <TemporaryTextBox />;
    case "session-feedback":
      return <SessionFeedback />;
    default:
      return <App />;
  }
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <GatewayProvider gateway={statusPreviewGateway}>
      {surfaceElement()}
    </GatewayProvider>
  </React.StrictMode>,
);
