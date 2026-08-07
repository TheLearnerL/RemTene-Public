# 辑语桌面端

本目录承载 Tauri 2 + React + TypeScript 桌面应用：

- `src/`：控制面板、录音 HUD、临时文字框和具类型前端调用；
- `src-tauri/`：Tauri 组装根、IPC 适配、桌面生命周期和平台能力接线；
- `package.json`：前端开发、类型检查、Lint、契约测试和 Tauri 命令入口。

开发、构建和验证命令统一从仓库根执行：

```bash
pnpm --filter @remtene/desktop typecheck
pnpm --filter @remtene/desktop lint
pnpm --filter @remtene/desktop test:contracts
pnpm --filter @remtene/desktop build
pnpm --filter @remtene/desktop tauri build --no-bundle
```

当前可运行主线是 macOS。Windows 仍包含关键平台 Stub，`tauri build --no-bundle` 也不代表签名、安装器、模型交付或正式发行已经完成。
