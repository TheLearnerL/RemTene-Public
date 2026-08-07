import {
  type KeyboardEvent,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";

import LlmSettingsPanel, {
  type LlmSettingsVisualState,
} from "@/app/LlmSettingsPanel";
import { useBackendGateway } from "@/backend/useBackendGateway";
import { Button } from "@/components/ui/button";
import {
  type AppSnapshot,
  type ControlPanelNavigationTarget,
  type LocalAsrModel,
  getIpcErrorCode,
} from "@/lib/ipc";
import "@/styles/model-page.css";

type ModelTab = "asr" | "text-service";
type ModelPreview =
  | "local-ready"
  | "local-failed"
  | LlmSettingsVisualState;

function navigationTargetToTab(
  target: ControlPanelNavigationTarget,
): ModelTab {
  return target === "model.asr" ? "asr" : "text-service";
}

function modelPreview(): ModelPreview | null {
  if (!import.meta.env.DEV) return null;
  const value = new URLSearchParams(window.location.search).get("preview");
  switch (value) {
    case "model-local-ready":
      return "local-ready";
    case "model-local-failed":
      return "local-failed";
    case "model-configured":
      return "configured";
    case "model-not-configured":
      return "not-configured";
    case "model-endpoint-saved":
      return "endpoint-saved";
    case "model-secret-recovery":
      return "secret-recovery";
    case "model-secret-unavailable":
      return "secret-unavailable";
    case "model-connection-failed":
      return "connection-failed";
    default:
      return null;
  }
}

function ModelHeader({ action }: { action?: React.ReactNode }) {
  return (
    <header className="model-header">
      <div>
        <p className="model-breadcrumb">模型</p>
        <h1>模型</h1>
        <p className="model-description">
          选择本地转录模型，连接文字整理服务。
        </p>
      </div>
      {action}
    </header>
  );
}

function StatusBanner({
  title,
  detail,
  tone,
  action,
}: {
  title: string;
  detail: string;
  tone: "success" | "warning" | "error";
  action?: React.ReactNode;
}) {
  return (
    <section className="model-banner" data-tone={tone} aria-live="polite">
      <span className="model-banner-dot" aria-hidden="true" />
      <div className="model-banner-copy">
        <strong>{title}</strong>
        <span>{detail}</span>
      </div>
      {action ? <div className="model-banner-action">{action}</div> : null}
    </section>
  );
}

function ModelRow({
  label,
  detail,
  trailing,
  tone = "neutral",
}: {
  label: string;
  detail: string;
  trailing: React.ReactNode;
  tone?: "neutral" | "success" | "warning" | "error";
}) {
  return (
    <div className="model-row">
      <div>
        <strong>{label}</strong>
        <span>{detail}</span>
      </div>
      <span className="model-row-trailing" data-tone={tone}>
        {trailing}
      </span>
    </div>
  );
}

function ModelDirectoryTrailing({
  modelName,
  status,
  opening,
  onOpen,
}: {
  modelName: "Qwen3-ASR" | "Whisper";
  status: string;
  opening: boolean;
  onOpen: () => void;
}) {
  return (
    <span className="model-directory-trailing">
      <span>{status}</span>
      <button
        type="button"
        className="model-directory-link"
        disabled={opening}
        aria-busy={opening}
        aria-label={`查看 ${modelName} 模型目录`}
        onClick={onOpen}
      >
        查看目录
      </button>
    </span>
  );
}

function ModelSwitchControls({
  selectedModel,
  switching,
  disabled,
  feedback,
  onSelect,
  onSwitch,
}: {
  selectedModel: LocalAsrModel;
  switching: boolean;
  disabled: boolean;
  feedback: { tone: "success" | "error"; text: string } | null;
  onSelect: (model: LocalAsrModel) => void;
  onSwitch: () => void;
}) {
  return (
    <div className="model-switch-block">
      <div className="model-switch-controls">
        <label>
          <span>目标模型</span>
          <select
            value={selectedModel}
            disabled={switching}
            aria-label="选择要切换的本地识别模型"
            onChange={(event) =>
              onSelect(event.target.value as LocalAsrModel)
            }
          >
            <option value="qwen">Qwen3-ASR</option>
            <option value="whisper">Whisper</option>
          </select>
        </label>
        <Button
          variant="outline"
          disabled={disabled || switching}
          aria-busy={switching}
          onClick={onSwitch}
        >
          {switching ? "正在切换…" : "切换"}
        </Button>
      </div>
      {feedback ? (
        <p
          className="model-switch-feedback"
          data-tone={feedback.tone}
          role={feedback.tone === "error" ? "alert" : "status"}
        >
          {feedback.text}
        </p>
      ) : null}
    </div>
  );
}

function ModelCard({
  title,
  subtitle,
  children,
  className = "",
}: {
  title: string;
  subtitle: string;
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <section className={`model-card ${className}`}>
      <header>
        <h2>{title}</h2>
        <p>{subtitle}</p>
      </header>
      {children}
    </section>
  );
}

function AsrPanel({
  snapshot,
  loading,
  checking,
  error,
  forceFailed,
  approvedReadyPreview,
  openingDirectory,
  directoryError,
  selectedModel,
  switching,
  switchFeedback,
  onOpenDirectory,
  onSelectModel,
  onSwitchModel,
}: {
  snapshot: AppSnapshot | null;
  loading: boolean;
  checking: boolean;
  error: boolean;
  forceFailed: boolean;
  approvedReadyPreview: boolean;
  openingDirectory: boolean;
  directoryError: string | null;
  selectedModel: LocalAsrModel;
  switching: boolean;
  switchFeedback: { tone: "success" | "error"; text: string } | null;
  onOpenDirectory: () => void;
  onSelectModel: (model: LocalAsrModel) => void;
  onSwitchModel: () => void;
}) {
  const qwenReady = snapshot?.model_summary.qwen_ready === true;
  const whisperReady = snapshot?.model_summary.whisper_ready === true;
  const qwenActive =
    snapshot?.asr_readiness === "qwen_ready" && qwenReady;
  const whisperActive =
    snapshot?.asr_readiness === "whisper_ready" && whisperReady;
  const ready = !forceFailed && !error && (qwenActive || whisperActive);
  const activeModel = whisperActive ? "Whisper" : "Qwen3-ASR";
  const activeSelection: LocalAsrModel | null = qwenActive
    ? "qwen"
    : whisperActive
      ? "whisper"
      : null;
  const taskActive = snapshot?.active_session !== null;
  const switchDisabled =
    checking ||
    loading ||
    taskActive ||
    (activeSelection !== null && activeSelection === selectedModel);
  const switchControls = (
    <ModelSwitchControls
      selectedModel={selectedModel}
      switching={switching}
      disabled={switchDisabled}
      feedback={switchFeedback}
      onSelect={onSelectModel}
      onSwitch={onSwitchModel}
    />
  );
  const directoryFeedback = directoryError ? (
    <span className="model-directory-error" role="alert">
      {directoryError}
    </span>
  ) : undefined;
  const directoryTrailing = (
    modelName: "Qwen3-ASR" | "Whisper",
    status: string,
  ) => (
    <ModelDirectoryTrailing
      modelName={modelName}
      status={status}
      opening={openingDirectory}
      onOpen={onOpenDirectory}
    />
  );

  if (loading && snapshot === null && !forceFailed) {
    return (
      <div className="model-asr-loading" role="status" aria-label="正在检查本地语音转文字">
        <span />
        <span />
        <span />
      </div>
    );
  }

  if (forceFailed) {
    return (
      <>
        <StatusBanner
          title="本地语音转文字暂不可用"
          detail="本地模型未准备好。音频没有离开本机。"
          tone="error"
          action={directoryFeedback}
        />
        <div className="model-asr-columns">
          <ModelCard
            title="本地识别模型"
            subtitle="模型切换只影响后续新任务。"
          >
            <div className="model-card-body">
              <ModelRow
                label="Qwen3-ASR"
                detail="默认模型 · 检查失败"
                trailing={directoryTrailing("Qwen3-ASR", "不可用")}
                tone="error"
              />
              <ModelRow
                label="Whisper"
                detail="可手动选择"
                trailing={directoryTrailing("Whisper", "未安装")}
              />
              <div className="model-card-actions">
                {switchControls}
              </div>
            </div>
          </ModelCard>
          <ModelCard
            title="模型状态"
            subtitle="检查失败时会继续使用当前选择。"
          >
            <div className="model-card-body">
              <ModelRow
                label="模型文件"
                detail="文件检查"
                trailing="异常"
                tone="error"
              />
              <ModelRow
                label="本地转录服务"
                detail="单独运行"
                trailing="未就绪"
                tone="error"
              />
              <ModelRow
                label="更新"
                detail="由应用管理"
                trailing="最新"
              />
            </div>
          </ModelCard>
        </div>
        <footer className="model-footer" data-tone="error">
          <span>音频始终只在本机处理</span>
          <span>当前：不可用</span>
        </footer>
      </>
    );
  }

  if (!ready) {
    const discovering =
      !error && snapshot?.asr_readiness === "discovering";
    const title = error
      ? "本地语音转文字状态暂不可读"
      : discovering
        ? checking
          ? "正在检查本地语音转文字"
          : "本地语音转文字尚未检查"
        : "本地语音转文字暂不可用";
    const detail = error
      ? "暂时无法读取模型状态。没有开始下载、切换或更新。"
      : discovering
        ? checking
          ? "正在检查当前模型，不会打开麦克风或读取选中的文字。"
          : "模型检查尚未完成，也可以手动重新检查。"
        : "当前没有可用的本地模型。音频不会上传到远端。";
    const tone = discovering ? "warning" : "error";
    const pendingLabel = error
      ? "状态未知"
      : discovering
        ? checking
          ? "检查中"
          : "尚未检查"
        : "未就绪";
    const qwenStateKnownReady = !error && qwenReady;
    const whisperStateKnownReady = !error && whisperReady;

    return (
      <>
        <StatusBanner
          title={title}
          detail={detail}
          tone={tone}
          action={directoryFeedback}
        />
        <div className="model-asr-columns">
          <ModelCard
            title="本地识别模型"
            subtitle="查看模型是否可以使用"
          >
            <div className="model-card-body">
              <ModelRow
                label="Qwen3-ASR"
                detail="当前状态"
                trailing={directoryTrailing(
                  "Qwen3-ASR",
                  qwenStateKnownReady
                    ? "已就绪"
                    : snapshot?.model_summary.selected_model === "qwen"
                      ? pendingLabel
                      : "切换时检查",
                )}
                tone={qwenStateKnownReady ? "success" : "warning"}
              />
              <ModelRow
                label="Whisper"
                detail="当前状态"
                trailing={directoryTrailing(
                  "Whisper",
                  whisperStateKnownReady
                    ? "已就绪"
                    : snapshot?.model_summary.selected_model === "whisper"
                      ? pendingLabel
                      : "切换时检查",
                )}
                tone={whisperStateKnownReady ? "success" : "warning"}
              />
              <div className="model-card-actions">
                {switchControls}
              </div>
            </div>
          </ModelCard>
          <ModelCard
            title="模型状态"
            subtitle="暂时没有更多模型详情。"
          >
            <div className="model-card-body">
              <ModelRow
                label="模型文件"
                detail="文件状态"
                trailing="暂无信息"
              />
              <ModelRow
                label="本地转录服务"
                detail="运行状态"
                trailing="暂无信息"
              />
              <ModelRow
                label="更新"
                detail="更新状态"
                trailing="暂无信息"
              />
            </div>
          </ModelCard>
        </div>
        <footer className="model-footer" data-tone={tone}>
          <span>音频始终只在本机处理</span>
          <span>
            当前：
            {discovering
              ? checking
                ? "检查中"
                : "尚未检查"
              : error
                ? "状态未知"
                : "不可用"}
          </span>
        </footer>
      </>
    );
  }

  return (
    <>
      <StatusBanner
        title="本地语音转文字已就绪"
        detail="当前模型已通过检查，可以使用。切换失败时会保留原模型。"
        tone="success"
        action={directoryFeedback}
      />
      <div className="model-asr-columns">
        <ModelCard
          title="本地识别模型"
          subtitle="模型切换只影响后续新任务。"
        >
          <div className="model-card-body">
            <ModelRow
              label="Qwen3-ASR"
              detail={
                approvedReadyPreview
                  ? "手动选择 · 已检查"
                  : "当前状态"
              }
              trailing={directoryTrailing(
                "Qwen3-ASR",
                qwenActive
                  ? "正在使用"
                  : qwenReady
                    ? "已就绪"
                    : "切换时检查",
              )}
              tone={qwenReady ? "success" : "neutral"}
            />
            <ModelRow
              label="Whisper"
              detail={
                approvedReadyPreview
                  ? "可手动切换"
                  : "当前状态"
              }
              trailing={directoryTrailing(
                "Whisper",
                whisperActive
                  ? "正在使用"
                  : whisperReady
                    ? "已就绪"
                    : "切换时检查",
              )}
              tone={whisperReady ? "success" : "neutral"}
            />
            <div className="model-card-actions">
              {switchControls}
            </div>
          </div>
        </ModelCard>
        <ModelCard
          title="模型状态"
          subtitle={
            approvedReadyPreview
              ? "检查失败时会继续使用当前选择。"
              : "暂时没有更多模型详情。"
          }
        >
          <div className="model-card-body">
            <ModelRow
              label="模型文件"
              detail={
                approvedReadyPreview
                  ? "文件检查"
                  : "文件状态"
              }
              trailing={approvedReadyPreview ? "正常" : "暂无信息"}
              tone={approvedReadyPreview ? "success" : "neutral"}
            />
            <ModelRow
              label="本地转录服务"
              detail={
                approvedReadyPreview
                  ? "单独运行"
                  : "运行状态"
              }
              trailing={approvedReadyPreview ? "可用" : "暂无信息"}
              tone={approvedReadyPreview ? "success" : "neutral"}
            />
            <ModelRow
              label="更新"
              detail={
                approvedReadyPreview
                  ? "由应用管理"
                  : "更新状态"
              }
              trailing={approvedReadyPreview ? "最新" : "暂无信息"}
              tone={approvedReadyPreview ? "success" : "neutral"}
            />
          </div>
        </ModelCard>
      </div>
      <footer className="model-footer" data-tone="success">
        <span>音频始终只在本机处理</span>
        <span>当前：{activeModel}</span>
      </footer>
    </>
  );
}

export function ModelPage({
  onChanged,
  navigationTarget,
}: {
  onChanged?: () => void;
  navigationTarget?: ControlPanelNavigationTarget;
}) {
  const preview = modelPreview();
  const gateway = useBackendGateway();
  const [tab, setTab] = useState<ModelTab>(() =>
    navigationTarget === undefined
      ? preview === null ||
        preview === "local-ready" ||
        preview === "local-failed"
        ? "asr"
        : "text-service"
      : navigationTargetToTab(navigationTarget),
  );
  const [snapshot, setSnapshot] = useState<AppSnapshot | null>(null);
  const [selectedModelDraft, setSelectedModelDraft] =
    useState<LocalAsrModel | null>(null);
  const [loading, setLoading] = useState(true);
  const [checking, setChecking] = useState(false);
  const [switching, setSwitching] = useState(false);
  const [switchFeedback, setSwitchFeedback] = useState<{
    tone: "success" | "error";
    text: string;
  } | null>(null);
  const [error, setError] = useState(false);
  const [openingDirectory, setOpeningDirectory] = useState(false);
  const [directoryError, setDirectoryError] = useState<string | null>(null);
  const checkingRef = useRef(false);
  const switchingRef = useRef(false);
  const openingDirectoryRef = useRef(false);
  const asrTabRef = useRef<HTMLButtonElement>(null);
  const textServiceTabRef = useRef<HTMLButtonElement>(null);
  const selectedModel =
    selectedModelDraft ?? snapshot?.model_summary.selected_model ?? "qwen";

  useEffect(() => {
    if (navigationTarget === undefined) return;
    const nextTab = navigationTargetToTab(navigationTarget);
    const frame = window.requestAnimationFrame(() => {
      const nextRef =
        nextTab === "asr" ? asrTabRef : textServiceTabRef;
      nextRef.current?.focus();
    });
    return () => window.cancelAnimationFrame(frame);
  }, [navigationTarget]);

  const loadSnapshot = useCallback(async () => {
    setLoading(true);
    setError(false);
    try {
      setSnapshot(await gateway.getAppSnapshot());
    } catch {
      setError(true);
    } finally {
      setLoading(false);
    }
  }, [gateway]);

  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;
    gateway
      .getAppSnapshot()
      .then((nextSnapshot) => {
        if (active) setSnapshot(nextSnapshot);
      })
      .catch(() => {
        if (active) setError(true);
      })
      .finally(() => {
        if (active) setLoading(false);
      });

    void gateway
      .listenToAppSnapshotChanged((nextSnapshot) => {
        if (!active) return;
        setSnapshot(nextSnapshot);
        setError(false);
        setLoading(false);
      })
      .then((stopListening) => {
        if (!active) {
          stopListening();
          return;
        }
        unlisten = stopListening;
      })
      .catch(() => undefined);

    return () => {
      active = false;
      unlisten?.();
    };
  }, [gateway]);

  const configurationChanged = async () => {
    await loadSnapshot();
    onChanged?.();
  };

  const checkAndReload = useCallback(async () => {
    if (checkingRef.current) return;
    checkingRef.current = true;
    setChecking(true);
    setError(false);
    try {
      setSnapshot(await gateway.checkAsrHealth());
    } catch {
      setError(true);
      try {
        setSnapshot(await gateway.getAppSnapshot());
      } catch {
        // The existing error surface already reports that Core state is unavailable.
      }
    } finally {
      checkingRef.current = false;
      setChecking(false);
    }
  }, [gateway]);

  const openModelDirectory = useCallback(async () => {
    if (openingDirectoryRef.current) return;
    openingDirectoryRef.current = true;
    setOpeningDirectory(true);
    setDirectoryError(null);
    try {
      await gateway.openModelDirectory();
    } catch {
      setDirectoryError("模型目录未打开，请稍后重试。");
    } finally {
      openingDirectoryRef.current = false;
      setOpeningDirectory(false);
    }
  }, [gateway]);

  const switchModel = useCallback(async () => {
    if (switchingRef.current) return;
    switchingRef.current = true;
    setSwitching(true);
    setSwitchFeedback(null);
    try {
      const nextSnapshot = await gateway.switchAsrModel(selectedModel);
      setSnapshot(nextSnapshot);
      setSelectedModelDraft(null);
      setError(false);
      setSwitchFeedback({
        tone: "success",
        text: `已切换到 ${selectedModel === "whisper" ? "Whisper" : "Qwen3-ASR"}`,
      });
      onChanged?.();
    } catch (switchError) {
      const code = getIpcErrorCode(switchError);
      const text =
        code === "asr.model.missing"
          ? "缺少模型"
          : code === "asr.model.hash_mismatch"
            ? "模型文件校验失败"
            : code === "asr.model.switch_busy"
              ? "当前任务结束后才能切换模型"
              : "模型切换失败，请稍后重试。";
      setSwitchFeedback({ tone: "error", text });
      try {
        setSnapshot(await gateway.getAppSnapshot());
      } catch {
        // 保留现有快照；切换命令失败不会发布新模型状态。
      }
    } finally {
      switchingRef.current = false;
      setSwitching(false);
    }
  }, [gateway, onChanged, selectedModel]);

  const selectModel = useCallback((model: LocalAsrModel) => {
    setSelectedModelDraft(model);
    setSwitchFeedback(null);
  }, []);

  const selectTab = (nextTab: ModelTab) => {
    setTab(nextTab);
    const nextRef =
      nextTab === "asr" ? asrTabRef : textServiceTabRef;
    window.requestAnimationFrame(() => nextRef.current?.focus());
  };

  const handleTabKeyDown = (
    event: KeyboardEvent<HTMLButtonElement>,
    currentTab: ModelTab,
  ) => {
    let nextTab: ModelTab | null = null;
    if (event.key === "ArrowRight" || event.key === "ArrowDown") {
      nextTab = currentTab === "asr" ? "text-service" : "asr";
    } else if (
      event.key === "ArrowLeft" ||
      event.key === "ArrowUp"
    ) {
      nextTab = currentTab === "asr" ? "text-service" : "asr";
    } else if (event.key === "Home") {
      nextTab = "asr";
    } else if (event.key === "End") {
      nextTab = "text-service";
    }
    if (nextTab === null) return;
    event.preventDefault();
    selectTab(nextTab);
  };

  return (
    <div className="model-page">
      <ModelHeader
        action={
          tab === "asr" ? (
            <Button
              className="model-header-action"
              variant="primary"
              disabled={checking || switching}
              aria-busy={checking}
              onClick={() => void checkAndReload()}
            >
              {checking ? "正在检查…" : "重新检查"}
            </Button>
          ) : undefined
        }
      />
      <div className="model-tabs" role="tablist" aria-label="模型设置">
        <button
          ref={asrTabRef}
          id="model-tab-asr"
          type="button"
          role="tab"
          aria-selected={tab === "asr"}
          aria-controls="model-tab-panel"
          tabIndex={tab === "asr" ? 0 : -1}
          className={tab === "asr" ? "is-current" : undefined}
          onClick={() => setTab("asr")}
          onKeyDown={(event) => handleTabKeyDown(event, "asr")}
        >
          语音转文字
        </button>
        <button
          ref={textServiceTabRef}
          id="model-tab-text-service"
          type="button"
          role="tab"
          aria-selected={tab === "text-service"}
          aria-controls="model-tab-panel"
          tabIndex={tab === "text-service" ? 0 : -1}
          className={tab === "text-service" ? "is-current" : undefined}
          onClick={() => setTab("text-service")}
          onKeyDown={(event) =>
            handleTabKeyDown(event, "text-service")
          }
        >
          第三方文字服务
        </button>
        <p>音频不会离开本地设备</p>
      </div>

      <div
        id="model-tab-panel"
        className="model-tab-panel"
        role="tabpanel"
        aria-labelledby={
          tab === "asr" ? "model-tab-asr" : "model-tab-text-service"
        }
        aria-label={tab === "asr" ? "语音转文字" : "第三方文字服务"}
      >
        {tab === "asr" ? (
          <AsrPanel
            snapshot={snapshot}
            loading={loading}
            checking={checking}
            error={error}
            forceFailed={preview === "local-failed"}
            approvedReadyPreview={preview === "local-ready"}
            openingDirectory={openingDirectory}
            directoryError={directoryError}
            selectedModel={selectedModel}
            switching={switching}
            switchFeedback={switchFeedback}
            onOpenDirectory={() => void openModelDirectory()}
            onSelectModel={selectModel}
            onSwitchModel={() => void switchModel()}
          />
        ) : (
          <LlmSettingsPanel
            onConfigurationChanged={configurationChanged}
            visualState={
              preview !== null &&
              preview !== "local-ready" &&
              preview !== "local-failed"
                ? preview
                : undefined
            }
          />
        )}
      </div>
    </div>
  );
}
