import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

function readSource(relativePath: string): string {
  return readFileSync(new URL(relativePath, import.meta.url), "utf8");
}

void test("Renderer exposes one narrow ASR health command without model inputs", () => {
  const ipc = readSource("./ipc.ts");
  const gateway = readSource("../backend/gateway.ts");

  assert.match(ipc, /invoke<AppSnapshot>\("model_check_health"\)/);
  assert.match(gateway, /checkAsrHealth\(\): Promise<AppSnapshot>/);
  assert.match(gateway, /checkAsrHealth,/);
  assert.doesNotMatch(ipc, /model_check_health"\s*,\s*\{/);
});

void test("model switching accepts only the closed engine enum and returns a refreshed snapshot", () => {
  const ipc = readSource("./ipc.ts");
  const gateway = readSource("../backend/gateway.ts");
  const preview = readSource("../features/status/status-preview.ts");

  assert.match(ipc, /type LocalAsrModel = "qwen" \| "whisper"/);
  assert.match(
    ipc,
    /invoke<AppSnapshot>\("model_switch_engine", \{ engine \}\)/,
  );
  assert.match(
    gateway,
    /switchAsrModel\(engine: LocalAsrModel\): Promise<AppSnapshot>/,
  );
  assert.match(gateway, /switchAsrModel,/);
  assert.match(preview, /switchAsrModel: \(engine: LocalAsrModel\)/);
  assert.doesNotMatch(ipc, /model_switch_engine[\s\S]{0,100}(path|model_id)/);
});

void test("startup health completion refreshes mounted control-panel views", () => {
  const ipc = readSource("./ipc.ts");
  const gateway = readSource("../backend/gateway.ts");
  const dashboard = readSource("../features/status/useStatusDashboard.ts");
  const model = readSource("../pages/ModelPage.tsx");

  assert.match(ipc, /APP_SNAPSHOT_CHANGED_EVENT = "app:snapshot-changed"/);
  assert.match(gateway, /listenToAppSnapshotChanged/);
  assert.match(dashboard, /gateway\.listenToAppSnapshotChanged/);
  assert.match(model, /gateway\s*\.listenToAppSnapshotChanged/);
});

void test("Renderer opens only the application-owned model directory and previews stay inert", () => {
  const ipc = readSource("./ipc.ts");
  const gateway = readSource("../backend/gateway.ts");
  const preview = readSource("../features/status/status-preview.ts");

  assert.match(ipc, /invoke<void>\("model_open_directory"\)/);
  assert.doesNotMatch(ipc, /model_open_directory"\s*,\s*\{/);
  assert.match(gateway, /openModelDirectory\(\): Promise<void>/);
  assert.match(gateway, /openModelDirectory,/);
  assert.match(
    preview,
    /openModelDirectory: \(\) => Promise\.resolve\(\)/,
  );
});

void test("ordinary dashboard refresh remains read-only", () => {
  const dashboard = readSource("../features/status/useStatusDashboard.ts");
  const effect = dashboard.slice(
    dashboard.indexOf("useEffect(() =>"),
    dashboard.indexOf("return {", dashboard.indexOf("useEffect(() =>")),
  );

  assert.match(effect, /safeRefresh\(\)/);
  assert.doesNotMatch(effect, /checkAsrHealth\(\)/);
  assert.match(dashboard, /const checkAsrHealth = useCallback/);
  assert.match(dashboard, /gateway\.checkAsrHealth\(\)/);
});

void test("status actions explicitly check health and previews stay inert", () => {
  const status = readSource("../pages/StatusPage.tsx");
  const preview = readSource("../features/status/status-preview.ts");

  assert.match(status, /const recheck = async/);
  assert.match(status, /dashboard\.checkAsrHealth\(\)/);
  assert.match(status, /onClick=\{\(\) => void recheck\(\)\}/);
  assert.match(status, /dashboard\.healthErrorMessage \? "alert" : undefined/);
  assert.match(preview, /checkAsrHealth: \(\) => Promise\.resolve/);
});
