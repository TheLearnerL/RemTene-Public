import {
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";

import { PageHeader } from "@/app/PageHeader";
import { type AppDomain } from "@/app/navigation";
import { useBackendGateway } from "@/backend/useBackendGateway";
import { userFacingGatewayError } from "@/backend/user-facing-error";
import { Button } from "@/components/ui/button";
import { Feedback } from "@/components/ui/feedback";
import { SelectField } from "@/components/ui/field";
import {
  deriveRecordingPageState,
} from "@/features/recording/recording-model";
import { RecordingControls } from "@/features/recording/RecordingControls";
import {
  classifyShortcutKeyboardInput,
  formatShortcutBinding,
  isPureModifierShortcutBinding,
} from "@/features/recording/shortcut-binding";
import { useRecordingSettings } from "@/features/recording/useRecordingSettings";
import { useStatusDashboard } from "@/features/status/useStatusDashboard";
import { classes } from "@/lib/classes";
import {
  type AsrReadiness,
  type MicrophonePermission,
  type PermissionStatusView,
  type RecordingMode,
  type SessionPublicSnapshot,
  type SystemPermission,
} from "@/lib/ipc";

const RECORDING_DURATION_OPTIONS = [
  { value: "180", label: "3 分钟" },
  { value: "300", label: "5 分钟" },
  { value: "600", label: "10 分钟" },
  { value: "1200", label: "20 分钟" },
] as const;

type RecordingTone =
  | "neutral"
  | "accent"
  | "success"
  | "warning"
  | "error"
  | "processing";

type RecordingPreviewState =
  | "idle-unbound"
  | "hold-selected"
  | "shortcut-conflict"
  | "shortcut-ready"
  | "permission-denied"
  | "busy-processing";

interface RecordingRowProps {
  label: string;
  detail: string;
  trailing?: string;
  tone?: RecordingTone;
  selected?: boolean;
  onClick?: () => void;
}

function recordingPreviewState(): RecordingPreviewState | null {
  if (!import.meta.env.DEV) return null;
  const preview = new URLSearchParams(window.location.search).get("preview");
  switch (preview) {
    case "recording-idle":
      return "idle-unbound";
    case "recording-hold":
      return "hold-selected";
    case "recording-shortcut-conflict":
      return "shortcut-conflict";
    case "recording-shortcut-ready":
      return "shortcut-ready";
    case "recording-permission":
      return "permission-denied";
    case "recording-busy":
      return "busy-processing";
    default:
      return null;
  }
}

function RecordingHeader({
  description,
  action,
}: {
  description: string;
  action: ReactNode;
}) {
  return (
    <header className="recording-spec-header" aria-live="polite">
      <div>
        <p className="recording-spec-breadcrumb">录音</p>
        <h1>录音</h1>
        <p className="recording-spec-description">{description}</p>
      </div>
      {action}
    </header>
  );
}

function RecordingStatus({
  title,
  detail,
  tone,
}: {
  title: string;
  detail: string;
  tone: Exclude<RecordingTone, "neutral" | "accent">;
}) {
  return (
    <section
      className="recording-status"
      data-tone={tone}
      role={tone === "error" ? "alert" : "status"}
      aria-live="polite"
    >
      <span className="recording-status-tone" aria-hidden="true" />
      <span className="recording-status-dot" aria-hidden="true" />
      <h2>{title}</h2>
      <p>{detail}</p>
    </section>
  );
}

function RecordingCard({
  title,
  subtitle,
  children,
  className,
}: {
  title: string;
  subtitle: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <section className={classes("recording-card", className)}>
      <header>
        <h2>{title}</h2>
        <p>{subtitle}</p>
      </header>
      {children}
    </section>
  );
}

function RecordingRow({
  label,
  detail,
  trailing,
  tone = "neutral",
  selected,
  onClick,
}: RecordingRowProps) {
  const content = (
    <>
      <span
        className="recording-row-dot"
        data-tone={tone}
        aria-hidden="true"
      />
      <span className="recording-row-copy">
        <strong>{label}</strong>
        <span>{detail}</span>
      </span>
      {trailing ? (
        <span className="recording-row-trailing">{trailing}</span>
      ) : null}
    </>
  );
  const className = classes(
    "recording-row",
    selected === true && "recording-row--selected",
  );
  const shared = {
    className,
    "data-has-trailing": trailing ? "true" : "false",
  };

  return onClick ? (
    <button
      type="button"
      {...shared}
      aria-pressed={selected}
      onClick={onClick}
    >
      {content}
    </button>
  ) : (
    <div {...shared}>{content}</div>
  );
}

function ShortcutEditor({
  value,
  status,
  onClick,
}: {
  value: string;
  status: string;
  onClick?: () => void;
}) {
  const field = (
    <span className="recording-shortcut-field-value">{value}</span>
  );
  return (
    <div className="recording-shortcut-editor">
      <span className="recording-shortcut-label">新的组合</span>
      {onClick ? (
        <button
          type="button"
          className="recording-shortcut-field"
          onClick={onClick}
        >
          {field}
        </button>
      ) : (
        <div className="recording-shortcut-field">{field}</div>
      )}
      <span className="recording-shortcut-status">{status}</span>
    </div>
  );
}

function RecordingShortcutEditor({
  current,
  active,
  saving,
  onSave,
}: {
  current: string | null;
  active: boolean;
  saving: boolean;
  onSave: (binding: string | null) => void;
}) {
  const [draft, setDraft] = useState<string | null>(current);
  const [capturing, setCapturing] = useState(false);
  const [pendingModifier, setPendingModifier] = useState<string | null>(null);
  const [captureMessage, setCaptureMessage] = useState<string | null>(null);
  const fieldRef = useRef<HTMLInputElement>(null);
  const pendingModifierRef = useRef<string | null>(null);

  const resetPendingModifier = useCallback(() => {
    pendingModifierRef.current = null;
    setPendingModifier(null);
  }, []);

  const stopCapture = useCallback(() => {
    setCapturing(false);
    setCaptureMessage(null);
    resetPendingModifier();
    fieldRef.current?.blur();
  }, [resetPendingModifier]);

  const captureKeyDown = useCallback((
    event: ReactKeyboardEvent<HTMLInputElement> | globalThis.KeyboardEvent,
  ) => {
    if (event.defaultPrevented) return;
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      setDraft(current);
      stopCapture();
      return;
    }
    if (event.key === "Tab") {
      setCapturing(false);
      setCaptureMessage(null);
      resetPendingModifier();
      return;
    }
    if (event.repeat) {
      event.preventDefault();
      return;
    }

    const result = classifyShortcutKeyboardInput(event);
    switch (result.kind) {
      case "accepted":
        event.preventDefault();
        event.stopPropagation();
        setDraft(result.binding);
        stopCapture();
        return;
      case "modifier_candidate":
        event.preventDefault();
        event.stopPropagation();
        pendingModifierRef.current = result.binding;
        setPendingModifier(result.binding);
        setCaptureMessage(null);
        return;
      case "rejected_daily_key":
        event.preventDefault();
        event.stopPropagation();
        resetPendingModifier();
        setCaptureMessage("这个键不能单独使用，请搭配修饰键。");
        return;
      case "waiting":
        event.preventDefault();
        event.stopPropagation();
        resetPendingModifier();
        return;
    }
  }, [current, resetPendingModifier, stopCapture]);

  const captureKeyUp = useCallback((
    event: ReactKeyboardEvent<HTMLInputElement> | globalThis.KeyboardEvent,
  ) => {
    if (event.defaultPrevented || pendingModifierRef.current !== event.code) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    setDraft(pendingModifierRef.current);
    stopCapture();
  }, [stopCapture]);

  useEffect(() => {
    if (!capturing) return;
    window.addEventListener("keydown", captureKeyDown, true);
    window.addEventListener("keyup", captureKeyUp, true);
    return () => {
      window.removeEventListener("keydown", captureKeyDown, true);
      window.removeEventListener("keyup", captureKeyUp, true);
    };
  }, [captureKeyDown, captureKeyUp, capturing]);

  const beginCapture = () => {
    setCaptureMessage(null);
    resetPendingModifier();
    setCapturing(true);
    window.requestAnimationFrame(() => fieldRef.current?.focus());
  };

  const changed = draft !== current;
  const shortcutHelp = captureMessage
    ?? (pendingModifier
      ? "继续按其他键可组成组合键，直接松开则使用单独修饰键。"
      : !capturing && isPureModifierShortcutBinding(draft)
        ? `每次按下“${formatShortcutBinding(draft)}”都会触发录音，建议使用不常用的右侧键。`
        : "支持组合键、F1–F20 和单独修饰键。输入键需搭配修饰键，Esc 取消。");
  return (
    <div className="recording-binding-editor">
      <div className="recording-binding-heading">
        <div>
          <strong>全局快捷键</strong>
          <span>点击后按下快捷键</span>
        </div>
        <span data-tone={current ? "success" : "warning"}>
          {current ? "已绑定" : "未绑定"}
        </span>
      </div>
      <input
        ref={fieldRef}
        type="text"
        readOnly
        className="recording-shortcut-field recording-shortcut-capture"
        data-capturing={capturing ? "true" : "false"}
        disabled={saving}
        aria-label="录入新的全局快捷键"
        aria-describedby="recording-shortcut-help"
        value={
          capturing
            ? pendingModifier
              ? `松开以使用 ${formatShortcutBinding(pendingModifier)}`
              : "请按快捷键…"
            : formatShortcutBinding(draft)
        }
        onClick={beginCapture}
        onFocus={() => setCapturing(true)}
        onKeyDown={captureKeyDown}
        onKeyUp={captureKeyUp}
        onBlur={() => {
          setCapturing(false);
          resetPendingModifier();
        }}
      />
      <span id="recording-shortcut-help" className="recording-binding-help">
        {shortcutHelp}
      </span>
      <div className="recording-binding-actions">
        <Button
          size="compact"
          variant="primary"
          disabled={saving || draft === null || (!changed && active)}
          onClick={() => onSave(draft)}
        >
          {saving
            ? "保存中…"
            : draft !== null && !changed && !active
              ? "重新注册"
              : "保存"}
        </Button>
        <Button
          size="compact"
          disabled={saving || current === null}
          onClick={() => onSave(null)}
        >
          清除
        </Button>
      </div>
    </div>
  );
}

