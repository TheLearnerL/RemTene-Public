import {
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";

import { useBackendGateway } from "@/backend/useBackendGateway";
import { userFacingGatewayError } from "@/backend/user-facing-error";
import {
  getIpcErrorCode,
  type RecordingMode,
  type SettingsView,
} from "@/lib/ipc";

function recordingSettingsFallback(code: string | null): string {
  switch (code) {
    case "shortcut.register_failed":
      return "这个快捷键无法使用，可能已被系统或其他应用占用。";
    case "shortcut.unsafe_bare_key":
      return "这个键不能单独使用，请搭配修饰键。";
    case "shortcut.accessibility_required":
      return "使用单独修饰键前，请先在系统设置中允许“辑语”使用辅助功能。";
    case "shortcut.unsupported":
      return "当前系统暂不支持单独修饰键。";
    case "shortcut.invalid":
      return "无法识别这个快捷键，请重新录入。";
    default:
      return "录音设置未保存，请稍后重试。";
  }
}

interface RecordingSettingsState {
  loading: boolean;
  saving: boolean;
  failed: boolean;
  errorMessage: string | null;
  settings: SettingsView | null;
}

const initialState: RecordingSettingsState = {
  loading: true,
  saving: false,
  failed: false,
  errorMessage: null,
  settings: null,
};

/** 录音页只呈现并修改后端持久化设置；Renderer 不保留平行的成功状态。 */
export function useRecordingSettings() {
  const gateway = useBackendGateway();
  const [state, setState] = useState<RecordingSettingsState>(initialState);
  const requestRevision = useRef(0);
  const savingRef = useRef(false);

  const refresh = useCallback(async () => {
    const revision = ++requestRevision.current;
    setState((current) => ({
      ...current,
      loading: current.settings === null,
      failed: false,
      errorMessage: null,
    }));
    try {
      const settings = await gateway.getSettings();
      if (requestRevision.current !== revision) return;
      setState({
        loading: false,
        saving: false,
        failed: false,
        errorMessage: null,
        settings,
      });
    } catch (error) {
      if (requestRevision.current !== revision) return;
      setState({
        loading: false,
        saving: false,
        failed: true,
        errorMessage: userFacingGatewayError(
          gateway,
          error,
          "无法读取录音设置，请稍后重试。",
        ),
        settings: null,
      });
    }
  }, [gateway]);

  const save = useCallback(
    async (
      mutation: (settings: SettingsView) => Promise<SettingsView>,
    ) => {
      if (state.settings === null || savingRef.current) return;
      savingRef.current = true;
      const revision = ++requestRevision.current;
      setState((current) => ({
        ...current,
        saving: true,
        failed: false,
        errorMessage: null,
      }));
      try {
        const settings = await mutation(state.settings);
        if (requestRevision.current !== revision) return;
        setState({
          loading: false,
          saving: false,
          failed: false,
          errorMessage: null,
          settings,
        });
      } catch (error) {
        if (requestRevision.current !== revision) return;
        const code = getIpcErrorCode(error);
        setState((current) => ({
          ...current,
          loading: false,
          saving: false,
          failed: false,
          errorMessage: userFacingGatewayError(
            gateway,
            error,
            recordingSettingsFallback(code),
          ),
        }));
      } finally {
        savingRef.current = false;
      }
    },
    [gateway, state.settings],
  );

  const updatePreferences = useCallback(
    async (recordingMode: RecordingMode, durationSeconds: number) => {
      await save((settings) =>
        gateway.setRecordingPreferences(
          settings.version,
          recordingMode,
          durationSeconds,
        ),
      );
    },
    [gateway, save],
  );

  const updateShortcut = useCallback(
    async (recordingShortcut: string | null) => {
      await save((settings) =>
        gateway.setRecordingShortcut(settings.version, recordingShortcut),
      );
    },
    [gateway, save],
  );

  useEffect(() => {
    let active = true;
    const safeRefresh = () => {
      if (active) void refresh();
    };
    safeRefresh();
    return () => {
      active = false;
      requestRevision.current += 1;
      savingRef.current = false;
    };
  }, [refresh]);

  return { ...state, refresh, updatePreferences, updateShortcut };
}
