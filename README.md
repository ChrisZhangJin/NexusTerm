# NexusTerm

跨平台 SSH / SFTP 终端客户端。Rust 后端 + TypeScript / React 前端，基于 Tauri 2，安装包约 2.6 MiB。

多标签终端、断线自动重连、SFTP 双栏、ZMODEM（rz/sz）、资源监控，一个可以直接在远端「阅读」
Markdown / HTML / 图片 / 配置文件的独立窗口，以及可以绑定到会话上的 AI 助手。

| 平台 | 状态 |
|---|---|
| Windows (x86_64) | ✅ 实机构建并跑通，产物为 NSIS 安装包 / 免安装 exe |
| macOS | ✅ 实机构建并跑通 |
| Linux | ⚠️ 代码已就位，未实机验证 |

> **独立性声明**：本项目所有代码均为独立实现，依据 SSH 公开标准（RFC 4251–4254）、
> Xshell `.xsh` 的公开文档格式，以及开源组件（russh / xterm.js / SQLite / zmodem.js）编写。
> 开发过程中**未反编译任何第三方商业软件**，也未参考其实现细节。
> 多会话、密钥管理、SFTP、主题属于 SSH 客户端的通用标准能力。

---

## 功能

### 连接与会话

| 能力 | 说明 |
|---|---|
| SSH 连接 | russh 纯 Rust 实现，密码 / 私钥 / agent 三种认证；`~/.ssh` 私钥自动发现 |
| 本地终端 | 启动即开一个本机 PTY，没有 SSH 连接时也能当终端用 |
| 会话管理 | SQLite 持久化，分组、搜索、去重；「☰ 连接」入口位于标签栏最左侧 |
| 导入 / 导出 | Xshell `.xsh` 会话文件互通（密码不迁移）；入口在连接弹窗内 |
| 主机指纹校验 | Trust-on-First-Use，指纹变化即拒绝连接并告警 |

### 终端与标签

| 能力 | 说明 |
|---|---|
| 终端渲染 | xterm.js + Canvas + Unicode11，中文与 emoji 宽度正确 |
| 多标签 | 切换不销毁终端实例，滚动历史完整保留；支持复制会话 |
| 断线重连 | 断开后保留标签与历史；自动重连最多 10 次，间隔 2s 起逐次翻倍 |
| 上下文恢复 | 重连后按原工作目录 / tmux 会话 / docker 容器还原（旁路读取，不污染终端） |
| 选中即复制 | 鼠标选中文本自动写入剪贴板（可在设置中关闭） |
| 关键字标记 | 对整个缓冲区（含滚动历史）的命中文本高亮 |
| 快捷键 | 可在设置中自定义 |

### 文件

| 能力 | 说明 |
|---|---|
| SFTP 双栏 | 终端右键打开；本地 / 远端并列浏览，路径栏可直接编辑；上传、下载、建目录、重命名、删除 |
| ZMODEM（lrzsz） | 远端 `rz` / `sz` 自动触发；也可把文件**拖进终端**直接 `rz` 上传 |
| 阅读窗口 | 在独立窗口中查看远端或本地文件，见下表 |

**阅读窗口**——SFTP 里右键「阅读」/ 双击，或在终端里选中文件名后右键「阅读」（相对路径按 shell 当前目录解析）：

| 类型 | 格式 | 展示方式 |
|---|---|---|
| Markdown | md / markdown | GFM 渲染（表格、任务列表、代码块），可切换源码 |
| HTML | html / htm | 沙箱 iframe 渲染，**默认禁止脚本**，窗口内可临时开启 |
| 图片 | png / jpg / gif / webp / bmp / ico / svg / avif | 适应窗口 / 1:1、滚轮缩放、拖拽平移、透明棋盘格背景 |
| 配置文件 | json / yaml / toml / ini / conf / properties / xml / `.env` / Dockerfile … | 语法高亮 + 行号；JSON 语法校验（错误行标红）与一键格式化 |

文本编码自动探测（UTF-8 优先，回退 GBK）。

### 状态栏与外观

| 能力 | 说明 |
|---|---|
| 资源监控 | 远端 / 本机 CPU、内存、磁盘实时显示；同一主机的多个标签共享一路采集 |
| 状态指示 | NUM / CAP 锁定状态、输出字符数 |
| 主题与字体 | 多套内置主题，外壳颜色随终端配色派生；字号 / 行高实时预览，设置持久化 |

### AI 助手

终端会话的输出一边照常显示在屏幕上，一边**旁路复制**一份给 AI 助手。人既能继续在终端里干活，
也能在侧栏跟助手对话——助手因为拿到了实时的 console 流，回答时自带完整上下文，不用你再贴一遍报错。

