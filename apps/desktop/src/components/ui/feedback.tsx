import { type ReactNode } from "react";

import { classes } from "@/lib/classes";

export type FeedbackTone = "neutral" | "success" | "warning" | "error";

const toneClasses: Record<FeedbackTone, string> = {
  neutral: "border-border bg-surface-soft text-foreground",
  success: "border-success/35 bg-success/8 text-success",
  warning: "border-warning/35 bg-warning/8 text-warning",
  error: "border-destructive/35 bg-destructive/8 text-destructive",
};

export function Feedback({
  title,
  children,
  tone = "neutral",
  action,
  className,
}: {
  title: string;
  children?: ReactNode;
  tone?: FeedbackTone;
  action?: ReactNode;
  className?: string;
}) {
  return (
    <div
      className={classes(
        "flex min-h-22 items-start justify-between gap-4 rounded-control border p-4",
        toneClasses[tone],
        className,
      )}
      role={tone === "error" ? "alert" : "status"}
    >
      <div className="min-w-0">
        <p className="text-label">{title}</p>
        {children ? (
          <div className="mt-1 text-caption leading-5 opacity-80">{children}</div>
        ) : null}
      </div>
      {action}
    </div>
  );
}

export function EmptyState({
  title,
  children,
  action,
}: {
  title: string;
  children: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="grid min-h-52 place-items-center rounded-panel border border-dashed border-border bg-surface px-8 py-10 text-center">
      <div className="max-w-md">
        <h2 className="text-section">{title}</h2>
        <div className="mt-2 text-body text-foreground-muted">{children}</div>
        {action ? <div className="mt-5">{action}</div> : null}
      </div>
    </div>
  );
}

export function ErrorState({
  title,
  children,
  action,
}: {
  title: string;
  children: ReactNode;
  action?: ReactNode;
}) {
  return (
    <Feedback title={title} tone="error" action={action}>
      {children}
    </Feedback>
  );
}
