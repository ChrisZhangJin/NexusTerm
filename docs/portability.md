# 跨平台说明

**结论：是跨平台的，三个桌面平台都能构建。** Rust + Tauri 2 的内核（russh / russh-sftp /
portable-pty / rusqlite）本身三端统一，窗口部分在 Windows 用 WebView2、Linux 用
WebKitGTK、macOS 用 WKWebView。当前仓库已按目标平台拆分依赖，不再是 macOS 专用。

实测状态：

| 平台 | 状态 |
|---|---|
| macOS | 实测通过（arm64） |
| **Windows** | **已实机构建并跑通**（x86_64-pc-windows-msvc，产物 NSIS 安装包） |
| Linux | 代码层面已就位，尚未实机构建 |

> Windows 支持不是“把 cfg 分支写完”就算数的：此前仓库在 Windows 上**连编译都过不了**
>（`cmd.rs` 顶层无条件 `use std::os::unix::fs::PermissionsExt`、`permissions().mode()`、
> `local/mod.rs` 里 `Result::filter` 的笔误）。本轮已修复并补齐运行时差异，
> 详见下方「Windows 实测中发现的问题」。

## 平台相关代码清单

| 位置 | 差异 | 处理方式 |
|---|---|---|
| `Cargo.toml` | 凭据后端：`keyring` 的 native backend 各平台不同 | `[target.'cfg(target_os = ...)'.dependencies]` 三分支：<br>macOS `apple-native`、Windows `windows-native`、Linux `linux-native` |
| `src-tauri/src/local/mod.rs` | 本地 shell 可执行文件：Windows 没有 `$SHELL` / `/bin/zsh` | `#[cfg(windows)]` 用 `%COMSPEC%` → `cmd.exe`；`#[cfg(unix)]` 用 `$SHELL -l` |
| `src-tauri/src/local/mod.rs` | Windows 本地 shell 的输出编码 | `cmd.exe` 在 ConPTY 下按系统 ANSI 代码页输出（简中为 GBK），必须 `/K chcp 65001` 切 UTF-8，否则中文全乱码 |
| `src-tauri/src/keys/mod.rs` | 家目录环境变量：Windows 是 `%USERPROFILE%` | `home_dir()` 按平台分支：Windows 优先 `USERPROFILE`，Unix 用 `HOME` |
| `src-tauri/src/cmd.rs` | 文件权限位：Windows 没有 POSIX mode | `permission_mode()` 按平台合成，`std::os::unix::fs::PermissionsExt` 只能在 `#[cfg(unix)]` 函数内 use |
| `src-tauri/src/cmd.rs` | 本地路径拼接 | 一律走 `PathBuf::join`，不手拼 `/`（否则 Windows 上是 `C:\Users\x/y` 混写） |
| `src-tauri/src/cmd.rs` | Windows 没有单一根目录 | `list_local_dir("")` 约定为「列盘符」，返回 `A:`–`Z:` 中真实存在的驱动器 |
| `src-tauri/src/cmd.rs` | 导出 `.xsh` 的文件名 | Windows 额外过滤 `\ * ? " < > ǀ`，这些字符在 NTFS 上无法落盘 |
| `src-tauri/src/errors.rs` | 网络 errno：Windows 用 Winsock 的 `WSAE*` | 错误码表同时列出 BSD errno 与 `errno + 10000`（如 `10060` = `WSAETIMEDOUT`） |
| `src-tauri/src/cmd.rs` | 平台探测给前端用 | `host_info` 命令返回 `std::env::consts::OS`，不靠 UA 嗅探 |
| `src/lib/platform.ts` + `styles.css` | 标题栏留白 / 窗口控制按钮 | 挂载前写入 `html[data-platform]`，CSS 据此把 `--lights-w` 归零并渲染自绘按钮 |
| `src-tauri/icons/` | 打包图标格式 | macOS `.icns`、Windows `.ico`（PNG-in-ICO）、Linux 直接用 PNG |
| `tauri.conf.json` | `bundle.targets` | 按平台拆分：基础配置给 macOS，[`tauri.windows.conf.json`](../src-tauri/tauri.windows.conf.json) 覆盖为 `nsis` |
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
| 密码存储 | Keychain | Windows 凭据管理器 | Secret Service（需 keyring daemon） |
| 终端渲染 | WKWebView + Canvas | WebView2 + Canvas | WebKitGTK + Canvas |
| 窗口装饰 | 系统 Overlay 标题栏（前端预留交通灯区） | 无边框 + 前端自绘标题栏与最小化/最大化/关闭 | 同 Windows（自绘） |
| 本地 shell | `$SHELL -l`（默认 zsh） | `%COMSPEC%`（cmd.exe + `chcp 65001`） | `$SHELL -l`（bash） |
| 复制 / 粘贴 | ⌘C / ⌘V（Ctrl+C 留给中断信号） | Ctrl+Shift+C / Ctrl+Shift+V | 同 Windows |
| 文件管理器根 | `/` | 盘符列表（`C:` `D:` …） | `/` |

## Windows 实测中发现的问题

按“不实机跑一遍就不会发现”排序：

1. **编译不过**。`cmd.rs` 顶层无条件 `use std::os::unix::fs::PermissionsExt`、
   `meta.permissions().mode()`、`local/mod.rs` 里 `env::var(..).filter(..)`
   （`Result` 没有 `filter`，应为 `.ok().filter(..)`）—— 三处都只在 Windows 上暴露。
2. **本地终端中文乱码**。ConPTY 下 cmd.exe 按 GBK 输出，xterm.js 按 UTF-8 解码。
   启动时执行 `chcp 65001` 解决；代价是少数老式控制台程序的进度重绘会错位。
3. **密钥目录找不到**。`dirs_ssh()` 只读 `$HOME`，Windows 上该变量通常不存在
   （Git Bash 会设成 `/c/Users/xxx` 这种 Rust 打不开的 POSIX 风格值）。
4. **文件管理器路径全错**。前端 `parentPath` 按 `/` 切 `C:\Users\dell` 得到 `/`，
   上级目录按钮直接把面板带飞。已拆成本地 / 远端两套路径函数。
5. **换不了盘符**。Windows 没有单一根目录，根目录再点一次「上级目录」会传空串，
   后端据此列出真实存在的驱动器。
6. **标题栏双层**。`decorations: true` + 前端自绘标题栏 = 两个标题栏。
   Windows 配置里关掉系统装饰，并补上自绘的最小化 / 最大化 / 关闭按钮。
7. **没有复制入口**。Windows 上 Ctrl+C 必须留给中断信号，`⌘C` 不存在。

## 已知限制

- 本地终端依赖 ConPTY，需要 **Windows 10 1809 / Server 2019 及以上**；更老的系统
  本地标签页会启动失败（远端 SSH 不受影响）。
- `chcp 65001` 下 `ping` 等少数程序的进度重绘会错位，属 Windows 控制台的已知行为。
- 无边框窗口无法用鼠标拖拽边缘调整大小，请用标题栏的最大化按钮。
