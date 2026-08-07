import {
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";

import { useBackendGateway } from "@/backend/useBackendGateway";
import { userFacingGatewayError } from "@/backend/user-facing-error";
import { type PermissionStatusView } from "@/lib/ipc";

export type PermissionAction =
  | "request_microphone"
  | "open_microphone"
  | "request_accessibility"
  | "open_accessibility";

export function usePermissions() {
  const gateway = useBackendGateway();
  const [status, setStatus] = useState<PermissionStatusView | null>(null);
  const [busyAction, setBusyAction] = useState<PermissionAction | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const requestRevision = useRef(0);

  const refresh = useCallback(async () => {
    const revision = ++requestRevision.current;
    const next = await gateway.getPermissionStatus();
    if (requestRevision.current === revision) {
      setStatus(next);
      setMessage(null);
    }
  }, [gateway]);

  useEffect(() => {
    let active = true;
    let stopTauriFocusListener: (() => void) | null = null;
    const load = () => {
      const revision = ++requestRevision.current;
      void gateway
        .getPermissionStatus()
        .then((next) => {
          if (active && requestRevision.current === revision) {
            setStatus(next);
            setMessage(null);
          }
        })
        .catch((error: unknown) => {
          if (active && requestRevision.current === revision) {
            setMessage(
              userFacingGatewayError(
                gateway,
                error,
                "无法读取系统权限，请确认应用后端正在运行。",
              ),
            );
          }
        });
    };
    const loadWhenVisible = () => {
      if (document.visibilityState === "visible") load();
    };
    load();
    window.addEventListener("focus", load);
    document.addEventListener("visibilitychange", loadWhenVisible);
    try {
      void getCurrentWindow()
        .onFocusChanged(({ payload: focused }) => {
          if (active && focused) load();
        })
        .then((stop) => {
          if (active) stopTauriFocusListener = stop;
          else stop();
        })
        .catch(() => {
          // 普通浏览器预览没有 Tauri window API；DOM 焦点与可见性监听仍可用。
        });
    } catch {
      // 无 Tauri globals 的浏览器可能同步失败，不影响预览。
    }
    return () => {
      active = false;
      requestRevision.current += 1;
      stopTauriFocusListener?.();
      window.removeEventListener("focus", load);
      document.removeEventListener("visibilitychange", loadWhenVisible);
    };
  }, [gateway]);

  const run = useCallback(
    async (action: PermissionAction) => {
      setBusyAction(action);
      setMessage(null);
      try {
        let next: PermissionStatusView | void;
        switch (action) {
          case "request_microphone":
            next = await gateway.requestMicrophonePermission();
            break;
          case "request_accessibility":
            next = await gateway.requestAccessibilityPermission();
            break;
          case "open_microphone":
            next = await gateway.openMicrophoneSettings();
            break;
          case "open_accessibility":
            next = await gateway.openAccessibilitySettings();
            break;
        }
        if (next) setStatus(next);
        else await refresh();
      } catch (error) {
        setMessage(
          userFacingGatewayError(
            gateway,
            error,
            "权限操作未完成，请稍后重试或手动打开系统设置。",
          ),
        );
      } finally {
        setBusyAction(null);
      }
    },
    [gateway, refresh],
  );

  return { status, busyAction, message, refresh, run };
}