| 能力 | 说明 |
|---|---|
| 旁路采集 | 每会话一个 256 KB 环形缓冲，区分「程序输出」与「人的输入」，剥离 ANSI 控制序列；超限丢弃最旧内容并计数 |
| 流式问答 | OpenAI 兼容协议与 Anthropic 原生协议双支持；SSE 流式返回逐段渲染，可随时取消；同一会话同时只放行一路提问 |
| 上下文窗口 | 默认取最近 32 KB（16 / 32 / 64 / 128 KB 可选），连同提问一起发出 |
| 出网前脱敏 | 默认开启，规则见下方「安全设计」 |
| 命令建议 | 回答里的代码块会升级成建议卡片，**需人工确认后才会写入终端**（只插入，不自动回车执行） |
| 每会话开关 | 新建会话时默认跟随「新会话自动旁路」，之后可在面板里单独切换；关掉即停止采集，清空即丢弃已缓存内容 |

**怎么配**：设置 → AI 助手。依次填 服务商（OpenAI / Anthropic / 自定义）、接口地址、模型、API Key，
可再选系统提示词（留空用内置默认）、上下文上限、是否脱敏、总开关与「新会话自动旁路」。
填完建议先点「测试连接」确认地址 / 模型 / Key 三件套可用。

面板里的消息按会话分桶隔离：切到另一个标签，看到的是那个会话自己的对话与缓冲，互不串台。

---

## 安全设计

- 会话密码明文保存在 `nexus.db` 的 `sessions.password` 列——刻意如此，换台机器复制数据库即可带着凭据走；安全边界落在文件权限 `0600` 上。
  旧版本存在系统凭据服务（macOS Keychain / Windows Credential Manager / Linux Secret Service）里的密码，会在首次启动时一次性迁入 db 并从凭据服务删除
- `nexus.db` 权限强制 `0600`，仅所有者可读写
- 主机公钥采用 TOFU 策略，指纹变化会中断连接并告警
- 导出 `.xsh` 时密码位永远留空，明文密码不落盘
- 阅读窗口：
  - Markdown 经 DOMPurify 净化，不执行任何脚本，外链交给系统浏览器
  - HTML 放在**不带 `allow-same-origin`** 的沙箱 iframe 中，由后端按需下发 CSP；即使开启脚本，页面也碰不到应用的 IPC 与存储
  - SVG 以 `<img>` 方式加载，其中的脚本不会执行
  - 单文件上限：图片 50 MB，其他 20 MB；窗口关闭即释放内容
- AI 助手（这是会**把数据发往外部**的功能，逐条看清楚再开）：
  - API Key 与会话密码**同一套策略**：明文保存在 `nexus.db`（便于跨机器迁移），安全边界是文件权限 `0600`；
    备份或迁移数据库文件时它就包含在里面，不是系统钥匙串
  - 总开关默认关闭；开启后，**该会话的终端输出会发往第三方模型服务**——屏幕上出现过的内容都可能出网
  - 脱敏默认开启（`sk-` 前缀 key、`Bearer`、AWS `AKIA`、PEM 私钥块、邮箱、IPv4，以及 URL / 命令行里
    `token` `api_key` `password` 等参数的值）。它只能**降低**风险，**无法保证不泄露**：
    敏感环境请谨慎开启，或改用自托管模型（服务商选「自定义」填入自己的地址）
  - AI 给出的命令**不会自动执行**，只进建议区，人工确认后才写入终端，且只插入不回车
  - 不放心时随时关：面板里的旁路开关即时生效，「清空」会丢弃该会话已缓存的输出

---

## 快速开始

```bash
npm install
npm run tauri:dev      # 开发模式（热重载）
npm run tauri:build    # 生产构建
```

各平台的前置依赖见 [docs/build.md](./docs/build.md) 与 [docs/portability.md](./docs/portability.md)。

- **Windows**：构建产物为 `src-tauri/target/release/nexus-term.exe`（可直接运行）与
  `bundle/nsis/NexusTerm_<版本>_x64-setup.exe`（安装包）。
- **macOS**：首次双击若被 Gatekeeper 拦下，**右键 → 打开**即可（仅首次），原因与签名方案见 [docs/build.md](./docs/build.md)。

---

## 目录结构

