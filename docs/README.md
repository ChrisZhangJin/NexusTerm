# NexusTerm 文档索引

| 文档 | 内容 |
|---|---|
| [architecture.md](./architecture.md) | 整体架构：前后端分层、IPC 方式、关键实现取舍（为什么 base64、为什么 SFTP 独立连接等） |
| [xsh-format.md](./xsh-format.md) | Xshell `.xsh` 会话文件格式核对报告：字段表、编码、密码不可迁移的证据与结论 |
| [build.md](./build.md) | 构建 / 打包 / 签名全流程，含 macOS 的已知坑（dmg、Keychain ACL、Gatekeeper） |
| [portability.md](./portability.md) | 跨平台说明：三端差异、平台相关代码清单、Windows / Linux 构建前置条件 |

顶层 [README.md](../README.md) 是唯一入口，包含快速开始、目录结构与安全边界。

## 约定

- Rust 侧变更在 `src-tauri/`，前端在 `src/`，两者通过 `npm run dev` 联动调试。
- 新增平台相关的 Rust 代码必须用 `#[cfg(...)]` 分支，并把差异登记到
  [portability.md](./portability.md) 的清单里，避免以后找不到隐含的平台依赖。
