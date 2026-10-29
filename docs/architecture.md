# macOS SSH/SFTP 工具 —— Rust + TypeScript 架构方案

> **声明**：本方案完全基于 SSH 公开标准（RFC 4251–4254、SFTP v3）、终端控制标准（ANSI X3.64 / ECMA-48）与开源生态设计，**不引用任何通过反编译获得的 Xshell 实现细节**。
> 从二进制反编译商业软件违反其 EULA，且对本项目没有增量价值——目标所用的五项能力均有更成熟的开源替代品。

---

## 一、结论先行

**可行，且工程风险可控。** 你要的四项能力对照如下：

| 你的需求 | 解决方案 | 依赖组件 | 难点等级 |
|---|---|---|---|
| 多窗格 / 多标签 UI | 布局树 + xterm.js 实例池 | `@xterm/xterm` + 自研 layout | ★★★☆ 中高 |
| 字体与配色 | 主题引擎 + ITheme 映射 | xterm.js `ITheme` | ★★☆ 低 |
| Session 保存与管理 | SQLite + macOS Keychain | `rusqlite` + `keyring` | ★★☆ 低 |
| SSH Key 管理 | OpenSSH 格式解析 + agent | `ssh-key` | ★★★ 中 |
| SFTP 文件传输 | russh-sftp + 双栏 UI | `russh-sftp` | ★★★★ 高 |

真正的难点不是某个技术点，而是**终端渲染的细节堆积**（宽字符、IME、resize 同步、大量输出的背压）和**SFTP 的工程完成度**（队列、断点、错误恢复）。这两块要留足时间。

---

## 二、技术选型

### 应用层框架：Tauri 2.0（而非 Electron）

| 维度 | Tauri | Electron |
|---|---|---|
| 空壳体积 | ~5–15 MB | ~150–250 MB |
| 内存占用 | 低（WKWebView 复用系统） | 高（自带 Chromium） |
| 后端能力 | Rust 原生 | Node + native addon |
| 启动速度 | 快 | 中等 |

Rust 后端放进来后，Tauri 是唯一自然的选择。macOS 上 WKWebView 由系统提供，不需要打包浏览器内核。

> ⚠️ WKWebView 有版本差异隔阂。建议最低支持 macOS 12 Monterey，Safari 16 对应的 WebKit 足够。若发现某个 CSS 特性在旧版失效，用 caniuse 按 Safari 查，不要按 Chrome 的习惯写。

### SSH 协议栈：`russh`（而非 `ssh2`）

```toml
russh       = "0.49"   # 纯 Rust SSH2 实现，tokio async
russh-sftp  = "2.0"    # SFTP v3 子系统
```

选择理由：

- **`ssh2` crate 是 libssh2 的 FFI 绑定** —— 要编译 C 库，macOS 上做通用二进制（arm64 + x86_64）会遇到 link 地狱
- **russh 是纯 Rust**，zero C dependency，`cargo build --target universal2-apple-darwin` 干净通过
- 原生 tokio async，与 SFTP 并发传输、多会话复用的模型天然契合

代价：russh 相对年轻，**老旧的 ssh-dss (DSA) 和部分冷门 cipher 不支持**。实际场景中 OpenSSH 8+ 服务端全覆盖，可以接受。若必须支持老旧交换机设备，再补一个 feature-gated 的 `ssh2` 回退路径。

### 后端依赖清单

```toml
[dependencies]
tauri          = { version = "2", features = ["protocol-asset"] }
tokio          = { version = "1", features = ["full"] }
russh          = "0.49"
russh-sftp     = "2.0"
ssh-key        = { version = "0.6", features = ["ed25519", "rsa", "ecdsa", "encryption"] }
rusqlite       = { version = "0.32", features = ["bundled"] }   # bundled = 自带 SQLite，不用系统库
keyring        = { version = "3", features = ["apple-native"] }  # macOS Keychain
portable-pty   = "0.8"          # 本地终端（对标 Xshell 的「本地 Shell」）
serde          = { version = "1", features = ["derive"] }
serde_json     = "1"
thiserror      = "2"
tracing        = "0.1"
base64         = "0.22"
uuid           = { version = "1", features = ["v4"] }
```

