import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

function readSource(relativePath: string): string {
  return readFileSync(new URL(relativePath, import.meta.url), "utf8");
}

const ipcSource = readSource("./ipc.ts");
const gatewaySource = readSource("../backend/gateway.ts");
const temporaryTextSource = readSource("../app/TemporaryTextBox.tsx");
const previewSource = readSource(
  "../features/status/status-preview.ts",
);

void test("temporary text copy invokes only the closed delivery command", () => {
  assert.match(
    ipcSource,
    /invoke<unknown>\("temporary_text_copy_all", \{ command \}\)/,
  );
  assert.match(
    ipcSource,
    /hasExactKeys\(value, \["contract_version", "delivery_id"\]\)/,
  );
  assert.match(
    gatewaySource,
    /copyTemporaryText\(deliveryId: string\): Promise<TemporaryTextCopyResult>/,
  );
});

void test("production and DEV preview use the same gateway capability", () => {
  assert.match(
    temporaryTextSource,
    /gateway\.copyTemporaryText\(deliveryId\)/,
  );
  assert.doesNotMatch(temporaryTextSource, /import\.meta\.env\.DEV/);
  assert.doesNotMatch(temporaryTextSource, /navigator\.clipboard/);

  for (const preview of [
    "not-inserted",
    "temporary-not-inserted",
    "temporary-indeterminate",
    "temporary-llm-fallback",
  ]) {
    assert.equal(previewSource.includes(preview), true, `missing ${preview}`);
  }
  assert.match(
    previewSource,
    /copyTemporaryText:[\s\S]*delivery_id: deliveryId/,
  );
  assert.match(
    previewSource,
    /getPendingTemporaryText: \(\) => Promise\.resolve\(delivery\)/,
  );
});
