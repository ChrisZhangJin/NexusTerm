# 统一托管引擎设计（v3）

> 基于 `bugfix/nexus-agent` @ `da4d100` 的评审结论。取代 `nexus-agent-steward-design.md` 中「零 LLM 守护进程动手」的部分。
> 行号是写作时的近似位置，动手前先核对。

## 0. 问题

现状有两个引擎：

- **LLM 工具循环** `act::run`（`mode:"steward"`）真正动手。「开始托管」（`AgentPanel.tsx` `runSteward` ~356–381）先签契约，再 `await api.agentRun`，`finally` 里停契约，所以模型一给最终答复（包括「需要问你」）托管就结束。用户按键会暂停它；300s（`PAUSE_MAX_WAIT`，act.rs:2783）内不交还就让模型收尾，累计 2 次（`MAX_TOOK_OVER`，act.rs:2819）就结束整轮。契约里的授权档位、deny 规则、Profile 硬偏好、意向、KB 都**拿不到**；也没有 checkpoint、success criteria 和 DecisionRecord。
- **零 LLM 守护进程** `steward_loop.rs`：契约、授权、意向、DecisionRecord、看门狗、checkpoint 都在它这里，可它在 shell 提示符处不能推进（decision.rs ~303），只会应答提示。

已确认的 bug：
- `hold_paused` 调了 `runtime.release_control`（act.rs:2759），控制权变成 UserDriving，守护进程随即恢复注入，同一终端出现两个写者。
- checkpoint 恢复时用的是不稳定的 `tab-${Date.now()}-${seq}` id，重启后找不到，恢复项无限重新排队（steward_loop.rs:1277/1330）。
- 用户按一下键再交还，预算用尽 / deadline 的状态就被绕过（ownership.rs:129–134、~217；steward_loop.rs:1367；checkpoint.rs:171）。
- 守护进程等 `sessions.write` 不设超时（steward_loop.rs:856–870）。
- 启动竞态：契约先签好，运行后才拿到控制权。

## 1. 目标架构

**只有一个写者：托管任务。** 新增 `HostSupervisor`（`agent/host/mod.rs`，用 `app.manage()` 注册），每个被托管的 console 起一个独立的 tokio task，跑 `act::run(RunMode::Hosted(HostHandle))`。
- 运行 id 改为 `host:{contract_id}`，避免和同一 tab 上的对话运行在 `begin()`（act.rs:2519）处冲突。
- `ControlTable`（control.rs:75）是**唯一**的写租约，从 `host_start` 一直持有到 Completed / Failed / Stopped。暂停、接管、挂起都**不释放**。
- 退役 `OwnershipTable` 的写角色和 `StewardCore::write_permit`（steward_core.rs:387）。
- `host_start` 立即返回 `contract_id`，UI 订阅 `nx://host/{id}` 事件。

**守护进程只观察。** 保留 ScreenStore、Situation 分类、心跳、stale feed 检测、EventLog，另把 `ObservedSituation{console, signature, kind, hash}` 发到 `tokio::broadcast`。它**没有任何写路径**：删掉 `inject_keys`、撤销窗口、升级卡消费者、回执注入。

**反射快路径（reflex）**放在托管任务**内部**，共用同一个租约：任务阻塞在 `nx_watch`（act.rs ~3997）而观察器报 NeedsAttention 时，先 `profile::match_playbook` 再 `policy::evaluate`；得到确定的 Answer 且 gate 为 Allow，就走正常的受控写入，记一条 `DecisionRecord{llm_used:false}`，把一行 `[reflex] …` 记进 feedback，然后继续观察；否则唤醒模型。没有托管运行的 console 不会被写入，守护进程只出建议卡。

v1：一份契约只管一个 console（可以给多个 console 各签一份）。

## 2. 生命周期（`host/phase.rs`，纯函数）

