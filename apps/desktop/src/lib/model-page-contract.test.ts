import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const modelPageSource = readFileSync(
  new URL("../pages/ModelPage.tsx", import.meta.url),
  "utf8",
);
const llmPanelSource = readFileSync(
  new URL("../app/LlmSettingsPanel.tsx", import.meta.url),
  "utf8",
);
const modelPageStyles = readFileSync(
  new URL("../styles/model-page.css", import.meta.url),
  "utf8",
);

void test("ASR presentation reads both model readiness flags and keeps unavailable details clear", () => {
  assert.match(
    modelPageSource,
    /snapshot\?\.model_summary\.qwen_ready === true/,
  );
  assert.match(
    modelPageSource,
    /snapshot\?\.model_summary\.whisper_ready === true/,
  );
  assert.match(
    modelPageSource,
    /暂时没有更多模型详情/,
  );
  assert.match(
    modelPageSource,
    /trailing=\{approvedReadyPreview \? "最新" : "暂无信息"\}/,
  );
  assert.match(modelPageSource, /本地语音转文字尚未检查/);
  assert.match(modelPageSource, /模型检查尚未完成/);
  assert.match(modelPageSource, /\? "检查中"\s*:\s*"尚未检查"/);
});

void test("ASR health can be invoked again from the ASR header action", () => {
  assert.match(modelPageSource, /const checkAndReload = useCallback/);
  assert.match(modelPageSource, /gateway\.checkAsrHealth\(\)/);
  assert.match(modelPageSource, /checkingRef\.current/);
  assert.match(modelPageSource, /aria-busy=\{checking\}/);
  assert.match(
    modelPageSource,
    /tab === "asr"[\s\S]*className="model-header-action"[\s\S]*onClick=\{\(\) => void checkAndReload\(\)\}/,
  );
});

void test("ASR model selection switches explicitly and preserves the required error copy", () => {
  assert.match(modelPageSource, /<option value="qwen">Qwen3-ASR<\/option>/);
  assert.match(modelPageSource, /<option value="whisper">Whisper<\/option>/);
  assert.match(modelPageSource, /gateway\.switchAsrModel\(selectedModel\)/);
  assert.match(modelPageSource, /if \(switchingRef\.current\) return/);
  assert.match(modelPageSource, /snapshot\?\.active_session !== null/);
  assert.match(modelPageSource, /当前任务结束后才能切换模型/);
  assert.match(modelPageSource, /code === "asr\.model\.missing"[\s\S]*\? "缺少模型"/);
  assert.match(
    modelPageSource,
    /code === "asr\.model\.hash_mismatch"[\s\S]*\? "模型文件校验失败"/,
  );
  assert.match(modelPageSource, /切换失败时会保留原模型/);
  assert.doesNotMatch(modelPageSource, /管理备用模型/);
});

void test("model directory links are explicit, uniquely named, and guarded against repeats", () => {
  assert.match(modelPageSource, /const openModelDirectory = useCallback/);
  assert.match(modelPageSource, /gateway\.openModelDirectory\(\)/);
  assert.match(modelPageSource, /if \(openingDirectoryRef\.current\) return/);
  assert.match(modelPageSource, /disabled=\{opening\}/);
  assert.match(modelPageSource, /aria-busy=\{opening\}/);
  assert.match(
    modelPageSource,
    /aria-label=\{`查看 \$\{modelName\} 模型目录`\}/,
  );
  assert.match(modelPageSource, /modelName: "Qwen3-ASR" \| "Whisper"/);
  assert.match(modelPageSource, /onClick=\{onOpen\}/);
  assert.equal(
    modelPageSource.match(/directoryTrailing\(\s*"Qwen3-ASR"/g)?.length,
    3,
  );
  assert.equal(
    modelPageSource.match(/directoryTrailing\(\s*"Whisper"/g)?.length,
    3,
  );
  assert.match(
    modelPageSource,
    /className="model-directory-error" role="alert"/,
  );
  assert.match(
    modelPageStyles,
    /\.model-directory-link \{[\s\S]*text-decoration: underline;/,
  );
});

void test("LLM production state fails closed while approved previews keep their sample values", () => {
  assert.match(
    llmPanelSource,
    /state\.settingsLoad === "error"\s*\?\s*"settings-unavailable"/,
  );
  assert.match(
    llmPanelSource,
    /state\.keyStatusLoad === "error"\s*\?\s*"key-status-unavailable"/,
  );
  assert.match(
    llmPanelSource,
    /keyNeedsRecovery\s*\?\s*"secret-unavailable"/,
  );
  assert.match(
    llmPanelSource,
    /state\.secretDraft\.source === "revealed"[\s\S]*state\.secretDraft\.visible[\s\S]*\?\s*"secret-recovery"/,
  );
  assert.match(llmPanelSource, /const settingsConfigured = state\.settings\?\.llm != null/);
  assert.match(llmPanelSource, /approvedVisualPreview[\s\S]*"https:\/\/api\.example\.com\/v1"/);
  assert.match(llmPanelSource, /approvedVisualPreview[\s\S]*"gpt-4\.1-mini"/);
  assert.match(llmPanelSource, /: "尚未填写"\);/);
  assert.match(llmPanelSource, /"连接：待测试"/);
  assert.match(llmPanelSource, /"连接：测试通过"/);
});

void test("LLM first-time setup is unordered, uses one save action, and has no replace action", () => {
  assert.match(llmPanelSource, /const saveInitialConfiguration = async/);
  assert.match(
    llmPanelSource,
    /gateway\.setLlmSettings\([\s\S]*gateway\.setLlmApiKey\(secretValue\)/,
  );
  assert.match(
    llmPanelSource,
    /填写服务地址、模型名称和 API Key 后保存/,
  );
  assert.match(llmPanelSource, /className="model-revealed-secret"/);
  assert.match(llmPanelSource, /event\.currentTarget\.select\(\)/);
  assert.doesNotMatch(
    llmPanelSource,
    />\s*(?:编辑服务设置|保存服务设置|替换)\s*</,
  );
});

void test("LLM connection failures render the selectable upstream response as escaped text", () => {
  assert.match(llmPanelSource, /服务返回信息（已隐藏敏感内容） · HTTP/);
  assert.match(llmPanelSource, /upstream\.response_body/);
  assert.match(llmPanelSource, /<pre tabIndex=\{0\}>/);
  assert.match(llmPanelSource, /API Key 等敏感信息已隐藏/);
  assert.doesNotMatch(llmPanelSource, /dangerouslySetInnerHTML/);
  assert.match(
    modelPageStyles,
    /\.model-upstream-error pre \{[\s\S]*user-select: text;[\s\S]*white-space: pre-wrap;/,
  );
});

void test("model tabs implement roving focus and keyboard navigation", () => {
  assert.match(modelPageSource, /tabIndex=\{tab === "asr" \? 0 : -1\}/);
  assert.match(
    modelPageSource,
    /tabIndex=\{tab === "text-service" \? 0 : -1\}/,
  );
  for (const key of [
    "ArrowRight",
    "ArrowDown",
    "ArrowLeft",
    "ArrowUp",
    "Home",
    "End",
  ]) {
    assert.match(modelPageSource, new RegExp(`event\\.key === "${key}"`));
  }
});
