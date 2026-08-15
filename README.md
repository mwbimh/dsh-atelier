# DSH Atelier

<a><img src="https://img.shields.io/badge/License-MIT-yellow.svg"></a> <img src="https://img.shields.io/badge/DeepSeek-4D6BFE?logo=deepseek&amp;logoColor=white"> <img src="https://shorturl.at/ggSqS">

DSH Atelier is a cross-platform desktop launcher for [Deepseek Harness](https://github.com/deepseek-ai/deepseek-harness).

[中文文档](README.zh-CN.md)

## Overview

DSH Atelier runs in the system tray, manages the Deepseek Harness lifecycle, and opens the managed Deepseek Harness instance in a lightweight desktop window.

## Usage

### Run a release build

Release packages contain two executables that must stay together:

```text
dsh-atelier.exe
dsh-atelier-runtime.exe
```

On Windows, double-click `dsh-atelier.exe`. On macOS, open the packaged `DSH Atelier.app`. By default, the first launch starts Deepseek Harness and opens the built-in GUI window.

When it starts, DSH Atelier looks for the local `dsh` command. If it is not available, Atelier automatically tries to provide it: it first looks for a compatible Node/npm installation; if none is available, it downloads and verifies a managed Node runtime. It then uses npm to install Deepseek Harness into an Atelier-managed directory.

## Building from source

### Prerequisites

- Rust 1.92 or newer (selected by `rust-toolchain.toml`).
- A working C/C++ toolchain and platform WebView dependencies.
- WebView2 runtime support on Windows.
- Xcode Command Line Tools on macOS.

### Windows

```powershell
cargo build --release -p atelier-bootstrap -p dsh-atelier
```

Keep `target/release/dsh-atelier.exe` and `target/release/dsh-atelier-runtime.exe` in the same directory when running the build.

### macOS

```bash
cargo build --release --locked -p atelier-bootstrap -p dsh-atelier
./packaging/macos/package-local.sh
```

The local packaging script writes the app to `dist/DSH Atelier Portable/DSH Atelier.app`.

### Verification

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
```

## Features

- **Deepseek Harness launcher:** start, stop, restart, and monitor the managed Deepseek Harness process from the system tray.
- **Simple desktop shell:** automatically open a lightweight desktop window that renders Deepseek Harness's own Web UI.
- **Automatic dependency setup:** discover a compatible Node/npm installation; install a managed Node runtime when needed; and automatically install Deepseek Harness when it is missing.
- **Deepseek Harness updates:** check the npm registry in the background and notify you when a newer version is available.

## Roadmap

Planned improvements include:

- Custom GUI surfaces that extend Deepseek Harness without forking its Web UI.
- A plugin marketplace for optional integrations and extensions.
- Selectable presets for common workflows and personal preferences.
- Additional quality-of-life features for setup, updates, diagnostics, and everyday use.

## Acknowledgements

Thanks to the Deepseek team for open-sourcing the [Deepseek Harness](https://github.com/deepseek-ai/deepseek-harness) project.

DSH Atelier is distributed under the [MIT License](LICENSE).

## Release notes

See [CHANGELOG.md](CHANGELOG.md) for the version history.
