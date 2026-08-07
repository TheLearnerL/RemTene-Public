import {
  type ReactNode,
  useState,
} from "react";

import { type AppDomain } from "@/app/navigation";
import { useBackendGateway } from "@/backend/useBackendGateway";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import {
  deriveStatusPageState,
  effectiveProcessingMode,
  selectionDetail,
  type StatusPageState,
} from "@/features/status/status-model";
import { useStatusDashboard } from "@/features/status/useStatusDashboard";
import { useTextProcessingSettings } from "@/features/status/useTextProcessingSettings";
import { classes } from "@/lib/classes";
import { type ProcessingMode } from "@/lib/ipc";

interface StatusPageProps {
  onNavigate: (domain: AppDomain) => void;
}

interface StatusCopy {
  title: string;
  description: string;
  action: string;
  processingDetail: string;
  selectionDetail: string;
}

const STATUS_COPY: Record<
  Exclude<StatusPageState, "loading">,
  StatusCopy
> = {
  partial: {
    title: "可以开始输入",
    description: "设置快捷键后，就能在其他应用中使用。",
    action: "设置快捷键",
    processingDetail: "原始转录可直接使用；文字整理需要先连接第三方服务。",
    selectionDetail: "原始转录不会读取选中的文字。",
  },
  ready: {
    title: "已准备好",
    description: "现在可以开始语音输入。",
    action: "开始录音",
    processingDetail: "忠实整理会修正错字和表达，但不改变原意。",
    selectionDetail: "开启后，选中的文字只用于本次整理。",
  },
  busy: {
    title: "正在完成本次输入",
    description: "完成前不能开始新的录音。",
    action: "正在处理",
    processingDetail: "完成后即可继续录音和修改设置。",
    selectionDetail: "本次使用的设置暂时不能更改。",
  },
  error: {
    title: "无法读取当前状态",
    description: "请重新读取后再继续。现有设置不会改变。",
    action: "重新读取",
    processingDetail: "读取恢复前，不会开始录音或更改设置。",
    selectionDetail: "当前不会读取选中的文字。",
  },
  empty: {
    title: "开始首次设置",
    description: "允许使用麦克风并设置快捷键，即可开始使用。",
    action: "开始设置",
    processingDetail: "先选择文字处理方式，之后可以随时更改。",
    selectionDetail: "原始转录不会读取选中的文字。",
  },
};

const MODE_LABELS: Record<ProcessingMode, string> = {
  raw: "原始转录",
  faithful: "忠实整理",
  structured: "结构优化",
};

type StatusTone = "success" | "warning" | "error" | "neutral";

function StatusSpecRow({
  label,
  detail,
  trailing,
  tone,
}: {
  label: string;
  detail: string;
  trailing: string;
  tone: StatusTone;
}) {
  return (
    <div className="status-spec-row">
      <span
        className="status-spec-dot"
        data-tone={tone}
        aria-hidden="true"
      />
      <div className="status-spec-copy">
        <strong>{label}</strong>
        <span>{detail}</span>
      </div>
      <span className="status-spec-trailing" data-tone={tone}>
        {trailing}
      </span>
    </div>
  );
}

function StatusCard({
  title,
  subtitle,
  children,
  className,
}: {
  title: string;
  subtitle?: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <section className={classes("status-spec-card", className)}>
      <header>
        <h2>{title}</h2>
        {subtitle ? <p>{subtitle}</p> : null}
      </header>
      {children}
    </section>
  );
}

function LoadingState({
  label = "正在读取状态",
  busy = true,
}: {
  label?: string;
  busy?: boolean;
}) {
  return (
    <div
      className="status-spec-loading"
      data-static={busy ? undefined : "true"}
      role={busy ? "status" : "region"}
      aria-live={busy ? "polite" : undefined}
      aria-busy={busy}
      aria-label={label}
    >
      <div className="status-spec-loading-header" />
      <div className="status-spec-loading-processing" />
      <div className="status-spec-loading-columns">
        <div />
        <div />
      </div>
      <div className="status-spec-loading-footer" />
    </div>
  );
}

