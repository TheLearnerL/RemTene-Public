# 辑语 RemTene

> 用嘴加速手指输入；在不改变原意的前提下，把语音转成可直接使用的干净文字。

「辑语」是源码可用、跨应用、系统级的语音输入工具。音频在本地完成 ASR（自动语音识别）；用户可以选择原始转录，或将文字发送到自己配置的 OpenAI-Compatible（OpenAI 兼容）服务做忠实整理与结构优化，最后把结果交付到原来的桌面输入位置。

本项目采用 [PolyForm Noncommercial License 1.0.0](./LICENSE)，不是 OSI 批准的开源许可证。非商业使用必须遵守正式许可条款；商业使用需要另行取得书面授权，参见[商业授权说明](./COMMERCIAL_LICENSE.md)。

## 当前状态

项目仍处于 V1 开发阶段，目前没有可对外承诺的正式安装包。

截至 2026-08-03：

- macOS 主线已接入真实录音、本地 Qwen／Whisper Worker、全局快捷键、三种处理模式、第三方文字服务、结果交付、临时文字框和本地历史；
- 核心正向流程已在一个真实 macOS 开发环境中运行，但这不等于干净安装、完整目标应用矩阵或正式发行验证已经完成；
- Windows 目前只有共享壳层和部分跨平台逻辑，录音、ASR Runtime、目标识别、交付、剪贴板与历史仍包含 Stub（占位实现），不能作为可用平台；
- 默认模型尚未随安装包交付，Developer ID 签名、公证、DMG、Windows 安装器、升级／卸载和双平台兼容矩阵尚未完成。

源码仓库不包含默认模型权重、本地用户数据、私人产品文档、签名证书或可执行 Runtime。

## 产品能力与边界

- **原始转录**：只使用本地 ASR，不读取选区，也不调用 LLM（大型语言模型）。
- **忠实整理／结构优化**：只在用户已配置第三方服务时处理文字；处理结果必须保留事实、立场、条件、因果和不确定程度。
- **本地 ASR**：Qwen 与 Whisper 经同一独立 Worker 接入，由用户显式选择引擎；失败时不会在同一任务中自动切换另一引擎。
- **跨应用交付**：macOS 优先使用经重新验证的精确新增写入；用户明确开启兼容贴上后，必要时可向派发时的当前焦点发送一次贴上。结果不确定时禁止自动重复写入。
- **控制面板**：只负责状态、设置和模型管理，不作为录音、笔记或会议内容工作区。

## 数据与隐私

- 音频永不离开本地设备，也不进入 LLM、历史或诊断；
- 麦克风只在用户明确触发的录音期间开启；
- 原始转录完全本地；AI 模式只向用户配置的服务发送完成任务所需的文字和经授权的选区内容；
- API Key 当前由 Rust 侧以 AES-256-GCM 进行本地字段级加密，主密钥与 SQLite 密文分开保存在应用私有目录；当前实现不是 macOS Keychain 或 Windows Credential Manager；
- 历史只保存最终文字和创建时间，可关闭后续保存；历史正文当前没有应用层加密；
- 日志、普通设置、快照和跨窗口事件不得保存音频、正文、选区、API Key 或 Provider 原始内容。

## 安装与模型状态

目前没有可供终端用户安装的正式 Release。源码仓库也不包含默认模型权重；本机已有模型与开发构建不能代表干净安装后开箱即用。

[`models/README.md`](./models/README.md) 记录计划使用的模型来源、固定 Revision、哈希和许可证。仓库只保留法律与来源材料，不分发权重。

## 开发与验证

开发环境需要锁文件中指定的 pnpm 与 Rust 工具链。常用入口：

```bash
pnpm install --frozen-lockfile
pnpm dev
pnpm test
pnpm check
pnpm lint
pnpm --filter @remtene/desktop build
pnpm --filter @remtene/desktop tauri build --no-bundle
```

`tauri build --no-bundle` 只生成未打包的桌面 release 二进制，不等于签名应用、安装器或发行完成。终端用户的正式发行包不得要求安装 Node.js、Rust、Python、编译工具或手动放置模型。

## 公共源码结构

```text
apps/desktop/                React／TypeScript 控制面板与 Tauri 桌面组装
crates/remtene-domain/       产品状态与不变量
crates/remtene-application/  工作流与 Ports
crates/remtene-contracts/    IPC／Worker 跨边界契约
crates/remtene-adapters/     ASR、LLM、设置、历史与秘密适配
crates/remtene-platform/     macOS／Windows 平台适配
crates/remtene-asr-worker/   独立本地 ASR Worker
models/                      模型来源与许可证；不含权重
scripts/                     公共构建与仓库自检脚本
```

为保留既有用户的系统权限、API Key 与模型目录，部分 Bundle ID、App Group 和密文格式仍保留历史 `bard` 标识作为兼容 ABI（旧版持久兼容接口）。它们不是现行产品名称，也不应被随意重命名。

## 许可证、商业授权与第三方内容

- 项目许可：[LICENSE](./LICENSE)
- 商业授权说明：[COMMERCIAL_LICENSE.md](./COMMERCIAL_LICENSE.md)
- 第三方声明：[THIRD_PARTY_NOTICES](./THIRD_PARTY_NOTICES)
- 模型许可证：[models/LICENSES/](./models/LICENSES/)
- 安全问题报告：[SECURITY.md](./SECURITY.md)
- 贡献规则：[CONTRIBUTING.md](./CONTRIBUTING.md)

项目许可只覆盖版权人有权许可的内容，不自动覆盖第三方依赖、模型、模型权重、Runtime、字体、图标、素材、数据集或外部 API 服务。
