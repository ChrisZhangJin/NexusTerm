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

## Windows / Linux

见 [portability.md](./portability.md) 的前置依赖清单。Windows 出 NSIS/MSI，Linux 出
AppImage/DEB，均无 Gatekeeper 那套限制。

## 构建产物体积参考（macOS arm64）

| 项 | 体积 |
|---|---|
| release 二进制 | 约 6 MB |
| `.app` | 约 6–8 MiB |
| `.dmg`（UDZO） | 约 3.5 MB |

体积小是因为用系统 WebView 而非内嵌 Chromium（Electron 方案约 150–250 MB）。
