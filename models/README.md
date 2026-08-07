# 辑语本地 ASR 模型包说明

更新日期：2026-08-01

本文件用于「辑语」模型发布包与安装目录。在源码仓库中，`models/` 只追踪本 README 和许可证，模型权重继续被 Git 忽略；安装或发布时，它们会与本地 ASR 模型及模型 Manifest 一起进入模型目录。模型权重属于上游作者，不因进入本目录而改用「辑语」项目自身的许可证。

许可证全文位于 [`LICENSES/`](./LICENSES/)：

- `Qwen3-ASR-0.6B-APACHE-2.0.txt`：Qwen3-ASR-0.6B 模型；
- `OpenAI-Whisper-MIT.txt`：OpenAI Whisper 代码与模型权重；
- `whisper.cpp-MIT.txt`：whisper.cpp Runtime 与 GGML 转换工具链。

## Qwen3-ASR-0.6B

### 来源

- 上游模型：`Qwen/Qwen3-ASR-0.6B`
- 官方地址：<https://huggingface.co/Qwen/Qwen3-ASR-0.6B>
- 固定 Revision：`5eb144179a02acc5e5ba31e748d22b0cf3e303b0`
- 许可证：Apache License 2.0
- 上游版权声明：`Copyright 2026 Alibaba Cloud`

### 本项目做了什么

1. 从上述固定 Revision 中只选择当前 Rust Runtime 必需的三个文件：`model.safetensors`、`vocab.json`、`merges.txt`。
2. 没有转换、裁剪、重新量化或改写这三个源文件；它们按原字节保存，只调整了外层包目录名。
3. `model.safetensors` 的固定大小为 `1,876,091,704` bytes，SHA-256 为 `79d6cbd4c98c7bbffe9db2edac07f56cd6637d0d5944b27f6c2b8353840323ea`。
4. `qwen-asr` Runtime 会在加载时进行 INT8 量化。应用通过 `QWEN_ASR_SIDECAR=0` 禁止把可再生的 INT8 缓存写回模型目录，因此分发包不包含 `qwen-asr-int8.sidecar`，每次冷加载时会重新生成内存中的运行表示。
5. 相邻的 `qwen3-asr-0.6b-v1.manifest.json` 逐文件记录完整性哈希。Manifest 中的 `quantization: int8` 描述当前运行路径，不表示上游 `model.safetensors` 已被本项目离线改写为另一份权重。

## Whisper large-v3-turbo Q5_0

### 来源

- 上游模型家族：`openai/whisper-large-v3-turbo`
- OpenAI 模型地址：<https://huggingface.co/openai/whisper-large-v3-turbo>
- 预转换模型仓库：`ggerganov/whisper.cpp`
- 固定 Revision：`5359861c739e955e79d9a303bcbc70fb988958b1`
- 原始资产名：`ggml-large-v3-turbo-q5_0.bin`
- 许可证：OpenAI Whisper MIT；whisper.cpp MIT

### 上游已经做了什么

whisper.cpp 模型仓库把 OpenAI Whisper large-v3-turbo 权重转换为 whisper.cpp 的自定义 GGML 二进制格式，并生成 Q5_0 量化版本。该转换与量化发生在上游预转换资产的制作阶段，不是「辑语」自行训练或重新量化。

### 本项目做了什么

1. 从上述固定 Revision 取得 `ggml-large-v3-turbo-q5_0.bin`。
2. 仅将外层文件名改为应用包 ID 对应的 `whisper-large-v3-turbo-q5_0-v1.bin`；没有再次转换、量化、裁剪或改写文件内容。
3. 当前文件大小为 `574,041,195` bytes，SHA-256 为 `394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2`。
4. 相邻的 `whisper-large-v3-turbo-q5_0-v1.manifest.json` 记录该单文件包的完整性哈希与 Worker 兼容范围。

## 包完整性边界

- `README.md` 与 `LICENSES/` 位于 `models/active` 文档层，不写入 `qwen3-asr-0.6b-v1/` 权重子目录。Qwen 目录型包要求内部文件与 Manifest 完全一致，多出任何文件都会失败关闭。
- 模型下载、复制或解包完成后，应用必须先校验 Manifest 与 SHA-256，校验通过后才能移入 `models/active`。
- 不得把 Runtime 生成的缓存、日志或临时文件作为模型资产上传或分发。

本文记录工程处理与来源溯源，不替代正式法律意见。每次升级上游 Revision、Runtime、格式或量化版本时，都必须重新核对许可证、版权声明、处理说明、文件大小与哈希。
