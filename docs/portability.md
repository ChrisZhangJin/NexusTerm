# 跨平台说明

**结论：是跨平台的，三个桌面平台都能构建。** Rust + Tauri 2 的内核（russh / russh-sftp /
portable-pty / rusqlite）本身三端统一，窗口部分在 Windows 用 WebView2、Linux 用
WebKitGTK、macOS 用 WKWebView。当前仓库已按目标平台拆分依赖，不再是 macOS 专用。

但要说实话：**只有 macOS 是实测过的**，Windows / Linux 目前是“代码层面已就位、尚未实机构建”。

## 平台相关代码清单

| 位置 | 差异 | 处理方式 |
|---|---|---|
| `Cargo.toml` | 凭据后端：`keyring` 的 native backend 各平台不同 | `[target.'cfg(target_os = ...)'.dependencies]` 三分支：<br>macOS `apple-native`、Windows `windows-native`、Linux `linux-native` |
| `src-tauri/src/local/mod.rs` | 本地 shell 可执行文件：Windows 没有 `$SHELL` / `/bin/zsh` | `#[cfg(windows)]` 用 `%COMSPEC%` → `cmd.exe`；`#[cfg(unix)]` 用 `$SHELL -l` |
| `src-tauri/src/local/mod.rs` | 家目录环境变量：Windows 是 `%USERPROFILE%` | 先试 `HOME`，再回退 `USERPROFILE` |
| `src-tauri/icons/` | 打包图标格式 | macOS `.icns`、Windows `.ico`（PNG-in-ICO）、Linux 直接用 PNG |
| `tauri.conf.json` | `bundle.targets` | `dmg`+`app`（macOS）、`nsis`（Windows）、`appimage`+`deb`（Linux） |
| `tauri.conf.json` | `bundle.macOS.minimumSystemVersion` | 仅 macOS 生效，其余平台忽略 |

## Windows 构建前置

```powershell
# 1. Rust 工具链（msvc 目标）
winget install Rustlang.Rustup
rustup default stable-x86_64-pc-windows-msvc

# 2. Node 22 LTS
winget install OpenJS.NodeJS.LTS

# 3. WebView2 Runtime（Windows 10/11 一般自带）
winget install Microsoft.EdgeWebView2Runtime

# 4. 构建
npm install
npm run tauri:build
```

产物：`src-tauri/target/release/bundle/nsis/*.exe`（安装包）、`bundle/msi/*.msi`。

> Windows 上 `tab 3`（无 Servo=None）无需额外配置；但 NSIS 需要下载第三方组件，
> 首次构建会有几分钟额外耗时，这是 Tauri 的正常行为。

## Linux 构建前置

以 Ubuntu / Debian 为例：

```bash
sudo apt update
sudo apt install -y \
  libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev \
  librsvg2-dev libsoup-3.0-dev javascriptcoregtk-4.1-dev \
  build-essential curl wget file libssl-dev pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# Node 22
npm install && npm run tauri:build
```

产物：`bundle/appimage/*.AppImage`、`bundle/deb/*.deb`。

> Linux 本地 PTY 走 fork + pty，`portable-pty` 已处理；AppImage 在部分发行版需要
> `--appimage-extract-and-run` 或 `chmod +x`。

## 已知平台差异

| 行为 | macOS | Windows | Linux |
|---|---|---|---|
| 密码存储 | Keychain | Windows Credential Manager | Secret Service（需 keyring daemon） |
| 终端渲染 | WKWebView + Canvas | WebView2 + Canvas | WebKitGTK + Canvas |
| 窗口装饰 | 系统 Overlay 标题栏（前端预留交通灯区） | 系统标题栏 | 系统标题栏 |
| 本地 shell | `$SHELL -l`（默认 zsh） | `%COMSPEC%`（cmd.exe） | `$SHELL -l`（bash） |

## 前端适配遗留项

`overlay` 标题栏下前端必须预留左侧约 80px 给 macOS 交通灯（`--lights-w`）。
在 Windows / Linux 上这段留白是多余的 —— 理想做法是按 `hostInfo.platform` 动态置 0，
目前还没做，Windows / Linux 上的表现是标题栏左侧多一小段空白，不影响功能。
