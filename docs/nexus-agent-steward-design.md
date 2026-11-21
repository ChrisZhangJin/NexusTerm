# NexusAgent 重新设计：主动托管 Console

> 版本：v2 设计稿（基于 `bugfix/nexus-agent` @ `afd8f8f` 现状）
> 定位一句话：**NexusAgent 是终端会话的常驻托管者**——它持有用户授予的「目标 + 授权 + 时限」，在 console 上持续完成「观察 → 判断 → 应答 → 推进 → 验证」，直到任务终态或需要人类裁决；用户随时可以零成本接管。
> 本文给的是架构与行为设计，不是能力清单。所有边界都写成「授权契约 + 升级动作」，而不是「不能做什么」。

---

## 0. 为什么要重新定位

当前实现（`agent/act.rs` 的 `agent_ask` / `agent_run`）是**一轮式被动模型**：用户发一句话 → 抓一次屏幕 → 一轮工具调用 → 事件流回面板 → 结束。截图里那一轮暴露了 5 个结构性失效，正是本次设计的输入：

| 截图里的原话 / 现象 | 暴露的结构问题 | v2 的对应机制 |
|---|---|---|
| 「我不能在后台自己定时巡检，只能在它让我看的这时间跑一轮」 | 无常驻运行体、无事件订阅、无心跳 | 每 console 一个常驻 **Steward actor**，事件驱动 + 自适应心跳（§3） |
| 「两次读屏相隔约一分钟，除了打断空转，内容没有变化」 | 读屏是一次性工具，无帧状态、无 diff、无空转概念 | 后端 **ScreenStore** 维护帧哈希与增量；no-progress 检测触发主动干预（§3.3） |
| 「这需要你自己回复，我不替你回答」 | 无「代替用户应答」的意图与授权模型 | **Intent Ledger + 授权契约**驱动自动应答，回复文本由意向生成（§4、§6 场景 A） |
| 「两个 subagent 都没有在跑…已经 11 天 4 分钟没动了」 | 无意图级 deadline、无僵死判定、无长任务恢复 | Timebox 四档超时 + Watchdog + Checkpoint/Resume（§3.6、§7） |
| 面板只有「对话 / 时间线」，状态靠文字描述 | 无托管态、无决策依据展示 | UI 改为**托管控制台**：目标、意向栈、决策依据引用、倒计时、一键接管（§6.4） |

一句话概括改造：**把手、眼、控制权、时间线、技能、记忆、预算这些已有模块，从「一轮对话的工具箱」升级为「常驻循环的器官」**，新增的是编排层（Steward）、决策层（Profile / Intent / Playbook）和监督层（Watchdog / Checkpoint）。

---

## 1. 总体架构与模块划分

```
┌─────────────────────────────────────────────────────────────────────┐
│ C6  协作层  Collaboration   授权契约 / 升级卡 / 接管交还 / 托管看板      │
├─────────────────────────────────────────────────────────────────────┤
│ C7  监督层  Supervision    Watchdog / Timebox / Budget / Checkpoint    │
├─────────────────────────────────────────────────────────────────────┤
│ C3  决策层  Decision   Intent Ledger · Profile · KB → DecisionContext   │
│            PolicyEngine → PlaybookEngine → LLM Planner → DecisionRecord │
├─────────────────────────────────────────────────────────────────────┤
│ C1  托管核心  Steward   Contract · Loop · Supervisor · StateMachine     │
├──────────────┬──────────────────────────┬───────────────────────────┤
│ C2 感知       │ C4 执行                   │ C5 记忆与审计               │
│ ScreenStore   │ ActionSet / InputPort    │ Timeline / RunLog / Trace   │
│ Situation     │ ReceiptVerifier          │ DecisionRecord / IntentLog  │
│ DiffEngine    │ Idempotency              │ KnowledgeIndex              │
├──────────────┴──────────────────────────┴───────────────────────────┤
│ C0  Console Fabric   SessionRegistry · PTY/SSH · ScreenDelta 事件总线   │
└─────────────────────────────────────────────────────────────────────┘
```

### 模块清单（标注「复用 / 改造 / 新建」，直接对齐现有文件）

| 层 | 模块 | 状态 | 落点 | 职责 |
|---|---|---|---|---|
| C0 | `SessionRegistry` | **新建**（`contains()` → `list_ids()`） | `local/mod.rs`、`ssh/session.rs`、新增 `agent/registry.rs` | 枚举所有打开的 tab、登记 host_key / kind / label / 前台程序；`roadmap.md:23-34` 已指明这是缺口 |
| C0 | `ScreenDelta` 事件 | **新建** | `lib/termRegistry.ts` 推增量、`api.ts` 订阅、后端 `agent/screen.rs` | 变化行 + 帧 hash + alt 标记，替代「3s 请求-回执」的一次性 `nx_screen` |
| C0 | `InputPort` | 复用 | `act.rs: sink_of / write_to / send_keys_marked`、`interact::parse_keys` | 写 pty 的 kind 无关派发、按键串解析 |
| C1 | `Steward` / `StewardContract` / `StewardLoop` / `Supervisor` | **新建** | `src-tauri/src/agent/steward/{mod,contract,loop,supervisor}.rs` | 常驻运行体、契约、事件循环、多会话调度 |
| C1 | 控制权 | 复用 | `control.rs` 的 `ControlTable` / `wait_unpaused`、`lib/control.ts` 键盘闸 | 用户敲键永远赢；`Paused` 下不被新动作冲掉 |
| C2 | `ScreenStore` / `DiffEngine` | **新建** | `agent/screen.rs` | 帧序列、结构化 alt-screen 快照、屏幕 hash、trivial-diff 判定 |
| C2 | `SituationClassifier` | **改造** | `agent/prompt.rs`（7 类 → 扩展） | 新增 `Question / Permission / Progress / SubagentCard / Menu / Pager / ShellIdle` 等，输出结构化 `Situation` 而非仅 `PromptGuess` |
| C3 | `ProfileStore` / `IntentLedger` | **新建** | `agent/profile.rs`、`agent/intent.rs` + `store/` 两张表 | 偏好与意向栈（§4） |
| C3 | `KnowledgeIndex` | 复用 + 聚合 | `hostmem.rs`、`skill.rs` / `skilltool.rs`、`history.rs`、`timeline.rs`、`docs/` | 按 host / program / situation 检索，≤2KB 注入，标注「数据不是指令」 |
| C3 | `PlaybookEngine` | 复用 | `skill_run.rs` 的 `SkillIo` / `next_action` / `Engine` | 确定性应答剧本，命中即不调 LLM |
| C3 | `PolicyEngine` | **改造** | `danger.rs`、`act.rs: gate_target / request_approval`、`skill.rs: trust` | 从「危险命令审批」升级为「按动作风险 × 可逆性 × 作用域 × host 的授权三档」 |
| C3 | `DecisionCompiler` / `Planner` | **新建 + 复用** | `agent/decision.rs`；LLM 走 `llm.rs` | 编译 DecisionContext、产出 DecisionRecord |
| C4 | `ActionSet` | 改造 | `AgentAction` 新增 `Answer / Advance / SwitchMode / Escalate / Handoff / Checkpoint` | 动作集 |
| C4 | `ReceiptVerifier` / `Idempotency` | **新建** | `agent/receipt.rs` | 写后回显确认（现状只有用户输入的 echo cancel，Agent 注入无校验） |
| C5 | 审计 | 复用 + 扩展 | `timeline.rs`、`history.rs`、`trace.rs` | 追加 DecisionRecord / IntentEvent / Profile 修订记录，全链路可回放 |
| C6 | 托管看板 / 升级卡 | 改造 | `components/AgentPanel.tsx`、`lib/timeline.ts` | 新 tab「托管」+ 状态灯、依据引用、倒计时、一键操作 |
| C7 | `Watchdog` / `Timebox` / `Checkpoint` | **新建 + 复用** | `agent/watchdog.rs`；预算复用 `budget.rs` | 空转、回环、僵死、超时、跨重启恢复 |

