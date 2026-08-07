# 品牌源资产

本目录只保存生成正式应用资源所需的可追溯源资产，不保存 ImageGen（图像生成）过程图、临时预览或构建输出。

- `remtene-app-icon-source.png`：辑语桌面应用图标的原始设计图。
- `apps/desktop/src-tauri/icons/`：由源图补齐方形画布后生成的 Tauri／macOS／Windows 正式图标集；应用构建只引用该目录中的生成结果。

重新生成图标时必须保持源图不被覆盖，并复核方形画布、透明度、ICNS／ICO 格式和最终 Bundle 实际引用。