```rust
enum HostPhase {
  Running,
  WaitingHuman { card: HumanCard },            // nx_ask_user / 审批 / 看门狗 L2 / 人工确认的 check
  UserDriving  { resume_to: Box<HostPhase> },  // 不设超时
  Paused   { cause: PauseCause },              // Budget(reason) | Deadline | User
  Suspended{ reason: SuspendReason },          // AppRestart | ConsoleGone | LlmDown
  Completed{ summary: String, checks: Vec<CheckResult> },
  Failed   { error: String },
  Stopped,
}
enum HostEvent { UserKey, Takeover, Handback, Ask(HumanCard), Answer(CardReply),
  BudgetExhausted(Reason), Resume{new_budget}, Restart, Reattach(ConsoleRef),
  CompleteVerified, CompleteRejected, Stop, Fatal(String) }
```

| 从 | 事件 | 到 | 触发方 |
|---|---|---|---|
| 任意非终态 | UserKey / Takeover | UserDriving{resume_to: 当前态} | `user_control`（act.rs:2493） |
| UserDriving | Handback | `resume_to`（**不一定**回 Running） | 用户 |
| Running | Ask | WaitingHuman | `nx_ask_user`、Approve 档的写入、看门狗 L2 |
| WaitingHuman | Answer | Running | 用户；答复以用户消息追加 |
| Running | 预算 / deadline 耗尽 | Paused | 契约级 `Spent` 检查 |
| Paused | Resume{new_budget} | Running | **只能**由用户触发 |
| Running | `nx_complete` 且 checks 全过 | Completed | 见 §5 |
| 任意 | Restart | Suspended(AppRestart) | 启动扫描 |
| Suspended | Reattach + 用户确认 | 重启前保存的状态 | 用户 |

这条恢复规则堵住了预算绕过：Paused(Budget) 时按键 → UserDriving{resume_to: Paused(Budget)} → 交还后仍回 Paused。

**已定（2026-11-13，修订「用户按键永远赢」）：AI 驾驶中的会话只读。** 托管和对话运行都一样：AI 正在驾驶某个会话（tab）时，这个会话的键盘输入与粘贴一律丢掉（xterm 自动应答照常放行），终端上显示「AI 托管中 · 只读」横幅。夺回控制权只有两条路：点「接管」，或按 **Ctrl+C**（立即接管并把 `^C` 发出去）。只锁被驾驶的那个会话，其它 tab 不受影响。判定见 `src/lib/control.ts` 的 `lockedInput`。原因：误触一个键就会把 AI 挤成「你在驾驶」，卡片上的操作也随之失效。

**已定：挂起的卡片永远等下去**，不倒计时，没有默认动作。UserDriving / WaitingHuman 期间**零 LLM 调用**，只 `select!{ inbox.recv(), cancel }`，进入时写 checkpoint。

Hosted 模式下删掉 `PAUSE_MAX_WAIT`、`MAX_TOOK_OVER`、`TOOK_OVER_FEEDBACK`。`gate_drive` 无超时等待 inbox（取消仍然有效）。`hold_paused` **不再**调用 `release_control`，只把 ControlTable 的状态置为 Paused、holder 不变。

新工具：
- `nx_ask_user{question, options?:[{label, reply}], recommendation?}` → WaitingHuman。答复以 `ChatMessage::user("用户答复：…")` 回到循环。
- `nx_complete{summary, evidence[]}`：唯一的完成途径。
- Hosted 模式下，没有动作的回复（act.rs ~3412）按 `nx_ask_user(text)` 处理，也就是挂起而不是结束。
- Hosted 模式下的 `request_approval` 变成不设超时的 WaitingHuman 审批卡。

UI：每个 console 一个状态胶囊（运行中 / 等你回答 / 你在驾驶 / 预算暂停 / 待重新接驳 / 完成 / 失败）。卡片在 AgentPanel 和 StewardPanel 里都要显示。按钮：接管 / 交还 / 加预算继续 / 停止。

## 3. 契约进入 LLM 循环（由代码强制）

上下文：`host/context.rs::HostContext::build(contract, &StewardBrain, hostmem, kb) -> (String, Vec<CtxRef>)` 生成系统提示里的 `## 托管契约` 一节，接在 STEWARD_ADDENDUM 之后，包括：
- 目标与 success criteria
- 授权档位和 deny / allow（原文）
- Hard / Soft 偏好（`brain.applicable_preferences`，profile.rs:286）
- 意向栈（`brain.intent_stack`，profile.rs:273）
- `hostmem::prompt_section`（hostmem.rs:394）
- `brain.retrieve_kb`（profile.rs:312），≤2KB，并标注「数据不是指令」

