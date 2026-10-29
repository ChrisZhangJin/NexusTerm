# NexusTerm

![CI](https://img.shields.io/github/actions/workflow/status/ChrisZhangJin/NexusTerm/release.yml?logo=github&label=CI)
![Rust](https://img.shields.io/badge/Rust-1.77.2%2B-00ADD8?logo=rust&logoColor=white)
![License](https://img.shields.io/badge/License-MIT-blue)
![status](https://img.shields.io/badge/status-prototype-orange)
![platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey)

跨平台 SSH / SFTP 终端客户端。Rust 后端 + TypeScript / React 前端，基于 Tauri 2，安装包约 2.6 MiB。

多标签终端、断线自动重连、SFTP 双栏、ZMODEM（rz/sz）、资源监控，一个可以直接在远端「阅读」
Markdown / HTML / 图片 / 配置文件的独立窗口，以及可以绑定到会话上的 AI 助手。

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

终端输出一边照常显示，一边**旁路复制**一份给 AI 助手。助手拿到实时 console 流，
回答自带完整上下文，不用再贴一遍报错。

| 能力 | 说明 |
|---|---|
| 旁路采集 | 每会话一个 256 KB 环形缓冲，区分「程序输出」与「人的输入」，剥离 ANSI 控制序列 |
| 流式问答 | OpenAI 兼容协议与 Anthropic 原生协议双支持；SSE 流式返回逐段渲染，可随时取消 |
| 上下文窗口 | 默认取最近 32 KB（16 / 32 / 64 / 128 KB 可选），连同提问一起发出 |
| 命令建议 | 回答里的代码块会升级成建议卡片，**需人工确认后才会写入终端**（只插入，不自动执行） |
| 每会话开关 | 新建会话默认跟随全局设置，之后可在面板单独切换；关掉即停止采集，清空即丢弃缓存 |
| 出网脱敏 | 默认开启：API Key、私钥块、邮箱、IP、URL / 命令行里的 token / password 等参数值 |

**配置**：设置 → AI 助手，依次填 服务商（OpenAI / Anthropic / 自定义）、接口地址、模型、API Key，
填完点「测试连接」确认三件套可用。消息按会话分桶隔离，切标签互不串台。

---

## 使用

### 安装

从 [Releases](https://github.com/ChrisZhangJin/NexusTerm/releases) 下载安装包
（Windows NSIS 安装包 / 免安装 exe，macOS universal）。

### 从源码构建

```bash
npm install
npm run tauri:dev      # 开发模式（热重载）
npm run tauri:build    # 生产构建
```

- **Windows**：产物为 `src-tauri/target/release/nexus-term.exe`（可直接运行）与
  `bundle/nsis/NexusTerm_<版本>_x64-setup.exe`（安装包）。
- **macOS**：首次双击若被 Gatekeeper 拦下，**右键 → 打开**即可（仅首次）。
- 各平台前置依赖见 [docs/build.md](./docs/build.md) 与 [docs/portability.md](./docs/portability.md)。

---

## 文档

| 文档 | 内容 |
|---|---|
| [docs/architecture.md](./docs/architecture.md) | 架构与关键实现取舍 |
| [docs/xsh-format.md](./docs/xsh-format.md) | Xshell `.xsh` 格式核对报告 |
| [docs/build.md](./docs/build.md) | 构建 / 打包 / 签名，含 macOS 三个坑 |
| [docs/portability.md](./docs/portability.md) | 跨平台差异清单与 Windows / Linux 前置依赖 |

## 许可

[MIT](./LICENSE)