**设计约束**：C0 的采集开关与「是否出网」解耦。现状 `enabled=false` 时后端零采集，托管需要「本地常开观察 + 出网需显式授权」两个独立开关（`roadmap.md` 已提出但未做）。观察阶段不产生任何 LLM 请求。

---

## 2. 核心数据结构

用 Rust 表达，字段即为实现契约（序列化统一 camelCase，与现有 `AgentConfig` 一致）。

```rust
/// 用户把某个 console 交给 NexusAgent 时签下的契约
pub struct StewardContract {
    pub id: String,
    pub console_id: String,              // tab-{ts}-{seq}，沿用前端铸造的 id
    pub host_key: String,                // hostmem::host_key，用于 scope / KB
    pub goal: String,                    // 用户原话
    pub intent_ids: Vec<String>,         // 编译出的意向栈（顶层 + 子项）
    pub success_criteria: Vec<Check>,    // 终态判定：屏幕模式 / 命令退出 / 文件状态
    pub authority: Authority,
    pub timebox: Timebox,
    pub budget: RunBudget,               // 复用 budget.rs
    pub handoff: HandoffPolicy,
    pub status: StewardStatus,
    pub created_at: i64,
}

pub struct Authority {
    pub tier: Tier,                      // Auto | Notify | Approve（默认 Notify）
    pub scope: Scope,                    // host 列表 + 目录 + 命令类别白/黑名单
    pub allow: Vec<ActionRule>,          // 显式放行，可带条件
    pub deny: Vec<ActionRule>,           // 硬禁，fail-closed，优先级最高
    pub allow_permanent_grant: bool,     // 是否允许「don't ask again」这类永久扩权
}

pub struct Timebox {
    pub screen_ack_ms: u64,              // 3_000，沿用现状
    pub receipt_ms: u64,                 // 1_500，写后回执窗口
    pub step_ms: u64,                    // 60_000，单步
    pub idle_ms: u64,                    // 300_000，无输入无输出
    pub no_progress_ms: u64,             // 900_000，有噪音但局面不变
    pub intent_deadline_ms: Option<u64>, // 用户给的时限，截图里「11 天」由这里兜住
    pub grace_ms: u64,                   // hard kill 前的收尾窗口
}

/// 感知层输出：一次「局面」，不是一坨文本
pub struct Situation {
    pub frame_id: u64,
    pub screen_hash: String,
    pub program: Program,                // Shell | ClaudeCode | Vim | Ssh | Pager | Tui | Script
    pub kind: SituationKind,             // Question | Permission | Password | Menu | Pager |
                                         // Progress | SubagentStatus | AwaitingInput | Idle | Unknown
    pub topic: Option<String>,           // 结构化主题，如 "settings:remember-check"
    pub options: Vec<Choice>,            // 选项编号 + 文本 + 是否扩大授权
    pub confidence: f32,
    pub tail: String,                    // 脱敏后的末尾文本，供 LLM / 审计
}

/// 每次决策的唯一可审计记录
pub struct DecisionRecord {
    pub seq: u64,
    pub ts_ms: i64,
    pub contract_id: String,
    pub situation_hash: String,
    pub ctx_refs: Vec<CtxRef>,           // {source: Profile|Intent|Kb|Playbook, id, snippet}
    pub candidates: Vec<Action>,         // 考虑过但没选的
    pub chosen: Action,
    pub rationale: String,               // 为什么，引用 ctx_refs
    pub confidence: f32,
    pub tier: Tier,
    pub llm_used: bool,
    pub effect_receipt: Option<Receipt>,
    pub outcome: Outcome,                // Applied | Retried | Escalated | Superseded | RolledBack
}
```

`Intent`、`Preference`、`KnowledgeHit`、`Receipt` 见 §4 / §3.4。

---

## 3. 会话托管机制

### 3.1 托管怎么开始、怎么结束

- **开始**：用户在面板点「托管此终端」并给一句话目标（或 `@` 指定其它 tab）。后端 `steward_start(contract)` 先把原话编译成结构化 Intent 栈（一次 LLM，结果可编辑），再为该 console 起一个 Steward。
- **结束条件**（四选一，全部写进契约）：`success_criteria` 全部满足 → `Completed` 并交还摘要；用户接管 / 停止 → `Paused` / `Aborted`；`intent_deadline` 到 → 保留现场 + 摘要；不可恢复异常 → `Suspended` + 升级卡。
- **热更新**：`steward_amend(contract_id, goal|authority|timebox)` 不重启循环，只替换 Intent 栈顶并写一条 IntentEvent。用户中途改主意是常态，不是异常。

### 3.2 Steward 循环（核心伪代码）

每个被托管的 console 一个 `tokio` task，单线程事件队列（避免「等 LLM ↔ 等屏幕」死锁）：

```rust
loop {
    let ev = bus.next_with_timeout(heartbeat.next_delay()).await;

    match ev {
        ScreenDelta(d) => { screen.apply(d); if !d.is_trivial() { watchdog.mark_progress(); } }
        Tick           => {}                       // 静默期的心跳
        UserKey        => { control.pause(); state = ObservingOnly; continue; } // 用户永远赢
        External(n)    => intents.push(n),        // 用户补充、子任务回执、外部通知
        Deadline(t)    => watchdog.raise(t),
    }

    let situation = classify(&screen);            // 本地分类器，零 LLM
    if situation == last && !watchdog.deadline_near() {
        heartbeat.backoff();                      // 局面没变 → 不决策、不烧 token
        watchdog.check_no_progress();             // 但空转检测继续
        continue;
    }

    let ctx   = compile_context(situation, intents.top(), profile, kb.retrieve(...));
    let decision = policy.first_match(&ctx)       // 1. 硬约束 / 授权（确定性）
        .or_else(|| playbook.match_rule(&ctx))    // 2. 剧本（确定性）
        .or_else(|| planner.decide(&ctx).await)   // 3. LLM，最后才用
        .unwrap_or(Action::Observe);

    let action = authorize(decision, &contract.authority);   // 可能被改写成 Escalate
    audit.record(DecisionRecord { ctx_refs: ctx.refs, .. });

    match action {
        Escalate(card) => hitl.present(card).await,           // 带建议 + 倒计时默认，见 §6.2
        Handoff        => { control.handback(); break; }
        Observe        => {}
        a => {
            let receipt = act(a).await;                      // InputPort + ReceiptVerifier
            audit.attach_receipt(receipt);
            if !receipt.ok { recovery.on_failed_receipt(receipt).await; }
        }
    }

    watchdog.check();                                        // 空转 / 回环 / 超时 / 预算
    checkpoint.maybe_save();                                 // 跨重启恢复
    heartbeat.reset_if_active();
}
```

关键点：**决策只在「局面变化」时发生**。屏幕静默时循环仍在跑，但只做本地分类与计时；这是长任务能挂几小时而不烧钱的前提。

### 3.3 心跳、空转与主动推进

| 屏幕状态 | 心跳间隔 | 行为 |
|---|---|---|
| 有输出 / delta 明显 | 250ms–1s | 每次 delta 触发分类，重置 no-progress 计时 |
| 静默但任务在跑（进度条、编译中） | 1 → 2 → 5s | 只更新 Progress，不决策 |
| 完全静默 | 5 → 15 → 30 → 60s（指数退避上限） | 分类一次；连续 N=3 次 `Situation` 相同 → 触发 no-progress |
| 临近 `intent_deadline` | 收紧到 1s | 准备收尾摘要与现场快照 |

截图里「两次读屏相隔约一分钟，内容没有变化」在 v2 里不再是「无事可做」，而是明确的 `NoProgress` 事件，按局面选一种干预：

1. `AwaitingInput`（claude code 等人回答）→ 按 §4 生成回复并注入。
2. `Progress` 但长时间不动（长编译、下载）→ 延长 wait，发一条状态摘要给用户，不打断。
3. `SubagentStatus` 显示子任务空转 → 依据 Intent 选择「催一句 / 换 prompt 重启子任务 / 判定僵死并汇报」，而不是只描述现象。
4. 反复同一 `Question` → 命中回环检测（§7），升级并附前几次的 DecisionRecord。

