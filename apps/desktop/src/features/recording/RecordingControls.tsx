import { useState } from "react";

import { useBackendGateway } from "@/backend/useBackendGateway";
import { userFacingGatewayError } from "@/backend/user-facing-error";
import { Button } from "@/components/ui/button";
import { Feedback } from "@/components/ui/feedback";
import { Section } from "@/components/ui/surface";
import {
  type SessionFinishView,
  type SessionPublicSnapshot,
} from "@/lib/ipc";

function finishMessage(result: SessionFinishView): string {
  switch (result.status) {
    case "delivered":
      if (result.notice === "llm_not_configured") {
        return "尚未设置文字整理服务，已输出本地转录文字。";
      }
      if (result.notice === "llm_unavailable") {
        return "文字整理暂不可用，已把本地转录文字放入临时文字框。";
      }
      switch (result.delivery) {
        case "inserted":
          return "已插入光标处。";
        case "clipboard":
          return "已通过兼容粘贴写入。";
        default:
          return "未能安全写入，文字已放入临时文字框。";
      }
    case "failed":
      return "本次没有产生文字，请稍后重试。";
    case "discarded":
      return "本次已作废，没有任何输出。";
    default:
      return "当前没有正在进行的录音。";
  }
}

export function RecordingControls({
  snapshot,
  onSettled,
}: {
  snapshot: SessionPublicSnapshot;
  onSettled: () => Promise<void>;
}) {
  const gateway = useBackendGateway();
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [messageTone, setMessageTone] = useState<"neutral" | "error">("neutral");

  const run = async (action: () => Promise<void>) => {
    setBusy(true);
    setMessage(null);
    setMessageTone("neutral");
    try {
      await action();
    } catch (error) {
      setMessage(
        userFacingGatewayError(
          gateway,
          error,
          "录音操作未完成，请检查麦克风权限后重试。",
        ),
      );
      setMessageTone("error");
    } finally {
      setBusy(false);
      await onSettled();
    }
  };

  return (
    <Section
      title="录音控制"
      description="一次只处理一段录音，完成后才能开始下一段。"
    >
      <div className="p-5">
        <div className="flex flex-wrap gap-3">
          {snapshot.can_finish ? (
            <Button
              variant="primary"
              disabled={busy}
              onClick={() =>
                void run(async () => {
                  const result = await gateway.finishSession(snapshot.session_id);
                  setMessage(finishMessage(result));
                })
              }
            >
              {busy ? "正在提交…" : "结束并提交"}
            </Button>
          ) : null}
          {snapshot.can_cancel ? (
            <Button
              disabled={busy}
              onClick={() =>
                void run(async () => {
                  await gateway.cancelSession(snapshot.session_id);
                  setMessage("本次已取消，没有产生输出。");
                })
              }
            >
              取消
            </Button>
          ) : null}
        </div>
        <p className="mt-3 text-caption text-foreground-muted">
          {snapshot.user_state === "preparing"
            ? "正在准备本地转录。"
            : "正在录音。结束后会开始本地转录。"}
        </p>
        {message ? (
          <Feedback
            className="mt-4"
            title={messageTone === "error" ? "录音操作未完成" : "录音状态"}
            tone={messageTone}
          >
            {message}
          </Feedback>
        ) : null}
      </div>
    </Section>
  );
}