export function StatusPage({ onNavigate }: StatusPageProps) {
  const gateway = useBackendGateway();
  const dashboard = useStatusDashboard();
  const textSettings = useTextProcessingSettings();
  const [starting, setStarting] = useState(false);
  const snapshot = dashboard.snapshot;
  const settings = textSettings.settings;
  const previewState =
    import.meta.env.DEV &&
    new URLSearchParams(window.location.search).get("preview") ===
      "status-empty"
      ? "empty"
      : null;
  const pageState =
    previewState ??
    deriveStatusPageState(
      snapshot,
      dashboard.permissions,
      settings,
      dashboard.loading || textSettings.loading,
      dashboard.failed || textSettings.failed,
    );
  const processingMode = effectiveProcessingMode(snapshot, settings);
  const activeSession = snapshot?.active_session ?? null;
  const sessionActive = activeSession !== null;
  const recordingActive =
    activeSession?.user_state === "preparing" ||
    activeSession?.user_state === "recording";
  const activeActionLabel = recordingActive ? "录音中" : "任务进行中";
  const activeSettingsNotice = recordingActive
    ? "录音结束后即可修改设置。"
    : "本次输入完成后即可修改设置。";
  const selectionEnabled =
    processingMode !== "raw" && Boolean(settings?.read_selected_text);
  const settingsFrozen =
    sessionActive ||
    pageState === "busy" ||
    pageState === "error" ||
    pageState === "loading" ||
    textSettings.saving;

  const refresh = async () => {
    await Promise.all([dashboard.refresh(), textSettings.refresh()]);
  };

  const recheck = async () => {
    if (
      snapshot?.asr_readiness === "discovering" ||
      snapshot?.asr_readiness === "unavailable"
    ) {
      await Promise.all([
        dashboard.checkAsrHealth(),
        textSettings.refresh(),
      ]);
      return;
    }
    await refresh();
  };

  const startRecording = async () => {
    if (starting || snapshot?.active_session) return;
    setStarting(true);
    try {
      await gateway.startSession();
      await dashboard.refresh();
    } catch {
      await refresh();
    } finally {
      setStarting(false);
    }
  };

  const selectMode = (mode: ProcessingMode) => {
    if (settingsFrozen || settings === null || mode === processingMode) return;
    void textSettings.update(mode, settings.read_selected_text);
  };

  const setReadSelectedText = (checked: boolean) => {
    if (settingsFrozen || settings === null || processingMode === "raw") return;
    void textSettings.update(processingMode, checked);
  };

  if (pageState === "loading") {
    return <LoadingState />;
  }
  const copy = STATUS_COPY[pageState];
  const headerAction =
    pageState === "busy" ? (
      <Button className="w-[140px]" disabled>
        {copy.action}
      </Button>
    ) : sessionActive ? (
      <Button className="w-[140px]" disabled>
        {activeActionLabel}
      </Button>
    ) : pageState === "ready" ? (
      <Button
        className="w-[140px]"
        disabled={starting}
        onClick={() => void startRecording()}
      >
        {copy.action}
      </Button>
    ) : pageState === "error" ? (
      <Button
        className="w-[140px]"
        variant="primary"
        disabled={dashboard.checkingAsr}
        aria-busy={dashboard.checkingAsr}
        onClick={() => void recheck()}
      >
        {dashboard.checkingAsr ? "正在检查…" : copy.action}
      </Button>
    ) : pageState === "empty" ? (
      <Button
        className="w-[140px]"
        onClick={() => onNavigate("system")}
      >
        {copy.action}
      </Button>
    ) : (
      <Button
        className="w-[140px]"
        onClick={() => onNavigate("recording")}
      >
        {copy.action}
      </Button>
    );

  const visibleSelectionDetail =
    pageState === "ready"
      ? selectionDetail(processingMode, selectionEnabled)
      : copy.selectionDetail;
  const exposeSelectedMode = pageState !== "error";

  return (
    <div className="status-spec-page" data-state={pageState}>
      <header className="status-spec-header" aria-live="polite">
        <div>
          <p className="status-spec-breadcrumb">状态</p>
          <h1>{copy.title}</h1>
          <p className="status-spec-description">{copy.description}</p>
          {sessionActive && pageState !== "busy" ? (
            <p className="status-session-notice" role="status">
              {activeSettingsNotice}
            </p>
          ) : null}
        </div>
        {headerAction}
      </header>

      <section className="status-processing-card">
        <div className="status-processing-heading">
          <h2>本次输入如何处理</h2>
          <p>{copy.processingDetail}</p>
        </div>
        <div className="status-selection-setting">
          <div>
            <h3>读取选取文字</h3>
            <p>{visibleSelectionDetail}</p>
          </div>
          <Switch
            checked={selectionEnabled}
            disabled={settingsFrozen || processingMode === "raw"}
            onCheckedChange={setReadSelectedText}
            aria-label="读取选取文字"
          />
        </div>
        <div
          className="status-mode-options"
          role="group"
          aria-label="本次输入如何处理"
        >
          {(Object.keys(MODE_LABELS) as ProcessingMode[]).map((mode) => (
            <button
              key={mode}
              type="button"
              className={classes(
                "status-mode-button",
                mode === "structured" && "status-mode-button--wide",
                exposeSelectedMode &&
                  mode === processingMode &&
                  "status-mode-button--selected",
              )}
              disabled={settingsFrozen}
              aria-pressed={exposeSelectedMode && mode === processingMode}
              onClick={() => selectMode(mode)}
            >
              {MODE_LABELS[mode]}
            </button>
          ))}
        </div>
      </section>

      <div className="status-spec-columns">
        {pageState === "partial" ? (
          <>
            <StatusCard
              title="本地输入已就绪"
              subtitle="不连接第三方服务，也能使用原始转录。"
            >
              <p className="status-spec-note">
                麦克风和本地转录已可用。设置快捷键后，可在任意应用开始录音。
              </p>
              <div className="status-spec-actions">
                <Button
                  className="w-[156px]"
                  disabled={sessionActive}
                  onClick={() => onNavigate("recording")}
                >
                  设置快捷键
                </Button>
                <Button className="w-[156px]" disabled>
                  完成设置
                </Button>
              </div>
            </StatusCard>
            <StatusCard title="开始前检查">
              <div className="status-spec-rows">
                <StatusSpecRow
                  label="麦克风"
                  detail="录音时才会开启"
                  trailing="已授权"
                  tone="success"
                />
                <StatusSpecRow
                  label="本地识别"
                  detail={
                    snapshot?.asr_readiness === "whisper_ready"
                      ? "Whisper"
                      : "Qwen3-ASR"
                  }
                  trailing="可用"
                  tone="success"
                />
                <StatusSpecRow
                  label="快捷键"
                  detail="开始与结束录音"
                  trailing={
                    snapshot?.shortcut_configured ? "已绑定" : "未绑定"
                  }
                  tone={snapshot?.shortcut_configured ? "success" : "warning"}
                />
                <StatusSpecRow
                  label="文字服务"
                  detail="文字整理时使用"
                  trailing={snapshot?.llm_configured ? "已配置" : "可选"}
                  tone={snapshot?.llm_configured ? "success" : "warning"}
                />
              </div>
            </StatusCard>
          </>
        ) : null}

        {pageState === "ready" ? (
          <>
            <StatusCard
              title="可以直接开始输入"
              subtitle="所需设置已完成。"
            >
              <p className="status-spec-note">
                按快捷键开始说话，结束后会自动整理并输出文字。
              </p>
              <div className="status-spec-actions">
                <Button
                  className="w-[156px]"
                  variant="primary"
                  disabled={starting || sessionActive}
                  onClick={() => void startRecording()}
                >
                  {sessionActive ? activeActionLabel : "开始录音"}
                </Button>
                <Button
                  className="w-[156px]"
                  disabled={sessionActive}
                  onClick={() => onNavigate("recording")}
                >
                  录音设置
                </Button>
              </div>
            </StatusCard>
            <StatusCard title="当前配置">
              <div className="status-spec-rows">
                <StatusSpecRow
                  label="快捷键"
                  detail="在任意应用开始录音"
                  trailing="已绑定"
                  tone="success"
                />
                <StatusSpecRow
                  label="语音转文字"
                  detail="本地模型"
                  trailing={
                    snapshot?.asr_readiness === "whisper_ready"
                      ? "Whisper"
                      : "Qwen3"
                  }
                  tone="success"
                />
                <StatusSpecRow
                  label="第三方文字服务"
                  detail="只用于文字整理"
                  trailing={snapshot?.llm_configured ? "已配置" : "可选"}
                  tone={snapshot?.llm_configured ? "success" : "warning"}
                />
                <StatusSpecRow
                  label="输出"
                  detail="写入当前输入位置"
                  trailing="就绪"
                  tone="success"
                />
              </div>
            </StatusCard>
          </>
        ) : null}

        {pageState === "busy" ? (
          <>
            <StatusCard
              title="正在完成本次输入"
              subtitle="完成前不能开始新的录音。"
            >
              <div className="status-spec-rows status-spec-rows--wide">
                <StatusSpecRow
                  label="录音"
                  detail="麦克风已关闭"
                  trailing="完成"
                  tone="success"
                />
                <StatusSpecRow
                  label="语音转文字"
                  detail="本地识别完成"
                  trailing="完成"
                  tone="success"
                />
                <StatusSpecRow
                  label="忠实整理"
                  detail="正在处理转录文字"
                  trailing="进行中"
                  tone="warning"
                />
              </div>
            </StatusCard>
            <StatusCard
              title="请稍候"
              subtitle="完成前暂时不能修改设置。"
            >
              <p className="status-spec-note status-spec-note--narrow">
                新录音不会排队；取消后不会留下文字结果。
              </p>
              <div className="status-spec-actions">
                <Button className="w-full" disabled>
                  任务进行中
                </Button>
              </div>
            </StatusCard>
          </>
        ) : null}

        {pageState === "error" ? (
          <>
            <StatusCard
              title="状态暂时无法读取"
              subtitle="没有开始录音，也没有修改任何文字。"
            >
              <p
                className="status-spec-note status-spec-note--error"
                role={dashboard.healthErrorMessage ? "alert" : undefined}
              >
                {dashboard.healthErrorMessage ??
                  "重新读取会检查本地模型和设置，不会打开麦克风或写入文字。"}
              </p>
              <div className="status-spec-actions">
                <Button
                  className="w-[156px]"
                  variant="primary"
                  disabled={dashboard.checkingAsr}
                  aria-busy={dashboard.checkingAsr}
                  onClick={() => void recheck()}
                >
                  {dashboard.checkingAsr ? "正在检查…" : "重新读取状态"}
                </Button>
                <Button
                  className="w-[156px]"
                  onClick={() => onNavigate("system")}
                >
                  前往系统
                </Button>
              </div>
            </StatusCard>
            <StatusCard
              title="为避免误操作"
              subtitle="读取恢复前，不会自动执行任何操作。"
            >
              <div className="status-spec-rows status-spec-rows--spaced">
                <StatusSpecRow
                  label="录音"
                  detail="不自动开始"
                  trailing="未开始"
                  tone="success"
                />
                <StatusSpecRow
                  label="输出"
                  detail="不自动写入"
                  trailing="未写入"
                  tone="success"
                />
                <StatusSpecRow
                  label="API Key"
                  detail="不会显示或使用"
                  trailing="已保护"
                  tone="success"
                />
              </div>
            </StatusCard>
          </>
        ) : null}

        {pageState === "empty" ? (
          <>
            <StatusCard
              title="完成两步即可开始"
              subtitle="本地语音转文字已随应用安装。"
            >
              <div className="status-spec-rows status-spec-rows--wide">
                <StatusSpecRow
                  label="允许麦克风"
                  detail="只在明确录音期间开启"
                  trailing="第 1 步"
                  tone="warning"
                />
                <StatusSpecRow
                  label="设置快捷键"
                  detail="在任意应用开始与结束录音"
                  trailing="第 2 步"
                  tone="warning"
                />
              </div>
              <div className="status-spec-actions">
                <Button
                  className="w-[156px]"
                  variant="primary"
                  onClick={() => onNavigate("system")}
                >
                  开始设置
                </Button>
              </div>
            </StatusCard>
            <StatusCard
              title="可稍后配置"
              subtitle="这些设置不影响使用原始转录。"
            >
              <div className="status-spec-rows status-spec-rows--spaced">
                <StatusSpecRow
                  label="第三方文字服务"
                  detail="文字整理时使用"
                  trailing="可选"
                  tone="warning"
                />
                <StatusSpecRow
                  label="读取选取文字"
                  detail="默认关闭"
                  trailing="可选"
                  tone="warning"
                />
                <StatusSpecRow
                  label="输出历史"
                  detail="仅保存最终文字"
                  trailing="可选"
                  tone="warning"
                />
              </div>
            </StatusCard>
          </>
        ) : null}
      </div>

      <footer className="status-spec-footer">
        <span className="status-spec-footer-dot" aria-hidden="true" />
        <strong>
          {pageState === "partial"
            ? "音频始终只在本地设备处理"
            : pageState === "ready"
              ? "麦克风在待命状态保持关闭"
              : pageState === "busy"
                ? "本次使用开始录音时的设置"
                : pageState === "error"
                  ? "配置不可用时保持原状"
                  : "默认能力不依赖第三方文字服务"}
        </strong>
        <span>
          {pageState === "partial" || pageState === "ready"
            ? `当前：${MODE_LABELS[processingMode]}`
            : pageState === "busy"
              ? "麦克风：已关闭"
              : pageState === "error"
                ? "当前状态：未知"
                : "尚未完成首次设置"}
        </span>
      </footer>
    </div>
  );
}