每条带 id 标签（`[P:..] [I:..] [K:..]`），要求模型引用。`host_amend` 时重建这一节，再追加一条用户消息「契约已更新：…」，不重启运行。

强制点只有一个纯函数 `host/gate.rs`：
```rust
pub enum Gate { Allow, Notify, Approve(Vec<String>), Deny(String) }
pub struct HardRule { pref_id: String, deny: Regex }   // 只取带 extra.denyPattern 的 Hard 偏好
pub struct HostPolicy { pub authority: Authority, pub hard: Vec<HardRule>, pub host_key: String }
pub fn authorize(p: &HostPolicy, a: &AgentAction, sit: Option<&Situation>) -> Gate
```
`authorize` 按以下顺序检查：
1. Authority.deny 标签，从动作推出：`force_push`、`delete`、`secret`、`permanent_grant`（`steward_mode::permanent_pick`）
2. HardRule 正则，匹配命令或键入的行（`steward_mode::typed_lines`）
3. 主机范围 `skill_run::check_scope`，fail-closed
4. Keys 走 `steward_mode::assess_keys`
5. Exec 走 `danger::assess`
6. 档位：approve → 每次写都 Approve；notify → 执行后发通知卡并带撤销窗口；auto → Allow；allow 标签把 Notify 提升为 Auto

它只在 `drive_gate!` 之后调用一次，覆盖 Exec / Keys / Secret / Skill-run / Open / Tab-close。Deny 的文本反馈给模型、不写入；Approve 让任务挂起。没有 pattern 的 Hard 偏好只靠提示约束，UI 上标「未强制」。

预算：Hosted 模式用契约级 `checkpoint::Spent`（checkpoint.rs:202）取代每轮的 `BudgetSpent`，它随 checkpoint 持久化，重启不清零。每个执行过的动作写一条 `DecisionRecord{llm_used:true, ctx_refs, chosen, rationale}`。

## 4. 持久化与恢复

**已定：存明文 SQLite**，保护级别与现有的历史表 / 时间线表相同。

新表 `store/host.rs`：`host_sessions(contract_id PK, state TEXT, json TEXT, updated_at)`。
```rust
struct HostCheckpoint { v: u32, contract: StewardContract, authority: Authority, phase: HostPhase,
  console: ConsoleRef, messages: Vec<ChatMessage>,   // 先 compact::compact（≤60k 字符），再 history::sanitize_messages
  spent: Spent, step: usize, woke_hash: HashMap<String,String>, last_screen_hash: String,
  watchdog: WatchdogState, created_wall_ms: i64, saved_wall_ms: i64 }
struct ConsoleRef { host_key: String, session_record_id: Option<String>, label: String,
  cwd: Option<String>, tmux: Option<String>, console_id: String /* 易变，重启后不作键 */ }
```
保存时机：每次状态迁移、每个「动作 + 反馈」周期之后、Running 期间每 30s。密文永远不进 `messages`。

**已定：重启后一律由用户确认再接驳。**
1. 启动时把所有非终态行标为 Suspended(AppRestart)，此时不写任何东西。
2. 面板显示「待重新接驳」卡，候选 tab 按 host_key → session_record_id → label → cwd 排序；也可以选「打开新连接」：SSH 用保存的记录打开，本地则新开 tab，然后 `cd`，记录了 tmux 的再 `tmux attach`。
3. 用户确认后调用 `host_reattach(contract_id, console_id)`，supervisor 带着保存的 messages 重启任务，强制先 `nx_screen`，并追加「应用重启后重新接驳，先读屏核对现场」。
4. 用 `checkpoint::resume_verdict` 比对屏幕：有漂移就把档位降为 approve；一致就恢复重启前的状态。挂起中的卡片原样恢复。
5. TTL 24h → Expired。

## 5. Success criteria 与看门狗

