# DSH Atelier

<a><img src="https://img.shields.io/badge/License-MIT-yellow.svg"></a> <img src="https://img.shields.io/badge/DeepSeek-4D6BFE?logo=deepseek&amp;logoColor=white"> <img src="https://shorturl.at/ggSqS">

DSH Atelier 是 [Deepseek Harness](https://github.com/deepseek-ai/deepseek-harness) 的跨平台桌面启动器。

[English documentation](README.md)

## 简介

DSH Atelier 常驻系统托盘，负责管理 Deepseek Harness 的生命周期，并通过轻量级桌面窗口打开受管 Deepseek Harness 实例。

## 用法

### 运行发布包

发布包包含两个必须放在同一目录中的可执行文件：

```text
dsh-atelier.exe
dsh-atelier-runtime.exe
```

Windows 上双击 `dsh-atelier.exe`；macOS 上打开打包好的 `DSH Atelier.app`。默认情况下，首次启动会启动 Deepseek Harness，并打开内置的 GUI 窗口。

打开时，DSH Atelier 会尝试寻找本机的dsh指令，如果不存在，会自动尝试补齐指令，首先会优先寻找兼容的现有 Node/npm；如果不存在，则下载并校验受管 Node。接下来会使用npm将 Deepseek Harness 安装到 Atelier 管理的目录。

## 构建方法

### 前置条件

- Rust 1.92 或更高版本（由 `rust-toolchain.toml` 选择）。
- 可用的 C/C++ 工具链和平台 WebView 依赖。
- Windows 需要 WebView2 运行时支持。
- macOS 需要 Xcode Command Line Tools。

### Windows

```powershell
cargo build --release -p atelier-bootstrap -p dsh-atelier
```

运行时请确保 `target/release/dsh-atelier.exe` 和 `target/release/dsh-atelier-runtime.exe` 位于同一目录。

### macOS

```bash
cargo build --release --locked -p atelier-bootstrap -p dsh-atelier
./packaging/macos/package-local.sh
```

本地打包脚本会将应用输出到 `dist/DSH Atelier Portable/DSH Atelier.app`。

### 验证

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
```

## 功能

- **Deepseek Harness 启动器：** 通过系统托盘启动、停止、重启和监控受管 Deepseek Harness 进程。
- **简单套壳应用：** 启动后自动打开轻量级桌面窗口，直接渲染 Deepseek Harness 自己的 Web UI。
- **自动补充依赖：** 自动发现兼容的 Node/npm；缺失时安装受管 Node，并在缺少 Deepseek Harness 时自动安装 Deepseek Harness。
- **Deepseek Harness 自动升级：** 后台检查 npm registry，发现新版本后通知用户。

## 未来路线

计划中的改进包括：

- 支持自定义 GUI Surface，同时不分叉 Deepseek Harness Web UI。
- 插件市场，用于分发可选集成和扩展。
- 可选预设，覆盖常见工作流和个人偏好。
- 更多方便日常使用的功能，包括安装、升级、诊断和管理体验优化。

## 致谢

感谢由Deepseek团队开发的 [Deepseek Harness](https://github.com/deepseek-ai/deepseek-harness) 项目

DSH Atelier 以 [MIT License](LICENSE) 发布。

## 发布说明

版本历史请参阅 [CHANGELOG.md](CHANGELOG.md)。
