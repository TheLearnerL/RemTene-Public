import { useEffect, useState } from "react";

import { useBackendGateway } from "@/backend/useBackendGateway";
import {
  type UserNotification,
  type UserNotificationCode,
} from "@/lib/ipc";
import "@/styles/surfaces.css";

interface SessionFeedbackCopy {
  title: string;
  guidance: string;
  action: string;
  tone: "warning" | "danger" | "neutral";
}

type FeedbackLoadState = "loading" | "ready" | "unavailable";

const SESSION_FEEDBACK_COPY: Record<
  UserNotificationCode,
  SessionFeedbackCopy
> = {
  "notification.permission_microphone": {
    title: "需要麦克风权限",
    guidance: "允许使用麦克风后即可开始录音。",
    action: "打开系统设置",
    tone: "warning",
  },
  "notification.asr": {
    title: "本地转录未完成",
    guidance: "请检查本地模型。音频仍只在本机处理。",
    action: "检查模型",
    tone: "danger",
  },
  "notification.llm": {
    title: "文字整理未完成",
    guidance: "原始转录已保留，可以直接复制。",
    action: "查看服务设置",
    tone: "neutral",
  },
  "notification.delivery": {
    title: "无法确认是否写入",
    guidance: "请先检查输入位置，系统不会自动重试。",
    action: "查看临时文字",
    tone: "warning",
  },
};

const FEEDBACK_LOADING_COPY: SessionFeedbackCopy = {
  title: "正在读取错误信息",
  guidance: "请稍候。",
  action: "读取中",
  tone: "neutral",
};

const FEEDBACK_UNAVAILABLE_COPY: SessionFeedbackCopy = {
  title: "暂时无法显示错误详情",
  guidance: "没有执行其他操作，可以重新读取。",
  action: "重新读取",
  tone: "warning",
};

function SessionFeedback() {
  const gateway = useBackendGateway();
  const [notification, setNotification] =
    useState<UserNotification | null>(null);
  const [loadState, setLoadState] =
    useState<FeedbackLoadState>("loading");
  const [applying, setApplying] = useState(false);

  useEffect(() => {
    document.documentElement.dataset.surface = "session-feedback";
    document.body.dataset.surface = "session-feedback";

    let active = true;
    let unlisten: (() => void) | undefined;
    let receivedNotification = false;

    void gateway
      .listenToNotificationRaised((nextNotification) => {
        if (active) {
          receivedNotification = true;
          setApplying(false);
          setLoadState("ready");
          setNotification(nextNotification);
        }
      })
      .then((stopListening) => {
        if (!active) {
          stopListening();
          return;
        }
        unlisten = stopListening;
        // 窗口按需创建，事件可能早于 React 监听器；挂上监听后再读取当前 pending。
        return gateway.getPendingNotification().then((pending) => {
          if (!active) return;
          if (pending !== null) {
            receivedNotification = true;
            setLoadState("ready");
            setNotification((current) => current ?? pending);
          } else if (!receivedNotification) {
            setLoadState("unavailable");
          }
        });
      })
      .catch(() => {
        if (active) {
          setLoadState("unavailable");
        }
      });

    return () => {
      active = false;
      unlisten?.();
      delete document.documentElement.dataset.surface;
      delete document.body.dataset.surface;
    };
  }, [gateway]);

  const copy = notification
    ? SESSION_FEEDBACK_COPY[notification.code]
    : loadState === "loading"
      ? FEEDBACK_LOADING_COPY
      : FEEDBACK_UNAVAILABLE_COPY;

  const applyAction = async () => {
    if (applying || notification === null) return;
    setApplying(true);
    try {
      // 回传使用者实际看到的通知；Rust 会拒绝已被新 pending 取代的旧动作。
      await gateway.applyNotificationAction(notification);
    } catch {
      setApplying(false);
    }
  };

  const retryPending = async () => {
    if (applying) return;
    setApplying(true);
    setLoadState("loading");
    try {
      const pending = await gateway.getPendingNotification();
      if (pending === null) {
        setLoadState("unavailable");
      } else {
        setNotification(pending);
        setLoadState("ready");
      }
    } catch {
      setLoadState("unavailable");
    } finally {
      setApplying(false);
    }
  };

  const loading = notification === null && loadState === "loading";

  return (
    <main
      className="session-feedback-shell"
      aria-labelledby="session-feedback-title"
      aria-busy={applying || loading}
    >
      <section className="surface-error" data-tone={copy.tone}>
        <div className="surface-error-heading">
          <span className="surface-error-dot" aria-hidden="true" />
          <h1 id="session-feedback-title">{copy.title}</h1>
        </div>
        <p>{copy.guidance}</p>
        <button
          type="button"
          disabled={applying || loading}
          onClick={() =>
            void (notification === null ? retryPending() : applyAction())
          }
        >
          {copy.action}
        </button>
      </section>
    </main>
  );
}

export default SessionFeedback;
