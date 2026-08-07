import {
  type ReactNode,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";

import { useBackendGateway } from "@/backend/useBackendGateway";
import { userFacingGatewayError } from "@/backend/user-facing-error";
import {
  type HistoryPage,
  type HistoryRecordView,
  type SettingsView,
} from "@/lib/ipc";
import "@/styles/output-page.css";

type OutputState =
  | "loading"
  | "populated"
  | "off"
  | "unavailable"
  | "clear"
  | "limit-confirm"
  | "retention-confirm"
  | "empty"
  | "copy-success"
  | "copy-failure";

type OutputTone = "neutral" | "success" | "warning" | "error";

interface HistoryCopyOutcome {
  recordId: string;
  status: "success" | "failure";
}

interface PendingRetentionChange {
  days: number;
  estimatedExpiredCount: number;
}

interface OutputPreview {
  interactive: boolean;
  state: Exclude<OutputState, "loading"> | null;
}

const MAX_HISTORY_LIMIT = 65_535;
const MAX_HISTORY_RETENTION_DAYS = 65_535;
const DAY_MS = 24 * 60 * 60 * 1_000;
const HISTORY_RETENTION_OPTIONS = [3, 10, 20, 30] as const;

function parseHistoryLimit(value: string): number | null {
  if (!/^\d+$/.test(value.trim())) return null;
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) && parsed >= 1 && parsed <= MAX_HISTORY_LIMIT
    ? parsed
    : null;
}

function parseHistoryRetention(value: string): {
  valid: boolean;
  days: number | null;
} {
  const normalized = value.trim();
  if (normalized === "") return { valid: true, days: null };
  if (!/^\d+$/.test(normalized)) return { valid: false, days: null };
  const parsed = Number(normalized);
  return Number.isSafeInteger(parsed) &&
    parsed >= 1 &&
    parsed <= MAX_HISTORY_RETENTION_DAYS
    ? { valid: true, days: parsed }
    : { valid: false, days: null };
}

function outputPreview(): OutputPreview {
  if (!import.meta.env.DEV) {
    return { interactive: false, state: null };
  }

  const preview = new URLSearchParams(window.location.search).get("preview");
  switch (preview) {
    case "output-populated":
      return { interactive: true, state: "populated" };
    case "output-off":
      return { interactive: true, state: "off" };
    case "output-unavailable":
      return { interactive: true, state: "unavailable" };
    case "output-clear":
      return { interactive: true, state: "clear" };
    case "output-empty":
      return { interactive: true, state: "empty" };
    case "output-copy-success":
      return { interactive: true, state: "copy-success" };
    case "output-copy-failure":
      return { interactive: true, state: "copy-failure" };
    default:
      return { interactive: false, state: null };
  }
}

function OutputActionButton({
  children,
  destructive = false,
  disabled = false,
  wide = false,
  onClick,
}: {
  children: ReactNode;
  destructive?: boolean;
  disabled?: boolean;
  wide?: boolean;
  onClick?: () => void;
}) {
  return (
    <button
      type="button"
      className="output-action-button"
      data-destructive={destructive ? "true" : undefined}
      data-wide={wide ? "true" : undefined}
      disabled={disabled}
      onClick={onClick}
    >
      {children}
    </button>
  );
}

function OutputHeader({
  state,
  clearEnabled,
  onClear,
  onRetry,
}: {
  state: OutputState;
  clearEnabled: boolean;
  onClear: () => void;
  onRetry: () => void;
}) {
  let action: ReactNode = null;

  if (
    state === "populated" ||
    state === "copy-success" ||
    state === "copy-failure"
  ) {
    action = (
      <OutputActionButton
        destructive
        wide
        disabled={!clearEnabled}
        onClick={clearEnabled ? onClear : undefined}
      >
        清除全部历史
      </OutputActionButton>
    );
  } else if (state === "unavailable") {
    action = (
      <OutputActionButton onClick={onRetry}>
        重新读取
      </OutputActionButton>
    );
  }

  return (
    <header className="output-header">
      <div>
        <p className="output-breadcrumb">输出</p>
        <h1>输出</h1>
        <p className="output-description">
          查看和管理保存在本机的文字。
        </p>
      </div>
      {action}
    </header>
  );
}

function outputStatus(
  state: Exclude<OutputState, "loading">,
  interactive: boolean,
  recordCount: number,
): {
  detail: string;
  title: string;
  tone: OutputTone;
} {
  switch (state) {
    case "populated":
      return {
        title: interactive
          ? "已保存 10 条最终文字"
          : `已保存 ${recordCount} 条最终文字`,
        detail: "只保存最终文字和时间，不保存音频、选中文字或 API Key。",
        tone: "success",
      };
    case "off":
      return {
        title: "历史保存已关闭",
        detail: "之后的文字不会保存，已有记录仍会保留。",
        tone: "warning",
      };
    case "unavailable":
      return {
        title: "暂时无法读取输出历史",
        detail: "没有更改任何记录，请稍后重试。",
        tone: "error",
      };
    case "clear":
      return {
        title: "清除全部历史需要确认",
        detail: "只会清除保存在本机的文字，不影响其他设置。",
        tone: "warning",
      };
    case "limit-confirm":
      return {
        title: "降低保存上限需要确认",
        detail: "确认后会立即删除超出新上限的最早记录，且无法撤销。",
        tone: "warning",
      };
    case "retention-confirm":
      return {
        title: "缩短保存期限需要确认",
        detail: "确认后会立即删除超过期限的记录，且无法撤销。",
        tone: "warning",
      };
    case "empty":
      if (!interactive) {
        return {
          title: "还没有历史记录",
          detail: "完成一次输入后，最终文字会出现在这里。",
          tone: "neutral",
        };
      }
      return {
        title: "历史保存已开启",
        detail: "之后完成的文字会保存在本机，不会保存音频或 API Key。",
        tone: "warning",
      };
    case "copy-success":
      return {
        title: "最终文字已复制",
        detail: "完整文字已复制，历史记录没有变化。",
        tone: "success",
      };
    case "copy-failure":
      return {
        title: "复制失败",
        detail: "请检查剪贴板后重试。历史记录没有变化。",
        tone: "error",
      };
  }
}

