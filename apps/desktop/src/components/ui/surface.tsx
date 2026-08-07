import {
  type HTMLAttributes,
  type ReactNode,
} from "react";

import { classes } from "@/lib/classes";

export function Section({
  title,
  description,
  action,
  children,
  className,
  ...props
}: HTMLAttributes<HTMLElement> & {
  title?: string;
  description?: string;
  action?: ReactNode;
}) {
  return (
    <section
      className={classes(
        "rounded-panel border border-border bg-surface shadow-panel",
        className,
      )}
      {...props}
    >
      {title || description || action ? (
        <header className="flex min-h-18 items-start justify-between gap-4 border-b border-border px-5 py-4">
          <div className="min-w-0">
            {title ? <h2 className="text-section">{title}</h2> : null}
            {description ? (
              <p className="mt-1 text-caption text-foreground-muted">
                {description}
              </p>
            ) : null}
          </div>
          {action}
        </header>
      ) : null}
      {children}
    </section>
  );
}

export function ScrollableList({
  title,
  description,
  children,
  className,
}: {
  title: string;
  description?: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <section
      className={classes(
        "flex max-h-[244px] min-h-0 flex-col overflow-hidden rounded-[16px] border border-border bg-surface",
        className,
      )}
    >
      <header className="shrink-0 border-b border-border px-4 py-3">
        <h3 className="text-label">{title}</h3>
        {description ? (
          <p className="mt-1 text-caption text-foreground-muted">{description}</p>
        ) : null}
      </header>
      <div
        className="remtene-scroll min-h-0 flex-1 overflow-y-auto overscroll-y-auto py-2"
        tabIndex={0}
        aria-label={`${title}列表`}
      >
        {children}
      </div>
    </section>
  );
}