### 3.4 后端屏幕状态与写后回执

- **ScreenStore**：前端 `termRegistry` 在 xterm `onRender` / 缓冲区变化时推 `ScreenDelta { frame_id, changed_lines, hash, alt, cursor }`；后端保存最近 N 帧与滚动摘要。全屏程序（tmux / claude code / vim，`alt=true`）用结构化 buffer 快照而非扁平文本，避免现状「alt screen 下旁路缓冲不可信」。前端不在（浮窗、最小化、锁屏）时降级到旁路缓冲并**标注置信度**，SituationClassifier 对低置信度局面只允许只读动作。
- **写后回执 `ReceiptVerifier`**：注入动作时声明 `expected_effect`（回显行 / 提示符变化 / 新 frame_id / 已知状态串）。在 `receipt_ms` 内校验：
  - 命中 → `Applied`；
  - 未命中 → 换通道重试一次（bracketed paste 或 `Ctrl-U` 重写整行）；
  - 二次失败 → 不再盲发第三遍，升级并附屏幕快照。
  这补上现状缺口：`buffer.rs` 的 echo cancel 只覆盖用户输入，Agent 自己注入的字节目前没有确认闭环。
- **幂等**：动作带 `idempotency_key = hash(situation_hash + intent_id + ctx_refs)`，冷却窗口内不重复；重连 / resume 后不重放已完成动作。

### 3.5 所有权、接管与并发

- 复用 `control.rs` 三态（`UserDriving / AgentDriving / Paused`）与 `next()` 里「用户永远赢」的语义，按 console 分片持有写锁。Steward 只在 `AgentDriving` 时写；`Paused` 时**继续观察和记录**，但不写、不决策出动作。
- 多 console：每个 tab 一个 Steward，共享 Profile / Intent Ledger / KB / Supervisor。Supervisor 限制 LLM 并发（默认 2）、全局 token 与步数预算、跨 console 的意向冲突（例如两个 tab 都要写同一仓库）。
- 交接协议：用户敲键 → 100ms 内 Steward 转 `ObservingOnly`；用户点「交还」→ 先 `nx_screen` 对齐现场，再给「离开期间发生了什么 + 当前待决 + 下一步建议」，然后从最后 checkpoint 恢复。

### 3.6 长时间运行与恢复

- **Checkpoint**（落 SQLite，新增 `steward_checkpoints` 表）：契约、Intent 栈、screen_hash、最近 20 条 DecisionRecord、待决升级卡、心跳与计时器剩余、预算已用。每完成一个「决策—动作—回执」闭环或每 30s 写一次。
- **恢复**：app 重启 / 崩溃后 `steward_resume` 先读屏对齐（若 screen_hash 与 checkpoint 不一致，先给差异摘要并进入 `Approve` 档），再恢复计时器。PTY / SSH 断连：重连后从最后 `Applied` 动作继续；不可恢复则 `Suspended` + 现场快照 + 升级卡。
- **预算整合**：复用 `budget.rs` 的相位状态机，把 `max_steps / max_minutes / max_output_chars` 从「本轮」提升为「本次托管」；耗尽时优雅降级——暂停托管、写摘要、交还，而不是硬杀进程。现状「暂停态只在内存」的限制由 Checkpoint 一并解决。

---

## 4. 决策依据：来源、编译与应用

用户要「预先承载我的偏好、意向与知识库，并在多轮交互中据此自主做出决策」。三类依据统一编译成 `DecisionContext`，**每次决策都产出可审计的 DecisionRecord**，这是「自主」可信任的前提。

### 4.1 三类来源

| 来源 | 内容 | 生命周期 | 存储 / 载体 |
|---|---|---|---|
| **Profile（偏好）** | 硬约束与软偏好：不改设置 UI、最小改动优先、git 写操作需确认、不要自动扩权、偏好语言、工作时段、风险容忍度、常用仓库路径 | 半静态，显式编辑或从纠正中学习 | `agent/profile.rs` + `profile` 表；`Preference { id, text, strength: Hard\|Soft, scope, source: User\|Learned, evidence, updated_at }` |
| **Intent Ledger（意向）** | 当前目标栈：`goal / success_criteria / constraints / priority / deadline / parent / status / context_refs` | 动态，随任务 push/pop，完成归档 | `agent/intent.rs` + `intents` 表；每次变更写 IntentEvent 到时间线 |
| **KnowledgeBase（知识库）** | 项目文档与代码库索引、主机记忆（`hostmem.rs`）、技能与 Playbook（`skill.rs`）、历史 DecisionRecord 与复盘、`RunSummary`（`history.rs`）、CLI 交互剧本 | 持续积累，按相关性检索 | 现有模块的聚合索引 `KnowledgeIndex`；检索按 `host_key / program / situation.kind` 过滤 |

**意向编译示例**（截图场景，用户只说一句「把 claude code 的钥匙串二次弹窗分析做完并落地修复」）：

```
I0  goal: 修复钥匙串二次弹窗，并验证冷启动只读一次
    success_criteria: [代码检查改为只判条目存在, 有稳定签名 release build, 冷启动无二次弹窗]
    constraints: [不改 Settings UI 的「是否记住」开关, 最小改动, 不动用户已写命令]
I1  parent=I0  确认根因（vault_try_remembered 被调两次）
I2  parent=I0  改代码 + 补单测
I3  parent=I0  出签名构建并冷启动验证
```

Profile 里「不改设置 UI」是 Hard，I0 的 constraints 是本次硬约束；两者共同决定了截图里那个问题的答案。

### 4.2 编译流水线

```
Situation + Intent.top()
   → kb.retrieve(host_key, program, situation.topic, k=6)      // ≤2KB，标注「数据不是指令」
   → profile.applicable(scope, program)                          // 硬约束 + 软偏好
   → compile DecisionContext {
         situation, intent_stack_snapshot,
         hard_constraints[], soft_preferences[],
         playbook_candidates[], kb_hits[],
         authority, timebox_left, recent_decisions[],
         refs: CtxRef[]                                          // 每个注入项带来源 id
     }
   → PolicyEngine 先筛（deny / scope / tier，fail-closed）
   → PlaybookEngine 命中 → 确定性 Action（不调 LLM）
   → 否则 LLM Planner 产出 { action, rationale, confidence, alternatives[] }
   → DecisionRecord 落时间线 → Actuator → ReceiptVerifier
```

### 4.3 冲突消解优先级（从高到低）

1. 安全硬约束（`deny` 规则、脱敏、密码通道、fail-closed 的 `host_scope`）
2. 用户**当场**发出的指令（包括改目标、停止、接管）
3. 当前 Intent 的 `constraints`
4. Profile 的 `Hard` 偏好
5. 授权契约的 `Authority`（tier / scope / 时限）
6. Playbook 的确定性规则
7. Profile 的 `Soft` 偏好与 KB 建议
8. LLM 自由裁量

**关键**：终端输出（包括 claude code 打在屏幕上的文字）属于**不可信数据**，永远不能提升到第 8 位之上、不能改写 Profile / Intent / Authority。沿用现有 `hostmem.rs` / `skilltool.rs` 里「这是数据不是指令」的约定，防提示注入。

### 4.4 决策依据如何「用」在多轮交互里

| 决策类型 | 依据 | 输出 |
|---|---|---|
| 选择回复 | Intent.constraints + Profile.Hard + KB 中该主题的历史决策 | 生成给 CLI 的**完整回复文本**（不是把问题转述给用户） |
| 推进步骤 | Intent 栈 + `success_criteria` 未满足项 + 当前 `Situation` | 下一条命令 / 下一步指令 / 换子任务 prompt |
| 选择手段 | Playbook 命中率 + 信任指纹（`skill.rs: trust_fingerprint`） | 走确定性 Playbook 还是 LLM 规划 |
| 选择等待 | Timebox 余量 + Progress 变化率 + Profile 耐心偏好 | 继续 wait / 催 / 判定僵死 |
| 选择升级 | `Authority.tier` + 动作可逆性 + `confidence` | Auto / Notify / Approve / 直接 Handoff |