function OutputStatus({
  state,
  interactive,
  recordCount,
  operationError,
  operationTitle,
}: {
  state: Exclude<OutputState, "loading">;
  interactive: boolean;
  recordCount: number;
  operationError?: string | null;
  operationTitle?: string | null;
}) {
  const status = operationError
    ? {
        title: operationTitle ?? "历史设置未更新",
        detail: operationError,
        tone: "error" as const,
      }
    : outputStatus(state, interactive, recordCount);

  return (
    <section
      className="output-status"
      data-tone={status.tone}
      aria-live="polite"
    >
      <span className="output-status-tone" aria-hidden="true" />
      <span className="output-status-dot" aria-hidden="true" />
      <h2>{status.title}</h2>
      <p>{status.detail}</p>
    </section>
  );
}

function OutputCard({
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
    <section className={`output-card${className ? ` ${className}` : ""}`}>
      <header>
        <h2>{title}</h2>
        <p>{subtitle}</p>
      </header>
      {children}
    </section>
  );
}

function OutputRow({
  label,
  detail,
  trailing,
  tone = "neutral",
  action,
  control,
}: {
  label: string;
  detail: string;
  trailing?: string;
  tone?: OutputTone;
  action?: ReactNode;
  control?: ReactNode;
}) {
  const endKind = action
    ? "action"
    : control
      ? "control"
      : trailing
        ? "trailing"
        : "none";

  return (
    <div className="output-row" data-end-kind={endKind}>
      <span
        className="output-row-dot"
        data-tone={tone}
        aria-hidden="true"
      />
      <span
        className="output-row-copy"
        data-has-control={control ? "true" : undefined}
      >
        <strong>{label}</strong>
        <span>{detail}</span>
      </span>
      {control ? (
        <span className="output-row-control">{control}</span>
      ) : null}
      {action ? (
        <span className="output-row-action">{action}</span>
      ) : null}
      {trailing ? (
        <span className="output-row-trailing">{trailing}</span>
      ) : null}
    </div>
  );
}

function OutputSwitch({
  checked,
  disabled,
  onCheckedChange,
}: {
  checked: boolean;
  disabled: boolean;
  onCheckedChange?: (checked: boolean) => void;
}) {
  return (
    <button
      type="button"
      className="output-switch"
      role="switch"
      aria-label="保存历史"
      aria-checked={checked}
      data-checked={checked ? "true" : "false"}
      disabled={disabled}
      onClick={() => onCheckedChange?.(!checked)}
    >
      <span aria-hidden="true" />
    </button>
  );
}

function EmptyHistory({ enabled }: { enabled: boolean }) {
  return (
    <div className="output-empty-history">
      <span aria-hidden="true" />
      <h3>还没有历史记录</h3>
      <p>
        {enabled
          ? "完成一次输入后，最终文字会出现在这里。"
          : "重新开启后，新完成的最终文字会出现在这里。"}
      </p>
    </div>
  );
}

function isSameLocalDay(left: Date, right: Date): boolean {
  return (
    left.getFullYear() === right.getFullYear() &&
    left.getMonth() === right.getMonth() &&
    left.getDate() === right.getDate()
  );
}

function formatHistoryCreatedAt(
  createdAt: string,
  now = new Date(),
): string {
  const created = new Date(createdAt);
  const time = new Intl.DateTimeFormat("zh-CN", {
    hour: "2-digit",
    minute: "2-digit",
    hourCycle: "h23",
  }).format(created);
  if (isSameLocalDay(created, now)) return `今天 ${time}`;

  const yesterday = new Date(now);
  yesterday.setDate(yesterday.getDate() - 1);
  if (isSameLocalDay(created, yesterday)) return `昨天 ${time}`;

  const weekday = new Intl.DateTimeFormat("zh-CN", {
    weekday: "short",
  }).format(created);
  return `${weekday} ${time}`;
}