function RecordingLoadingState({
  label,
}: {
  label: string;
}) {
  return (
    <div
      className="recording-spec-loading"
      role="status"
      aria-live="polite"
      aria-busy="true"
      aria-label={label}
    >
      <div className="recording-loading-header" />
      <div className="recording-loading-status" />
      <div className="recording-loading-columns">
        <div />
        <div />
      </div>
    </div>
  );
}

function IdleRecordingView({
  recordingMode,
  recordingLimitSeconds,
  recordingShortcut,
  shortcutConfigured,
  starting,
  saving,
  onStart,
  onSelectMode,
  onSelectDuration,
  onSaveShortcut,
}: {
  recordingMode: RecordingMode;
  recordingLimitSeconds: number;
  recordingShortcut: string | null;
  shortcutConfigured: boolean;
  starting: boolean;
  saving: boolean;
  onStart: () => void;
  onSelectMode: (mode: RecordingMode) => void;
  onSelectDuration: (seconds: number) => void;
  onSaveShortcut: (binding: string | null) => void;
}) {
  const shortcutSavedButInactive =
    recordingShortcut !== null && !shortcutConfigured;
  return (
    <div
      className="recording-spec-page"
      data-state={shortcutConfigured ? "idle-bound" : "idle-unbound"}
    >
      <RecordingHeader
        description="选择录音方式、时长和快捷键。"
        action={
          <Button
            className="recording-header-action recording-header-action--wide"
            variant="primary"
            disabled={starting || saving}
            onClick={onStart}
          >
            {starting ? "正在开始…" : "开始录音"}
          </Button>
        }
      />
      <RecordingStatus
        title={
          shortcutSavedButInactive
            ? "快捷键需要重新确认"
            : "可以开始本地输入"
        }
        detail={
          shortcutSavedButInactive
            ? "保存的快捷键当前不可用，请重新设置。"
            : shortcutConfigured
            ? "快捷键已设置，也可以使用面板开始录音。"
            : "尚未设置快捷键，仍可使用面板开始录音。"
        }
        tone={shortcutSavedButInactive ? "warning" : "success"}
      />
      <div className="recording-columns">
        <RecordingCard
          title="录音方式"
          subtitle="选择最顺手的操作方式"
        >
          <div className="recording-settings-controls">
            <SelectField
              id="recording-mode"
              label="录音模式"
              description="按一下开始和结束，或按住说话、放开结束。"
              value={recordingMode}
              disabled={saving}
              options={[
                { value: "toggle", label: "按一下说话" },
                { value: "push_to_talk", label: "按住说话" },
              ]}
              onChange={(event) =>
                onSelectMode(event.target.value as RecordingMode)
              }
            />
            <SelectField
              id="recording-duration"
              label="单次录音上限"
              description="达到上限后会自动结束。"
              value={String(recordingLimitSeconds)}
              disabled={saving}
              options={[...RECORDING_DURATION_OPTIONS]}
              onChange={(event) =>
                onSelectDuration(Number(event.target.value))
              }
            />
            <div
              className="recording-passive-status"
              role="status"
              aria-label="麦克风权限已允许"
            >
              <span className="recording-passive-status-dot" aria-hidden="true" />
              <span>
                <strong>麦克风可以使用</strong>
                <small>只会在录音时开启。</small>
              </span>
              <span className="recording-passive-status-value">可用</span>
            </div>
          </div>
        </RecordingCard>
        <RecordingCard
          title="快捷键"
          subtitle="设置后可在任意应用开始录音"
        >
          <RecordingShortcutEditor
            key={recordingShortcut ?? "unbound"}
            current={recordingShortcut}
            active={shortcutConfigured}
            saving={saving}
            onSave={onSaveShortcut}
          />
          <div className="recording-rows recording-rows--passive">
            <RecordingRow
              label="快捷键状态"
              detail={
                shortcutConfigured
                  ? "当前快捷键可以使用"
                  : "还没有可用的快捷键"
              }
              trailing={shortcutConfigured ? "可用" : "未设置"}
              tone={shortcutConfigured ? "success" : "warning"}
            />
            <RecordingRow
              label="输入设备"
              detail="使用系统当前默认麦克风"
              trailing="系统默认"
            />
            <RecordingRow
              label="音频处理"
              detail="音频只在本机处理"
              trailing="仅本机"
              tone="success"
            />
          </div>
        </RecordingCard>
      </div>
    </div>
  );
}

