export interface ShortcutKeyboardInput {
  code: string;
  metaKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
}

const MODIFIER_CODES = new Set([
  "MetaLeft",
  "MetaRight",
  "ControlLeft",
  "ControlRight",
  "AltLeft",
  "AltRight",
  "ShiftLeft",
  "ShiftRight",
]);

const SAFE_BARE_KEY_CODE = /^F(?:[1-9]|1[0-9]|20)$/;

export type ShortcutCaptureResult =
  | { kind: "accepted"; binding: string }
  | { kind: "modifier_candidate"; binding: string }
  | { kind: "waiting" }
  | { kind: "rejected_daily_key" };

export function isPureModifierShortcutBinding(
  binding: string | null,
): boolean {
  return binding !== null && MODIFIER_CODES.has(binding);
}

/**
 * 把浏览器物理键码分类为产品允许的快捷键。
 *
 * 纯修饰键先作为候选，必须等到 keyup 才确认；否则用户刚按下 ⌘，还没来得及
 * 继续输入 ⌘R，就会被提前保存成单独的 ⌘。
 */
export function classifyShortcutKeyboardInput(
  input: ShortcutKeyboardInput,
): ShortcutCaptureResult {
  if (!input.code || input.code === "Unidentified") {
    return { kind: "waiting" };
  }

  const modifiers: string[] = [];
  if (input.metaKey) modifiers.push("Command");
  if (input.ctrlKey) modifiers.push("Control");
  if (input.altKey) modifiers.push("Alt");
  if (input.shiftKey) modifiers.push("Shift");

  if (MODIFIER_CODES.has(input.code)) {
    return modifiers.length <= 1
      ? { kind: "modifier_candidate", binding: input.code }
      : { kind: "waiting" };
  }

  if (modifiers.length > 0) {
    return {
      kind: "accepted",
      binding: [...modifiers, input.code].join("+"),
    };
  }

  if (SAFE_BARE_KEY_CODE.test(input.code)) {
    return { kind: "accepted", binding: input.code };
  }

  return { kind: "rejected_daily_key" };
}

export function formatShortcutBinding(binding: string | null): string {
  if (binding === null) return "未绑定";
  return binding
    .split("+")
    .map((token) => {
      switch (token.toLowerCase()) {
        case "metaleft":
          return "左 ⌘";
        case "metaright":
          return "右 ⌘";
        case "controlleft":
          return "左 ⌃";
        case "controlright":
          return "右 ⌃";
        case "altleft":
          return "左 ⌥";
        case "altright":
          return "右 ⌥";
        case "shiftleft":
          return "左 ⇧";
        case "shiftright":
          return "右 ⇧";
        case "command":
        case "super":
          return "⌘";
        case "control":
          return "⌃";
        case "alt":
        case "option":
          return "⌥";
        case "shift":
          return "⇧";
        case "space":
          return "Space";
        default:
          if (/^Key[A-Z]$/i.test(token)) return token.slice(3).toUpperCase();
          if (/^Digit[0-9]$/i.test(token)) return token.slice(5);
          return token;
      }
    })
    .join(" ");
}