function HistoryList({
  records,
  state,
  actionsEnabled,
  copyingRecordId,
  copyOutcome,
  onCopy,
}: {
  records: HistoryRecordView[];
  state: "populated" | "off" | "empty" | "copy-success" | "copy-failure";
  actionsEnabled: boolean;
  copyingRecordId: string | null;
  copyOutcome: HistoryCopyOutcome | null;
  onCopy: (recordId: string) => void;
}) {
  const [scrollProgress, setScrollProgress] = useState(0);

  return (
    <>
      <div
        className="output-history-scroll"
        tabIndex={0}
        aria-label="输出历史列表"
        onScroll={(event) => {
          const target = event.currentTarget;
          const maximum = target.scrollHeight - target.clientHeight;
          setScrollProgress(maximum > 0 ? target.scrollTop / maximum : 0);
        }}
      >
        {records.map((record, index) => {
          const copied =
            copyOutcome?.recordId === record.record_id
              ? copyOutcome.status === "success"
              : copyOutcome === null &&
                index === 0 &&
                state === "copy-success";
          const failed =
            copyOutcome?.recordId === record.record_id
              ? copyOutcome.status === "failure"
              : copyOutcome === null &&
                index === 0 &&
                state === "copy-failure";
          const showCopyOutcome = copied || failed;
          const copying = copyingRecordId === record.record_id;
          const canCopy = actionsEnabled && copyingRecordId === null;

          return (
            <article
              className="output-history-item"
              key={record.record_id}
            >
              <span
                className="output-history-mark"
                data-tone={index === 0 ? "success" : "neutral"}
                aria-hidden="true"
              />
              <h3>{record.final_text}</h3>
              <p
                className="output-history-time"
                data-tone={
                  copied ? "success" : failed ? "error" : "neutral"
                }
              >
                {formatHistoryCreatedAt(record.created_at)}
                {showCopyOutcome
                  ? failed
                    ? " · 复制失败"
                    : " · 已复制"
                  : ""}
              </p>
              <button
                type="button"
                className="output-copy-button"
                data-tone={
                  copied ? "success" : failed ? "error" : "neutral"
                }
                disabled={!canCopy}
                onClick={
                  canCopy ? () => onCopy(record.record_id) : undefined
                }
                aria-busy={copying}
              >
                {copying
                  ? "复制中"
                  : copied
                    ? "已复制"
                    : failed
                      ? "重试"
                      : "复制"}
              </button>
            </article>
          );
        })}
      </div>
      <span className="output-scrollbar" aria-hidden="true">
        <span
          style={{
            transform: `translateY(${Math.round(scrollProgress * 132)}px)`,
          }}
        />
      </span>
    </>
  );
}

function HistoryLimitEditor({
  value,
  disabled,
  onChange,
  onApply,
}: {
  value: string;
  disabled: boolean;
  onChange: (value: string) => void;
  onApply: () => void;
}) {
  return (
    <div className="output-limit-editor">
      <input
        type="number"
        min={1}
        max={65_535}
        step={1}
        inputMode="numeric"
        aria-label="历史保存条数上限"
        value={value}
        disabled={disabled}
        onChange={(event) => onChange(event.currentTarget.value)}
        onBlur={onApply}
        onKeyDown={(event) => {
          if (event.key !== "Enter") return;
          event.preventDefault();
          event.currentTarget.blur();
        }}
      />
    </div>
  );
}

function HistoryRetentionEditor({
  value,
  disabled,
  onChange,
}: {
  value: string;
  disabled: boolean;
  onChange: (value: string) => void;
}) {
  const isLegacyValue =
    value !== "" &&
    !HISTORY_RETENTION_OPTIONS.some((days) => String(days) === value);

  return (
    <div className="output-limit-editor output-retention-editor">
      <select
        aria-label="历史保存期限（天）"
        value={value}
        disabled={disabled}
        onChange={(event) => onChange(event.currentTarget.value)}
      >
        <option value="" disabled>
          天数
        </option>
        {isLegacyValue ? (
          <option value={value} disabled>
            {value} 天（当前）
          </option>
        ) : null}
        {HISTORY_RETENTION_OPTIONS.map((days) => (
          <option key={days} value={days}>
            {days} 天
          </option>
        ))}
      </select>
    </div>
  );
}

function HistoryPolicy({
  policy,
  interactive,
  saving,
  onToggle,
  limitInput,
  onLimitInput,
  onLimitApply,
  retentionInput,
  onRetentionChange,
}: {
  policy: SettingsView["history_policy"];
  interactive: boolean;
  saving: boolean;
  onToggle: (checked: boolean) => void;
  limitInput: string;
  onLimitInput: (value: string) => void;
  onLimitApply: () => void;
  retentionInput: string;
  onRetentionChange: (value: string) => void;
}) {
  return (
    <div className="output-rows" aria-busy={saving}>
      <OutputRow
        label="保存历史"
        detail={policy.enabled ? "只影响后续最终文字" : "后续结果不再写入"}
        control={
          <OutputSwitch
            checked={policy.enabled}
            disabled={!interactive || saving}
            onCheckedChange={interactive ? onToggle : undefined}
          />
        }
      />
      <OutputRow
        label="保存上限"
        detail={
          policy.enabled
            ? "超出后删除最早记录"
            : "开启历史后可修改"
        }
        control={
          <HistoryLimitEditor
            value={limitInput}
            disabled={!interactive || saving || !policy.enabled}
            onChange={onLimitInput}
            onApply={onLimitApply}
          />
        }
        tone="success"
      />
      <OutputRow
        label="保存期限"
        detail="按天计，与数量上限同时生效"
        control={
          <HistoryRetentionEditor
            value={retentionInput}
            disabled={!interactive || saving || !policy.enabled}
            onChange={onRetentionChange}
          />
        }
        tone="success"
      />
      <OutputRow
        label="保存内容"
        detail="仅最终文字和保存时间"
        trailing="本机"
        tone="success"
      />
    </div>
  );
}