function ShortcutRecordingView({
  ready,
  preview,
  onToggleValidation,
  onSave,
}: {
  ready: boolean;
  preview: boolean;
  onToggleValidation: () => void;
  onSave: () => void;
}) {
  return (
    <div
      className="recording-spec-page"
      data-state={ready ? "shortcut-ready" : "shortcut-conflict"}
    >
      <RecordingHeader
        description="选择一个不会和其他应用冲突的组合键。"
        action={
          <Button
            className="recording-header-action"
            disabled={!ready}
            onClick={ready ? onSave : undefined}
          >
            保存快捷键
          </Button>
        }
      />
      <RecordingStatus
        title={ready ? "快捷键可以保存" : "快捷键未保存"}
        detail={
          ready
            ? "该组合未发现冲突；保存后可在任意应用开始与结束录音。"
            : "该组合已被其他应用使用，请换一个组合后再试。"
        }
        tone="warning"
      />
      <div className="recording-columns">
        <RecordingCard
          className="recording-card--shortcut"
          title="录入快捷键"
          subtitle="已有组合不会被自动替换"
        >
          <ShortcutEditor
            value={ready ? "⌥ Space" : "等待按键…"}
            status={ready ? "未发现冲突" : "检测到系统或应用冲突"}
            onClick={preview ? onToggleValidation : undefined}
          />
          <div className="recording-rows recording-rows--shortcut">
            <RecordingRow
              label={ready ? "组合可用" : "组合不可用"}
              detail={ready ? "可保存为全局快捷键" : "请按下其他组合"}
              trailing={ready ? "可用" : "冲突"}
              tone="warning"
            />
            <RecordingRow
              label="取消录入"
              detail="取消后不会保存"
              trailing="不更改"
              tone="success"
            />
          </div>
        </RecordingCard>
        <RecordingCard
          title="保存状态"
          subtitle="冲突必须先解决"
        >
          <div className="recording-rows">
            <RecordingRow
              label="当前快捷键"
              detail="首次安装不提供默认值"
              trailing={ready ? "⌥ Space" : "未绑定"}
              tone="warning"
            />
            <RecordingRow
              label="冲突检测"
              detail="保存前必须通过"
              trailing={ready ? "通过" : "失败"}
              tone="warning"
            />
            <RecordingRow
              label="保存状态"
              detail={
                ready
                  ? "当前组合可以保存"
                  : "请先解决快捷键冲突"
              }
              trailing={ready ? "可保存" : "不可用"}
              tone="warning"
            />
            <RecordingRow
              label="取消录入"
              detail="不会更改当前设置"
              trailing="可关闭"
              tone="success"
            />
          </div>
        </RecordingCard>
      </div>
    </div>
  );
}