`StewardContract.success_criteria: Vec<Check>`，`enum Check { ScreenMatches{re}, CommandOk{cmd}, FileExists{path}, Manual{text} }`。

**已定：由模型起草、用户确认**——开始托管时一次 LLM 调用把目标转成草稿，用户编辑或接受。**YOLO 模式**：直接采用草稿，不经确认就开始托管（契约里加 `yolo: bool`，UI 上明显标出）。YOLO 只跳过确认这一步，不改变授权档位和 deny 规则。

`nx_complete` 触发 `host::verify`。**已定（2026-11-14）：判据命令不往终端里敲**——在旁路里执行（SSH 同连接的 exec 通道 / 本地 `sh -c` 子进程，工作目录 = 终端当前目录）；终端前台是 Claude Code、vim 等也能核验，也不会在终端里留下 `__NX_CHECK_RC` 残渣。没有旁路（堡垒机兼容模式、Windows 本地）时，只有终端停在空闲 shell 提示符上才退回在终端里执行；否则这条判据记为「未核验」，不阻塞完成。


- CommandOk 要过 `gate::authorize`，且 `danger::assess` 判为只读，否则拒绝执行
- FileExists 用 `test -e`
- ScreenMatches 匹配当前帧

有一项自动判据没通过就把「未满足：…」反馈给模型，仍是 Running；自动项全部通过 → Completed，释放租约。**已定（2026-11-13）：Manual 项不阻塞完成**，不再出「等你确认完成」卡，只在完成报告里列为「未自动核验（仅供参考）」。

看门狗 `host/watchdog.rs`（纯函数，状态写进 checkpoint）：
```rust
struct StepObs { action_fp: u64, sig: String, fb_hash: u64, acted: bool }
enum Alarm { Loop{fp, n}, NoProgress{steps, ms}, Thrash{rounds} }
fn observe(&mut self, o: StepObs, mono_ms: i64) -> Option<Alarm>
```
- Loop：最近 8 步里同一个 fp 出现 ≥3 次且 sig 没变
- NoProgress：6 步内 sig / fb_hash 都没变，或超过 `no_progress_ms`
- Thrash：连续 ≥3 轮没有动作

升级梯度：L1 往 feedback 里追加提示；同一警报再次出现 → L2，强制 WaitingHuman，卡片附最近 5 条 DecisionRecord。

## 5.1 少打扰人（已定，2026-11-15）

托管的主旨是全托管：只有**不可逆 / 关键**的事才停下来问人，其余由 AI 自己定。

- **闸门分级**（`gate::authorize` 第 4–5 步）：`danger::assess` 新增 `critical`（格式化 / 写裸设备 / 关机重启 / 删根·家·系统目录 / 删库 / `git reset --hard` / `curl|sh` / 数据卷清理……），按键审批原因带 `steward_mode::CRITICAL_MARK` 的同理（含屏幕上有「永久 / 不再询问」字样又认不出选项的菜单）。不可逆的任何档位都 Approve；其余「危险但可恢复」或「看不清提交的那行」的，只有「确认」档 Approve，通知 / 自动档执行并发通知卡（Notify）。删除、force push 由契约禁止项管（默认禁止，用户放开即同意），不在危险分级里重复拦。
- **自主作答**：`nx_ask_user` 带了 `recommendation` 的，非「确认」档直接按推荐继续（时间线记一条「替你决定」），一次托管最多 12 次；不带推荐的（只有用户能答：账号、密码、放开禁止项……）照常挂起问人。
- **纯文本回复**：第一次退回让模型继续，连着第二次才当成向人提问。
- **完成判据**：默认不确认，直接采用草稿（面板上「开始前确认判据」可勾回来）。未确认的判据与 nx_exec 同一道闸，但除 deny 外一律不问人：要批准的或判危险的记「未核验」，不阻塞完成。
- 仍然问人：密码、不可逆写入、契约禁止项需要放开、看门狗二级（确实卡住了）、预算用完 / 模型不可用、受保护路径的文件写入。

## 5.1.1 YOLO 档（已定，2026-11-20）

