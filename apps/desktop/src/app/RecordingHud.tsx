import { useEffect, useRef, useState } from "react";

import { useBackendGateway } from "@/backend/useBackendGateway";
import { CheckIcon, CloseIcon } from "@/components/icons";
import { type SessionPublicSnapshot } from "@/lib/ipc";
import "@/styles/surfaces.css";
import {
  recordingClockLabel,
  recordingElapsedFromAnchor,
} from "./recording-hud-clock";
import { waitForRecordingHudExit } from "./recording-hud-motion";

type HudLoadState =
  | { status: "loading" }
  | { status: "ready"; snapshot: SessionPublicSnapshot | null }
  | { status: "unavailable" };

type PendingAction = "finish" | "cancel" | null;
type FailedAction = Exclude<PendingAction, null>;
type HudVisualState =
  | "preparing"
  | "recording"
  | "processing"
  | "delivering"
  | "completed";

interface HudPresentation {
  state: HudVisualState;
  label: string;
}

const HUD_PREVIEW_SESSION_ID = "00000000-0000-4000-8000-000000000001";

function previewSnapshot(): SessionPublicSnapshot | null {
  if (!import.meta.env.DEV) return null;

  const preview = new URLSearchParams(window.location.search).get("preview");
  const common = {
    contract_version: 1,
    session_id: HUD_PREVIEW_SESSION_ID,
    recording_limit_ms: 120_000,
  };

  switch (preview) {
    case "hud-preparing":
      return {
        ...common,
        user_state: "preparing",
        phase: "preparing",
        recording_elapsed_ms: 0,
        can_finish: false,
        can_cancel: true,
        status_code: "session.preparing",
      };
    case "recording":
    case "hud-recording":
      return {
        ...common,
        user_state: "recording",
        phase: "recording",
        recording_elapsed_ms: 12_000,
        can_finish: true,
        can_cancel: true,
        status_code: "session.recording",
      };
    case "hud-processing":
      return {
        ...common,
        user_state: "processing",
        phase: "recognizing",
        recording_elapsed_ms: 12_000,
        can_finish: false,
        can_cancel: false,
        status_code: "session.recognizing",
      };
    case "hud-delivering":
      return {
        ...common,
        user_state: "processing",
        phase: "delivering",
        recording_elapsed_ms: 12_000,
        can_finish: false,
        can_cancel: false,
        status_code: "session.delivering",
      };
    case "hud-completed":
    case "hud-exiting":
      return {
        ...common,
        user_state: "completed",
        phase: "terminated",
        recording_elapsed_ms: 12_000,
        can_finish: false,
        can_cancel: false,
        status_code: "session.completed",
      };
    default:
      return null;
  }
}

function recordingLabel(
  snapshot: SessionPublicSnapshot,
  elapsed: number | null,
): string {
  const limit = snapshot.recording_limit_ms;
  if (elapsed !== null && limit !== null) {
    return `录音中，已录制 ${recordingClockLabel(elapsed)}，上限 ${recordingClockLabel(limit)}`;
  }
  if (elapsed !== null) return `录音中，已录制 ${recordingClockLabel(elapsed)}`;
  return "录音中";
}

function hudPresentation(snapshot: SessionPublicSnapshot): HudPresentation | null {
  if (
    snapshot.user_state === "completed" &&
    snapshot.phase === "terminated"
  ) {
    return {
      state: "completed",
      label: "已完成",
    };
  }

  if (
    snapshot.user_state === "processing" &&
    (snapshot.phase === "delivering" || snapshot.phase === "finalizing")
  ) {
    return {
      state: "delivering",
      label: "正在输出",
    };
  }

  if (
    snapshot.user_state === "processing" &&
    (snapshot.phase === "recognizing" || snapshot.phase === "processing")
  ) {
    return {
      state: "processing",
      label: snapshot.phase === "processing" ? "正在整理" : "正在转录",
    };
  }

  if (
    snapshot.user_state === "recording" &&
    snapshot.phase === "recording"
  ) {
    return {
      state: "recording",
      label: "录音中",
    };
  }

  if (
    snapshot.user_state === "preparing" &&
    snapshot.phase === "preparing"
  ) {
    return {
      state: "preparing",
      label: "准备中",
    };
  }

  return null;
}

const initialPreviewSnapshot = previewSnapshot();
const initialPreviewExiting =
  import.meta.env.DEV &&
  new URLSearchParams(window.location.search).get("preview") === "hud-exiting";