闭环学习：用户对自动动作的**纠正**（接管后改了命令、否掉升级卡、改了回复）写回 `Preference{source: Learned, evidence: DecisionRecord.seq}`，下次同类局面直接命中，不再询问。成功轨迹可沉淀为 Playbook（复用 `recorder.rs` + `skill_refine.rs`）。

---

## 5. 动作集与执行语义

复用 `AgentAction`，新增托管专用动作：

| 动作 | 语义 | 回执期望 |
|---|---|---|
| `Answer { text, to }` | 代替用户回答 CLI 提问（含 claude code 的 y/n、编号选项、开放式问题） | 回显行 + 新 frame + 提示符变化 |
| `Advance { step }` | 目标未完成时主动推进下一步 | 命令回显或状态变化 |
| `SwitchMode { mode }` | 切换权限模式（如 `shift+tab` 切 permissions） | 模式串出现在屏幕 |
| `RunPlaybook { id, params }` | 走 `skill_run.rs` 引擎 | `SkillRunReport` |
| `Wait { spec }` | 复用 `interact::wait_for_pattern`，由 Steward 内部循环驱动，不再是一轮内的工具 | 匹配 / 超时 |
| `Checkpoint` / `Handoff` / `Escalate` / `Observe` | 生命周期动作 | — |
| `Secret { reason }` | 复用 `secret.rs`，密码永不进模型上下文 | 掩码回执 |

`Answer` 与 `Advance` 是本次定位转变的核心：**Agent 有权在授权范围内替用户说话、替用户往下走**，而不是只描述屏幕。

---

## 6. 人机协作边界（授权契约，不是拒绝列表）

### 6.1 三档授权 × 动作映射

按「风险 × 可逆性 × 作用域 × host」四维把动作归档，用户签托管契约时选默认档，也可逐类覆盖：

| 档 | 含义 | 典型动作 |
|---|---|---|
| **Auto** | 自主执行，事后可查 | 读屏、等待、状态摘要、催进度、回答 CLI 的已知提问、切换权限模式（单会话内）、跑只读命令与测试 |
| **Notify** | 自主执行 + 立即通知 + 短窗口可撤销 | 写文件 / 改代码、装依赖、`git commit`（指定仓库内）、重启子任务；执行前留 diff / 备份，30s 内可「撤销 / 接管」 |
| **Approve** | 先给建议、等人一键确认（带超时默认） | 删除、`push -f`、密钥 / 凭据相关、跨 host 扩大授权、生产环境写操作、任何「don't ask again」式永久扩权、`deny` 未覆盖的高风险动作 |

`Authority.deny` 是硬禁，任何档位都不可越过；`allow` 可把某类动作从 Notify 提升到 Auto（例如「这台测试机上 `cargo test` 随便跑」）。命中 `host_scope` fail-closed 时自动降到 Approve，复用 `skill_run.rs: check_scope`。

### 6.2 升级 ≠ 拒绝：带建议、带倒计时、带默认动作

```rust
pub struct EscalationCard {
    pub situation: Situation,
    pub recommendation: Action,          // 「建议执行 X」
    pub why: Vec<CtxRef>,                // 引用了哪些偏好 / 意向 / 知识
    pub alternatives: Vec<Action>,       // 2 个备选，含「我自己来」
    pub default_action: Action,          // 超时后执行什么（默认：暂停并保留现场 + 通知）
    pub timeout_ms: u64,                 // 30s，可按 Profile 调整
    pub one_click: bool,
}
```

升级时 Steward **继续观察但不写**，屏幕若有新变化会刷新卡片；超时按 `default_action` 执行，会话不会僵死。这与截图里「这需要你自己回复」的本质区别是：人不必先理解问题再动手，一句「按建议来」或什么都不做都能让流程继续。

### 6.3 场景剧本（真实行为序列）

**场景 A — 截图原案：claude code 等用户回答要不要改设置**

```
t+0s   ScreenDelta → Situation::Question{ topic:"settings:remember-check",
         options:[改设置/不改], from: ClaudeCode, confidence:0.93 }
t+0s   compile: I0.constraints「不改 Settings UI」+ Profile.Hard「最小改动」
       + KB「vault_try_remembered 两次读钥匙串的根因分析」→ 无 Playbook 命中 → Planner
t+2s   DecisionRecord: chosen=Answer, tier=Auto（回复文本不改变系统状态）
       文本："No — 不要改设置。直接在代码里把 'is remembered' 检查改成只判条目是否存在
              （vault.dek / keychain entry existence），并做稳定签名的 release build；
              改完跑一次冷启动，验证只读一次钥匙串。"
t+2s   InputPort 注入（bracketed paste + Enter）→ ReceiptVerifier 校验回显与新 frame → Applied
t+…    持续值守 claude code 改代码 → Progress → 跑测试 → 报告完成
       success_criteria 判定：签名构建存在 + 冷启动日志一次读取 → Completed
终态   交还摘要：改了哪些文件、验证结果、剩余风险（dev build 二次弹窗仍在）
```
用户回来只看到结果和「为什么这么答」的依据引用；若不同意，可一键回滚到「我自己来」并让 Profile 学习。

**场景 B — 权限提示含「Yes, and don't ask again」**：`Situation::Permission{ options:[1 Yes, 2 Yes+永久, 3 No] }`。选 1 属 Auto；选项 2 会永久扩大授权 → 命中 `deny` / `allow_permanent_grant=false` → 降为 Approve，升级卡默认动作是「选 1，本次允许」，超时也按此执行，绝不误选永久项。

**场景 C — subagent 空转 / 11 天没动**：`Situation::SubagentStatus{ running:0, idle:"11d4m" }` + 连续 3 次相同 frame → `NoProgress`。Watchdog 依次判断：是否 `intent_deadline` 已过 → 给「离开期间」摘要并暂停；否则依据 Intent 选择 `Advance{催一句}` 或 `RunPlaybook{重启子任务}`，并把前几次 DecisionRecord 附在卡片里，避免「只是描述现象」。

**场景 D — 交互式脚本全流程**（Playbook 表，命中即确定性执行，不调 LLM）：

| 局面 | 依据 | 动作 | 回执 | 下一步 |
|---|---|---|---|---|
| `Are you sure you want to continue connecting (yes/no)?` | Playbook: ssh-known-hosts + 目标 host 在 scope 内 | `Answer{"yes"}` | 出现密码提示 | 继续 |
| `password:` / `[sudo] password` | `secret.rs` 通道 | `Secret{reason}` | 掩码回执，值不进模型 | 继续 |
| `Package name: (foo)` 等 npm init 多问 | Profile 默认 + Intent 参数 | `Answer{…}` 逐项 | 下一问出现 | 循环 |
| `pick / edit / squash…` 交互 rebase | Playbook + 用户 Intent | 生成指令序列 | 编辑器退出 | 校验状态 |
| `--More--` / `(END)` | `Situation::Pager` | `Answer{"q"}` 或空格翻页 | 回到 shell | 完成判定 |
| 回到 shell 提示符 | `Situation::Idle` + success_criteria | `Handoff` | — | 摘要 |

**场景 E — 长时间运行 + 需要确认 + 超时**：用户托管后离开两小时。期间 Steward 完成 12 个闭环、写 3 次 Checkpoint；遇到一次 `Approve` 动作发升级卡，30s 无人确认 → 按 Profile 默认「暂停并保留现场 + 通知」，Watchdog 继续守护不写；用户回来看到「离开期间」摘要卡（已做 / 在做 / 待决 / 依据），点「按建议继续」后从 Checkpoint 恢复，无需重跑。

### 6.4 面板改造：从「对话」到「托管控制台」

`AgentPanel` 新增第三个 tab「托管」：状态灯（托管中 / 观察中 / 待决 / 已交还）、当前目标与意向栈（可展开/改）、决策依据引用（点开看 DecisionRecord）、动作流水与倒计时、托管等级开关、一键「接管 / 交还 / 停止 / 改目标」。每个自动动作旁边显示「为什么」，引用到具体 Profile / Intent 条目。原有「对话」「时间线」保留为审计视图。

---