`TIER_YOLO = "yolo"`：一切自主执行，包括不可逆操作（危险的发通知卡留痕，不停）；受保护路径的文件读写、预览不完整也不出卡。
**只有越过契约才停下来问人**：禁止项（标签 / 自定义关键字）、硬偏好正则、主机范围 —— 一般档位是直接拒，YOLO 改成审批卡
（「⛔ 越过契约」，批准即放行这一次）。两条底线（`LOCKED_DENY`：替人输密码、选「永久允许」）任何档位都直接拒；按键闸里
「先读屏再选」这类 Refuse 照旧（它们让模型自己去看，不打扰人）。托管卡上可在运行中切换档位（`host_amend`）。

## 5.2 断线重连自动接着托管（已定，2026-11-18）

SSH 断线 → 会话从 `SessionManager` 摘掉 → 托管挂起（`Suspended{ConsoleGone}`，租约不放）。前端自动重连用的是**同一个会话 id**，
每条连接有一个新的代号（`SessionManager::generation`）。挂起中的 `park()` 每 3 秒查一次：同一个 console 连回来、连续两轮都在
（≈6 秒，给自动宏 / tmux attach 留时间）→ `HostEvent::ConsoleBack` → 回到挂起前的状态，强制读屏，并告诉模型「新 shell，
前台程序 / 目录可能没了，上一条命令可能没跑完」（`RECONNECT_NOTE`）。断得太短、没被判成 ConsoleGone 的，运行循环在每步开头
比对连接代号，同样提醒模型。应用重启（`AppRestart`）与 tab 被关掉仍走人工接驳。

## 6. 迁移计划

| PR | 内容 | 依赖 |
|---|---|---|
| PR1 止血 | 守护进程只观察（删掉 inject_keys 的调用点，保留建议卡）；`llm_run_owns` 在 `control.holders_of(console)` 非空时也为真；找不到 console 的恢复项过 TTL 就丢弃；Suspended / Exhausted 在 user_key → handback 后仍保留（ownership.rs、checkpoint.rs）；`hold_paused` 在 steward 模式下不释放租约（act.rs:2759）。测试：LLM 运行暂停期间零注入；预算耗尽状态经 user_key → handback 后仍在；缺失 console 的恢复项会过期 | 无 |
| PR2 纯模块 | 新增 `agent/host/{mod,phase,gate,watchdog,context,checkpoint}.rs`。测试：完整迁移表；gate 真值表；看门狗序列；context 渲染与大小上限；HostCheckpoint serde 往返与版本 | 无（只新增文件） |
| PR3 存储 | `store/host.rs` + schema。测试：upsert / list / 非终态扫描 / TTL | 无 |
| PR4 托管运行 | `RunMode::{Chat, Hosted}`；`host_start`（先拿 ControlTable，再签契约，再起任务）；inbox；`nx_ask_user` / `nx_complete`；无动作回复时挂起；去掉 Hosted 模式的超时；审批改为挂起；前端 `runSteward` → `host_start`，删掉 `finally` 里停契约那段，订阅 `nx://host/{id}` | PR1–3 |
| PR5 强制 + 上下文 | 接入 `gate::authorize`；注入 HostContext；契约级 Spent；DecisionRecords；success criteria 起草 + 确认 / YOLO | PR4 |
| PR6 恢复 | 启动扫描 → Suspended；`host_reattach`；接驳卡；App.tsx 把 ConsoleRef 数据（sessionId、ctx）传给后端 | PR5 |
| PR7 看门狗 + criteria + 反射 | 接入 act.rs 和 watch.rs | PR6 |
| PR8 删除 | 守护进程的决策 / 注入 / 升级 / 撤销 / 回执 / 恢复 / planner / distill；ownership.rs；decision.rs 的写路径；planner.rs；steward_core 里写相关的部分；steward_cmd 里的写命令；StewardPanel 的旧卡片 UI；按假 IO 重写 steward_e2e 场景 A–E | PR7 |

PR1 ∥ PR2 ∥ PR3 并行；PR4 → PR5 → PR6 → PR7 都改 act.rs，必须串行；PR8 最后。
