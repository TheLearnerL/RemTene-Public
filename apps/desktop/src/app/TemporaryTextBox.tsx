import { useEffect, useState } from "react";

import { useBackendGateway } from "@/backend/useBackendGateway";
import { type TemporaryTextDelivery } from "@/lib/ipc";
import "@/styles/surfaces.css";

interface TemporaryTextCopy {
  title: string;
  reason: string;
}

const TEMPORARY_TEXT_COPY: Record<
  TemporaryTextDelivery["status_code"],
  TemporaryTextCopy
> = {
  "temporary_text.not_inserted": {
    title: "未写入目标位置",
    reason: "当前应用不支持直接写入，文字已保留在下方。",
  },
  "temporary_text.indeterminate": {
    title: "无法确认是否已写入",
    reason: "文字可能已经写入。请先检查，系统不会自动重试。",
  },
  "temporary_text.llm_fallback": {
    title: "已保留原始转录",
    reason: "文字整理暂不可用，已保留本地转录结果。",
  },
};

function TemporaryTextBox() {
  const gateway = useBackendGateway();
  const [delivery, setDelivery] = useState<TemporaryTextDelivery | null>(null);
  const [copiedDeliveryId, setCopiedDeliveryId] = useState<string | null>(null);
  const [copyingDeliveryId, setCopyingDeliveryId] = useState<string | null>(
    null,
  );

  useEffect(() => {
    document.documentElement.dataset.surface = "temporary-text-box";
    document.body.dataset.surface = "temporary-text-box";

    let active = true;
    let unlisten: (() => void) | undefined;

    void gateway
      .listenToTemporaryText((payload) => {
        if (active) {
          setCopiedDeliveryId(null);
          setDelivery(payload);
        }
      })
      .then((stopListening) => {
        if (!active) {
          stopListening();
          return;
        }
        unlisten = stopListening;
        // 监听挂上后再补一次拉取：本窗口是按需新建的，事件可能早于挂载送达。
        return gateway.getPendingTemporaryText().then((pending) => {
          if (active && pending !== null) {
            setDelivery((current) => current ?? pending);
          }
        });
      })
      .catch(() => undefined);

    return () => {
      active = false;
      unlisten?.();
      delete document.documentElement.dataset.surface;
      delete document.body.dataset.surface;
    };
  }, [gateway]);

  const copy = delivery ? TEMPORARY_TEXT_COPY[delivery.status_code] : null;
  const copied =
    delivery !== null && copiedDeliveryId === delivery.delivery_id;
  const copying = copyingDeliveryId !== null;

  const copyAll = async () => {
    if (copying || delivery === null) return;
    const deliveryId = delivery.delivery_id;
    setCopyingDeliveryId(deliveryId);
    try {
      await gateway.copyTemporaryText(deliveryId);
      setCopiedDeliveryId(deliveryId);
    } catch {
      // 复制失败时保持原状态，不用前端状态伪造系统剪贴板成功。
    } finally {
      setCopyingDeliveryId((current) =>
        current === deliveryId ? null : current,
      );
    }
  };

  return (
    <main
      className="temporary-text-surface"
      aria-labelledby="temporary-text-title"
      aria-busy={delivery === null}
    >
      <h1 id="temporary-text-title">{copy?.title ?? ""}</h1>
      <p className="temporary-text-reason">
        {copied ? "已复制到剪贴板；文字仍保留在这里。" : (copy?.reason ?? "")}
      </p>

      <textarea
        className="temporary-text-content"
        readOnly
        value={delivery?.final_text ?? ""}
        aria-label="本次转录结果"
      />

      <div className="temporary-text-actions">
        <button
          className={`temporary-text-copy${copied ? " is-copied" : ""}`}
          type="button"
          disabled={delivery === null || copying}
          aria-label={copied ? "已复制全部文字" : "复制全部文字"}
          aria-busy={copying}
          onClick={() => void copyAll()}
        >
          {copied ? "✓ 已复制" : "复制全部"}
        </button>
        <button
          className="temporary-text-close"
          type="button"
          onClick={() =>
            void gateway.dismissTemporaryText().catch(() => undefined)
          }
        >
          关闭
        </button>
      </div>
    </main>
  );
}

export default TemporaryTextBox;
