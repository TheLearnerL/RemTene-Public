import { type SVGProps } from "react";

type IconProps = SVGProps<SVGSVGElement>;

function IconFrame({
  children,
  ...props
}: IconProps) {
  return (
    <svg
      viewBox="0 0 24 24"
      width="20"
      height="20"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.7"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      {...props}
    >
      {children}
    </svg>
  );
}

export function StatusIcon(props: IconProps) {
  return (
    <IconFrame {...props}>
      <path d="M5 12a7 7 0 1 1 14 0 7 7 0 0 1-14 0Z" />
      <path d="m9.2 12 1.8 1.8 3.9-4.1" />
    </IconFrame>
  );
}

export function RecordingIcon(props: IconProps) {
  return (
    <IconFrame {...props}>
      <rect x="8" y="3.5" width="8" height="12" rx="4" />
      <path d="M5.5 11.5a6.5 6.5 0 0 0 13 0M12 18v3M9 21h6" />
    </IconFrame>
  );
}

export function ModelIcon(props: IconProps) {
  return (
    <IconFrame {...props}>
      <path d="M7 4.5h10a2.5 2.5 0 0 1 2.5 2.5v10a2.5 2.5 0 0 1-2.5 2.5H7A2.5 2.5 0 0 1 4.5 17V7A2.5 2.5 0 0 1 7 4.5Z" />
      <path d="M9 9h6M9 12h6M9 15h3" />
    </IconFrame>
  );
}

export function OutputIcon(props: IconProps) {
  return (
    <IconFrame {...props}>
      <path d="M7 3.5h7l4 4V20H7V3.5Z" />
      <path d="M14 3.5V8h4M10 12h5M10 15h5" />
    </IconFrame>
  );
}

export function SystemIcon(props: IconProps) {
  return (
    <IconFrame {...props}>
      <path d="M12 8.5a3.5 3.5 0 1 1 0 7 3.5 3.5 0 0 1 0-7Z" />
      <path d="m19 13.2 1.4 1.1-1.7 3-1.7-.7a7.7 7.7 0 0 1-2.2 1.3l-.3 1.8h-3.4l-.3-1.8a7.7 7.7 0 0 1-2.2-1.3l-1.7.7-1.7-3 1.4-1.1a7.6 7.6 0 0 1 0-2.4L5.2 9.7l1.7-3 1.7.7a7.7 7.7 0 0 1 2.2-1.3l.3-1.8h3.4l.3 1.8A7.7 7.7 0 0 1 17 7.4l1.7-.7 1.7 3-1.4 1.1a7.6 7.6 0 0 1 0 2.4Z" />
    </IconFrame>
  );
}

export function ArrowRightIcon(props: IconProps) {
  return (
    <IconFrame {...props}>
      <path d="M5 12h13M14 8l4 4-4 4" />
    </IconFrame>
  );
}

export function RefreshIcon(props: IconProps) {
  return (
    <IconFrame {...props}>
      <path d="M19 8a7.5 7.5 0 1 0 .2 7.6M19 4v4h-4" />
    </IconFrame>
  );
}

export function CloseIcon(props: IconProps) {
  return (
    <IconFrame {...props}>
      <path d="m7 7 10 10M17 7 7 17" />
    </IconFrame>
  );
}

export function CheckIcon(props: IconProps) {
  return (
    <IconFrame {...props}>
      <path d="m5 12.5 4.2 4L19 7" />
    </IconFrame>
  );
}
