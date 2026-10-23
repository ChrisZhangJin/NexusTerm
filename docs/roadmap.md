# Roadmap / 待办方向

记录已经讨论过设计、但还没动手实现的大方向，避免讨论过的结论随对话丢失。

## Nexus 控制面：本机 Socket API + nexus-cli

**现状**：AI Agent 面板只能在 app 内部、对当前激活的单个标签工作（进程内 `invoke()` 调用）。

**目标**：让本机其它 AI agent（如 Claude Code）能通过一个独立的 `nexus-cli` 二进制 + 本机
Unix socket / Windows 命名管道，枚举/读取/控制 NexusTerm 管理的所有会话（本地 + 远程 SSH），
实现"session A 的输出原样转发给 session B 做输入"这类编排。这是 NexusTerm 相对于
`herdr`（本机进程型 agent 编排器）的差异化场景——Nexus 管理的大多是**远程**会话，对端通常没有
权限装任何探针，只能靠这一端"看字节流 + 发按键"。

**已确认的设计约束**：
- 传输：本机 Unix socket / 命名管道，不监听网络端口。
- 鉴权：进程启动时生成一次性 token，写入仅当前用户可读的文件，CLI 调用必须带 token。
- A→B 转发语义：原样转发（把 A 的某段输出原文当作 B 的输入发送），不做模型摘要/解析。
- **必须保留现有隐私契约**：`agent/mod.rs` 的"AI Agent 总开关关闭时后端完全不采集旁路，
  零开销"不能被控制面绕过——控制面需要自己独立的开关，和 AI Agent 开关各自独立、按位或后
  共同决定"是否缓冲"，但不共享"是否允许 LLM 出网"等其它语义。

**关键发现**（为后续实现省去重新调研）：
- 后端 `agent/buffer.rs` 的按 session id 环形缓冲**已经**覆盖所有打开的本地/SSH 标签，
  与前端是否渲染无关——`local/mod.rs` 的 PTY 读线程、`ssh/session.rs` 的读循环在往前端发
  事件的同时，无条件调用 `state.agent.feed(id, data, kind)`。控制面的"读"完全可以复用
  `AppState.agent.snapshot(id)` / `.context(id, n)`，不需要另起一份缓冲。
- "枚举当前打开了哪些 session"目前完全不存在：标签 id 是前端铸造的不透明字符串
  （`tab-<ts>-<seq>`），后端 `LocalManager`/`SessionManager` 只把它当 `HashMap` key 用，
  没有任何 `list_ids()` 之类的方法——这是新增能力，不是复用。
- 详细架构方案见 `.claude/plans/harmonic-splashing-wirth.md`（如果本地还留着的话），
  核心包括：`control/` 新模块（protocol/server/auth）、`AgentManager` 拆分
  `enabled` 和 `control_plane_enabled` 两个开关、`nexus-cli` 作为同一 cargo package 下的
  第二个 `[[bin]]`、设置页「行为」弹窗里加开关 + token 展示。

**优先级**：低于"完善面板上的 Nexus Agent 本身能力"——先把单会话场景做扎实，再考虑跨会话编排。

## 面板 Nexus Agent 能力增强（进行中）

见对话记录；下一步是梳理现有 tool call 清单，决定要补齐哪些。
