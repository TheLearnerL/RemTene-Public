import {
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";

import { useBackendGateway } from "@/backend/useBackendGateway";
import { userFacingGatewayError } from "@/backend/user-facing-error";
import {
  type AppSnapshot,
  type PermissionStatusView,
} from "@/lib/ipc";

export interface StatusDashboardState {
  loading: boolean;
  failed: boolean;
  errorMessage: string | null;
  snapshot: AppSnapshot | null;
  permissions: PermissionStatusView | null;
}

const initialState: StatusDashboardState = {
  loading: true,
  failed: false,
  errorMessage: null,
  snapshot: null,
  permissions: null,
};

export function useStatusDashboard() {
  const gateway = useBackendGateway();
  const [state, setState] = useState<StatusDashboardState>(initialState);
  const [checkingAsr, setCheckingAsr] = useState(false);
  const [healthErrorMessage, setHealthErrorMessage] = useState<string | null>(null);
  const requestRevision = useRef(0);
  const healthRequestRevision = useRef(0);
  const checkingAsrRef = useRef(false);

  const refresh = useCallback(async () => {
    const revision = ++requestRevision.current;
    setState((current) => ({
      ...current,
      loading: current.snapshot === null || current.permissions === null,
      failed: false,
      errorMessage: null,
    }));
    try {
      const [snapshot, permissions] = await Promise.all([
        gateway.getAppSnapshot(),
        gateway.getPermissionStatus(),
      ]);
      if (requestRevision.current !== revision) return;
      setState({
        loading: false,
        failed: false,
        errorMessage: null,
        snapshot,
        permissions,
      });
    } catch (error) {
      if (requestRevision.current !== revision) return;
      setState({
        loading: false,
        failed: true,
        snapshot: null,
        permissions: null,
        errorMessage: userFacingGatewayError(
          gateway,
          error,
          "暂时无法读取应用状态，请稍后重试。",
        ),
      });
    }
  }, [gateway]);

  const checkAsrHealth = useCallback(async () => {
    if (checkingAsrRef.current) return;
    checkingAsrRef.current = true;
    const revision = ++healthRequestRevision.current;
    setCheckingAsr(true);
    setHealthErrorMessage(null);
    try {
      const snapshot = await gateway.checkAsrHealth();
      if (healthRequestRevision.current !== revision) return;
      setState((current) => ({
        ...current,
        loading: false,
        failed: false,
        errorMessage: null,
        snapshot,
      }));
    } catch (error) {
      if (healthRequestRevision.current !== revision) return;
      setHealthErrorMessage(
        userFacingGatewayError(
          gateway,
          error,
          "本地模型检查未完成，请稍后重试。",
        ),
      );
    } finally {
      if (healthRequestRevision.current === revision) {
        checkingAsrRef.current = false;
        setCheckingAsr(false);
      }
    }
  }, [gateway]);

  useEffect(() => {
    let active = true;
    const stops: Array<() => void> = [];
    const safeRefresh = () => {
      if (active) void refresh();
    };
    const track = (subscription: Promise<() => void>) => {
      void subscription
        .then((stop) => {
          if (active) stops.push(stop);
          else stop();
        })
        .catch(() => undefined);
    };

    safeRefresh();
    track(
      gateway.listenToAppSnapshotChanged((snapshot) => {
        if (!active) return;
        setState((current) => ({
          ...current,
          loading: false,
          failed: false,
          errorMessage: null,
          snapshot,
        }));
      }),
    );
    track(gateway.listenToRecordingState(safeRefresh));
    track(gateway.listenToSessionTerminal(safeRefresh));
    track(gateway.listenToSessionEnded(safeRefresh));
    window.addEventListener("focus", safeRefresh);
    const refreshWhenVisible = () => {
      if (document.visibilityState === "visible") safeRefresh();
    };
    document.addEventListener("visibilitychange", refreshWhenVisible);
    try {
      track(
        getCurrentWindow().onFocusChanged(({ payload: focused }) => {
          if (focused) safeRefresh();
        }),
      );
    } catch {
      // 普通浏览器预览没有 Tauri window API；DOM 生命周期监听仍然可用。
    }

    return () => {
      active = false;
      requestRevision.current += 1;
      healthRequestRevision.current += 1;
      checkingAsrRef.current = false;
      window.removeEventListener("focus", safeRefresh);
      document.removeEventListener("visibilitychange", refreshWhenVisible);
      for (const stop of stops) stop();
    };
  }, [gateway, refresh]);

  return {
    ...state,
    checkingAsr,
    healthErrorMessage,
    refresh,
    checkAsrHealth,
  };
}