### 前端依赖清单

```jsonc
{
  "@xterm/xterm": "^5.5",
  "@xterm/addon-fit": "^0.10",       // 自适应尺寸
  "@xterm/addon-canvas": "^0.7",      // Canvas 渲染器（稳，优于 WebGL 的兼容性）
  "@xterm/addon-unicode11": "^0.8",   // 宽字符宽度标准 —— 中文/emoji 必须
  "@xterm/addon-web-links": "^0.11",
  "@xterm/addon-search": "^0.15",
  "react": "^19", "react-dom": "^19",
  "zustand": "^5",                    // 轻量状态，比 Redux 适合这种局部更新密集的场景
  "react-resizable-panels": "^2",     // 多窗格拖拽分隔
  "@tanstack/react-virtual": "^3"     // SFTP 大目录虚拟滚动
}
```

---

## 三、整体架构

```
┌───────────────────────── Frontend (TypeScript / React) ─────────────────────────┐
│                                                                                 │
│  ┌── WorkspaceLayout ──┐  ┌── ThemeEngine ──┐  ┌── KeyManagerPane ──┐          │
│  │  TabStrip           │  │  配色 / 字体    │  │  SFTPBrowser       │          │
│  │  ├─ PaneTree        │  │  CSS 变量下发   │  │  (双栏 + 队列)     │          │
│  │  │   └─ TermHost    │  └────────────────┘  └────────────────────┘          │
│  │  └─ xterm instance  │                                                       │
│  └─────────────────────┘                                                        │
│                              ▲ invoke ▼ emit                                    │
└──────────────────────────────┼──────────────────────────────────────────────────┘
                               │  Tauri IPC (JSON commands / events)
┌──────────────────────────────┼──────────────────────────────────────────────────┐
│                       Backend (Rust)                                            │
│  ┌─ cmd/ ─────────────┐  ┌─ ssh/ ──────────┐  ┌─ sftp/ ───────┐                │
│  │ #[tauri::command]  │  │ SessionPool     │  │ TransferQueue │                │
│  │ 参数校验 + 转调用   │  │ 连接复用/保活   │  │ 断点/进度     │                │
│  └────────────────────┘  └─────────────────┘  └───────────────┘                │
│  ┌─ keys/ ────────────┐  ┌─ store/ ────────┐  ┌─ pty/ ────────┐                │
│  │ OpenSSH 解析       │  │ SQLite 仓库     │  │ portable-pty  │                │
│  │ Keychain 读写      │  │ 迁移 / 导出     │  │ 本地 shell    │                │
│  └────────────────────┘  └─────────────────┘  └───────────────┘                │
└─────────────────────────────────────────────────────────────────────────────────┘
```

**关键设计：连接所有权在 Rust，渲染在 WebView。** 每个 SSH 会话是一个 tokio task，持有 `russh::client::Handle`；后端把 PTY 数据流通过 `app.emit("term:data", {id, chunk})` 推给前端。反向输入走 `invoke("term_write", ...)`。

---

## 四、四项核心能力的实现细节

### 4.1 多窗格 / 多标签 —— 最难的一块

**数据模型用树，不要用扁平数组：**

```ts
type PaneNode =
  | { kind: 'leaf'; id: string; terminalId: string }
  | { kind: 'split'; id: string; dir: 'row' | 'col'; ratio: number;
      children: [PaneNode, PaneNode] };

interface Tab { id: string; title: string; root: PaneNode; activePaneId: string }
```

**三个必须避开的坑：**