function RecordingClock({
  snapshot,
  freezePreview,
}: {
  snapshot: SessionPublicSnapshot;
  freezePreview: boolean;
}) {
  const [clock, setClock] = useState(() => {
    const now = performance.now();
    return { anchorTimeMs: now, currentTimeMs: now };
  });
  const snapshotElapsed = snapshot.recording_elapsed_ms;
  const elapsed =
    snapshotElapsed === null || freezePreview
      ? snapshotElapsed
      : recordingElapsedFromAnchor(
          snapshotElapsed,
          clock.anchorTimeMs,
          clock.currentTimeMs,
          snapshot.recording_limit_ms,
        );

  useEffect(() => {
    if (freezePreview || snapshotElapsed === null) return;

    const timer = window.setInterval(() => {
      setClock((current) => ({
        ...current,
        currentTimeMs: performance.now(),
      }));
    }, 250);
    return () => window.clearInterval(timer);
  }, [freezePreview, snapshotElapsed]);

  return (
    <span
      className="hud-label hud-label--clock"
      aria-label={recordingLabel(snapshot, elapsed)}
      title={recordingLabel(snapshot, elapsed)}
    >
      {elapsed === null ? "录音中" : recordingClockLabel(elapsed)}
    </span>
  );
}

function HudStatusGlyph() {
  return (
    <span className="hud-status-glyph" aria-hidden="true">
      <span className="hud-status-dot" />
      <span className="hud-status-ring" />
      <span className="hud-status-track">
        <span />
      </span>
      <CheckIcon className="hud-status-check" />
      <span className="hud-status-alert" />
    </span>
  );
}

function HudUnavailable() {
  return (
    <main className="hud-shell">
      <section
        className="hud-surface"
        data-state="unavailable"
        role="alert"
        aria-label="辑语录音状态"
      >
        <HudStatusGlyph />
        <div className="hud-copy hud-copy--switch">
          <strong className="hud-label" title="请在控制面板确认当前状态">
            状态异常
          </strong>
        </div>
        <span className="hud-actions-slot" aria-hidden="true" />
      </section>
    </main>
  );
}

