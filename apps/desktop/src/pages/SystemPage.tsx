import {
  type ReactNode,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";

import { useBackendGateway } from "@/backend/useBackendGateway";
import { userFacingGatewayError } from "@/backend/user-facing-error";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { permissionRepairAction } from "@/features/permissions/permission-actions";
import { classes } from "@/lib/classes";
import {
  CONTRACT_VERSION,
  type AppSnapshot,
  type AutostartStatusView,
  type MicrophonePermission,
  type PermissionStatusView,
  type SettingsView,
  type SystemPermission,
} from "@/lib/ipc";
import "@/styles/system-page.css";

type SystemPageState =
  | "loading"
  | "normal"
  | "permission"
  | "unavailable"
  | "error";

type SystemPreview =
  | "normal"
  | "permission"
  | "unavailable"
  | "error"
  | "paste-on"
  | "copy-on"
  | "both-on";

type SystemTone = "neutral" | "success" | "warning" | "error";
type PermissionKind = "microphone" | "accessibility";

type SystemControl =
  | {
      kind: "value";
      content: ReactNode;
    }
  | {
      kind: "switch";
      content: ReactNode;
    }
  | {
      kind: "button";
      content: ReactNode;
    };

interface SystemRowData {
  label: string;
  detail: string;
  tone: SystemTone;
  control: SystemControl;
}

interface SystemData {
  snapshot: AppSnapshot | null;
  autostart: AutostartStatusView | null;
  permissions: PermissionStatusView | null;
  settings: SettingsView | null;
}

const EMPTY_DATA: SystemData = {
  snapshot: null,
  autostart: null,
  permissions: null,
  settings: null,
};

function systemPreview(): SystemPreview | null {
  if (!import.meta.env.DEV) return null;
  const preview = new URLSearchParams(window.location.search).get("preview");
  switch (preview) {
    case "system-normal":
      return "normal";
    case "system-permission":
      return "permission";
    case "system-unavailable":
      return "unavailable";
    case "system-error":
      return "error";
    case "system-paste-on":
      return "paste-on";
    case "system-copy-on":
      return "copy-on";
    case "system-both-on":
      return "both-on";
    default:
      return null;
  }
}

function previewSnapshot(): AppSnapshot {
  return {
    contract_version: CONTRACT_VERSION,
    lifecycle_state: "ready",
    active_session: null,
    microphone_permission: "granted",
    accessibility_permission: "granted",
    asr_readiness: "qwen_ready",
    llm_configured: false,
    model_summary: {
      selected_model: "qwen",
      active_model_id: "qwen3-asr",
      qwen_ready: true,
      whisper_ready: false,
    },
    shortcut_configured: true,
    autostart_enabled: false,
  };
}

function previewPermissions(
  preview: Exclude<SystemPreview, "error" | "unavailable">,
): PermissionStatusView {
  return {
    contract_version: CONTRACT_VERSION,
    microphone: "granted",
    accessibility: preview === "permission" ? "denied" : "granted",
    app_display_name: "辑语",
    process_name: "remtene-desktop",
  };
}

function previewSettings(
  pasteEnabled: boolean,
  autoCopyEnabled: boolean,
): SettingsView {
  return {
    contract_version: CONTRACT_VERSION,
    version: 1,
    recording_mode: "toggle",
    max_recording_duration_seconds: 600,
    recording_shortcut: null,
    processing_mode: "raw",
    read_selected_text: false,
    clipboard_bridge_allowed: pasteEnabled,
    auto_copy_result: autoCopyEnabled,
    local_diagnostics_enabled: true,
    history_policy: {
      enabled: true,
      limit: 10,
      retention_days: null,
    },
    llm: null,
  };
}

function previewData(preview: SystemPreview): SystemData {
  if (preview === "error") return EMPTY_DATA;
  if (preview === "unavailable") {
    return {
      snapshot: previewSnapshot(),
      autostart: null,
      permissions: null,
      settings: null,
    };
  }
  const pasteEnabled = preview === "paste-on" || preview === "both-on";
  const autoCopyEnabled = preview === "copy-on" || preview === "both-on";
  const snapshot = previewSnapshot();
  if (preview === "permission") {
    snapshot.accessibility_permission = "denied";
  }
  return {
    snapshot,
    autostart: {
      contract_version: CONTRACT_VERSION,
      enabled: snapshot.autostart_enabled,
    },
    permissions: previewPermissions(preview),
    settings: previewSettings(pasteEnabled, autoCopyEnabled),
  };
}

function permissionReady(
  permission: MicrophonePermission | SystemPermission,
): boolean {
  return (
    permission === "granted" ||
    permission === "not_required" ||
    permission === "inherited_from_launcher"
  );
}

function permissionValue(
  permission: MicrophonePermission | SystemPermission | undefined,
): string {
  switch (permission) {
    case "granted":
      return "已允许";
    case "not_required":
      return "当前不需要";
    case "inherited_from_launcher":
      return "继承授权";
    case "denied":
      return "未开启";
    case "not_determined":
      return "尚未授权";
    default:
      return "状态未知";
  }
}

function permissionTone(
  permission: MicrophonePermission | SystemPermission | undefined,
): SystemTone {
  return permission !== undefined && permissionReady(permission)
    ? "success"
    : "warning";
}

function permissionsMatchSnapshot(data: SystemData): boolean {
  if (data.snapshot === null || data.permissions === null) return false;
  return (
    data.snapshot.microphone_permission === data.permissions.microphone &&
    data.snapshot.accessibility_permission === data.permissions.accessibility
  );
}

function deriveSystemPageState(data: SystemData): SystemPageState {
  if (
    data.snapshot === null ||
    data.permissions === null ||
    data.settings === null
  ) {
    return "unavailable";
  }
  if (
    data.snapshot.lifecycle_state !== "ready" ||
    !permissionsMatchSnapshot(data)
  ) {
    return "unavailable";
  }
  if (
    !permissionReady(data.permissions.microphone) ||
    !permissionReady(data.permissions.accessibility)
  ) {
    return "permission";
  }
  return "normal";
}

function SystemSwitch({
  label,
  checked,
  disabled,
  onCheckedChange,
}: {
  label: string;
  checked: boolean;
  disabled?: boolean;
  onCheckedChange?: (checked: boolean) => void;
}) {
  return (
    <Switch
      className="system-spec-switch"
      aria-label={label}
      checked={checked}
      disabled={disabled}
      onCheckedChange={onCheckedChange}
    />
  );
}

function valueControl(content: ReactNode): SystemControl {
  return { kind: "value", content };
}

function switchControl(content: ReactNode): SystemControl {
  return { kind: "switch", content };
}

function buttonControl(content: ReactNode): SystemControl {
  return { kind: "button", content };
}

function SystemHeader({
  action,
}: {
  action?: ReactNode;
}) {
  return (
    <header className="system-spec-header">
      <div>
        <p className="system-spec-breadcrumb">系统</p>
        <h1>系统</h1>
        <p className="system-spec-description">
          管理权限、启动方式和文字输入设置。
        </p>
      </div>
      {action}
    </header>
  );
}

function SystemStatus({
  title,
  detail,
  tone,
}: {
  title: string;
  detail: string;
  tone: Exclude<SystemTone, "neutral">;
}) {
  return (
    <section
      className="system-spec-status"
      data-tone={tone}
      role={tone === "error" ? "alert" : "status"}
      aria-live="polite"
    >
      <span className="system-spec-status-tone" aria-hidden="true" />
      <span className="system-spec-status-dot" aria-hidden="true" />
      <h2>{title}</h2>
      <p>{detail}</p>
    </section>
  );
}

function SystemRow({
  row,
}: {
  row: SystemRowData;
}) {
  return (
    <div
      className="system-spec-row"
      data-control={row.control.kind}
    >
      <span
        className="system-spec-row-dot"
        data-tone={row.tone}
        aria-hidden="true"
      />
      <span className="system-spec-row-copy">
        <strong>{row.label}</strong>
        <span>{row.detail}</span>
      </span>
      <span className="system-spec-row-control">
        {row.control.content}
      </span>
    </div>
  );
}

function SystemCard({
  title,
  subtitle,
  rows,
  compact = false,
}: {
  title: string;
  subtitle: string;
  rows: SystemRowData[];
  compact?: boolean;
}) {
  const [scrollRatio, setScrollRatio] = useState(0);

  return (
    <section
      className={classes(
        "system-spec-card",
        compact && "system-spec-card--compact",
      )}
    >
      <header>
        <h2>{title}</h2>
        <p>{subtitle}</p>
      </header>
      <div
        className="system-spec-list"
        tabIndex={0}
        aria-label={`${title}设置列表`}
        onScroll={(event) => {
          const viewport = event.currentTarget;
          const scrollRange =
            viewport.scrollHeight - viewport.clientHeight;
          setScrollRatio(
            scrollRange > 0
              ? viewport.scrollTop / scrollRange
              : 0,
          );
        }}
      >
        {rows.map((row) => (
          <SystemRow key={row.label} row={row} />
        ))}
      </div>
      <span className="system-spec-scroll-track" aria-hidden="true">
        <span
          className="system-spec-scroll-thumb"
          style={{
            transform: `translateY(${Math.round(scrollRatio * 92)}px)`,
          }}
        />
      </span>
    </section>
  );
}

function LoadingSystemPage() {
  return (
    <div
      className="system-spec-loading"
      role="status"
      aria-live="polite"
      aria-label="正在读取系统状态"
      aria-busy="true"
    >
      <div className="system-loading-header" />
      <div className="system-loading-status" />
      <div className="system-loading-columns">
        <div />
        <div />
      </div>
      <div className="system-loading-about" />
    </div>
  );
}

export function SystemPage() {
  const gateway = useBackendGateway();
  const preview = systemPreview();
  const requestRevision = useRef(0);
  const [data, setData] = useState<SystemData>(() =>
    preview === null ? EMPTY_DATA : previewData(preview),
  );
  const [pageState, setPageState] = useState<SystemPageState>(() => {
    if (preview === "error") return "error";
    if (preview === "unavailable") return "unavailable";
    if (preview !== null) return deriveSystemPageState(previewData(preview));
    return "loading";
  });
  const [busyAction, setBusyAction] = useState<
    | "open-settings"
    | "refresh"
    | "paste"
    | "auto-copy"
    | "diagnostics"
    | "logs"
    | "autostart"
    | "permission-microphone"
    | "permission-accessibility"
    | null
  >(null);
  const [operationMessage, setOperationMessage] = useState<string | null>(
    null,
  );
  const [operationTitle, setOperationTitle] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    if (preview !== null) return;
    const revision = ++requestRevision.current;
    setBusyAction("refresh");
    setOperationMessage(null);
    setOperationTitle(null);

    const [snapshotResult, permissionsResult, settingsResult, autostartResult] =
      await Promise.allSettled([
        gateway.getAppSnapshot(),
        gateway.getPermissionStatus(),
        gateway.getSettings(),
        gateway.getAutostartStatus(),
      ]);

    if (requestRevision.current !== revision) return;

    if (snapshotResult.status === "rejected") {
      setData(EMPTY_DATA);
      setPageState("error");
      setOperationMessage(null);
      setOperationTitle(null);
      setBusyAction(null);
      return;
    }

    const nextData: SystemData = {
      snapshot: snapshotResult.value,
      autostart:
        autostartResult.status === "fulfilled"
          ? autostartResult.value
          : null,
      permissions:
        permissionsResult.status === "fulfilled"
          ? permissionsResult.value
          : null,
      settings:
        settingsResult.status === "fulfilled"
          ? settingsResult.value
          : null,
    };
    setData(nextData);
    setPageState(deriveSystemPageState(nextData));
    setBusyAction(null);
  }, [gateway, preview]);

  useEffect(() => {
    if (preview !== null) return;
    let active = true;
    let stopTauriFocusListener: (() => void) | null = null;
    const reload = () => {
      if (active) void refresh();
    };
    const reloadWhenVisible = () => {
      if (document.visibilityState === "visible") reload();
    };
    reload();
    window.addEventListener("focus", reload);
    document.addEventListener("visibilitychange", reloadWhenVisible);
    try {
      void getCurrentWindow()
        .onFocusChanged(({ payload: focused }) => {
          if (active && focused) reload();
        })
        .then((stop) => {
          if (active) stopTauriFocusListener = stop;
          else stop();
        })
        .catch(() => {
          // 普通浏览器预览没有 Tauri window API；DOM 生命周期监听仍然可用。
        });
    } catch {
      // 无 Tauri globals 的浏览器可能同步失败，不影响预览。
    }
    return () => {
      active = false;
      requestRevision.current += 1;
      stopTauriFocusListener?.();
      window.removeEventListener("focus", reload);
      document.removeEventListener("visibilitychange", reloadWhenVisible);
    };
  }, [preview, refresh]);

  const openMissingPermissionSettings = async () => {
    if (preview !== null || busyAction !== null) return;
    setBusyAction("open-settings");
    setOperationMessage(null);
    setOperationTitle(null);
    try {
      if (
        data.permissions !== null &&
        !permissionReady(data.permissions.accessibility)
      ) {
        await gateway.openAccessibilitySettings();
      } else {
        await gateway.openMicrophoneSettings();
      }
    } catch (error) {
      setOperationTitle("系统设置未更新");
      setOperationMessage(
        userFacingGatewayError(
          gateway,
          error,
          "系统设置未打开，请稍后重试。",
        ),
      );
    } finally {
      setBusyAction(null);
    }
  };

  const recheckPermissions = () => {
    if (preview === null) void refresh();
  };

  const runPermissionRepair = async (kind: PermissionKind) => {
    if (
      preview !== null ||
      busyAction !== null ||
      data.permissions === null
    ) {
      return;
    }
    const permission = data.permissions[kind];
    const action = permissionRepairAction(permission);
    if (action === null) return;

    setBusyAction(`permission-${kind}`);
    setOperationMessage(null);
    setOperationTitle(null);
    try {
      if (kind === "microphone") {
        if (action === "request") {
          await gateway.requestMicrophonePermission();
          await refresh();
        } else {
          await gateway.openMicrophoneSettings();
        }
      } else if (action === "request") {
        await gateway.requestAccessibilityPermission();
        await refresh();
      } else {
        await gateway.openAccessibilitySettings();
      }
    } catch (error) {
      setOperationTitle("权限操作未完成");
      setOperationMessage(
        userFacingGatewayError(
          gateway,
          error,
          "权限操作未完成，请稍后重试或手动打开系统设置。",
        ),
      );
    } finally {
      setBusyAction(null);
    }
  };

  const setPasteEnabled = async (checked: boolean) => {
    if (preview !== null) {
      setData((current) => ({
        ...current,
        settings:
          current.settings === null
            ? null
            : {
                ...current.settings,
                version: current.settings.version + 1,
                clipboard_bridge_allowed: checked,
              },
      }));
      return;
    }
    if (
      data.settings === null ||
      busyAction !== null ||
      pageState === "error"
    ) {
      return;
    }
    setBusyAction("paste");
    setOperationMessage(null);
    setOperationTitle(null);
    try {
      const settings = await gateway.setClipboardBridgeAllowed(checked);
      setData((current) => ({ ...current, settings }));
    } catch (error) {
      setOperationTitle("兼容粘贴设置未更新");
      setOperationMessage(
        userFacingGatewayError(
          gateway,
          error,
          "兼容粘贴设置未更新，请稍后重试。",
        ),
      );
    } finally {
      setBusyAction(null);
    }
  };

  const setAutoCopyEnabled = async (checked: boolean) => {
    if (preview !== null) {
      setData((current) => ({
        ...current,
        settings:
          current.settings === null
            ? null
            : {
                ...current.settings,
                version: current.settings.version + 1,
                auto_copy_result: checked,
              },
      }));
      return;
    }
    if (data.settings === null || busyAction !== null || pageState === "error") {
      return;
    }
    setBusyAction("auto-copy");
    setOperationMessage(null);
    setOperationTitle(null);
    try {
      const settings = await gateway.setAutoCopyResult(
        data.settings.version,
        checked,
      );
      setData((current) => ({ ...current, settings }));
    } catch (error) {
      setOperationTitle("自动复制设置未更新");
      setOperationMessage(
        userFacingGatewayError(
          gateway,
          error,
          "自动复制设置未更新，请稍后重试。",
        ),
      );
    } finally {
      setBusyAction(null);
    }
  };

  const setDiagnosticsEnabled = async (checked: boolean) => {
    if (preview !== null) {
      setData((current) => ({
        ...current,
        settings:
          current.settings === null
            ? null
            : {
                ...current.settings,
                version: current.settings.version + 1,
                local_diagnostics_enabled: checked,
              },
      }));
      return;
    }
    if (data.settings === null || busyAction !== null || pageState === "error") {
      return;
    }
    setBusyAction("diagnostics");
    setOperationMessage(null);
    setOperationTitle(null);
    try {
      const settings = await gateway.setLocalDiagnosticsEnabled(
        data.settings.version,
        checked,
      );
      setData((current) => ({ ...current, settings }));
    } catch (error) {
      setOperationTitle("系统日志设置未更新");
      setOperationMessage(
        userFacingGatewayError(
          gateway,
          error,
          "系统日志设置未更新，请稍后重试。",
        ),
      );
    } finally {
      setBusyAction(null);
    }
  };

  const openDiagnosticsDirectory = async () => {
    if (preview !== null || busyAction !== null || pageState === "error") return;
    setBusyAction("logs");
    setOperationMessage(null);
    setOperationTitle(null);
    try {
      await gateway.openDiagnosticsDirectory();
    } catch (error) {
      setOperationTitle("日志文件夹未打开");
      setOperationMessage(
        userFacingGatewayError(
          gateway,
          error,
          "日志文件夹未打开，请稍后重试。",
        ),
      );
    } finally {
      setBusyAction(null);
    }
  };

  const setAutostartEnabled = async (checked: boolean) => {
    if (preview !== null) {
      setData((current) => ({
        ...current,
        autostart: {
          contract_version: CONTRACT_VERSION,
          enabled: checked,
        },
        snapshot:
          current.snapshot === null
            ? null
            : { ...current.snapshot, autostart_enabled: checked },
      }));
      return;
    }
    if (
      data.autostart === null ||
      busyAction !== null ||
      pageState === "error"
    ) {
      return;
    }

    setBusyAction("autostart");
    setOperationMessage(null);
    setOperationTitle(null);
    try {
      const autostart = await gateway.setAutostartEnabled(checked);
      setData((current) => ({
        ...current,
        autostart,
        snapshot:
          current.snapshot === null
            ? null
            : {
                ...current.snapshot,
                autostart_enabled: autostart.enabled,
              },
      }));
    } catch (error) {
      setOperationTitle("开机自动启动未更新");
      setOperationMessage(
        userFacingGatewayError(
          gateway,
          error,
          "开机自动启动未更新，请稍后重试。",
        ),
      );
      try {
        const autostart = await gateway.getAutostartStatus();
        setData((current) => ({ ...current, autostart }));
      } catch {
        setData((current) => ({ ...current, autostart: null }));
      }
    } finally {
      setBusyAction(null);
    }
  };

  if (pageState === "loading") return <LoadingSystemPage />;

  const isPreview = preview !== null;
  const pasteEnabled =
    data.settings?.clipboard_bridge_allowed ?? false;
  const copyEnabled = data.settings?.auto_copy_result ?? false;
  const diagnosticsEnabled =
    data.settings?.local_diagnostics_enabled ?? false;
  const accessibilityMissing =
    data.permissions !== null &&
    !permissionReady(data.permissions.accessibility);

  const pasteSwitch = (enabled = true) => (
    <SystemSwitch
      label="兼容粘贴"
      checked={pasteEnabled}
      disabled={!enabled || busyAction !== null}
      onCheckedChange={(checked) => void setPasteEnabled(checked)}
    />
  );
  const copySwitch = (enabled: boolean) => (
    <SystemSwitch
      label="自动复制结果"
      checked={copyEnabled}
      disabled={!enabled || busyAction !== null}
      onCheckedChange={
        enabled ? (checked) => void setAutoCopyEnabled(checked) : undefined
      }
    />
  );
  const diagnosticsSwitch = (enabled: boolean) => (
    <SystemSwitch
      label="系统日志记录"
      checked={diagnosticsEnabled}
      disabled={!enabled || busyAction !== null}
      onCheckedChange={
        enabled ? (checked) => void setDiagnosticsEnabled(checked) : undefined
      }
    />
  );
  const autostartSwitch = (
    <SystemSwitch
      label="开机自动启动"
      checked={data.autostart?.enabled ?? false}
      disabled={data.autostart === null || busyAction !== null}
      onCheckedChange={(checked) => void setAutostartEnabled(checked)}
    />
  );
  const permissionControl = (
    kind: PermissionKind,
    permission: MicrophonePermission | SystemPermission | undefined,
  ): SystemControl => {
    const action = permissionRepairAction(permission);
    if (action === null) {
      return valueControl(permissionValue(permission));
    }
    const permissionLabel = kind === "microphone" ? "麦克风" : "辅助功能";
    return buttonControl(
      <Button
        className="system-row-action"
        aria-label={
          action === "request"
            ? `请求${permissionLabel}授权`
            : `打开${permissionLabel}设置`
        }
        disabled={busyAction !== null}
        onClick={() => void runPermissionRepair(kind)}
      >
        {action === "request" ? "请求授权" : "打开设置"}
      </Button>,
    );
  };

  let headerAction: ReactNode;
  let statusTitle: string;
  let statusDetail: string;
  let statusTone: Exclude<SystemTone, "neutral">;
  let leftTitle = "系统能力";
  let leftRows: SystemRowData[];
  let rightRows: SystemRowData[];

  const sharedLeftRows: SystemRowData[] = [
    {
      label: "本地数据",
      detail: "设置和文字历史只保存在本机",
      tone: "success",
      control: valueControl("本地保存"),
    },
    {
      label: "系统日志",
      detail: "不记录文字内容，默认保留最近 3 天",
      tone: diagnosticsEnabled ? "success" : "neutral",
      control: switchControl(diagnosticsSwitch(data.settings !== null)),
    },
    {
      label: "日志文件夹",
      detail: "保存在应用缓存文件夹中",
      tone: "success",
      control: buttonControl(
        <Button
          className="system-row-action"
          variant="ghost"
          aria-label="打开日志文件夹"
          disabled={busyAction !== null || pageState === "error"}
          onClick={() => void openDiagnosticsDirectory()}
        >
          打开文件夹
        </Button>,
      ),
    },
  ];
  const sharedRightRows: SystemRowData[] = [
    {
      label: "临时文字框",
      detail: "无法写入时保留可复制文字",
      tone: "success",
      control: valueControl("自动显示"),
    },
    {
      label: "剪贴板内容",
      detail: "兼容粘贴后会恢复原内容",
      tone: "success",
      control: valueControl("自动恢复"),
    },
  ];

  if (pageState === "permission") {
    const actionLabel = accessibilityMissing
      ? "打开系统设置"
      : "打开麦克风设置";
    headerAction = (
      <Button
        className="system-header-action system-header-action--wide"
        variant="primary"
        disabled={busyAction !== null}
        onClick={() => void openMissingPermissionSettings()}
      >
        {actionLabel}
      </Button>
    );
    statusTitle = accessibilityMissing
      ? "辅助功能权限未开启"
      : "麦克风权限未开启";
    statusDetail = accessibilityMissing
      ? "仍可使用本地转录，但暂时无法直接写入文字。"
      : "允许使用麦克风后即可开始录音。";
    statusTone = "warning";
    leftTitle = "权限";
    leftRows = [
      {
        label: "麦克风",
        detail: "本地录音所需",
        tone: permissionTone(data.permissions?.microphone),
        control: permissionControl(
          "microphone",
          data.permissions?.microphone,
        ),
      },
      {
        label: "辅助功能",
        detail: "直接写入和读取选中文字时需要",
        tone: permissionTone(data.permissions?.accessibility),
        control: permissionControl(
          "accessibility",
          data.permissions?.accessibility,
        ),
      },
      {
        label: "授权范围",
        detail: "不读取整份文件，也不会持续监听",
        tone: "warning",
        control: valueControl("仅在需要时"),
      },
      {
        label: "返回后检查",
        detail: "返回后重新读取授权状态",
        tone: "success",
        control: buttonControl(
          <Button
            className="system-row-action"
            disabled={busyAction !== null}
            onClick={recheckPermissions}
          >
            重新检查
          </Button>,
        ),
      },
      ...sharedLeftRows,
    ];
    rightRows = [
      {
        label: "直接写入",
        detail: "允许辅助功能后即可使用",
        tone: "warning",
        control: valueControl("暂不可用"),
      },
      {
        label: "兼容粘贴",
        detail: "需要单独开启",
        tone: pasteEnabled ? "success" : "neutral",
        control: switchControl(pasteSwitch(data.settings !== null)),
      },
      {
        label: "自动复制结果",
        detail: "仅复制最终文字，不执行粘贴",
        tone: copyEnabled ? "success" : "neutral",
        control: switchControl(copySwitch(data.settings !== null)),
      },
      {
        label: "粘贴位置",
        detail: "使用当前输入位置，可能替换选中文字",
        tone: "warning",
        control: valueControl("请确认位置"),
      },
      ...sharedRightRows,
    ];
  } else if (pageState === "unavailable") {
    statusTitle = "部分设置暂时无法使用";
    statusDetail = "暂时无法读取或保存这些设置，请稍后重试。";
    statusTone = "warning";
    leftRows = [
      {
        label: "开机自动启动",
        detail:
          data.autostart === null
            ? "当前无法读取启动状态"
            : "启动后只进入待命，不会自动录音",
        tone: data.autostart === null ? "warning" : "neutral",
        control:
          data.autostart === null
            ? valueControl("暂不可用")
            : switchControl(autostartSwitch),
      },
      {
        label: "常驻状态",
        detail: "可从菜单栏重新打开或退出辑语",
        tone: "success",
        control: valueControl("可用"),
      },
      {
        label: "本地数据位置",
        detail: "暂不支持打开数据文件夹",
        tone: "warning",
        control: valueControl("不可用"),
      },
      {
        label: "全部本地数据清理",
        detail: "暂不支持一键清除所有数据",
        tone: "warning",
        control: valueControl("不提供"),
      },
      ...sharedLeftRows,
    ];
    rightRows = [
      {
        label: "直接写入",
        detail: "仍会优先写入原来的光标位置",
        tone: "success",
        control: valueControl("保留"),
      },
      {
        label: "兼容粘贴",
        detail:
          data.settings === null
            ? "当前无法读取或保存"
            : "直接写入失败时，粘贴到当前输入位置",
        tone:
          data.settings !== null && pasteEnabled
            ? "success"
            : "neutral",
        control: switchControl(
          pasteSwitch(data.settings !== null),
        ),
      },
      {
        label: "自动复制结果",
        detail:
          data.settings === null
            ? "当前无法读取或保存"
            : "完成后把最终文字复制到剪贴板",
        tone: copyEnabled ? "success" : "neutral",
        control: switchControl(copySwitch(data.settings !== null)),
      },
      {
        label: "设置变化",
        detail: "本次没有更改设置",
        tone: "success",
        control: valueControl("无变化"),
      },
      ...sharedRightRows,
    ];
  } else if (pageState === "error") {
    headerAction = (
      <Button
        className="system-header-action"
        disabled={busyAction !== null}
        onClick={() => void refresh()}
      >
        重新读取
      </Button>
    );
    statusTitle = "暂时无法读取系统状态";
    statusDetail =
      "没有开启麦克风，也没有修改任何设置。";
    statusTone = "error";
    leftRows = [
      {
        label: "应用状态",
        detail: "暂时无法读取",
        tone: "error",
        control: valueControl("不可用"),
      },
      {
        label: "设置修改",
        detail: "本次没有更改设置",
        tone: "success",
        control: valueControl("无变化"),
      },
      {
        label: "麦克风",
        detail: "没有自动开启",
        tone: "success",
        control: valueControl("关闭"),
      },
      {
        label: "本地模型",
        detail: "没有开始检查",
        tone: "success",
        control: valueControl("未触发"),
      },
      ...sharedLeftRows,
    ];
    rightRows = [
      {
        label: "直接写入",
        detail: "仍会优先使用",
        tone: "success",
        control: valueControl("保留"),
      },
      {
        label: "兼容粘贴",
        detail: "当前设置暂时无法读取",
        tone: "neutral",
        control: switchControl(
          <SystemSwitch
            label="兼容粘贴（状态未知）"
            checked={false}
            disabled
          />,
        ),
      },
      {
        label: "自动复制结果",
        detail: "当前设置暂时无法读取",
        tone: "neutral",
        control: switchControl(copySwitch(false)),
      },
      {
        label: "当前输入任务",
        detail: "保持原有状态",
        tone: "success",
        control: valueControl("不变"),
      },
      ...sharedRightRows,
    ];
  } else {
    if (pasteEnabled && copyEnabled) {
      statusTitle = "兼容粘贴与自动复制均已开启";
      statusDetail =
        "两项设置可以分别关闭；系统仍会优先直接写入。";
    } else if (pasteEnabled) {
      statusTitle = "兼容粘贴已开启";
      statusDetail =
        "系统仍会优先直接写入；自动复制保持关闭。";
    } else if (copyEnabled) {
      statusTitle = "自动复制结果已开启";
      statusDetail =
        "完成后的文字会复制到剪贴板；兼容粘贴保持关闭。";
    } else {
      statusTitle = "系统设置正常";
      statusDetail =
        "关闭面板后，辑语仍在菜单栏待命；待命时不会使用麦克风。";
    }
    statusTone = "success";
    leftRows = [
      {
        label: "麦克风",
        detail: "只在明确触发的录音期间开启",
        tone: permissionTone(data.permissions?.microphone),
        control: valueControl(
          permissionValue(data.permissions?.microphone),
        ),
      },
      {
        label: "辅助功能",
        detail: "直接写入和读取选中文字时需要",
        tone: permissionTone(data.permissions?.accessibility),
        control: valueControl(
          permissionValue(data.permissions?.accessibility),
        ),
      },
      {
        label: "开机自动启动",
        detail:
          data.autostart === null
            ? "当前无法读取启动状态"
            : "启动后只进入待命，不会自动录音",
        tone: data.autostart === null ? "warning" : "neutral",
        control: switchControl(autostartSwitch),
      },
      {
        label: "常驻入口",
        detail: "关闭面板后继续待命；点击菜单栏图标可重新打开",
        tone: "success",
        control: valueControl("菜单栏／系统托盘"),
      },
      ...sharedLeftRows,
    ];
    let boundaryDetail = "兼容粘贴可能替换选中的文字";
    let boundaryValue = "默认关闭";
    if (pasteEnabled && copyEnabled) {
      boundaryDetail = "可能替换选中的文字，结果也会复制";
      boundaryValue = "均已开启";
    } else if (pasteEnabled) {
      boundaryDetail = "粘贴到当前输入位置，可能替换选中的文字";
      boundaryValue = "已授权";
    } else if (copyEnabled) {
      boundaryDetail = "自动复制不会同时开启兼容粘贴";
      boundaryValue = "独立开启";
    }
    rightRows = [
      {
        label: "直接写入",
        detail: "优先写入原来的光标位置",
        tone: "success",
        control: valueControl("默认"),
      },
      {
        label: "兼容粘贴",
        detail: "直接写入失败时，粘贴到当前输入位置",
        tone: pasteEnabled ? "success" : "neutral",
        control: switchControl(pasteSwitch(true)),
      },
      {
        label: "自动复制结果",
        detail: "与兼容粘贴分开设置，默认关闭",
        tone: copyEnabled ? "success" : "neutral",
        control: switchControl(copySwitch(true)),
      },
      {
        label: "粘贴提示",
        detail: boundaryDetail,
        tone: "warning",
        control: valueControl(boundaryValue),
      },
      ...sharedRightRows,
    ];
  }

  if (operationMessage) {
    statusTitle = operationTitle ?? "系统设置未更新";
    statusDetail = operationMessage;
    statusTone = "error";
  }

  return (
    <div
      className="system-spec-page"
      data-state={pageState}
      data-preview={isPreview ? "true" : undefined}
    >
      <SystemHeader action={headerAction} />
      <SystemStatus
        title={statusTitle}
        detail={statusDetail}
        tone={statusTone}
      />
      <div className="system-spec-columns">
        <SystemCard
          title={leftTitle}
          subtitle="显示当前系统状态"
          rows={leftRows}
        />
        <SystemCard
          title="文字输入"
          subtitle="两项兼容设置彼此独立"
          rows={rightRows}
          compact
        />
      </div>
      <footer className="system-spec-about">
        关于辑语 · RemTene · 版本信息 · 使用说明 · 许可证与数据说明
      </footer>
    </div>
  );
}
