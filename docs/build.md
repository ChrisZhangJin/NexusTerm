# 构建、打包与签名

## 本地开发

```bash
npm install
npm run tauri:dev      # 带热重载
```

## 生产构建

```bash
npm run tauri:build
```

当前 `bundle.targets`：`dmg`、`app`、`nsis`、`appimage`、`deb`。在 macOS 上只会产出前两种。

---

## macOS 的三个坑（都已踩过）

### 1. `bundle_dmg.sh` 报 `brackets ([ ]) not balanced`

报错里的路径（如 `/dev/disk7s`）明明没有方括号，**这条错误信息是误导性的**。
真正的触发条件：**PATH 里的 `grep` 是受限环境的垫片**（本项目所在的沙箱把 `grep`
换成了 toybox 0.8.13），它的正则引擎处理 `hdiutil info` 输出的**二进制 alias 记录**时会
给出这个诊断。

逐字复现：

```bash
printf 'a /dev/disk1s1 b\n' | grep -E --color=never '/dev/disk1s1'   # 纯文本 → 正常
hdiutil info | grep -E --color=never '/dev/disk7s'                   # 二进制 → 报 bad regex
```

普通终端 / CI 用系统 grep 不会遇到，脚本可正常跑通。受限环境下绕开：

```bash
cd src-tauri/target/release/bundle
/usr/bin/hdiutil create -volname "NexusTerm" \
  -srcfolder "macos/NexusTerm.app" -ov -format UDZO "NexusTerm_0.1.0_aarch64.dmg"
```

⚠️ 每次脚本失败都会**残留一个挂载卷**，务必清理：

```bash
for d in $(hdiutil info | /usr/bin/grep -oE "^/dev/disk[0-9]+" | sort -u); do hdiutil detach $d; done
```

### 2. Tauri 图标必须是 RGBA

早期用 Python 生成的 PNG 是 RGB（color type 2），Tauri 打包时 panic。必须是 **RGBA（6）**，
且 `.icns` 要包含 `ic07`–`ic11` 全套尺寸。

### 3. 签名与 Gatekeeper

本机实测（`security find-identity -v -p codesigning` 为 0 个有效身份）：

```bash
xattr -cr NexusTerm.app              # 清 com.apple.quarantine 隔离标记
codesign --force --deep --sign - NexusTerm.app   # ad-hoc 签名
spctl -a -vv NexusTerm.app           # → rejected（未公证）
```

| 场景 | 是否需要 Apple 开发者账号 | 操作 |
|---|---|---|
| 只在自己这台 Mac 用 | 不需要 | 右键 → 打开（仅首次） |
| 发给别人 / 放网上下载 | 需要（¥688/年） | Developer ID 签名 + `notarytool` 公证 |

### 钥匙串弹窗

Keychain 的 ACL 靠**代码签名**识别访问者。ad-hoc 签名每次重建都会变，Keychain 视其为
陌生应用 → 反复弹窗。弹一次点「始终允许」即可；**但每次重新构建后授权会失效，需要再点一次**。

---

## Windows（已实测）

前置依赖见 [portability.md](./portability.md)。本机实测环境：

| 项 | 版本 |
|---|---|
| 目标三元组 | `x86_64-pc-windows-msvc` |
| rustc / cargo | 1.97.1 |
| Node | 22 LTS |

```powershell
npm install
npm run tauri:build
```

首次构建约 9 分钟（russh / rusqlite bundled 编译 SQLite、release LTO），
其中 Tauri 会自行下载 NSIS 与 `nsis_tauri_utils.dll`，多花 1–2 分钟，属正常行为。

产物：

| 文件 | 体积 | 用途 |
|---|---|---|
| `src-tauri/target/release/bundle/nsis/NexusTerm_0.1.0_x64-setup.exe` | 约 2.5 MB | 安装包（开始菜单 / 卸载入口） |
| `src-tauri/target/release/nexus-term.exe` | 约 6.7 MB | **免安装版**，可直接双击运行 |

### 免安装（便携）用法

`nexus-term.exe` 是自包含的 —— 前端资源在构建时已嵌进二进制，不需要同目录的
`dist/` 或任何 DLL。把它单独拷到任意空目录实测可正常启动（进程树里同样出现
`cmd.exe` + `conhost.exe`），因此可以直接分发这个单文件，或压成 zip：

```powershell
Compress-Archive -Path src-tauri\target\release\nexus-term.exe `
  -DestinationPath NexusTerm-0.1.0-windows-x64-portable.zip
```

压缩后约 3 MB。两个前提：

- 目标机器要有 **WebView2 Runtime**（Windows 10/11 一般自带；缺失时
  `winget install Microsoft.EdgeWebView2Runtime`）。
- 会话库与凭据仍写用户目录（`%APPDATA%\com.nexus-term.app`），不在 exe 旁边，
  所以放只读目录也能跑，也不需要管理员权限。

### 冒烟验证做了什么

启动 `nexus-term.exe` 后检查三件事，能同时过就说明「后端 + WebView + 本地 PTY」
这条主链路在 Windows 上是通的：

1. `%APPDATA%\com.nexus-term.app\nexus.db` 被创建 —— SQLite 初始化未 panic；
2. 进程树里出现 `cmd.exe` 与 `conhost.exe` —— ConPTY 真的拉起了本地 shell；
3. 日志打印 `nexus-term 后端就绪` —— `setup` 钩子走完。

> 注意：用 CI 或脚本启动时不要在同一次调用里 `&` 后台起进程再退出，
> 子进程会随 shell 一起被回收，看起来像是应用自己崩了。

### 平台配置

Windows 的差异写在 [`tauri.windows.conf.json`](../src-tauri/tauri.windows.conf.json)
（Tauri 2 会自动与 `tauri.conf.json` 合并）：关掉系统装饰改由前端自绘标题栏，
`bundle.targets` 收敛为 `nsis`。这样 macOS 的 `dmg` / `app` 不会在 Windows 上被尝试构建。

## Linux

见 [portability.md](./portability.md) 的前置依赖清单。出 AppImage / DEB，无 Gatekeeper
那套限制。尚未实机验证。

## 构建产物体积参考（macOS arm64）

| 项 | 体积 |
|---|---|
| release 二进制 | 约 6 MB |
| `.app` | 约 6–8 MiB |
| `.dmg`（UDZO） | 约 3.5 MB |

体积小是因为用系统 WebView 而非内嵌 Chromium（Electron 方案约 150–250 MB）。
