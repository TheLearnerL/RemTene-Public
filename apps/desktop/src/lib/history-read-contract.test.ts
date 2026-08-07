import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

function readSource(relativePath: string): string {
  return readFileSync(new URL(relativePath, import.meta.url), "utf8");
}

const ipcSource = readSource("./ipc.ts");
const gatewaySource = readSource("../backend/gateway.ts");
const outputSource = readSource("../pages/OutputPage.tsx");
const previewSource = readSource(
  "../features/status/status-preview.ts",
);

void test("history IPC sends only the versioned query and validates the returned page", () => {
  assert.match(
    ipcSource,
    /invoke<unknown>\("history_list", \{ query \}\)/,
  );
  assert.match(
    ipcSource,
    /hasExactKeys\(value, \["contract_version"\]\)/,
  );
  assert.match(
    ipcSource,
    /hasExactKeys\(value, \["contract_version", "records"\]\)/,
  );
  assert.match(
    ipcSource,
    /hasExactKeys\(value, \["record_id", "final_text", "created_at"\]\)/,
  );
  assert.match(gatewaySource, /listHistory\(\): Promise<HistoryPage>/);
  assert.match(
    gatewaySource,
    /copyHistoryRecord\(recordId: string\): Promise<HistoryCopyResult>/,
  );
  assert.match(
    ipcSource,
    /invoke<unknown>\("history_copy", \{ command \}\)/,
  );
  assert.match(
    ipcSource,
    /hasExactKeys\(value, \["contract_version", "request_id", "record_id"\]\)/,
  );
});

void test("output renders backend order with an invisible record identity key", () => {
  assert.match(outputSource, /records\.map\(\(record, index\) =>/);
  assert.match(outputSource, /key=\{record\.record_id\}/);
  assert.match(outputSource, /\{record\.final_text\}/);
  assert.match(
    outputSource,
    /formatHistoryCreatedAt\(record\.created_at\)/,
  );
  assert.match(outputSource, /new Intl\.DateTimeFormat\("zh-CN"/);
  assert.ok((outputSource.match(/record\.record_id/g)?.length ?? 0) >= 1);
  assert.doesNotMatch(outputSource, /<[^>]+>\{record\.record_id\}/);
  assert.doesNotMatch(outputSource, /明天下午三点继续评审/);
  assert.match(outputSource, /gateway\.copyHistoryRecord\(recordId\)/);
  assert.doesNotMatch(outputSource, /navigator\.clipboard/);
});

void test("empty production copy does not claim that history saving is enabled", () => {
  assert.match(
    outputSource,
    /if \(!interactive\) \{\s*return \{\s*title: "还没有历史记录",\s*detail: "完成一次输入后，最终文字会出现在这里。"/,
  );
  assert.match(outputSource, /\? "已保存 10 条最终文字"/);
  assert.match(
    outputSource,
    /: `已保存 \$\{recordCount\} 条最终文字`/,
  );
});

void test("all output previews provide history through the shared gateway stub", () => {
  for (const preview of [
    "output-populated",
    "output-off",
    "output-unavailable",
    "output-clear",
    "output-empty",
    "output-copy-success",
    "output-copy-failure",
  ]) {
    assert.equal(previewSource.includes(preview), true, `missing ${preview}`);
  }
  assert.match(previewSource, /function previewHistoryPage/);
  assert.match(previewSource, /listHistory:[\s\S]*output preview unavailable/);
  assert.match(
    previewSource,
    /listHistory: \(\) =>\s*Promise\.resolve<HistoryPage>/,
  );
});
