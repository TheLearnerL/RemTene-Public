import {
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";

import { useBackendGateway } from "@/backend/useBackendGateway";
import {
  type ProcessingMode,
  type SettingsView,
} from "@/lib/ipc";

interface TextProcessingSettingsState {
  loading: boolean;
  saving: boolean;
  failed: boolean;
  settings: SettingsView | null;
}

const initialState: TextProcessingSettingsState = {
  loading: true,
  saving: false,
  failed: false,
  settings: null,
};

export function useTextProcessingSettings() {
  const gateway = useBackendGateway();
  const [state, setState] =
    useState<TextProcessingSettingsState>(initialState);
  const requestRevision = useRef(0);
  const savingRef = useRef(false);

  const refresh = useCallback(async () => {
    const revision = ++requestRevision.current;
    setState((current) => ({
      ...current,
      loading: current.settings === null,
      failed: false,
    }));
    try {
      const settings = await gateway.getSettings();
      if (requestRevision.current !== revision) return;
      setState({
        loading: false,
        saving: false,
        failed: false,
        settings,
      });
    } catch {
      if (requestRevision.current !== revision) return;
      setState({
        loading: false,
        saving: false,
        failed: true,
        settings: null,
      });
    }
  }, [gateway]);

  const update = useCallback(
    async (
      processingMode: ProcessingMode,
      readSelectedText: boolean,
    ) => {
      if (state.settings === null || savingRef.current) return;
      savingRef.current = true;
      const revision = ++requestRevision.current;
      setState((current) => ({ ...current, saving: true, failed: false }));
      try {
        const settings = await gateway.setTextProcessingSettings(
          state.settings.version,
          processingMode,
          readSelectedText,
        );
        if (requestRevision.current !== revision) return;
        setState({
          loading: false,
          saving: false,
          failed: false,
          settings,
        });
      } catch {
        if (requestRevision.current !== revision) return;
        setState({
          loading: false,
          saving: false,
          failed: true,
          settings: null,
        });
      } finally {
        savingRef.current = false;
      }
    },
    [gateway, state.settings],
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

  return { ...state, refresh, update };
}