function PermissionDeniedView({
  onRepair,
}: {
  onRepair: () => void;
}) {
  return (
    <div className="recording-spec-page" data-state="permission-denied">
      <RecordingHeader
        description="允许使用麦克风后即可继续。"
        action={
          <Button
            className="recording-header-action recording-header-action--wide"
            variant="primary"
            onClick={onRepair}
          >
            打开系统设置
          </Button>
        }
      />
      <RecordingStatus
        title="麦克风权限未开启"
        detail="没有开始录音；开启权限后返回并重新检查。"
        tone="error"
      />
      <div className="recording-columns">
        <RecordingCard
          title="权限状态"
          subtitle="显示当前系统授权结果"
        >
          <div className="recording-rows">
            <RecordingRow
              label="麦克风"
              detail="录音所需"
              trailing="已拒绝"
              tone="error"
            />
            <RecordingRow
              label="辅助功能"
              detail="精确写入所需"
              trailing="已允许"
              tone="success"
            />
            <RecordingRow
              label="当前任务"
              detail="没有开启麦克风"
              trailing="未开始"
              tone="success"
            />
            <RecordingRow
              label="本地模型"
              detail="权限修复后可继续"
              trailing="可用"
              tone="success"
            />
          </div>
        </RecordingCard>
        <RecordingCard
          title="恢复步骤"
          subtitle="开启后返回重新检查"
        >
          <div className="recording-rows">
            <RecordingRow
              label="1. 打开设置"
              detail="进入系统隐私设置"
              trailing="现在"
              tone="accent"
            />
            <RecordingRow
              label="2. 允许麦克风"
              detail="只授权辑语"
            />
            <RecordingRow
              label="3. 返回检查"
              detail="状态不会假定成功"
              trailing="重新检查"
              tone="accent"
            />
            <RecordingRow
              label="隐私边界"
              detail="音频不会发送到远端"
              trailing="本地"
              tone="success"
            />
          </div>
        </RecordingCard>
      </div>
    </div>
  );
}

