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

## 连接配置关联 TOTP 种子，二次验证自动填码

**现状**：连跳板机要走两道——先密码，再 keyboard-interactive 问 MFA 验证码。
后者每次都弹框等人手输，开一个标签输一次，频繁连同一台机器时很烦。

**外面的做法**：有人用 expect 脚本绕过去——`spawn ssh ...`，匹配到 `password:`
发密码，匹配到 `[OTP Code]:` 就 `exec oathtool --totp -b <种子>` 算一个发进去。
能跑，但要在客户端外面维护一份脆弱的提示语匹配，密码和种子还常常明文写在脚本里。
这件事放进客户端做更干净：我们本来就拿得到结构化的 prompt，不必猜。

**目标**：连接配置里可选地存一个 TOTP 种子（base32）；遇到二次验证提示时按 RFC 6238
本地算码并预填，用户确认或直接自动提交。只做标准 TOTP——Duo / RSA SecurID 这类
种子在厂商侧的拿不到，不在范围内。

**关键发现**（省去重新调研）：
- **接入点已经现成**：`ssh/session.rs` 的 KBI 循环（`Resp::InfoRequest` 分支，约
  1010-1045 行）已经有一套预填机制——`AuthPromptField { prompt, echo, prefill }`，
  当前只在 `asks_for_password()` 命中时用 `reusable_password()` 填密码。加 TOTP 就是
  在同一处多一条判断，填 `prefill`，前端那边一行不用改。
- **提示语判定可以照抄**：`asks_for_password()`（session.rs:821）就是
  `echo == false` 加几个关键词匹配。验证码正相反——服务端通常 `echo == true`
  （截图里 `257357` 是回显出来的），关键词 `otp` / `mfa` / `verification` /
  `one-time` / `验证码` / `动态码`。`echo` 这个信号比关键词可靠，先看它。
- **`saw_challenge` 的语义要跟着改**：现在的逻辑是「`prefill` 为空 ⇒ 这是挑战题」，
  一旦给验证码也填上 `prefill`，这个推断就废了，失败信息会从「验证码错误或已过期」
  退化成「密码不正确」。得把判定从「有没有预填」换成「这道题是不是密码题」。
- **种子存哪里是个要单独决定的事**：`store/mod.rs` 的 `migrate_legacy_secrets()`
  说明密码已经从系统凭据服务**迁出**、改存 db 明文列了。TOTP 种子跟着走最省事，
  但密码和种子躺在同一个明文文件里，二次验证就名存实亡——至少值得考虑种子单独
  回到 keyring（`keyring` crate 还在 Cargo.toml 里，三端都有）。
- 算码本身不用造轮子，Rust 侧有现成的 TOTP 库；注意剩余窗口不足两三秒时要等下一个
  窗口再发，否则刚算出来就过期。

**待定**：预填后要不要自动提交（省一次回车 vs 失手连错触发账号锁定）；多台机器共用
一个种子的情况要不要支持种子独立于连接存一份。

**优先级**：看实际使用频率。要连跳板机的人每天烦好几次，不连的人完全用不上。