function RecordingHud() {
  const gateway = useBackendGateway();
  const [hud, setHud] = useState<HudLoadState>(
    initialPreviewSnapshot
      ? { status: "ready", snapshot: initialPreviewSnapshot }
      : { status: "loading" },
  );
  const [pendingAction, setPendingAction] = useState<PendingAction>(null);
  const [failedAction, setFailedAction] = useState<FailedAction | null>(null);
  const [closingSessionId, setClosingSessionId] = useState<string | null>(
    initialPreviewExiting ? HUD_PREVIEW_SESSION_ID : null,
  );
  const actionInFlight = useRef(false);
  const actionSequence = useRef(0);

  useEffect(() => {
    document.documentElement.dataset.surface = "recording-hud";
    document.body.dataset.surface = "recording-hud";
    if (initialPreviewSnapshot) {
      return () => {
        delete document.documentElement.dataset.surface;
        delete document.body.dataset.surface;
      };
    }

    let active = true;
    let receivedEvent = false;
    let unlistenState: (() => void) | undefined;
    let unlistenEnded: (() => void) | undefined;

    void gateway
      .listenToRecordingState((snapshot) => {
        if (!active) return;
        receivedEvent = true;
        actionSequence.current += 1;
        actionInFlight.current = false;
        setPendingAction(null);
        setFailedAction(null);
        setClosingSessionId(null);
        setHud({ status: "ready", snapshot });
      })
      .then((stopListening) => {
        if (!active) {
          stopListening();
          return;
        }
        unlistenState = stopListening;
      })
      .catch(() => {
        if (active) {
          setHud((current) =>
            current.status === "loading" ? { status: "unavailable" } : current,
          );
        }
      });

    void gateway
      .listenToSessionEnded((ended) => {
        if (!active) return;
        receivedEvent = true;
        actionSequence.current += 1;
        actionInFlight.current = true;
        setPendingAction(null);
        setFailedAction(null);
        setClosingSessionId(ended.session_id);
      })
      .then((stopListening) => {
        if (!active) {
          stopListening();
          return;
        }
        unlistenEnded = stopListening;
      })
      .catch(() => undefined);

    void gateway
      .getRecordingHudState()
      .then((snapshot) => {
        if (active && !receivedEvent) setHud({ status: "ready", snapshot });
      })
      .catch(() => {
        if (active && !receivedEvent) setHud({ status: "unavailable" });
      });

    return () => {
      active = false;
      actionSequence.current += 1;
      actionInFlight.current = false;
      unlistenState?.();
      unlistenEnded?.();
      delete document.documentElement.dataset.surface;
      delete document.body.dataset.surface;
    };
  }, [gateway]);

  const snapshot = hud.status === "ready" ? hud.snapshot : null;

  if (hud.status === "loading") {
    return (
      <main className="hud-shell hud-shell--empty" aria-busy="true">
        <span className="surface-visually-hidden" role="status">
          正在读取录音状态
        </span>
      </main>
    );
  }

  if (hud.status === "unavailable") {
    return <HudUnavailable />;
  }

  if (snapshot === null) {
    return <main className="hud-shell hud-shell--empty" aria-hidden="true" />;
  }

  const presentation = hudPresentation(snapshot);
  if (presentation === null) {
    return <HudUnavailable />;
  }

  const submit = async (action: Exclude<PendingAction, null>) => {
    if (!snapshot || actionInFlight.current) return;
    if (action === "finish" && !snapshot.can_finish) return;
    if (action === "cancel" && !snapshot.can_cancel) return;

    actionInFlight.current = true;
    const sequence = ++actionSequence.current;
    setPendingAction(action);
    setFailedAction(null);
    const reducedMotion =
      window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false;
    await waitForRecordingHudExit(reducedMotion);
    if (actionSequence.current !== sequence) return;

    try {
      if (action === "finish") await gateway.finishRecording(snapshot.session_id);
      else await gateway.cancelRecording(snapshot.session_id);
    } catch {
      if (actionSequence.current !== sequence) return;
      actionInFlight.current = false;
      setPendingAction(null);
      setFailedAction(action);
    }
  };

  const isExiting =
    pendingAction !== null || closingSessionId === snapshot.session_id;
  const canCancel = Boolean(snapshot?.can_cancel && !isExiting);
  const canFinish = Boolean(snapshot?.can_finish && !isExiting);
  const showRecordingActions =
    presentation.state === "preparing" || presentation.state === "recording";
  const visualState = isExiting
    ? "exiting"
    : failedAction
      ? "recovering"
      : "visible";

  return (
    <main className="hud-shell" aria-live="polite">
      <section
        key={snapshot.session_id}
        className="hud-surface"
        data-state={presentation.state}
        data-phase={snapshot.phase}
        data-visual-state={visualState}
        data-action-error={failedAction ?? undefined}
        aria-busy={isExiting}
        aria-label="辑语录音状态"
      >
        <HudStatusGlyph />
        <div
          key={`${presentation.state}:${snapshot.phase}`}
          className="hud-copy hud-copy--switch"
        >
          {presentation.state === "recording" ? (
            <RecordingClock
              key={`${snapshot.session_id}:${snapshot.recording_elapsed_ms}:${snapshot.recording_limit_ms}`}
              snapshot={snapshot}
              freezePreview={initialPreviewSnapshot !== null}
            />
          ) : (
            <strong className="hud-label">{presentation.label}</strong>
          )}
        </div>

        <div
          className="hud-actions-slot"
          data-visible={showRecordingActions ? "true" : "false"}
          aria-hidden={!showRecordingActions}
        >
          <span className="hud-action-divider" aria-hidden="true" />
          <div className="hud-actions" aria-label="录音操作">
            <button
              className="hud-button hud-button--cancel"
              type="button"
              aria-label="取消本次录音"
              aria-describedby={
                failedAction === "cancel" ? "hud-action-error" : undefined
              }
              title="取消本次录音"
              data-action-state={
                pendingAction === "cancel" ? "pending" : "idle"
              }
              tabIndex={showRecordingActions ? 0 : -1}
              disabled={!showRecordingActions || !canCancel}
              onClick={() => void submit("cancel")}
            >
              <CloseIcon />
            </button>
            <button
              className="hud-button hud-button--confirm"
              type="button"
              aria-label="结束录音"
              aria-describedby={
                failedAction === "finish" ? "hud-action-error" : undefined
              }
              title="结束录音"
              data-action-state={
                pendingAction === "finish" ? "pending" : "idle"
              }
              tabIndex={showRecordingActions ? 0 : -1}
              disabled={!showRecordingActions || !canFinish}
              onClick={() => void submit("finish")}
            >
              <CheckIcon />
            </button>
          </div>
        </div>
        {failedAction ? (
          <p
            id="hud-action-error"
            className="surface-visually-hidden"
            role="alert"
          >
            {failedAction === "finish"
              ? "结束录音失败，请重试。"
              : "取消录音失败，请重试。"}
          </p>
        ) : null}
      </section>
    </main>
  );
}

export default RecordingHud;