function permissionLabel(
  permission: MicrophonePermission | SystemPermission,
): string {
  switch (permission) {
    case "granted":
      return "已允许";
    case "denied":
      return "已拒绝";
    case "not_determined":
      return "尚未授权";
    case "not_required":
      return "当前不需要";
    case "inherited_from_launcher":
      return "继承授权";
    default:
      return "状态未知";
  }
}

function permissionTone(
  permission: MicrophonePermission | SystemPermission,
): RecordingTone {
  return permission === "granted" || permission === "not_required"
    ? "success"
    : "warning";
}

function PermissionRequiredView({
  permissions,
  asrReadiness,
  onRepair,
  onModel,
}: {
  permissions: PermissionStatusView;
  asrReadiness: AsrReadiness;
  onRepair: () => void;
  onModel: () => void;
}) {
  const modelUnavailable = asrReadiness === "unavailable";
  const modelReady =
    asrReadiness === "qwen_ready" || asrReadiness === "whisper_ready";
  return (
    <div className="recording-spec-page" data-state="permission-required">
      <RecordingHeader
        description="修复麦克风权限后继续使用本地转录。"
        action={
          <Button
            className="recording-header-action recording-header-action--wide"
            variant="primary"
            onClick={onRepair}
          >
            前往系统
          </Button>
        }
      />
      <RecordingStatus
        title="麦克风权限未开启"
        detail="没有开始录音；开启权限后返回并重新检查。"
        tone="warning"
      />
      <div className="recording-columns">
        <RecordingCard
          title="权限状态"
          subtitle="显示当前系统授权结果"
        >
          <div className="recording-rows">
            <RecordingRow
              label="麦克风"
              detail="本地录音所需"
              trailing={permissionLabel(permissions.microphone)}
              tone={permissionTone(permissions.microphone)}
            />
            <RecordingRow
              label="辅助功能"
              detail="直接写入和读取选中的文字时需要"
              trailing={permissionLabel(permissions.accessibility)}
              tone={permissionTone(permissions.accessibility)}
            />
            <RecordingRow
              label="当前任务"
              detail="没有开启麦克风"
              trailing="未开始"
              tone="success"
            />
            <RecordingRow
              label="本地模型"
              detail={
                modelUnavailable
                  ? "本地模型未准备好"
                  : modelReady
                    ? "权限修复后可继续"
                    : "还没有完成检查"
              }
              trailing={
                modelUnavailable ? "不可用" : modelReady ? "可用" : "尚未检查"
              }
              tone={modelUnavailable ? "error" : modelReady ? "success" : "warning"}
            />
          </div>
        </RecordingCard>
        <RecordingCard
          title="恢复步骤"
          subtitle="修复后返回重新检查"
        >
          <div className="recording-rows">
            <RecordingRow
              label="允许麦克风"
              detail="只在明确录音期间开启"
              trailing="前往系统"
              tone="accent"
              onClick={onRepair}
            />
            <RecordingRow
              label="辅助功能"
              detail="直接写入和读取选中的文字"
              trailing="前往系统"
              tone="warning"
              onClick={onRepair}
            />
            {modelUnavailable ? (
              <RecordingRow
                label="检查模型"
                detail="本地语音转文字暂不可用"
                trailing="前往模型"
                tone="error"
                onClick={onModel}
              />
            ) : (
            <RecordingRow
              label="返回检查"
              detail="重新读取授权状态"
                trailing="重新检查"
                tone="accent"
              />
            )}
            <RecordingRow
              label="音频处理"
              detail="音频只在本机处理"
              trailing="本地"
              tone="success"
            />
          </div>
        </RecordingCard>
      </div>
    </div>
  );
}