## 7. 异常处理与超时分级

| 异常 | 检测 | 默认动作 | 恢复 |
|---|---|---|---|
| 屏幕无变化 / 空转 | 连续 N=3 次相同 `Situation` | 按 §3.3 四类干预之一 | 局面变化即重置 |
| 反复同一提问（回环） | `decision_fingerprint` 在冷却窗口内重复 | 停止重复动作，升级并附历史记录 | 用户改目标 / 更新 Playbook |
| 写后回执不符（注入被吞、TUI 拦截、IME） | `receipt_ms` 内未见 expected_effect | 换通道重试 1 次 → 升级 + 屏幕快照 | 不盲发第三遍 |
| alt-screen / 全屏程序 | `frame.alt=true` | 用结构化 buffer；旁路缓冲标低置信度 | 低置信度只允许只读 |
| 前端不在（浮窗 / 最小化 / 锁屏） | `ScreenDelta` 断流 > 2×心跳 | 降级旁路缓冲 + 降低授权档，不写破坏性动作 | 恢复推送后先对齐现场 |
| LLM 失败 / 超时 / 限流 | `llm.rs` 错误 + `REQUEST_TIMEOUT` | 退回 Playbook / `Observe`，暂停托管并保留现场 | 指数退避重试，仍失败则升级 |
| PTY / SSH 断连 | `LocalManager` / `SessionManager` 写失败 | `Suspended` + 现场快照 | 重连后从最后 `Applied` 续跑 |
| app 重启 / 崩溃 | 启动扫描未完成契约 | `steward_resume`：先读屏对齐，hash 不符则降为 Approve | Checkpoint 恢复计时器与预算 |
| 预算耗尽（步数 / 时长 / 字符） | `budget.rs: check_budget` | 优雅降级：写摘要、交还、不硬杀 | 用户提高预算或改目标后继续 |
| 死锁（等 LLM ↔ 等屏幕） | 单事件队列 + `step_ms` 熔断 | 取消在途 LLM，回到观察 | 下一 tick 重新分类 |
| 确认超时 | 升级卡计时 | 执行 `default_action`（默认暂停保留现场 + 通知） | 用户回来一键继续 |
| 并发争抢输入 | `ControlTable` 写锁按 console 分片 | 用户键优先，Steward 转观察 | 交还后恢复 |
| 系统睡眠 / 时间漂移 | 单调时钟 + 唤醒后重算 timebox | 不误判为超时，先读屏对齐 | 继续或给差异摘要 |

**超时分级**（每级都有明确默认动作，不存在「什么都不做」）：

```
screen_ack 3s → 降级旁路缓冲并标置信度
receipt 1.5s → 换通道重试一次
step 60s → 取消在途 LLM，回观察
idle 5min → 收紧心跳 + 状态通知
no_progress 15min → 主动干预（§3.3）
intent_deadline（用户给）→ 收尾摘要 + 保留现场
deadline + grace → hard kill，写 Checkpoint 与升级卡
```

所有异常与决策都进时间线（`timeline.rs` / `trace.rs`），出网前统一 `redact::redact`；密码只走 `secret.rs`，不进 LLM / trace / 对话 state（沿用现有安全底线）。

---

## 8. 落地路径与验收标准

| 阶段 | 主要改动 | 验收标准 |
|---|---|---|
| **P0 会话织物**（已实现） | `LocalManager::ids()` / `SessionManager::ids()`、`ScreenDelta` 增量采集、`ScreenStore`、`steward_*` 命令、面板「托管」tab | 能枚举全部 tab；关掉面板仍持续收到增量；alt-screen 有结构化快照 |
| **P1 常驻循环（只读，已实现）** | `Steward` Supervisor、自适应心跳、空转检测、`SituationClassifier`、`Watchdog` 骨架 | 截图场景里 1 分钟内识别出「claude code 在等回答」并出建议卡；全程 0 次 LLM 调用。已实现见 §11；`ReceiptVerifier` 因 P1 不写终端，顺延到 P2 与注入一起做 |
| **P2 决策依据（已实现）** | `StewardBrain` / 意向栈 I0 / KB 检索 / `DecisionCompiler` / `DecisionRecord` / Playbook / 写后回执 / 升级卡 | 截图那句提问按意向编译出**完整回复**并注入（总闸打开时）；DecisionRecord 可回放并列出引用来源；Playbook 命中时不调 LLM。已实现见 §12 |
| **P3 授权与协作** | `PolicyEngine` 三档、升级卡（建议+倒计时+默认）、接管/交还热更新、托管控制台 UI | 需要确认时 30s 内不僵死；用户敲键 100ms 内夺权；改目标不重启循环 |
| **P4 长运行** | Checkpoint / resume、断连重连、多 console Supervisor、预算与 Timebox 整合 | 强杀 app 重启后恢复托管且先读屏对齐；两个 tab 并行不抢键盘；预算耗尽优雅交还 |
| **P5 学习闭环** | 从纠正学 Preference、成功轨迹沉淀 Playbook/Skill、跨会话复盘 | 同类提问第二次不再询问；能解释「为什么这次自动放行」 |

每阶段沿用现有约定：`cargo test --lib` / `npx tsc --noEmit` / `npm test`（当前 202+ 用例），新增模块按 `agent/*` 的纯逻辑 + Tauri 接线分层惯例，纯函数先写测试再接运行时。

---

## 9. 与现状的差异总表

| 维度 | 现状（`afd8f8f`） | NexusAgent v2 |
|---|---|---|
| 触发 | 用户发一句话起一轮 | 契约签下后常驻，事件驱动 + 心跳 |
| 运行体 | `act::run` 跑完即结束 | 每 console 一个 Steward，跨轮、跨重启 |
| 屏幕 | 一次性 `nx_screen` 请求-回执，3s 降级 | 后端 ScreenStore 帧序列 + diff + hash |
| 输入校验 | 仅用户输入 echo cancel | 写后 ReceiptVerifier + 幂等 |
| 决策依据 | 系统提示 + 最近 3 条 RunSummary + 按需 KB | Profile / Intent 栈 / KB 编译成 DecisionContext，每条可引用 |
| 应答 | 把问题转述给用户 | 按意向生成回复并注入（授权范围内） |
| 边界 | 危险命令审批 + 作用范围 | 授权三档 + 撤销窗口 + 带建议带默认的升级卡 |
| 超时 | 单轮预算、暂停态只在内存 | 六档 Timebox + Checkpoint + 僵死判定 + 优雅降级 |
| UI | 对话 / 时间线 | 新增托管控制台：目标、依据、倒计时、一键接管 |

**一句话**：现有代码已经把「器官」做齐了；v2 新增的是「神经系统」——一个始终醒着、知道用户想要什么、能替用户说话、也能在拿不准时把选择权优雅递回来的 Steward。

---

## 10. P0 实施记录（2026-11-10，已落地）

| 落点 | 文件 | 说明 |
|---|---|---|
| 会话枚举 | `src-tauri/src/local/mod.rs`、`src-tauri/src/ssh/session.rs` | 各加 `ids()`，只在锁内拷键，不碰 PTY / 通道 |
| 屏幕状态 | `src-tauri/src/agent/steward.rs` | `ScreenStore`：全量/增量帧合并、`windowStart` 对齐、FNV-1a 哈希漂移检测、`idleFrames` 空转计数、`tailText()` |
| 契约登记 | 同上 | `StewardRegistry` + `StewardContract`（一 console 一份，status 恒为 `observing`） |
| 命令 | 同上 + `src-tauri/src/lib.rs` | `steward_list_consoles / screen_delta / screen_get / start / stop / status / amend`；屏幕态与契约用 `app.manage()` 挂独立 State，不给热路径 AppState 添锁 |
| 关闭清理 | `src-tauri/src/cmd.rs` | `local_close` / `ssh_close` 顺手 `screen.drop(id)`，避免每个关掉的标签留 200 行 |
| 前端采集 | `src/lib/termRegistry.ts` | `frameHash` / `setScreenDeltaSink` / `setScreenCaptureEnabled` / `requestFullFrame`；挂在 `registerTerm` 的 `onWriteParsed` + `onCursorMove`，180ms 防抖，尾部 200 行，只推变化行 |
| IPC 接线 | `src/App.tsx`、`src/lib/steward.ts` | App 启动时注入 sink 并开闸；`needFull` 时立刻补一帧全量 |
| 面板 | `src/components/StewardPanel.tsx`、`AgentPanel.tsx`、`styles.css` | 新增「托管」tab：console 清单、帧号/哈希/空转计数、签/改/停契约、后端屏幕快照回显 |
| 测试 | `steward.rs` 内 8 例、`tests/fe/steward.test.ts` 3 例 | 含跨语言哈希校验向量（`ee7ae512` / `16952930`）；`npm test` 311 例全过、`tsc --noEmit` / `npm run build` / `cargo test --lib steward` 通过 |