1. **不要把 Terminal 绑定给 pane。** 应该**绑定到 Tab/session**，pane 只是容器。拖动时直接移动 DOM 节点：
   ```ts
   targetContainer.appendChild(term.element);   // 保留滚动缓冲，不重建
   fitAddon.fit();                               // 再重算行列
   ```
   若 pane 销毁就 `term.dispose()`，你会发现拖个标签就把历史输出丢了。

2. **resize 必须防抖。** spinner 激烈拖动会每秒触发上百次 fit。用 rAF 合并，并在拖动停止后才向后端发 `window_change`（对应 SSH 的 `pty-req` 尺寸变更）。忘记同步远端 `COLUMNS`/`LINES` 会导致 vim、top 显示错乱。

3. **拆解比率要持久化。** `ratio` 存进 UI state 并写 SQLite，重启后布局恢复 —— 这是 Xshell 用户习惯的一部分，缺失会明显觉得"不像成品"。

**多少 tab 合适**：每个 xterm 实例约 3–8 MB（含 canvas backing store）。超过 20 个标签时做**后台 tab 的渲染降级**（暂停渲染、只保留 buffer），这是所有成品终端都做的事。

### 4.2 字体与配色

xterm.js 的 `ITheme` 只吃 **16 色 + 前景/背景**，直接映射即可。主题文件用自己的 JSON，**建议格式对齐公开标准**：

```jsonc
{
  "name": "My Theme",
  "foreground": "#E5E5E5",
  "background": "#000000",
  "cursorColor": "#FFFFFF",
  "black": "#000000", "red": "#CD0000", /* ... 8 normal + 8 bright ... */,
  "fontFamily": "Menlo, SF Mono, monospace",
  "fontSize": 13, "lineHeight": 1.2, "letterSpacing": 0
}
```

> 为什么对齐公开标准而不是自创：Windows Terminal 和 iTerm2 的主题格式都是这个形状，**网上成千上万的现成主题可以一键导入**。造一套私有格式，生态就跟你无关了。顺带一提，`ls /Users/chris/Downloads/xshell8/ColorSchemes/` 里那 21 个 `.scs` 是 UTF-16LE 的 `key=value` 文本——但我不会去解析复制它们，配色方案这件事有更好的公开资源。

**必须处理的字体细节：**

- **等宽验证** —— 用户选了非等宽字体会导致表格全乱。启动时检测 `i` 和 `W` 的宽度，不等宽就警告
- **Nerd Font / 连字** —— `font-feature-settings: 'liga' 1, 'calt' 1`，并在 Canvas renderer 下验证渲染正确
- **行高必须是整数像素** —— 非整数会在高分屏出现字符裁切，这是最常见的终端字体 bug

### 4.3 Session 管理

```sql
CREATE TABLE sessions (
  id           TEXT PRIMARY KEY,
  name         TEXT NOT NULL,
  folder       TEXT DEFAULT '/',      -- 支持文件夹分组
  host         TEXT NOT NULL,
  port         INTEGER NOT NULL DEFAULT 22,
  username     TEXT,
  auth_type    TEXT NOT NULL,          -- password | publickey | agent | keyboard-interactive
  key_id       TEXT REFERENCES ssh_keys(id) ON DELETE SET NULL,
  startup_cmd  TEXT,
  encoding     TEXT DEFAULT 'UTF-8',
  sort_order   INTEGER NOT NULL DEFAULT 0,
  created_at   INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL
);
CREATE INDEX idx_sessions_folder ON sessions(folder);

CREATE TABLE ssh_keys (
  id             TEXT PRIMARY KEY,
  name           TEXT NOT NULL,
  path           TEXT NOT NULL UNIQUE,   -- ~/.ssh/id_ed25519
  key_type       TEXT NOT NULL,          -- ed25519 | rsa | ecdsa
  fingerprint    TEXT NOT NULL,          -- SHA256:...
  has_passphrase INTEGER NOT NULL
);
```

**四条安全底线**（第 1 条是历史记录，已被实现推翻，保留是为了说明为什么）：