function AsrUnavailableView({ onModel }: { onModel: () => void }) {
  return (
    <div className="recording-spec-page" data-state="asr-unavailable">
      <RecordingHeader
        description="本地模型准备好后即可继续录音。"
        action={
          <Button
            className="recording-header-action recording-header-action--wide"
            variant="primary"
            onClick={onModel}
          >
            检查模型
          </Button>
        }
      />
      <RecordingStatus
        title="本地语音转文字暂不可用"
        detail="本地模型未准备好。音频没有离开本机。"
        tone="error"
      />
      <div className="recording-columns">
        <RecordingCard
          title="本地识别模型"
          subtitle="查看模型是否可以使用"
        >
          <div className="recording-rows">
            <RecordingRow
              label="Qwen3-ASR"
              detail="本地模型状态"
              trailing="未就绪"
              tone="error"
            />
            <RecordingRow
              label="Whisper"
              detail="本地模型状态"
              trailing="未就绪"
              tone="error"
            />
            <RecordingRow
              label="当前任务"
              detail="没有开启麦克风"
              trailing="未开始"
              tone="success"
            />
            <RecordingRow
              label="音频处理"
              detail="不会上传到远端"
              trailing="仅本机"
              tone="success"
            />
          </div>
        </RecordingCard>
        <RecordingCard
          title="恢复步骤"
          subtitle="重新检查本地模型"
        >
          <div className="recording-rows">
            <RecordingRow
              label="检查模型"
              detail="检查模型和本地转录服务"
              trailing="前往模型"
              tone="accent"
              onClick={onModel}
            />
            <RecordingRow
              label="模型文件"
              detail="当前状态"
              trailing="暂无信息"
            />
            <RecordingRow
              label="本地转录服务"
              detail="当前状态"
              trailing="暂无信息"
            />
            <RecordingRow
              label="当前操作"
              detail="不会打开麦克风或写入文字"
              trailing="未开始"
              tone="success"
            />
          </div>
        </RecordingCard>
      </div>
    </div>
  );
}

function RecordingErrorView({
  detail,
  retrying,
  onRetry,
}: {
  detail: string;
  retrying: boolean;
  onRetry: () => void;
}) {
  return (
    <div className="recording-spec-page" data-state="error">
      <RecordingHeader
        description="请重新读取后再继续。现有设置不会改变。"
        action={
          <Button
            className="recording-header-action recording-header-action--wide"
            variant="primary"
            disabled={retrying}
            aria-busy={retrying}
            onClick={onRetry}
          >
            {retrying ? "正在读取…" : "重新读取"}
          </Button>
        }
      />
      <RecordingStatus
        title="状态暂时无法读取"
        detail={detail}
        tone="error"
      />
      <div className="recording-columns">
        <RecordingCard
          title="为避免误操作"
          subtitle="读取恢复前不会自动执行任何操作"
        >
          <div className="recording-rows">
            <RecordingRow
              label="录音"
              detail="不自动开始"
              trailing="未开始"
              tone="success"
            />
            <RecordingRow
              label="输出"
              detail="不自动写入"
              trailing="未写入"
              tone="success"
            />
            <RecordingRow
              label="当前任务"
              detail="没有开启麦克风"
              trailing="未开始"
              tone="success"
            />
            <RecordingRow
              label="重新读取"
              detail="再次检查当前状态"
              trailing="重试"
              tone="accent"
              onClick={onRetry}
            />
          </div>
        </RecordingCard>
        <RecordingCard
          title="现有内容不受影响"
          subtitle="读取失败不会更改设置或文字"
        >
          <div className="recording-rows">
            <RecordingRow
              label="麦克风"
              detail="不会自动打开"
              trailing="关闭"
              tone="success"
            />
            <RecordingRow
              label="选取文字"
              detail="不会读取任何内容"
              trailing="关闭"
              tone="success"
            />
            <RecordingRow
              label="本地设置"
              detail="不会自动更改"
              trailing="不变"
              tone="success"
            />
            <RecordingRow
              label="文字输出"
              detail="不会自动写入"
              trailing="未写入"
              tone="success"
            />
          </div>
        </RecordingCard>
      </div>
    </div>
  );
}

function ActiveTaskView({ snapshot }: { snapshot: SessionPublicSnapshot }) {
  const phaseLabel: Record<SessionPublicSnapshot["phase"], string> = {
    preparing: "准备中",
    recording: "录音中",
    recognizing: "本地转录",
    processing: "文字整理",
    delivering: "正在写入",
    finalizing: "正在完成",
    terminated: "已结束",
  };
  return (
    <div className="recording-spec-page" data-state="active-task">
      <RecordingHeader
        description="本次输入完成前不能开始新的录音。"
        action={
          <Button className="recording-header-action" disabled>
            任务进行中
          </Button>
        }
      />
      <RecordingStatus
        title="任务进行中"
        detail="本次使用的设置不会中途改变。"
        tone="processing"
      />
      <div className="recording-columns">
        <RecordingCard
          title="当前阶段"
          subtitle="一次只处理一段录音"
        >
          <div className="recording-rows">
            <RecordingRow
              label="当前任务"
              detail={phaseLabel[snapshot.phase]}
              trailing="进行中"
              tone="processing"
            />
            <RecordingRow
              label="新录音"
              detail="当前任务完成前不会开始"
              trailing="等待"
              tone="warning"
            />
            <RecordingRow
              label="处理设置"
              detail="本次使用的设置已固定"
              trailing="不变"
              tone="success"
            />
            <RecordingRow
              label="麦克风"
              detail="只在明确录音期间开启"
              trailing={snapshot.user_state === "recording" ? "开启" : "关闭"}
              tone={snapshot.user_state === "recording" ? "accent" : "success"}
            />
          </div>
        </RecordingCard>
        <RecordingCard
          title="完成前请稍候"
          subtitle="设置和新录音暂时不可用"
        >
          <div className="recording-rows">
            <RecordingRow
              label="新录音"
              detail="完成前不会开始"
              trailing="等待"
              tone="success"
            />
            <RecordingRow
              label="输出"
              detail="完成后才会写入文字"
              trailing="等待"
              tone="warning"
            />
            <RecordingRow
              label="既有文字"
              detail="不会被当前状态更改"
              trailing="不变"
              tone="success"
            />
            <RecordingRow
              label="本地处理"
              detail="音频不会发送到远端"
              trailing="仅本机"
              tone="success"
            />
          </div>
        </RecordingCard>
      </div>
    </div>
  );
}