**P0 的边界（诚实说明）**：只建立织物与契约登记，**没有任何自主决策、自动应答或按键注入**，也不产生 LLM 请求。契约的 `authorityTier` 只是先把授权档位存下来，P3 的 `PolicyEngine` 才会真正执行它。屏幕采集是纯本地行为，不出网；`enabled=false` 的既有 AI 旁路开关不受影响。

**真机验收建议**：打开「托管」tab → 在任意终端敲几条命令 → 观察 `帧 N` 与 8 位哈希随输出变化、空转计数在静默时增长；把面板关掉再打开，帧号应继续增长（证明上报与面板生命周期解耦）；在 tmux / vim 里看「全屏」标记与快照内容是否对应 alt buffer。

---

## 11. P1 实施记录（2026-11-10，已落地）

### 落点

| 落点 | 文件 | 说明 |
|---|---|---|
| 局面分类器 | `src-tauri/src/agent/situation.rs` | `Program` / `SituationKind`（12 类）/ `Choice`（`grantsPermanent` 硬标记）/ `Situation`；复用 `prompt::classify` 作底层信号，屏幕文本出事件流前走 `redact::redact`，密码局面连脱敏原文都不留 |
| 语义签名 | 同上 | `signature = program\|kind\|topic\|options`，**不含帧哈希**：进度条刷屏不会误判成「局面变了」，也不会把空转计时反复清零 |
| 常驻循环 | `src-tauri/src/agent/steward_loop.rs` | 应用启动时 `spawn_daemon` 起一个常驻 Supervisor，内部按 console 分状态槽；`ScreenDelta` / 契约变更 / 手动唤醒走 `StewardBus(Notify)`，静默期按自适应心跳自己醒 |
| 心跳与空转 | 同上 | 静默 `1→5→15→30→60s`，Progress 收紧到 `1→2→5s`；连续 `N=3` 次同签名触发一次 `NoProgress`（带建议卡），局面变化即重置；墙钟跳变 >120s 判系统睡眠，重置各槽时钟避免误报 |
| 断流检测 | 同上 | 前端帧 `stale_ms > 6s` → `StaleFeed` 事件 + 置信度减半，恢复时 `FeedRestored`；无屏帧时 `ScreenLost`，只报一次 |
| 事件流 | 同上 | `EventLog` 200 条环形缓冲、单调 seq、`nx://steward/event` 逐条推前端；`StewardEvent` 带 `situation`、`suggestion`、`llmCalls=0` |
| 命令 | 同上 + `lib.rs` | `steward_events / runtime_info / classify_now / clear_events / config / set_config / wake`；状态与 `ScreenStore` 一样用 `app.manage()` 挂独立 State |
| 命令唤醒 | `src-tauri/src/agent/steward.rs` | `screen_delta / start / stop / amend` 落地后敲 Bus，循环立刻响应，不必等下一次心跳 |
| 前端心跳帧 | `src/lib/termRegistry.ts` | **补 P0 缺口**：静默期每 2s 补一帧同哈希帧（只带 frameId、空 lines）。后端这才分得清「屏幕没动」与「前端不在」，`idleFrames` 也按真实时间增长 |
| 前端命令层 | `src/lib/steward.ts` | P1 类型、7 个命令、`onEvent` 订阅、`situationLabel / eventLabel / programLabel` |
| 面板 | `src/components/StewardPanel.tsx`、`styles.css` | 运行状态条（tick / 分类 / 事件数 / **LLM 0** / 阈值开关）、当前局面卡片（含选项与「永久授权」红标、判据、签名、脱敏屏幕）、观察事件流与建议卡 |

### 关键行为

- **决策只在局面变化时发生**：帧一到即分类（250ms 防抖），但同哈希心跳帧不额外触发分类；静默期只跑本地计时与分类，零模型调用。
- **建议卡是文字建议，不是动作**：`SituationChanged` 在 Question / Permission / Password / Error / Menu / Pager 上立刻附建议；`NoProgress` 按 §3.3 四类干预给建议，重复同一提问升级为「疑似回环」。P1 不注入任何按键。
- **安全底线沿用现状**：密码局面 `tail` 恒为 `[redacted: credential prompt]`；含「永久 / 不再询问 / always」的选项带 `grantsPermanent`，P3 的 PolicyEngine 会据此拒绝自动选中。

### 与设计稿的有意偏差（都要在后续阶段补）

1. **单 Supervisor + 状态槽**，而非每 console 一个 tokio task：语义等价、不因契约反复签停而泄漏 task；`StewardState::step()` 是纯状态机，P4 拆真 actor 时可原样搬走。
2. **独立 `EventLog`**，未并入 `timeline.rs`：时间线是既有 Agent 会话级的，P1 的观察事件跨会话、跨面板生命周期。P2 把 `DecisionRecord` 落进了 SQLite（见 §12），但统一审计视图仍未并入 `timeline.rs`，顺延到 P3。
3. **墙钟 + 跳变保护**，未上单调时钟：系统睡眠唤醒后重置各槽时钟，不误判超时；P4 的 Checkpoint / timebox 需要真正的单调计时再统一换。
4. **尚无 Checkpoint / resume / 多 console 抢键盘 / 接管夺权**：这些是 P3 / P4；P1 只在契约签下后观察，契约停止即退出该 console。
5. **没有写后回执 `ReceiptVerifier`**：P1 不写终端，回执无对象可校验，顺延到 P2 与注入动作一起落地，避免写一个空壳。**已在 §12 落地**（1.5s 窗口、新帧校验、超时不重发）。

### 验证与真机验收

- `cargo test --lib` 全量通过（新增 `situation` 12 例、`steward_loop` 10 例）；`npm test` 312 例通过（新增「静默期心跳帧持续上报」1 例）；`tsc --noEmit` / `npm run build` 通过。
- 真机：打开「托管」tab → 填一句话目标签契约 → 在 claude code 里制造一个提问或权限提示 → **帧到达 1 秒内**应出现「等待回答 / 授权确认」局面卡与建议卡；把面板关掉再打开，事件流应继续增长（证明循环与面板生命周期解耦）；点「阈值 1」后静默几秒应看到一条「空转」事件；进 vim / tmux 再看「全屏程序（未细分）」与低置信度。
- 全程可在状态条确认 `LLM 0`，即「1 分钟内识别在等回答」完全由本地分类器完成。

---

## 12. P2 实施记录：决策依据 + 自动应答闭环

P2 把「托管」从「看得懂屏幕」推进到「替用户说话」。核心不是加一个模型调用，而是把用户预先承载的
偏好 / 意向 / 知识 / 剧本编译成一条可审计的决策，再在授权范围内注入并校验回执。

### 模块与落点

