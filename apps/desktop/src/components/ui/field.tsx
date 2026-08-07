import {
  type InputHTMLAttributes,
  type SelectHTMLAttributes,
} from "react";

import { classes } from "@/lib/classes";

interface FieldChrome {
  label: string;
  description?: string;
  error?: string;
}

export function TextField({
  label,
  description,
  error,
  className,
  id,
  ...props
}: FieldChrome & InputHTMLAttributes<HTMLInputElement>) {
  const controlId = id ?? props.name;
  const descriptionId = controlId ? `${controlId}-description` : undefined;
  return (
    <label className="grid gap-1.5 text-body">
      <span className="font-medium">{label}</span>
      <input
        id={controlId}
        className={classes(
          "h-10 min-w-0 rounded-control border bg-surface px-3 text-body text-foreground outline-none transition-colors duration-fast placeholder:text-foreground-muted/70 disabled:cursor-not-allowed disabled:opacity-45",
          error ? "border-destructive" : "border-border",
          className,
        )}
        aria-invalid={error ? true : undefined}
        aria-describedby={description || error ? descriptionId : undefined}
        {...props}
      />
      {description || error ? (
        <span
          id={descriptionId}
          className={classes(
            "text-caption",
            error ? "text-destructive" : "text-foreground-muted",
          )}
        >
          {error ?? description}
        </span>
      ) : null}
    </label>
  );
}

export interface SelectOption {
  value: string;
  label: string;
}

export function SelectField({
  label,
  description,
  error,
  options,
  className,
  id,
  ...props
}: FieldChrome &
  SelectHTMLAttributes<HTMLSelectElement> & {
    options: SelectOption[];
  }) {
  const controlId = id ?? props.name;
  const descriptionId = controlId ? `${controlId}-description` : undefined;
  return (
    <label className="grid gap-1.5 text-body">
      <span className="font-medium">{label}</span>
      <span className="relative">
        <select
          id={controlId}
          className={classes(
            "h-10 w-full appearance-none rounded-control border bg-surface px-3 pr-10 text-body text-foreground outline-none transition-colors duration-fast disabled:cursor-not-allowed disabled:opacity-45",
            error ? "border-destructive" : "border-border",
            className,
          )}
          aria-invalid={error ? true : undefined}
          aria-describedby={description || error ? descriptionId : undefined}
          {...props}
        >
          {options.map((option) => (
            <option key={option.value} value={option.value}>
              {option.label}
            </option>
          ))}
        </select>
        <svg
          className="pointer-events-none absolute top-1/2 right-3 size-4 -translate-y-1/2 text-foreground-muted"
          viewBox="0 0 16 16"
          aria-hidden="true"
        >
          <path
            d="m4.5 6 3.5 3.5L11.5 6"
            fill="none"
            stroke="currentColor"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        </svg>
      </span>
      {description || error ? (
        <span
          id={descriptionId}
          className={classes(
            "text-caption",
            error ? "text-destructive" : "text-foreground-muted",
          )}
        >
          {error ?? description}
        </span>
      ) : null}
    </label>
  );
}
