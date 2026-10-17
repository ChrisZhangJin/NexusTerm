# NexusTerm

跨平台 SSH / SFTP 终端。Rust 后端 + TypeScript / React 前端，基于 Tauri 2。

macOS / Windows / Linux 三端可构建（详见 [跨平台说明](./docs/portability.md)）。

实测状态：**macOS 与 Windows 均已实机构建并跑通**，Windows 产物为 NSIS 安装包
（约 2.5 MiB，见 [docs/build.md](./docs/build.md)）；Linux 仍是「代码已就位、未实机验证」。

> **独立性声明**：本项目所有代码均为独立实现，依据 SSH 公开标准（RFC 4251–4254）、
> Xshell `.xsh` 的公开文档格式，以及开源组件（russh / xterm.js / SQLite / zmodem.js）编写。
> 开发过程中**未反编译任何第三方商业软件**，也未参考其实现细节。
> 多会话、密钥管理、SFTP、主题属于 SSH 客户端的通用标准能力。

---

## 功能

| 能力 | 说明 |
|---|---|
| SSH 连接 | russh 纯 Rust 实现，密码 / 私钥 / agent 三种认证 |
| 终端渲染 | xterm.js + Canvas + Unicode11，中文与 emoji 宽度正确 |
| 本地终端 | 启动即开一个本机 PTY，没有 SSH 连接时也能当终端用 |
| 多标签 | 切换标签不销毁 Terminal 实例，滚动历史完整保留 |
| SFTP 双栏 | 本地 / 远端并列浏览，上传、下载、建目录、重命名、删除 |
| ZMODEM（lrzsz） | 远端 `rz` / `sz` 自动触发上传 / 下载，无需额外操作 |
| 会话持久化 | SQLite，支持分组、搜索、增删改 |
| Xshell 互操作 | 导入 / 导出 `.xsh` 会话文件（密码不可迁移） |
| 凭据安全 | 密码只存系统凭据服务（Keychain / Credential Manager / Secret Service），绝不落库 |
| 主题与字体 | 多套内置主题，外壳颜色随终端配色派生；字号 / 行高可实时预览 |
| 主机指纹校验 | Trust-on-First-Use，指纹变化即拒绝连接 |

---

## 快速开始

```bash
npm install
npm run tauri:dev      # 开发模式（热重载）
npm run tauri:build    # 生产构建
```

各平台的前置依赖见 [docs/build.md](./docs/build.md) 与 [docs/portability.md](./docs/portability.md)。

macOS 首次双击若被 Gatekeeper 拦下，**右键 → 打开**即可（仅首次），
原因与各类签名方案见 [docs/build.md](./docs/build.md)。

---

## 目录结构

```
nexus-term/
├── src/                      前端（TypeScript / React）
│   ├── components/           TitleBar / Toolbar / TabBar / SessionList / TerminalView …
│   ├── lib/                  theme（配色）· chrome（外壳派生）· errors（友好报错）
│   │                         · zmodem（lrzsz）· auth（认证判定）
│   │                         · platform（宿主平台探测）
│   ├── api.ts                IPC 封装 · base64 转码
│   └── types.ts
├── src-tauri/src/           后端（Rust）
│   ├── cmd.rs                #[tauri::command] 入口
│   ├── ssh/                  russh Handler · 会话池 · 主机指纹 TOFU
│   ├── local/                本地 PTY（portable-pty）
│   ├── sftp/                 SFTP 子系统
│   ├── store/                SQLite 会话仓库 + 系统凭据封装
│   ├── keys/                 OpenSSH 私钥解析 · 家目录 / ~/.ssh 自动发现
│   ├── xsh.rs                Xshell 会话文件解析 / 生成
│   ├── errors.rs             errno（含 Windows WSAE*）→ 中文可读提示
│   └── capabilities/         权限集
├── src-tauri/tauri.windows.conf.json   Windows 专属配置（无边框 + NSIS）
├── docs/                     文档（索引见 docs/README.md）
└── src-tauri/icons/          macOS .icns / Windows .ico / Linux PNG
```

## 架构

```
┌── Frontend · TypeScript / React ─────────────────────────────┐
│  App.tsx            Tab / 布局编排 · 外观设置                 │
│  TerminalView       xterm.js 实例 · 尺寸同步 · ZMODEM 嗅探    │
│  api.ts             IPC 封装 · base64 转码                    │
└──────────────────────────┬───────────────────────────────────┘
                      Tauri IPC · invoke / emit
┌── Backend · Rust / tokio ┴───────────────────────────────────┐
│  cmd.rs        命令入口        ssh/    russh + 会话池          │
│  local/        本机 PTY        sftp/   russh-sftp + SFTP 命令  │
│  store/        SQLite + 凭据   xsh.rs  Xshell 会话文件          │
└───────────────────────────────────────────────────────────────┘
```

完整的关键实现取舍（为什么 SSH 数据走 base64、为什么 SFTP 用独立连接、为什么 Terminal
绑定 Tab 而非面板）见 [docs/architecture.md](./docs/architecture.md)。

---

## 文档

| 文档 | 内容 |
|---|---|
| [docs/architecture.md](./docs/architecture.md) | 架构与关键实现取舍 |
| [docs/xsh-format.md](./docs/xsh-format.md) | Xshell `.xsh` 格式核对报告 |
| [docs/build.md](./docs/build.md) | 构建 / 打包 / 签名，含 macOS 三个坑 |
| [docs/portability.md](./docs/portability.md) | 跨平台差异清单与 Windows / Linux 前置依赖 |

## 待办

- [ ] 传输队列与进度条（SFTP 当前为单次同步传输，无并发 / 断点续传）
- [ ] 多窗格拆分（布局树已设计，尚未实现拖拽）
- [ ] 端口转发隧道
- [ ] 会话 / 外观配置持久化（目前每次启动回到默认值）
- [x] Windows 实机构建验证（x86_64-pc-windows-msvc，NSIS 安装包）
- [ ] Linux 实机构建验证
- [ ] 旧 ssh-dss 算法兼容（russh 不支持，需老旧设备时再加 `ssh2` 回退路径）

## 安全边界

- 密码存系统凭据服务（macOS Keychain / Windows Credential Manager / Linux Secret Service）
- `nexus.db` 权限强制 `0600`，仅所有者可读写
- 主机公钥采用 TOFU 策略，指纹变化会中断连接并告警
- 导出 `.xsh` 时密码位永远留空，明文密码不落盘

## 许可

待定。代码中不含任何第三方专有实现。