function BusyProcessingView() {
  return (
    <div className="recording-spec-page" data-state="busy-processing">
      <RecordingHeader
        description="本次输入完成前不能开始新的录音。"
        action={
          <Button className="recording-header-action" disabled>
            任务进行中
          </Button>
        }
      />
      <RecordingStatus
        title="正在处理上一段输入"
        detail="本次使用的设置不会中途改变。"
        tone="processing"
      />
      <div className="recording-columns">
        <RecordingCard
          title="当前阶段"
          subtitle="一次只处理一段录音"
        >
          <div className="recording-rows">
            <RecordingRow
              label="录音已结束"
              detail="麦克风已经关闭"
              trailing="完成"
              tone="success"
            />
            <RecordingRow
              label="本地转录"
              detail="已生成中间文字"
              trailing="完成"
              tone="success"
            />
            <RecordingRow
              label="文字整理"
              detail="正在处理"
              trailing="当前"
              tone="processing"
            />
            <RecordingRow
              label="写入文字"
              detail="尚未开始"
              trailing="等待"
              tone="warning"
            />
          </div>
        </RecordingCard>
        <RecordingCard
          title="本次使用的设置"
          subtitle="完成前不会更改"
        >
          <div className="recording-rows">
            <RecordingRow
              label="处理模式"
              detail="开始录音时已确定"
              trailing="忠实整理"
            />
            <RecordingRow
              label="本地模型"
              detail="开始录音时已确定"
              trailing="Qwen3-ASR"
            />
            <RecordingRow
              label="新的录音"
              detail="完成前不会开始"
              trailing="等待"
              tone="warning"
            />
            <RecordingRow
              label="处理取消"
              detail="当前不能取消文字处理"
              trailing="不可用"
              tone="warning"
            />
          </div>
        </RecordingCard>
      </div>
    </div>
  );
}

function RecordingPreviewPage({
  initialState,
  onNavigate,
}: {
  initialState: RecordingPreviewState;
  onNavigate: (domain: AppDomain) => void;
}) {
  const [previewState, setPreviewState] = useState(initialState);

  switch (previewState) {
    case "hold-selected":
      return (
        <IdleRecordingView
          recordingMode="push_to_talk"
          recordingLimitSeconds={600}
          recordingShortcut={null}
          shortcutConfigured={false}
          starting={false}
          saving={false}
          onStart={() => undefined}
          onSelectMode={(mode) =>
            setPreviewState(
              mode === "toggle" ? "idle-unbound" : "hold-selected",
            )
          }
          onSelectDuration={() => undefined}
          onSaveShortcut={() => undefined}
        />
      );
    case "shortcut-conflict":
    case "shortcut-ready":
      return (
        <ShortcutRecordingView
          ready={previewState === "shortcut-ready"}
          preview
          onToggleValidation={() =>
            setPreviewState((current) =>
              current === "shortcut-ready"
                ? "shortcut-conflict"
                : "shortcut-ready",
            )
          }
          onSave={() => onNavigate("status")}
        />
      );
    case "permission-denied":
      return <PermissionDeniedView onRepair={() => onNavigate("system")} />;
    case "busy-processing":
      return <BusyProcessingView />;
    default:
      return (
        <IdleRecordingView
          recordingMode="toggle"
          recordingLimitSeconds={600}
          recordingShortcut={null}
          shortcutConfigured={false}
          starting={false}
          saving={false}
          onStart={() => undefined}
          onSelectMode={(mode) =>
            setPreviewState(
              mode === "toggle" ? "idle-unbound" : "hold-selected",
            )
          }
          onSelectDuration={() => undefined}
          onSaveShortcut={() => undefined}
        />
      );
  }
}

