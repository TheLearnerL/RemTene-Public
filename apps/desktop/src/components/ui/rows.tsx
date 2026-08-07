import {
  type HTMLAttributes,
  type ReactNode,
} from "react";

import { classes } from "@/lib/classes";

export type StatusTone =
  | "neutral"
  | "success"
  | "warning"
  | "error"
  | "processing";

const toneClasses: Record<StatusTone, string> = {
  neutral: "bg-foreground-muted",
  success: "bg-success",
  warning: "bg-warning",
  error: "bg-destructive",
  processing: "bg-processing",
};

export function StatusRow({
  label,
  value,
  description,
  tone = "neutral",
  action,
  className,
}: {
  label: string;
  value: string;
  description?: string;
  tone?: StatusTone;
  action?: ReactNode;
  className?: string;
}) {
  return (
    <div
      className={classes(
        "flex min-h-18 items-center gap-3 border-b border-border px-4 py-3 last:border-b-0",
        className,
      )}
    >
      <span
        className={classes("size-2 shrink-0 rounded-full", toneClasses[tone])}
        aria-hidden="true"
      />
      <div className="min-w-0 flex-1">
        <p className="text-label">{label}</p>
        <p className="mt-0.5 text-caption text-foreground-muted">
          <span className="text-foreground">{value}</span>
          {description ? ` · ${description}` : ""}
        </p>
      </div>
      {action}
    </div>
  );
}

export function SettingRow({
  title,
  description,
  control,
  className,
  ...props
}: HTMLAttributes<HTMLDivElement> & {
  title: string;
  description?: string;
  control: ReactNode;
}) {
  return (
    <div
      className={classes(
        "flex min-h-18 items-center justify-between gap-6 border-b border-border px-5 py-4 last:border-b-0",
        className,
      )}
      {...props}
    >
      <div className="min-w-0">
        <p className="text-label">{title}</p>
        {description ? (
          <p className="mt-1 max-w-[48rem] text-caption text-foreground-muted">
            {description}
          </p>
        ) : null}
      </div>
      <div className="shrink-0">{control}</div>
    </div>
  );
}