```
nexus-term/
├── src/                        前端（TypeScript / React）
│   ├── components/             TitleBar · TabBar · SessionList · TerminalView · FileManager
│   │                           · StatusBar · SettingsDialogs · IOMenu · ConfirmDialog
│   │                           · AgentPanel（AI 对话面板）…
│   ├── reader/                 阅读窗口：Reader · ImageView · CodeView
│   ├── lib/                    theme / chrome（配色与外壳派生）· prefs（偏好 store）
│   │                           · zmodem（lrzsz）· context（远端上下文）· highlight（关键字标记）
│   │                           · readable（可阅读类型）· agent（AI 配置 store）
│   │                           · errors · auth · platform
│   ├── api.ts                  IPC 封装 · base64 转码
│   └── main.tsx                入口：主窗口 / 阅读窗口分流
├── src-tauri/src/              后端（Rust）
│   ├── cmd.rs                  #[tauri::command] 入口
│   ├── ssh/                    russh Handler · 会话池 · 主机指纹 TOFU · exec 旁路
│   ├── local/                  本地 PTY（portable-pty）
│   ├── sftp/                   SFTP 子系统
│   ├── reader.rs               阅读窗口：文档暂存 · 窗口管理 · nxdoc 协议（CSP）
│   ├── sysstat.rs              本机 CPU / 内存 / 磁盘采集
│   ├── agent/                  AI 旁路：环形缓冲 · 脱敏 · LLM 客户端（OpenAI / Anthropic）
│   ├── store/                  SQLite 会话仓库 + 系统凭据封装
│   ├── keys/                   OpenSSH 私钥解析 · ~/.ssh 自动发现
│   ├── xsh.rs                  Xshell 会话文件解析 / 生成
│   └── errors.rs               errno（含 Windows WSAE*）→ 中文可读提示
├── src-tauri/capabilities/     权限集（main 与 reader-* 窗口）
├── src-tauri/tauri.windows.conf.json   Windows 专属配置（无边框 + NSIS）
└── docs/                       文档
```

## 架构

```
┌── Frontend · TypeScript / React ─────────────────────────────────┐
│  App.tsx         标签 / 布局编排 · 重连调度 · 上下文还原          │
│  TerminalView    xterm.js 实例 · 尺寸同步 · ZMODEM 嗅探 · 右键菜单 │
│  Reader          阅读窗口（独立 WebviewWindow，同一前端包分流）   │
│  api.ts          IPC 封装 · base64 转码                          │
│  AgentPanel   AI 对话面板 · 按会话分桶 · 建议命令二次确认         │
└──────────────────────────────┬───────────────────────────────────┘
                     Tauri IPC · invoke / emit · nxdoc://
┌── Backend · Rust / tokio ────┴───────────────────────────────────┐
│  cmd.rs     命令入口          ssh/      russh + 会话池 + exec     │
│  local/     本机 PTY          sftp/     russh-sftp                │
│  store/     SQLite + 凭据     reader.rs 阅读文档 + 自定义协议     │
│  sysstat.rs 本机资源          xsh.rs    Xshell 会话文件           │
│  agent/     AI 旁路缓冲 · 脱敏 · LLM 客户端                       │
└──────────────────────────────────────────────────────────────────┘
```

关键实现取舍（为什么 SSH 数据走 base64、为什么 SFTP 用独立连接、为什么终端绑定标签而非面板）
见 [docs/architecture.md](./docs/architecture.md)。

---

## 文档

| 文档 | 内容 |
|---|---|
| [docs/architecture.md](./docs/architecture.md) | 架构与关键实现取舍 |
| [docs/xsh-format.md](./docs/xsh-format.md) | Xshell `.xsh` 格式核对报告 |
| [docs/build.md](./docs/build.md) | 构建 / 打包 / 签名，含 macOS 三个坑 |
| [docs/portability.md](./docs/portability.md) | 跨平台差异清单与 Windows / Linux 前置依赖 |

## 路线图

- [x] Windows 实机构建验证（x86_64-pc-windows-msvc，NSIS 安装包）
- [x] 外观与偏好持久化
- [x] 断线自动重连与远端上下文恢复
- [x] 资源监控（远端 / 本机）
- [x] 阅读窗口（Markdown / HTML / 图片 / 配置文件）
- [x] AI Agent 绑定会话：把每个 session 的输出流实时旁路给 agent，人既能与 console 交互、
      也能与 agent 交互。本次落地范围是**旁路采集 + 流式问答 + 出网前脱敏 + 每会话开关**
      （抓原始字节流），关键取舍里的脱敏与开关已解决；
      **「抓语义块」（OSC 133 shell 集成，区分 prompt / 命令输出 / 退出码）仍为待办**
- [ ] 传输队列与进度条（SFTP 当前为单次同步传输，无并发 / 断点续传）
- [ ] 多窗格拆分（布局树已设计，尚未实现拖拽）
- [ ] 端口转发隧道
- [ ] Linux 实机构建验证
- [ ] 旧 ssh-dss 算法兼容（russh 不支持，需老旧设备时再加 `ssh2` 回退路径）
- [ ] 内置会话多路复用：不依赖远端 tmux，把「pty 持有 + 屏幕模型 + 布局树 + 客户端协议」
      做进 Rust 后端。注意远端会话保活仍必须在远端有进程（tmux / dtach / 自研 agent），
      客户端无法凭空改变远端进程生命周期

## 许可

待定。代码中不含任何第三方专有实现。