function ProductionRecordingPage({
  onNavigate,
}: {
  onNavigate: (domain: AppDomain) => void;
}) {
  const gateway = useBackendGateway();
  const dashboard = useStatusDashboard();
  const recordingSettings = useRecordingSettings();
  const [starting, setStarting] = useState(false);
  const [retrying, setRetrying] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const pageState = deriveRecordingPageState(
    dashboard.snapshot,
    dashboard.permissions,
    dashboard.loading,
    dashboard.failed,
  );
  const activeSession = dashboard.snapshot?.active_session ?? null;

  const retry = async () => {
    if (retrying) return;
    setRetrying(true);
    try {
      await Promise.all([dashboard.refresh(), recordingSettings.refresh()]);
    } finally {
      setRetrying(false);
    }
  };

  const startRecording = async () => {
    if (starting || activeSession) return;
    setStarting(true);
    setActionError(null);
    try {
      await gateway.startSession();
      await dashboard.refresh();
    } catch (error) {
      setActionError(
        userFacingGatewayError(
          gateway,
          error,
          "录音未开始，请检查麦克风权限后重试。",
        ),
      );
      await dashboard.refresh();
    } finally {
      setStarting(false);
    }
  };

  const saveRecordingShortcut = async (binding: string | null) => {
    await recordingSettings.updateShortcut(binding);
    // 设置命令同时替换系统全局注册。主动读取 Core 快照，避免 WebKit
    // 偶发错过同一命令末尾发出的 snapshot event 后继续显示旧运行时状态。
    await dashboard.refresh();
  };

  if (pageState === "loading") {
    return <RecordingLoadingState label="正在读取录音状态" />;
  }

  if (pageState === "error") {
    return (
      <RecordingErrorView
        detail={
          dashboard.errorMessage ??
          "暂时无法读取录音状态，请稍后重试。"
        }
        retrying={retrying}
        onRetry={() => void retry()}
      />
    );
  }

  if (pageState === "active") {
    if (
      activeSession &&
      (activeSession.can_finish || activeSession.can_cancel)
    ) {
      return (
        <>
          <PageHeader
            eyebrow="录音"
            title="录音与触发"
            description="麦克风只会在你开始录音后开启。"
          />
          <RecordingControls
            snapshot={activeSession}
            onSettled={dashboard.refresh}
          />
        </>
      );
    }
    return activeSession ? <ActiveTaskView snapshot={activeSession} /> : null;
  }

  if (pageState === "permission-required") {
    return (
      <PermissionRequiredView
        permissions={dashboard.permissions!}
        asrReadiness={dashboard.snapshot!.asr_readiness}
        onRepair={() => onNavigate("system")}
        onModel={() => onNavigate("model")}
      />
    );
  }

  if (pageState === "asr-unavailable") {
    return <AsrUnavailableView onModel={() => onNavigate("model")} />;
  }

  if (recordingSettings.loading) {
    return <RecordingLoadingState label="正在读取录音设置" />;
  }

  if (recordingSettings.failed || recordingSettings.settings === null) {
    return (
      <RecordingErrorView
        detail={
          recordingSettings.errorMessage ??
          "无法读取录音设置，请稍后重试。"
        }
        retrying={retrying}
        onRetry={() => void retry()}
      />
    );
  }

  const persistedRecordingSettings = recordingSettings.settings;
  const view = (
    <IdleRecordingView
      recordingMode={persistedRecordingSettings.recording_mode}
      recordingLimitSeconds={
        persistedRecordingSettings.max_recording_duration_seconds
      }
      recordingShortcut={persistedRecordingSettings.recording_shortcut}
      shortcutConfigured={dashboard.snapshot?.shortcut_configured === true}
      starting={starting}
      saving={recordingSettings.saving}
      onStart={() => void startRecording()}
      onSelectMode={(mode) =>
        void recordingSettings.updatePreferences(
          mode,
          persistedRecordingSettings.max_recording_duration_seconds,
        )
      }
      onSelectDuration={(seconds) =>
        void recordingSettings.updatePreferences(
          persistedRecordingSettings.recording_mode,
          seconds,
        )
      }
      onSaveShortcut={(binding) => void saveRecordingShortcut(binding)}
    />
  );

  return (
    <>
      {view}
      {recordingSettings.errorMessage ? (
        <Feedback
          className="recording-operation-feedback"
          title="录音设置未保存"
          tone="error"
        >
          {recordingSettings.errorMessage}
        </Feedback>
      ) : null}
      {actionError ? (
        <Feedback
          className="recording-operation-feedback"
          title="录音操作未完成"
          tone="error"
        >
          {actionError}
        </Feedback>
      ) : null}
    </>
  );
}

export function RecordingPage({
  onNavigate,
}: {
  onNavigate: (domain: AppDomain) => void;
}) {
  const previewState = recordingPreviewState();
  if (previewState !== null) {
    return (
      <RecordingPreviewPage
        initialState={previewState}
        onNavigate={onNavigate}
      />
    );
  }
  return <ProductionRecordingPage onNavigate={onNavigate} />;
}
