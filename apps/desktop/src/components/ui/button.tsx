import {
  type ButtonHTMLAttributes,
  type ReactNode,
} from "react";

import { classes } from "@/lib/classes";

export type ButtonVariant =
  | "primary"
  | "outline"
  | "ghost"
  | "destructive";
export type ButtonSize = "standard" | "compact" | "icon";

const variantClasses: Record<ButtonVariant, string> = {
  primary:
    "border-foreground bg-foreground text-background hover:opacity-88",
  outline:
    "border-border-strong bg-surface text-foreground hover:bg-surface-soft",
  ghost:
    "border-transparent bg-transparent text-foreground hover:bg-surface-soft",
  destructive:
    "border-destructive bg-destructive text-white hover:opacity-88",
};

const sizeClasses: Record<ButtonSize, string> = {
  standard: "h-10 rounded-pill px-5 text-label",
  compact: "h-8 rounded-pill px-4 text-caption",
  icon: "size-10 rounded-full p-0",
};

export interface ButtonProps
  extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
  size?: ButtonSize;
  leadingIcon?: ReactNode;
}

export function Button({
  className,
  variant = "outline",
  size = "standard",
  leadingIcon,
  children,
  type = "button",
  ...props
}: ButtonProps) {
  return (
    <button
      type={type}
      className={classes(
        "inline-flex shrink-0 items-center justify-center gap-2 border font-medium whitespace-nowrap transition-[background-color,border-color,color,opacity,transform] duration-fast ease-standard disabled:pointer-events-none disabled:opacity-40 active:not-disabled:scale-[0.98]",
        variantClasses[variant],
        sizeClasses[size],
        className,
      )}
      {...props}
    >
      {leadingIcon}
      {children}
    </button>
  );
}

export function IconButton({
  children,
  ...props
}: Omit<ButtonProps, "size"> & { "aria-label": string }) {
  return (
    <Button size="icon" {...props}>
      {children}
    </Button>
  );
}