1. ~~**密码绝不进 SQLite。**~~ **这条后来被推翻了**，当前实现是密码与私钥口令**明文存在 `sessions` 表**里，
   `store::migrate_legacy_secrets()`（`store/mod.rs`）负责把旧版留在系统凭据服务里的密码搬进来。
   推翻的理由记在那个函数的注释里：凭据服务是**机器绑定**的，而这个工具的常见用法就是把
   `nexus.db` 拷到另一台机器接着用；凭据留在 Keychain 里会导致"配置拷过去了但一条都连不上"。
   代价是**拿到 `nexus.db` 就等于拿到全部密码**，所以第 3 条（0600）不是锦上添花，是这条反转之后仅剩的防线。
2. **第二因子例外：TOTP 种子必须加密。** `sessions.totp_seed` 存的是密文（`secret.rs`，AES-256-GCM，
   密钥在系统凭据服务里）。理由正是第 1 条的代价：密码既然已经明文躺在同一个文件里，
   种子再明文存进去，2FA 就直接降级成 1FA——拖走一个文件就两个因子全有了。
   加密之后这个字段**不跟着数据库走**：换机器要重填一次种子，密码则照常可用。
   这是有意的不对称，不是遗漏。
3. **导出功能必须显式地问：** 导出时是否包含凭据？默认**只导出连接参数不含密码/私钥**，需要"完整导出"就明确二次确认 + 提示风险。
   （现状比这还保守：`xsh::encode_xsh` 的签名里根本没有密码参数，结构上就导不出凭据。）
4. **数据库权限 0600。** `rusqlite` 打开后立刻 `fs::set_permissions(path, 0o600)`。

### 4.4 SSH Key 管理

```rust
use ssh_key::PrivateKey;

// 同时支持 OpenSSH 新格式与 PEM 旧格式
let key = PrivateKey::from_openssh(contents)   // BEGIN OPENSSH PRIVATE KEY
    .or_else(|_| PrivateKey::from_pem(contents))?;  // -----BEGIN RSA PRIVATE KEY-----

// 加密私钥需要 passphrase
if key.is_encrypted() {
    let k = key.decrypt(&passphrase)?;
}
```

要做的四件事：

- **扫描 `~/.ssh/`** 自动发现密钥，读取 public key 算 fingerprint
- **加密私钥的密码也进 Keychain**，按 key_id 索引，首次使用时从 Keychain 取，避免每次输
- **Agent 转发**：实现 `ChannelId` 的 `auth-agent-req` 转发；安全提示要明确——转发 agent 到不可信主机有风险，UI 上按会话独立开关
- **生成新密钥**：`PrivateKey::random(&mut OsRng, Algorithm::Ed25519)`，默认推 Ed25519（Xshell 8 也已支持，这是当前最佳实践）

### 4.5 SFTP —— 工程量最大的模块

**双栏 UI：左本地（Rust std::fs / tokio::fs），右远端（russh-sftp）。**

后端传输设计：

```rust
pub struct TransferTask {
    id: Uuid,
    direction: Direction,     // Upload | Download
    local: PathBuf, remote: PathBuf,
    size: u64, written: u64,
    cancel: CancellationToken,
}
```

必须实现的细节：

| 功能 | 要点 |
|---|---|
| **断点续传** | 记录已写 offset，重连后用 sftp `write` 的 offset 参数继续，切勿从头覆盖 |
| **进度上报** | 每 256KB 或每 200ms emit 一次，别每 chunk 都 emit——IPC 会打满 |
| **并发分子块** | 大文件（>100MB）拆 4 并发请求，russh-sftp 支持同一 handle 多路 read |
| **权限/时间的保留** | sftp `setstat` 回写 mtime 与 permissions，否则下载出来的文件权限全丢 |
| **编码问题** | Windows 服务端 GBK 文件名——提供编码开关，不要用 `String::from_utf8_lossy` 静默吞掉 |
| **取消语义** | 取消后必须 trunc 到 offset，不能留下半个损坏文件 |

