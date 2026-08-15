# DSH Atelier

DSH Atelier 是 DSH 的跨平台桌面入口。它提供常驻系统托盘、DSH/Node 发现与安装、DSH Web 启停、内置 DSH Surface，以及独立的稳定 Bootstrap。

## 当前行为

- 优先使用已存在且可执行的 `dsh`；找不到时安装 npm `latest` 到 `~/.atelier/installations/dsh`。
- 已有 DSH 会先直接启动并打开页面，npm 更新检查在后台并行执行，不占用启动关键路径。
- 检查到新 DSH 后只发送通知并更新 Tray 菜单；用户明确选择后才安装。外部 DSH 只提示其原渠道更新，不由 Atelier 覆盖。
- Atelier 管理的更新安装到独立版本目录，通过 `pending → readiness/HTTP → active` 激活；失败自动回滚。
- Atelier 本体更新通过签名的 HTTPS feed 在后台检查，不阻塞 DSH 启动；用户确认后下载对应平台的 Portable ZIP，只抽取 Runtime，重启健康检查失败时立即回滚。
- 优先使用兼容的现有 Node/npm；找不到时下载并校验托管 Node `24.19.0`。
- npm registry 顺序为官方、npmmirror、腾讯云、华为云；元数据查询和安装都支持 fallback。
- DSH 数据仍由 DSH 保存在默认 `~/.dsh`，Atelier 不修改该目录。
- 显式启动默认启动 DSH 并显示内置 `surface:dsh`；登录启动只启动 DSH，不显示页面。
- DSH 异常退出后按 1、2、4、8、16 秒重试五次；退出 Atelier 会停止完整 DSH 进程树。
- Windows 可执行文件和默认 Tray 使用 DeepSeek 官方蓝色图标；macOS Tray 使用黑色
  Template Icon，由系统自动适配浅色和深色菜单栏。

## Windows 本机构建

```powershell
cargo build --release -p atelier-bootstrap -p dsh-atelier
```

将以下两个文件放在同一目录，双击 `dsh-atelier.exe`：

```text
dsh-atelier.exe
dsh-atelier-runtime.exe
```

配置文件是 `~/.atelier/config/atelier.toml`。文件不存在时使用默认值：

```toml
[dsh]
auto_start = true
first_launch = "surface:dsh" # 或 "web"、"none"

[atelier]
launch_at_login = false
theme = "dark" # 也可使用 "light" 或 "system"；暂不提供设置界面

[atelier.update]
enabled = true
feeds = ["https://github.com/mwbimh/dsh-atelier/releases/latest/download/"]
# 官方发布包在构建时嵌入验签公钥；自定义发行版可在这里提供 raw Ed25519 公钥的 base64。
# public_key = "..."

[atelier.surface]
title = "DeepSeek Harness"
loading_title = "DeepSeek Harness"
loading_starting_text = "正在启动…"
loading_started_text = "已启动"
# title_icon = "branding/title.png"
# loading_icon = "branding/loading.png"
```

`atelier.surface` 控制 Atelier 自己维护的 Surface 标题栏和 loading 页，不修改 DSH 页面。`title_icon` 与 `loading_icon` 省略时使用内置黑白 DeepSeek 图标；配置相对路径时以 `~/.atelier` 为根目录解析。标题栏图标、loading 图标可以分别配置。

`atelier.update.feeds` 是按顺序尝试的 HTTPS 更新目录。每个目录必须提供 `update-manifest.json`、`update-manifest.json.sig` 和 manifest 引用的 Portable ZIP；网络、验签或下载失败只更新 Tray 状态和日志，不会中断 DSH。非 Bootstrap 启动和未嵌入验签公钥的本地开发构建默认禁用自更新。

`first_launch` 控制显式启动后的打开方式：

- `surface:dsh`：默认，在内置 DSH Surface 中打开。
- `web`：在系统默认浏览器中打开。
- `none`：不显示页面，继续通过 Tray 管理和打开 DSH。

## 内置 DSH Surface

`surface:dsh` 使用系统 WebView 原样加载 Controller 验证过的 DSH readiness URL：Windows 使用 WebView2，macOS 使用 WKWebView。重复打开会显示并聚焦同一个窗口；关闭窗口只会隐藏 Surface，不会停止 DSH，可从 Tray 再次打开。