function HistorySettingsUnavailable() {
  return (
    <div className="output-rows">
      <OutputRow
        label="保存历史"
        detail="当前设置暂时无法读取"
        control={<OutputSwitch checked={false} disabled />}
      />
      <OutputRow
        label="设置变化"
        detail="本次没有更改设置"
        trailing="无变化"
        tone="success"
      />
      <OutputRow
        label="文字内容"
        detail="错误信息不包含正文"
        trailing="未读取"
        tone="success"
      />
      <OutputRow
        label="文字输出"
        detail="仍可正常使用"
        trailing="继续"
        tone="success"
      />
    </div>
  );
}

function StandardOutput({
  records,
  state,
  historyActionsEnabled,
  historyPolicy,
  policyInteractive,
  policySaving,
  copyingRecordId,
  copyOutcome,
  onCopy,
  onToggleHistory,
  limitInput,
  onLimitInput,
  onLimitApply,
  retentionInput,
  onRetentionChange,
}: {
  records: HistoryRecordView[];
  state: "populated" | "off" | "empty" | "copy-success" | "copy-failure";
  historyActionsEnabled: boolean;
  historyPolicy: SettingsView["history_policy"] | null;
  policyInteractive: boolean;
  policySaving: boolean;
  copyingRecordId: string | null;
  copyOutcome: HistoryCopyOutcome | null;
  onCopy: (recordId: string) => void;
  onToggleHistory: (checked: boolean) => void;
  limitInput: string;
  onLimitInput: (value: string) => void;
  onLimitApply: () => void;
  retentionInput: string;
  onRetentionChange: (value: string) => void;
}) {
  const hasHistory = records.length > 0;

  return (
    <div className="output-columns">
      <OutputCard
        title="输出历史"
        subtitle="只显示最终文字和保存时间"
      >
        {hasHistory ? (
          <HistoryList
            records={records}
            state={state}
            actionsEnabled={historyActionsEnabled}
            copyingRecordId={copyingRecordId}
            copyOutcome={copyOutcome}
            onCopy={onCopy}
          />
        ) : (
          <EmptyHistory enabled={historyPolicy?.enabled ?? false} />
        )}
      </OutputCard>
      <OutputCard
        title="历史保存"
        subtitle="设置只影响历史记录"
      >
        {historyPolicy ? (
          <HistoryPolicy
            policy={historyPolicy}
            interactive={policyInteractive}
            saving={policySaving}
            onToggle={onToggleHistory}
            limitInput={limitInput}
            onLimitInput={onLimitInput}
            onLimitApply={onLimitApply}
            retentionInput={retentionInput}
            onRetentionChange={onRetentionChange}
          />
        ) : (
          <HistorySettingsUnavailable />
        )}
      </OutputCard>
    </div>
  );
}

function UnavailableOutput({
  onRetry,
}: {
  onRetry: () => void;
}) {
  return (
    <div className="output-columns">
      <OutputCard
        title="输出历史"
        subtitle="只显示最终文字和保存时间"
      >
        <div className="output-rows">
          <OutputRow
            label="历史列表"
            detail="当前无法读取记录"
            trailing="不可用"
            tone="error"
          />
          <OutputRow
            label="复制记录"
            detail="恢复读取后即可复制"
            trailing="不可用"
            tone="warning"
          />
          <OutputRow
            label="清除全部"
            detail="恢复读取后才能清除"
            trailing="不可用"
            tone="warning"
          />
          <OutputRow
            label="重新读取"
            detail="再次获取历史记录"
              tone="neutral"
              action={
                <OutputActionButton onClick={onRetry}>
                  重新读取
                </OutputActionButton>
              }
          />
        </div>
      </OutputCard>
      <OutputCard
        title="历史保存"
        subtitle="设置只影响历史记录"
      >
        <HistorySettingsUnavailable />
      </OutputCard>
    </div>
  );
}

function LoadingOutput() {
  return (
    <div className="output-columns" aria-busy="true">
      <OutputCard
        title="输出历史"
        subtitle="只显示最终文字和保存时间"
      >
        <div
          className="output-history-loading"
          role="status"
          aria-label="正在读取输出历史"
        >
          <span />
          <span />
          <span />
        </div>
      </OutputCard>
      <OutputCard
        title="历史保存"
        subtitle="设置只影响历史记录"
      >
        <HistorySettingsUnavailable />
      </OutputCard>
    </div>
  );
}

function ClearConfirmation({
  interactive,
  confirming,
  onCancel,
  onConfirm,
}: {
  interactive: boolean;
  confirming: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div className="output-columns">
      <OutputCard
        title="清除输出历史"
        subtitle="只清除本地最终文字记录"
      >
        <div className="output-rows">
          <OutputRow
            label="清除范围"
            detail="所有本地最终文字记录"
            trailing="不可撤销"
            tone="warning"
          />
          <OutputRow
            label="音频"
            detail="原本就不会保存"
            trailing="不涉及"
            tone="success"
          />
          <OutputRow
            label="API Key 与普通设置"
            detail="不会被清除"
            trailing="不涉及"
            tone="success"
          />
          <OutputRow
            label="后续保存策略"
            detail="清除后仍保持当前设置"
            trailing="不改变"
            tone="success"
          />
        </div>
      </OutputCard>
      <OutputCard
        className="output-confirm-card"
        title="确认清除"
        subtitle="此操作完成后无法撤销"
      >
        <div className="output-confirm-copy">
          <h3>确定清除全部输出历史？</h3>
          <p>只有清除成功后列表才会更新；失败时会保留记录。</p>
        </div>
        <div className="output-confirm-actions">
          <OutputActionButton
            disabled={!interactive}
            onClick={interactive ? onCancel : undefined}
          >
            取消
          </OutputActionButton>
          <OutputActionButton
            destructive
            wide
            disabled={!interactive}
            onClick={interactive ? onConfirm : undefined}
          >
            {confirming ? "清除中" : "确认清除"}
          </OutputActionButton>
        </div>
      </OutputCard>
    </div>
  );
}