| 层 | 文件 | 内容 |
|---|---|---|
| 存储 | `src-tauri/src/store/steward.rs`（新增）、`store/mod.rs` | 两张幂等表：`steward_ctx`（四类依据共用，`extra` JSON）、`steward_decisions`（整条记录 JSON + 可原地更新的 `outcome`）。旧库打开只多两张空表，不加密，与 `host_memory` 同级 |
| 依据 | `src-tauri/src/agent/profile.rs`（新增） | `Preference / Intent / Knowledge / Playbook` 四类、`StewardBrain` 内存镜像、scope 过滤、KB 打分检索、剧本四项 AND 匹配、首次启动可编辑的种子条目、闭环学习 `learn_correction` |
| 决策 | `src-tauri/src/agent/decision.rs`（新增） | `DecisionContext → DecisionRecord` 的纯函数编译器、`CtxRef` 依据引用、`EscalationCard`、`DecisionAction::send_bytes`、1.5s `verify_receipt`、`can_execute` 总闸 |
| 契约 | `src-tauri/src/agent/steward.rs` | 签契约时把一句话目标**确定性编译**成意向栈 I0（分句抽「别 / 不要 / 不改 / 必须」为约束），挂到 `intentIds`；`amend` 热更新 I0，`stop` 归档意向栈 |
| 循环 | `src-tauri/src/agent/steward_loop.rs` | 同一条常驻循环里补：回执校验、升级卡超时、局面变化时编译决策、落库、`nx://steward/event` 推 `DecisionMade / ActionApplied / Escalation / ReceiptTimeout / EscalationTimeout / DecisionRejected`；`should_decide` 去重、注入只在循环里发生 |
| 命令 | `src-tauri/src/agent/steward_cmd.rs`（新增） | 依据 CRUD、意向压栈、决策回放、立即决策、自动应答总闸、升级卡 approve/reject（只投递 seq）、清空决策、策略说明 |
| 前端 | `src/lib/steward.ts`、`src/components/StewardPanel.tsx`、`StewardContext.tsx`（新增）、`styles.css` | 决策卡（chosen / rationale / 依据引用 / 备选 / outcome / 回执）、升级卡倒计时与一键确认、自动应答开关与注入计数、四类依据的可视化编辑 |
| 测试 | Rust `profile` 8 例 / `decision` 13 例 / `steward_loop` +3 例；前端 `steward.test.ts` +3 例 | 跨语言契约、门控、去重、回执、种子校验 |

### 关键行为

- **冲突消解按 §4.3 固定顺序，安全闸门最先 return**：密码 → `Secret` + 强制升级卡；含永久扩权选项 → 只允许「本次允许」，超时不写；断流 / 低置信度 / 未知全屏 → `Observe`；`approve` 档 → 一律先给卡；Playbook 命中 → 确定性 `Answer`；最后才由意向 + 硬约束 + KB 编译完整回复。
- **「按意向自动回答」是可复核的模板编译，不是转述问题**：`answerTemplate` 支持 `{goal} {constraints} {hard} {options} {topic} {knowledge}`；没给模板时由目标 / 约束 / 硬偏好 / 知识条目拼出一段完整指令。`ctxRefs` 每条都能点回具体条目 id，KB 引用固定标 `[不可信数据]`。
- **注入有总闸且 fail-closed**：`StewardBrain.executionEnabled` 每次冷启动默认 `false`、不持久化；打开后仍要求 `outcome=proposed`、非 approve 档、无升级卡、动作为 `answer`、`sendBytes` 合法。密码与永久授权在任何档位都不自动写。
- **回执绝不重发**：注入后记 `PendingReceipt{frameId, hash, echo, deadline=1.5s}`，循环把心跳收紧到 250ms；新帧且哈希变化 → `applied`，超时 → `no_receipt` 并升级给人，**不会把同一条命令再发一遍**。
- **升级卡不是拒绝**：带建议、2 个备选（我自己来 / 交还）、30s 倒计时、默认动作（暂停写入保留现场）。用户点「按建议执行」只投递 seq，由常驻循环注入；点「我自己来」可附一句纠正，写回 `Learned` 偏好并留 `DecisionRecord #N` 作为 evidence。
- **多轮不重复**：`last_decision_signature` + 30s 冷却 + 待回执阻塞，保证进度条刷屏、回显抖动、同一签名短期内复现都不会连发。

### 与 §4 / §9 的有意偏差

1. **两类依据合并成一张 `steward_ctx`，而非 `profile / intents / kb / playbooks` 四张表**。四类条目总数几十条、启动即全量载入内存，无需按列查询；`extra` JSON 免掉后续字段演进的 `ALTER TABLE`。
2. **意向编译用确定性分句，不调 LLM**（设计 §3.1 写的是一次 LLM 编译）。P2 全程 `llmUsed=false` 是可验证的硬保证；LLM Planner 顺延到 P3，只在规则抽不出约束时兜底。
3. **KB 是独立的 `steward_ctx` 条目集合**，尚未聚合 `hostmem.rs` / `skill.rs` / `RunSummary`。检索已按 `program / topic / tail` 打分并标不可信，聚合索引留到 P3。
4. **Checkpoint / 六档 Timebox / 接管夺权仍是 P3–P4**。P2 的待回执状态只在内存，重启后不重试（这是**安全选择**：绝不把没确认的动作再发一遍）。
5. **`DecisionRecord` 落在 SQLite 与独立事件流，未并入 `timeline.rs`**。时间线是既有 Agent 会话级的，P2 的决策跨契约、跨面板生命周期；统一审计视图留到 P3。
6. **升级卡倒计时由前端按 `createdAt` 渲染，后端按墙钟判超时**（沿用 P1 的跳变保护）。真正单调时钟随 P4 的 timebox 一起换。

### 验证与真机验收

- `cargo test --lib`：**1045 passed / 1 failed**，唯一失败是既有的 `sysstat::tests::collect_has_core_keys`（macOS 上 `sysstat.rs` 只有 Linux `/proc` 分支，拿不到 `CPU_PCT`），与 P2 无关。
- `npm test` **315 例全过**（新增决策卡标签 / 升级卡收敛 / 中英文列表分隔 3 例，为此给 `scripts/fe-test.mjs` 加了 `@tauri-apps/api/*` 空桩）；`tsc --noEmit`、`npm run build` 通过。
- 真机：打开「托管」tab → 填一句带约束的目标签契约（意向栈 I0 应立即出现在「编辑依据 → 意向」）→ 在 claude code 制造提问 → 「决策记录」出现一张带依据引用的卡，状态条 `LLM 0`、注入 0（总闸关）→ 点「自动应答：开」再制造一次提问 → 注入计数 +1、1.5s 内出现「回执确认」、决策 `outcome=applied` → 把档位改成「确认」制造一次提问 → 出 30s 升级卡，点「按建议执行」才注入、点「我自己来」写 Learned 偏好。
- 安全回归（已由单测覆盖）：密码局面、含 `don't ask again` 的权限卡、断流 / 低置信度、approve 档、总闸关闭时，注入计数必须始终为 0。

---

## 13. P3 实施记录：所有权 / 授权契约 / 撤销窗口（已落地）

P3 解决「谁此刻有资格写这条终端」。核心是一个显式的所有权状态机 + 真授权契约，
把 P2 的静态策略数组换成可热更新、可审计的 `Authority`。

### 模块与落点

| 层 | 文件 | 内容 |
|---|---|---|
| 所有权 | `src-tauri/src/agent/ownership.rs`（新增） | `OwnershipTable`：`AgentDriving / ObservingOnly / Suspended` 三态；`user_key` 100ms SLA 夺权（返回处理延迟）；`AbsenceNote` 流水（观察 / 决策 / 回执 / 超时都记一条）；`handback` 出 `AbsenceSummary`（done / pending / nextStep / screenAligned）并恢复驾驶 |
| 授权 | `src-tauri/src/agent/policy.rs`（重写） | `Authority{tier, deny, allow, allowPermanentGrant, undoWindowMs, hostScope, maxSteps, maxMinutes, maxOutputChars, timeboxMs}`；`evaluate` 八步裁决（deny 硬禁 → 密码 / 永久 fail-closed → host 作用域 → 现场可信度 → approve 档 → allow 提权 → 破坏性 → notify 留撤销窗口）；`UndoEntry` 30s 撤销窗口；`policy_view` 给前端真值 |
| 共享态 | `src-tauri/src/agent/steward_core.rs`（新增） | `StewardCore`：`app.manage()` 单例聚合 ownership / authorities / checkpoints / timeboxes / spent / undo / planner / 写锁分片 / LLM 并发闸 / 恢复队列，循环与命令层共享；`CoreView` 一次性总览（camelCase） |
| 循环 | `steward_loop.rs` | 注入前查 `may_write`（观察态只记录不写）；Notify 档执行后开 `UndoWindow`；`PolicyDenied` 硬禁事件；升级卡超时按 `default_action` 收敛不僵死 |
| 命令 | `steward_cmd.rs` | `steward_core_view / steward_user_key / steward_takeover / steward_handback（读屏对齐，不一致自动降 approve）/ steward_undo（转观察 + rolled_back + 写 Learned）/ steward_policy_v2 / steward_set_authority` |
| 前端 | `termRegistry.ts` + `App.tsx` + `StewardPanel.tsx` | `setUserKeySink`（与 screenDeltaSink 同模式，按 console 250ms 节流）→ `steward_user_key`；面板「托管核心」区：所有权状态灯、接管 / 交还、离开摘要卡、撤销窗口一键回滚、授权契约编辑（档位 / 永久扩权 / 预算 / deadline） |