Surface 只允许当前受管 DSH 的已验证 loopback origin，不复制或修改上游 Web UI，也不注入 JavaScript、CSS、DOM 或窗口控制 bridge。外部 `http`/`https` 顶层链接交给系统浏览器，其他 scheme 和未经验证的地址会被拒绝。

Windows WebView2 profile 保存在 `~/.atelier/surfaces/dsh/`。WKWebView 不支持指定 data directory，因此 macOS 使用 non-persistent data store，避免把 Atelier 的 WebView 状态写入系统默认存储。

诊断日志位于 `~/.atelier/logs/atelier.log`。以下真实自检只验证 DSH lifecycle，不创建 Tray、窗口或 WebView：

```powershell
dsh-atelier.exe --smoke-test
```

## 自定义 Tray 图标

将自定义图标放到 Atelier 根目录，重启 Atelier 后生效：

```text
~/.atelier/icon.png
~/.atelier/icon.ico
```

同时存在时优先使用 `icon.png`。图标会保持比例缩放到 32×32；文件最大 4 MiB、原始尺寸最大 1024×1024。文件损坏或不符合限制时会记录警告并回退到平台默认图标。

官方蓝色 ICO、从官方首页矢量路径提取的黑色 SVG，以及可直接复制使用的黑色 ICO 保存在 [`assets/icons`](assets/icons)。

## 验证

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features
```

## 发布

版本发布通过 `release/x.y.z → main` 的 Release PR 完成；分支中的 `x.y.z` 必须与两个发布 crate 的版本及 `CHANGELOG.md` 标题完全一致。PR 会构建 Windows ZIP、macOS Portable ZIP 和 macOS DMG 候选包。合并前应下载并验收候选包，合并后 `Release` Workflow 会校验候选源码 tree 与合并 tree 一致，自动创建 annotated `vX.Y.Z` tag，原样签名并发布，不重新构建 binary。不要手工创建或 push 发布 tag。

例如 `release/0.1.0` 只允许发布 crate 版本 `0.1.0` 和 `CHANGELOG.md` 中的 `## [0.1.0]`；手动运行 Workflow 时也必须从对应的 `release/x.y.z` 分支启动。

Repository 需要配置：

```text
Variable: DSH_ATELIER_UPDATE_PUBLIC_KEY
Secret:   UPDATE_MANIFEST_SIGNING_KEY_PEM_B64
```

二者必须属于同一 Ed25519 密钥。`DSH_ATELIER_UPDATE_PUBLIC_KEY` 是 raw 32-byte public key 的标准 Base64；Secret 是 private PEM 整体的 Base64。私钥只用于合并后的发布 job，不得提交到仓库或提供给 PR job。Workflow 会在发布前重新派生公钥并确认二者一致。

## macOS 发布包

每个 macOS Release 同时提供两种 universal 包：

- `dsh-atelier-vX.Y.Z-macos-universal.dmg`：标准安装包。打开后将 `DSH Atelier.app` 拖入 `Applications`。
- `dsh-atelier-vX.Y.Z-macos-universal-portable.zip`：便携包，解压后的结构如下：

```text
DSH Atelier Portable/
├─ DSH Atelier.app
├─ atelier.portable
└─ data/
```

Portable 模式必须整体移动该目录；数据写入同级 `data/`，不会写入 `.app`。删除
`atelier.portable` 后，Atelier 会恢复使用 `~/.atelier`。

应用内自动更新始终下载签名 manifest 指向的 Portable ZIP，只抽取并切换 Runtime；不会挂载或改写 DMG。涉及 Bootstrap、图标或 `Info.plist` 的完整应用升级，需要下载新 DMG，并将其中的 `.app` 重新拖入 `Applications` 覆盖旧版本。

当前两种 macOS 产物都未使用 Developer ID 签名或 notarization。从网络下载后，可能需要在系统“隐私与安全性”中手动允许。

macOS 本地构建和打包：

```bash
cargo build --release --locked -p atelier-bootstrap -p dsh-atelier
./packaging/macos/package-local.sh
```

本地脚本输出位于 `dist/DSH Atelier Portable/DSH Atelier.app`。应用会显示正常的 Dock
图标，Dock 使用黑白 `.icns`，菜单栏继续使用可随系统主题反色的黑色 Template Icon。
