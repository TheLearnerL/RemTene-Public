import { type ButtonHTMLAttributes } from "react";

import { classes } from "@/lib/classes";

export interface SwitchProps
  extends Omit<
    ButtonHTMLAttributes<HTMLButtonElement>,
    "onChange" | "onClick"
  > {
  checked: boolean;
  onCheckedChange?: (checked: boolean) => void;
}

export function Switch({
  checked,
  onCheckedChange,
  className,
  disabled,
  ...props
}: SwitchProps) {
  return (
    <button
      {...props}
      type="button"
      role="switch"
      aria-checked={checked}
      disabled={disabled}
      className={classes(
        "relative h-6 w-11 shrink-0 rounded-full border transition-colors duration-base ease-standard disabled:cursor-not-allowed disabled:opacity-40",
        checked
          ? "border-accent bg-accent"
          : "border-border-strong bg-surface-soft",
        className,
      )}
      onClick={() => onCheckedChange?.(!checked)}
    >
      <span
        className={classes(
          "absolute top-0.5 left-0.5 size-[18px] rounded-full bg-surface shadow-control transition-transform duration-base ease-standard",
          checked && "translate-x-5",
        )}
        aria-hidden="true"
      />
    </button>
  );
}