function LimitConfirmation({
  currentCount,
  nextLimit,
  interactive,
  confirming,
  onCancel,
  onConfirm,
}: {
  currentCount: number;
  nextLimit: number;
  interactive: boolean;
  confirming: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const removalCount = Math.max(0, currentCount - nextLimit);
  return (
    <div className="output-columns">
      <OutputCard
        title="降低保存上限"
        subtitle="新上限会立即应用到本地历史"
      >
        <div className="output-rows">
          <OutputRow
            label="当前记录"
            detail="保存在本机的最终文字"
            trailing={`${currentCount} 条`}
          />
          <OutputRow
            label="新保存上限"
            detail="之后最多保留的记录数"
            trailing={`${nextLimit} 条`}
            tone="warning"
          />
          <OutputRow
            label="立即删除"
            detail="从最早的记录开始删除"
            trailing={`${removalCount} 条`}
            tone="warning"
          />
          <OutputRow
            label="其他本地数据"
            detail="普通设置、API Key 与音频均不涉及"
            trailing="不改变"
            tone="success"
          />
        </div>
      </OutputCard>
      <OutputCard
        className="output-confirm-card"
        title="确认修改"
        subtitle="被删除的历史无法恢复"
      >
        <div className="output-confirm-copy">
          <h3>将保存上限改为 {nextLimit} 条？</h3>
          <p>保存成功后，会删除超出上限的最早记录。</p>
        </div>
        <div className="output-confirm-actions">
          <OutputActionButton
            disabled={!interactive}
            onClick={interactive ? onCancel : undefined}
          >
            取消
          </OutputActionButton>
          <OutputActionButton
            destructive
            wide
            disabled={!interactive}
            onClick={interactive ? onConfirm : undefined}
          >
            {confirming ? "保存中" : "确认修改"}
          </OutputActionButton>
        </div>
      </OutputCard>
    </div>
  );
}

function RetentionConfirmation({
  currentCount,
  expiredCount,
  nextRetentionDays,
  interactive,
  confirming,
  onCancel,
  onConfirm,
}: {
  currentCount: number;
  expiredCount: number;
  nextRetentionDays: number;
  interactive: boolean;
  confirming: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div className="output-columns">
      <OutputCard
        title="缩短保存期限"
        subtitle="新期限会立即应用到本地历史"
      >
        <div className="output-rows">
          <OutputRow
            label="当前记录"
            detail="保存在本机的最终文字"
            trailing={`${currentCount} 条`}
          />
          <OutputRow
            label="新保存期限"
            detail="超过期限后自动删除"
            trailing={`${nextRetentionDays} 天`}
            tone="warning"
          />
          <OutputRow
            label="预计立即删除"
            detail="会按每条记录的保存时间再次确认"
            trailing={`${expiredCount} 条`}
            tone="warning"
          />
          <OutputRow
            label="其他本地数据"
            detail="普通设置、API Key 与音频均不涉及"
            trailing="不改变"
            tone="success"
          />
        </div>
      </OutputCard>
      <OutputCard
        className="output-confirm-card"
        title="确认修改"
        subtitle="已过期历史无法恢复"
      >
        <div className="output-confirm-copy">
          <h3>将保存期限改为 {nextRetentionDays} 天？</h3>
          <p>保存成功后，会删除已经超过期限的记录。</p>
        </div>
        <div className="output-confirm-actions">
          <OutputActionButton
            disabled={!interactive}
            onClick={interactive ? onCancel : undefined}
          >
            取消
          </OutputActionButton>
          <OutputActionButton
            destructive
            wide
            disabled={!interactive}
            onClick={interactive ? onConfirm : undefined}
          >
            {confirming ? "保存中" : "确认修改"}
          </OutputActionButton>
        </div>
      </OutputCard>
    </div>
  );
}

export function OutputPage() {
  const preview = outputPreview();
  const gateway = useBackendGateway();
  const [state, setState] = useState<OutputState>("loading");
  const [records, setRecords] = useState<HistoryRecordView[]>([]);
  const [settings, setSettings] = useState<SettingsView | null>(null);
  const [policySaving, setPolicySaving] = useState(false);
  const [clearSaving, setClearSaving] = useState(false);
  const [limitInput, setLimitInput] = useState("10");
  const [pendingLimit, setPendingLimit] = useState<number | null>(null);
  const [retentionInput, setRetentionInput] = useState("");
  const [pendingRetention, setPendingRetention] =
    useState<PendingRetentionChange | null>(null);
  const [operationError, setOperationError] = useState<string | null>(null);
  const [operationTitle, setOperationTitle] = useState<string | null>(null);
  const [copyingRecordId, setCopyingRecordId] = useState<string | null>(
    null,
  );
  const [copyOutcome, setCopyOutcome] =
    useState<HistoryCopyOutcome | null>(null);
  const historyRequestGeneration = useRef(0);
  const copyRequestGeneration = useRef(0);
  const policyRequestGeneration = useRef(0);
  const clearRequestGeneration = useRef(0);

  const loadedState = useCallback(
    (recordCount: number, historyEnabled?: boolean): OutputState => {
      if (
        preview.interactive &&
        preview.state !== null &&
        preview.state !== "unavailable"
      ) {
        return preview.state;
      }
      if (recordCount > 0) return "populated";
      return historyEnabled === false ? "off" : "empty";
    },
    [preview.interactive, preview.state],
  );

  const applyHistoryPage = useCallback(
    (nextRecords: HistoryRecordView[], historyEnabled?: boolean) => {
      copyRequestGeneration.current += 1;
      setCopyingRecordId(null);
      setCopyOutcome(null);
      setRecords(nextRecords);
      setState(loadedState(nextRecords.length, historyEnabled));
    },
    [loadedState],
  );

  const applyOutputData = useCallback((
    generation: number,
    historyResult: PromiseSettledResult<HistoryPage>,
    settingsResult: PromiseSettledResult<SettingsView>,
  ) => {
    if (historyRequestGeneration.current !== generation) return;
    setOperationError(null);
    setOperationTitle(null);

    const historyEnabled =
      settingsResult.status === "fulfilled"
        ? settingsResult.value.history_policy.enabled
        : undefined;
    const nextSettings =
      settingsResult.status === "fulfilled" ? settingsResult.value : null;
    setSettings(nextSettings);
    if (nextSettings !== null) {
      setLimitInput(String(nextSettings.history_policy.limit));
      setRetentionInput(
        nextSettings.history_policy.retention_days === null
          ? ""
          : String(nextSettings.history_policy.retention_days),
      );
    }
    if (historyResult.status === "fulfilled") {
      applyHistoryPage(historyResult.value.records, historyEnabled);
    } else {
      setRecords([]);
      setState("unavailable");
    }
  }, [applyHistoryPage]);

  useEffect(() => {
    const generation = historyRequestGeneration.current + 1;
    historyRequestGeneration.current = generation;
    void Promise.allSettled([
      gateway.listHistory(),
      gateway.getSettings(),
    ]).then(([historyResult, settingsResult]) => {
      applyOutputData(generation, historyResult, settingsResult);
    });
    return () => {
      historyRequestGeneration.current += 1;
      policyRequestGeneration.current += 1;
      clearRequestGeneration.current += 1;
    };
  }, [applyOutputData, gateway]);

  const retry = () => {
    setState("loading");
    setOperationError(null);
    setOperationTitle(null);
    const generation = historyRequestGeneration.current + 1;
    historyRequestGeneration.current = generation;
    void Promise.allSettled([
      gateway.listHistory(),
      gateway.getSettings(),
    ]).then(([historyResult, settingsResult]) => {
      applyOutputData(generation, historyResult, settingsResult);
    });
  };

  const copyRecord = async (recordId: string) => {
    if (copyingRecordId !== null) return;

    if (preview.interactive) {
      const selectedIndex = records.findIndex(
        (record) => record.record_id === recordId,
      );
      const failed =
        state === "populated" && selectedIndex === records.length - 1;
      setCopyOutcome({
        recordId,
        status: failed ? "failure" : "success",
      });
      setState(failed ? "copy-failure" : "copy-success");
      return;
    }

    const generation = copyRequestGeneration.current + 1;
    copyRequestGeneration.current = generation;
    setCopyingRecordId(recordId);
    setCopyOutcome(null);
    try {
      await gateway.copyHistoryRecord(recordId);
      if (copyRequestGeneration.current !== generation) return;
      setCopyOutcome({ recordId, status: "success" });
      setState("copy-success");
    } catch {
      if (copyRequestGeneration.current !== generation) return;
      setCopyOutcome({ recordId, status: "failure" });
      setState("copy-failure");
    } finally {
      if (copyRequestGeneration.current === generation) {
        setCopyingRecordId(null);
      }
    }
  };

  const changeHistoryEnabled = async (enabled: boolean) => {
    if (settings === null || policySaving) return;
    const generation = policyRequestGeneration.current + 1;
    policyRequestGeneration.current = generation;
    setPolicySaving(true);
    setOperationError(null);
    setOperationTitle(null);
    try {
      const updated = await gateway.setHistoryEnabled(settings.version, enabled);
      if (policyRequestGeneration.current !== generation) return;
      setSettings(updated);
      if (records.length === 0) {
        setState(updated.history_policy.enabled ? "empty" : "off");
      }
    } catch (error) {
      if (policyRequestGeneration.current !== generation) return;
      setOperationError(
        userFacingGatewayError(
          gateway,
          error,
          "保存历史设置未更新，请稍后重试。",
        ),
      );
      setOperationTitle("保存历史设置未更新");
      try {
        const latest = await gateway.getSettings();
        if (policyRequestGeneration.current === generation) {
          setSettings(latest);
        }
      } catch {
        // 保留最后一次已验证设置；错误状态已经明确显示。
      }
    } finally {
      if (policyRequestGeneration.current === generation) {
        setPolicySaving(false);
      }
    }
  };

  const saveHistoryLimit = async (
    nextLimit: number,
    acknowledgeDataLoss: boolean,
  ) => {
    if (settings === null || policySaving || clearSaving) return;
    const generation = policyRequestGeneration.current + 1;
    policyRequestGeneration.current = generation;
    setPolicySaving(true);
    setOperationError(null);
    setOperationTitle(null);
    try {
      const updated = await gateway.setHistoryLimit(
        settings.version,
        nextLimit,
        acknowledgeDataLoss,
      );
      if (policyRequestGeneration.current !== generation) return;
      setSettings(updated);
      setLimitInput(String(updated.history_policy.limit));
      setPendingLimit(null);

      if (acknowledgeDataLoss) {
        if (preview.interactive) {
          applyHistoryPage(
            records.slice(0, updated.history_policy.limit),
            updated.history_policy.enabled,
          );
        } else {
          try {
            const page = await gateway.listHistory();
            if (policyRequestGeneration.current !== generation) return;
            applyHistoryPage(
              page.records,
              updated.history_policy.enabled,
            );
          } catch (error) {
            if (policyRequestGeneration.current !== generation) return;
            setOperationTitle("保存上限已更新");
            setOperationError(
              userFacingGatewayError(
                gateway,
                error,
                "新上限已保存并应用，但当前无法重新读取历史列表。",
              ),
            );
            setState("unavailable");
          }
        }
      } else {
        setState(loadedState(records.length, updated.history_policy.enabled));
      }
    } catch (error) {
      if (policyRequestGeneration.current !== generation) return;
      setOperationTitle("保存上限未完整应用");
      setOperationError(
        userFacingGatewayError(
          gateway,
          error,
          "请查看重新读取后的设置和记录，再决定是否重试。",
        ),
      );
      setPendingLimit(null);
      const [settingsResult, historyResult] = await Promise.allSettled([
        gateway.getSettings(),
        gateway.listHistory(),
      ]);
      if (policyRequestGeneration.current !== generation) return;
      if (settingsResult.status === "fulfilled") {
        setSettings(settingsResult.value);
        setLimitInput(String(settingsResult.value.history_policy.limit));
      }
      if (historyResult.status === "fulfilled") {
        applyHistoryPage(
          historyResult.value.records,
          settingsResult.status === "fulfilled"
            ? settingsResult.value.history_policy.enabled
            : settings?.history_policy.enabled,
        );
      } else {
        setState("unavailable");
      }
    } finally {
      if (policyRequestGeneration.current === generation) {
        setPolicySaving(false);
      }
    }
  };

  const requestHistoryLimitChange = () => {
    if (settings === null || policySaving || clearSaving) return;
    const nextLimit = parseHistoryLimit(limitInput);
    if (nextLimit === null) {
      setOperationTitle("保存上限格式不正确");
      setOperationError(`请输入 1–${MAX_HISTORY_LIMIT} 之间的整数。`);
      return;
    }
    if (nextLimit === settings.history_policy.limit) return;
    if (!settings.history_policy.enabled) return;
    setOperationError(null);
    setOperationTitle(null);
    if (records.length > nextLimit) {
      setPendingLimit(nextLimit);
      setState("limit-confirm");
      return;
    }
    void saveHistoryLimit(nextLimit, false);
  };

  const saveHistoryRetention = async (
    nextRetentionDays: number | null,
    acknowledgeDataLoss: boolean,
  ) => {
    if (settings === null || policySaving || clearSaving) return;
    const generation = policyRequestGeneration.current + 1;
    policyRequestGeneration.current = generation;
    setPolicySaving(true);
    setOperationError(null);
    setOperationTitle(null);
    try {
      const updated = await gateway.setHistoryRetention(
        settings.version,
        nextRetentionDays,
        acknowledgeDataLoss,
      );
      if (policyRequestGeneration.current !== generation) return;
      setSettings(updated);
      setRetentionInput(
        updated.history_policy.retention_days === null
          ? ""
          : String(updated.history_policy.retention_days),
      );
      setPendingRetention(null);

      if (acknowledgeDataLoss) {
        const page = await gateway.listHistory();
        if (policyRequestGeneration.current !== generation) return;
        applyHistoryPage(page.records, updated.history_policy.enabled);
      } else {
        setState(loadedState(records.length, updated.history_policy.enabled));
      }
    } catch (error) {
      if (policyRequestGeneration.current !== generation) return;
      setOperationTitle("保存期限未完整应用");
      setOperationError(
        userFacingGatewayError(
          gateway,
          error,
          "请查看重新读取后的设置和记录，再决定是否重试。",
        ),
      );
      setPendingRetention(null);
      const [settingsResult, historyResult] = await Promise.allSettled([
        gateway.getSettings(),
        gateway.listHistory(),
      ]);
      if (policyRequestGeneration.current !== generation) return;
      if (settingsResult.status === "fulfilled") {
        setSettings(settingsResult.value);
        setRetentionInput(
          settingsResult.value.history_policy.retention_days === null
            ? ""
            : String(settingsResult.value.history_policy.retention_days),
        );
      }
      if (historyResult.status === "fulfilled") {
        applyHistoryPage(
          historyResult.value.records,
          settingsResult.status === "fulfilled"
            ? settingsResult.value.history_policy.enabled
            : settings?.history_policy.enabled,
        );
      } else {
        setState("unavailable");
      }
    } finally {
      if (policyRequestGeneration.current === generation) {
        setPolicySaving(false);
      }
    }
  };

  const requestHistoryRetentionChange = (candidate = retentionInput) => {
    if (settings === null || policySaving || clearSaving) return;
    const parsed = parseHistoryRetention(candidate);
    if (!parsed.valid) {
      setOperationTitle("保存期限格式不正确");
      setOperationError(
        `请留空，或输入 1–${MAX_HISTORY_RETENTION_DAYS} 之间的整数天数。`,
      );
      return;
    }
    if (parsed.days === settings.history_policy.retention_days) return;
    if (!settings.history_policy.enabled) return;
    setOperationError(null);
    setOperationTitle(null);

    if (parsed.days !== null) {
      const cutoff = Date.now() - parsed.days * DAY_MS;
      const estimatedExpiredCount = records.filter(
        (record) => Date.parse(record.created_at) < cutoff,
      ).length;
      if (estimatedExpiredCount > 0) {
        setPendingRetention({
          days: parsed.days,
          estimatedExpiredCount,
        });
        setState("retention-confirm");
        return;
      }
    }
    void saveHistoryRetention(parsed.days, false);
  };

  const cancelLimitChange = () => {
    setPendingLimit(null);
    if (settings !== null) {
      setLimitInput(String(settings.history_policy.limit));
    }
    setState(loadedState(records.length, settings?.history_policy.enabled));
  };

  const cancelRetentionChange = () => {
    setPendingRetention(null);
    if (settings !== null) {
      setRetentionInput(
        settings.history_policy.retention_days === null
          ? ""
          : String(settings.history_policy.retention_days),
      );
    }
    setState(loadedState(records.length, settings?.history_policy.enabled));
  };

  const openClearConfirmation = () => {
    if (records.length === 0 || policySaving || clearSaving) return;
    setOperationError(null);
    setOperationTitle(null);
    setState("clear");
  };

  const cancelClear = () => {
    if (clearSaving) return;
    setState(loadedState(records.length, settings?.history_policy.enabled));
  };

  const confirmClear = async () => {
    if (clearSaving || policySaving) return;
    if (preview.interactive) {
      setRecords([]);
      setState(settings?.history_policy.enabled === false ? "off" : "empty");
      return;
    }

    const generation = clearRequestGeneration.current + 1;
    clearRequestGeneration.current = generation;
    setClearSaving(true);
    setOperationError(null);
    setOperationTitle(null);
    try {
      await gateway.clearAllHistory();
      if (clearRequestGeneration.current !== generation) return;
      applyHistoryPage([], settings?.history_policy.enabled);
    } catch (error) {
      if (clearRequestGeneration.current !== generation) return;
      setOperationTitle("输出历史未清除");
      setOperationError(
        userFacingGatewayError(
          gateway,
          error,
          "输出历史未清除，请稍后重试。",
        ),
      );
      setState(loadedState(records.length, settings?.history_policy.enabled));
    } finally {
      if (clearRequestGeneration.current === generation) {
        setClearSaving(false);
      }
    }
  };

  const visibleState =
    state === "empty" && settings?.history_policy.enabled === false
      ? "off"
      : state;
  let content: ReactNode;
  if (visibleState === "loading") {
    content = <LoadingOutput />;
  } else if (visibleState === "unavailable") {
    content = (
      <UnavailableOutput
        onRetry={retry}
      />
    );
  } else if (visibleState === "clear") {
    content = (
      <ClearConfirmation
        interactive={!clearSaving && !policySaving}
        confirming={clearSaving}
        onCancel={cancelClear}
        onConfirm={() => void confirmClear()}
      />
    );
  } else if (visibleState === "limit-confirm") {
    content = pendingLimit === null ? (
      <LoadingOutput />
    ) : (
      <LimitConfirmation
        currentCount={records.length}
        nextLimit={pendingLimit}
        interactive={!policySaving && !clearSaving}
        confirming={policySaving}
        onCancel={cancelLimitChange}
        onConfirm={() => void saveHistoryLimit(pendingLimit, true)}
      />
    );
  } else if (visibleState === "retention-confirm") {
    content = pendingRetention === null ? (
      <LoadingOutput />
    ) : (
      <RetentionConfirmation
        currentCount={records.length}
        expiredCount={pendingRetention.estimatedExpiredCount}
        nextRetentionDays={pendingRetention.days}
        interactive={!policySaving && !clearSaving}
        confirming={policySaving}
        onCancel={cancelRetentionChange}
        onConfirm={() => void saveHistoryRetention(pendingRetention.days, true)}
      />
    );
  } else {
    content = (
      <StandardOutput
        records={records}
        state={visibleState}
        historyActionsEnabled={records.length > 0}
        historyPolicy={settings?.history_policy ?? null}
        policyInteractive={settings !== null && !policySaving}
        policySaving={policySaving}
        copyingRecordId={copyingRecordId}
        copyOutcome={copyOutcome}
        onCopy={(recordId) => void copyRecord(recordId)}
        onToggleHistory={(enabled) => void changeHistoryEnabled(enabled)}
        limitInput={limitInput}
        onLimitInput={setLimitInput}
        onLimitApply={requestHistoryLimitChange}
        retentionInput={retentionInput}
        onRetentionChange={(value) => {
          setRetentionInput(value);
          requestHistoryRetentionChange(value);
        }}
      />
    );
  }

  return (
    <div className="output-page" data-state={visibleState}>
      <OutputHeader
        state={visibleState}
        clearEnabled={
          records.length > 0 &&
          !policySaving &&
          !clearSaving &&
          copyingRecordId === null
        }
        onClear={openClearConfirmation}
        onRetry={retry}
      />
      {visibleState === "loading" ? null : (
        <OutputStatus
          state={visibleState}
          interactive={preview.interactive}
          recordCount={records.length}
          operationError={operationError}
          operationTitle={operationTitle}
        />
      )}
      {content}
    </div>
  );
}
