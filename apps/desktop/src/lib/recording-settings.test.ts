import assert from "node:assert/strict";
import test from "node:test";

import {
  classifyShortcutKeyboardInput,
  formatShortcutBinding,
  isPureModifierShortcutBinding,
} from "../features/recording/shortcut-binding.ts";

void test("shortcut capture preserves combinations and allows bare function keys", () => {
  assert.deepEqual(
    classifyShortcutKeyboardInput({
      code: "KeyR",
      metaKey: true,
      ctrlKey: false,
      altKey: true,
      shiftKey: false,
    }),
    { kind: "accepted", binding: "Command+Alt+KeyR" },
  );
  assert.deepEqual(
    classifyShortcutKeyboardInput({
      code: "F20",
      metaKey: false,
      ctrlKey: false,
      altKey: false,
      shiftKey: false,
    }),
    { kind: "accepted", binding: "F20" },
  );
});

void test("shortcut capture defers a pure modifier until release", () => {
  assert.deepEqual(
    classifyShortcutKeyboardInput({
      code: "ShiftLeft",
      metaKey: false,
      ctrlKey: false,
      altKey: false,
      shiftKey: true,
    }),
    { kind: "modifier_candidate", binding: "ShiftLeft" },
  );
  assert.deepEqual(
    classifyShortcutKeyboardInput({
      code: "ShiftLeft",
      metaKey: true,
      ctrlKey: false,
      altKey: false,
      shiftKey: true,
    }),
    { kind: "waiting" },
  );
});

void test("daily input and navigation keys cannot be registered alone", () => {
  for (const code of ["KeyR", "Digit4", "Space", "Enter", "ArrowLeft", "F21"]) {
    assert.deepEqual(
      classifyShortcutKeyboardInput({
        code,
        metaKey: false,
        ctrlKey: false,
        altKey: false,
        shiftKey: false,
      }),
      { kind: "rejected_daily_key" },
    );
  }
});

void test("shortcut display uses familiar macOS symbols without changing stored syntax", () => {
  assert.equal(formatShortcutBinding("Command+Alt+Shift+KeyR"), "⌘ ⌥ ⇧ R");
  assert.equal(formatShortcutBinding("MetaLeft"), "左 ⌘");
  assert.equal(formatShortcutBinding("ShiftRight"), "右 ⇧");
  assert.equal(formatShortcutBinding("F13"), "F13");
  assert.equal(formatShortcutBinding(null), "未绑定");
  assert.equal(isPureModifierShortcutBinding("MetaLeft"), true);
  assert.equal(isPureModifierShortcutBinding("Command+KeyR"), false);
});