### 关键行为

- **夺权是物理的**：用户在 xterm 敲第一个键，`emitInput` 旁路上报（不拦截字节本身），后端 100ms 内
  切 `ObservingOnly`；期间决策照常编译落库（可复盘），但 `may_write` 挡住一切注入。
- **交还要给摘要**：`handback` 先比 Checkpoint 的 `screen_hash`——漂移就自动把档位降为「确认」
  （设计 §3.6），再交还 `AbsenceSummary`：离开多久、你接管几次、做了什么、还差什么、下一步。
- **撤销不是 Undo 键**：不撤销终端动作本身，而是「立刻转观察 + 标 rolled_back + 写一条 Learned
  偏好」，让同类动作下次不再自动放行。窗口默认 30s，每条只消费一次。

## 14. P4 实施记录：Timebox / Checkpoint / 预算 / 恢复（已落地）

P4 让托管能「活着过夜」：断电、强杀、锁屏、SSH 断连之后，现场可恢复、预算可收敛。

| 层 | 文件 | 内容 |
|---|---|---|
| 时间盒 | `checkpoint.rs` | `TimeboxConfig` 六档（读屏 3s / 回执 1.5s / 单步 60s / 空闲 5min / 无进展 15min / 意向 deadline + 宽限）+ `TimeboxState`（BTreeSet 去重，同一档只报一次，mark_* 复位）；`TimeboxHit` 事件 |
| 现场恢复 | `checkpoint.rs` | `StewardCheckpoint`（契约 / 意向栈 / screen_hash / 最近 20 条决策 / 待决升级卡 / 预算 / 时间盒）整条 JSON 落 `steward_checkpoints`；`should_save`：每个闭环或 30s；`resume_verdict`：Clean / Drift（降 approve）/ Expired（24h TTL 或已完成）/ Suspended（等用户） |
| 预算 | `checkpoint.rs` + `budget.rs` | `Spent` 托管级累计（步数 / 分钟 / 输出字符），耗尽走 `BudgetExhausted`：优雅交还 + 摘要，不硬杀进程 |
| 恢复链 | `lib.rs` + `steward_loop.rs` + `store/steward.rs` | 启动扫描未完成契约 → `registry.restore` + `core.set_checkpoint` + `push_resume` → 循环先读屏对齐（hash 不符降 Approve，相符直接续跑）；`steward_resume` 命令支持手动恢复；挂起 / 断连走 `Suspended` 事件并落 Checkpoint |
| 命令 | `steward_cmd.rs` | `steward_timebox / steward_set_timebox / steward_checkpoints / steward_resume` |

时间唯一来源是 `Instant`（单调），墙钟只用于展示与 TTL——P2 偏差 6 就此关闭。

## 15. P5 实施记录 + 端到端验证（已落地）

P5 补上「解释 / 复盘 / 蒸馏」与 LLM Planner 兜底，并完成全链路 e2e。

### 模块与落点

| 层 | 文件 | 内容 |
|---|---|---|
| 学习 | `src-tauri/src/agent/learning.rs`（新增） | `should_learn`：Skip（太短 / 等价已有）/ Merge（同主题合并）/ Create 三决策；`explain`：把 `ctxRefs` 展开成人能核对的解释链（含 resolvedText 回原文、`why_auto` 一句话）；`review`：跨会话复盘（done / pending / 高频依据 / 风险动作）；`merge_audit`：事件 + 决策合成统一时间线；`distill`：同签名 ≥2 次 applied 且回复一致 → 剧本候选（循环每 60s 自动写入 Brain） |
| Planner | `src-tauri/src/agent/planner.rs`（新增） | `PlannerGate` 总闸（默认关、不持久化）；`should_plan` 只兜 `observe / wait + observed`（规则编不出回复才调模型）；`build_request` 把 KB 标为不可信数据再过一次 `redact`；全局并发 ≤2（RAII permit）；结果必须再过 `policy::authorize`，失败一律回退 Observe |
| 命令 | `steward_cmd.rs` | `steward_explain / steward_review / steward_audit / steward_distill / steward_planner / steward_set_planner / steward_plan_now` |
| 循环 | `steward_loop.rs` | 空转时按局面主动推进（Pager → `q`、Question/AwaitingInput → 一句「请继续」）、时间盒触发、预算耗尽、`PlannerUsed` 事件 |
| 前端 | `steward.ts` + `StewardPanel.tsx` | `coreView / userKey / takeover / handback / undo / policyV2 / setAuthority / timebox / setTimebox / checkpoints / resume / explain / review / audit / distill / planner / setPlanner / planNow` 全量命令层；「托管核心」与「复盘」两个可折叠区 |

### 端到端测试（`src-tauri/src/agent/steward_e2e.rs`，9 例全绿，纯逻辑无 GUI / PTY / LLM）

覆盖设计 §8 的场景 A–E 全链路，与常驻循环共用 `build_context`（只是把 `inject_keys` 换成
字节检查、「等回执」换成立即喂下一帧）：

- **A** claude code 提问屏：帧落地（哈希校验）→ 分类 Question/置信度 ≥0.8 → 意向装载 → 编译
  `answer`（零 LLM、引用意向）→ Notify 档放行 → `send_bytes` 单行 + `\r` → 30s 撤销窗口 →
  回显帧 → `verify_receipt` Applied（回显命中）→ review / explain / distill（同签名两次
  applied → 1 条剧本候选）。
- **B** 永久扩权选项：编译只挑「本次允许」并出升级卡；伪造自动选 "don't ask again" 的决策被
  `PolicyVerdict::Deny` 硬禁。
- **C** 密码屏：走 `secret` 通道、tail 脱敏、不可注入；auto 档也被 Deny。
- **D** Checkpoint：Clean / Drift（降 approve）/ Expired（completed、24h TTL）/ Suspended 四判定 +
  `StewardCore` 存取往返。
- **E** 夺权 / 交还：`user_key` 延迟 ≤100ms、观察态不可写、AbsenceNote 进摘要、`handback` 恢复
  驾驶、撤销写 Learned 偏好且不重复 Create。

另有：断流 60s → observe + Approve（fail-closed 两道闸）；时间盒六档触发与复位；
预算耗尽（Exhausted）；Planner 总闸（默认关；有确定性回复不调模型；只有 observe 兜底）。

### 验证汇总（P3–P5 收口）

- `cargo test --lib`：**1101 passed / 0 failed**（含修掉的 `sysstat::collect_has_core_keys`
  ——macOS 补 `top -l 2` idle 反算 CPU、`ioreg` 取内存，Windows/Linux 分支不变）。
- `npm test`：**318 passed**（+3：P3 事件标签完整、按键夺权 250ms 节流、空字节不触发）；
  `tsc --noEmit` 通过。
- 与 §9 差异总表的收口情况：P2 偏差 2（LLM Planner）已落地且默认关；偏差 3（KB 聚合
  hostmem / skill 索引）仍未做——KB 目前是独立 `steward_ctx` 条目集合；偏差 5（决策并入
  timeline）已做（`push_timeline` → Kind::Decision + 前端图标）；偏差 6（单调时钟）已关闭。