> 拖拽上传务必注意：**WebView 的 drop 事件拿到的是文件路径清单**，走 `tauri::path` 的权限校验后才能读。Tauri 2 默认禁止任意路径访问，要显式配置 `fs` scope。

---

## 五、MVP 里程碑

| 阶段 | 目标 | 验收标准 |
|---|---|---|
| **M0** | Tauri 壳 + xterm 单标签 + russh 连通 | 能连上一台服务器执行 `ls` |
| **M1** | 布局树 + 多标签/多窗格 + 拖动 | 拖动 pane 不丢滚动历史 ⭐关键 |
| **M2** | SQLite 会话 + 增删改查 + Keychain | 重启后会话列表和布局都在 |
| **M3** | 密钥管理 + ed25519 生成 + agent | 用密钥无密码登录成功 |
| **M4** | SFTP 双栏 + 传输队列 | 传 1GB 文件不断线，中断可续 |
| **M5** | 主题字体打磨 + 签名公证 + dmg | 双击安装不报「无法验证开发者」 |

M0–M3 是"能用"，M4 是"好用"，M5 是"能分发"。**建议 M4 之前不要对外分享** —— 半成品终端工具的口碑很难挽回。

---

## 六、macOS 特有的坑（提前知道能省几天）

1. **公证（Notarization）** —— 没公证的 app 首次打开会被 Gatekeeper 拦。需要 Apple Developer 账号 + `codesign` + `xcrun notarytool submit`。**这是 macOS 分发最大的隐性成本。**
2. **输入法（IME）** —— 中文输入法在 WebView 里的组合输入需要 `compositionstart/end` 正确处理，否则打字会漏字或提前 commit。这是国产终端工具最常见的差评来源。
3. **`Ctrl+C` vs `Cmd+C`** —— macOS 习惯 Cmd+C 复制，但终端里 Ctrl+C 是 SIGINT。必须在 UI 层区分清楚，不可二选一。
4. **通用二进制** —— 本机是 arm64，若要支持 Intel Mac 需要 `lipo` 合并，`universal2-apple-darwin` target 要提前配 Rust toolchain。
5. **避免塞 Chromium** —— 你已经用 Tauri 了，别因为某个 CSS 效果去内嵌 WebView2 之类的东西，体积会失控。

---

## 七、边界与合规

| 项目 | 说明 |
|---|---|
| **不反编译 Xshell** | 违反其 EULA。本方案未使用任何反编译产物 |
| **不复用其专有实现** | 多进程 COM 架构值得"借鉴思路"，但 Rust 里用 tokio task 实现，代码层面零关联 |
| **配色/快捷键** | 通用部分可自由设计；若发布，建议 UI 布局和文档自成体系，不做"Xshell 复刻版"的市场定位 |
| **可以合法做的事** | 读公开 RFC、用 MIT/Apache 开源组件、做净室（clean-room）设计 |

**给这个工具的建议定位**：Mac 上确实缺一个「Xshell 体验」的原生轻量终端——有标签树、可靠的会话管理、顺手的 SFTP。这个需求真实存在，而且不需要踩任何灰线就能做到。

---

## 八、扩展：REA 在这个项目里能做什么

REA 不该用在分析 Xshell 上，但**用在自己项目上很有价值**：

```bash
# 分析自己编译产物的二进制布局
rea inspect-binary-layout target/release/bundle/macos/YourApp.app/Contents/MacOS/your-app

# 检查 dmg 里的实际内容与签名状态
rea inspect-artifact target/release/bundle/dmg/YourApp_0.1.0_aarch64.dmg

# 版本迭代时对比两个版本的差异（找体积膨胀、意外引入的依赖）
rea compare themed-app-v1.json themed-app-v2.json
```

这是它真正的用武之地——**分析你自己的产物**，而不是别人的专有软件。

---

*本方案所有技术选型均为公开标准与开源组件，可独立实现，无需参考任何专有二进制。*
