import { type ReactNode } from "react";

export function PageHeader({
  eyebrow,
  title,
  description,
  action,
}: {
  eyebrow: string;
  title: string;
  description: string;
  action?: ReactNode;
}) {
  return (
    <header className="flex min-h-[116px] items-start justify-between gap-6 pb-6">
      <div className="min-w-0">
        <p className="text-caption font-medium tracking-[0.08em] text-foreground-muted">
          {eyebrow}
        </p>
        <h1 className="mt-2 text-page">{title}</h1>
        <p className="mt-2 max-w-[42rem] text-body text-foreground-muted">
          {description}
        </p>
      </div>
      {action ? <div className="shrink-0 pt-1">{action}</div> : null}
    </header>
  );
}
