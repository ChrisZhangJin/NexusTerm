# Nexus Agent 协同化改造 · TODO

目标：Nexus Agent 不再只是「替用户跑命令的循环」，而是**与用户共用 Nexus 终端的协作者**——
能操作 console（输入、读取、代替用户做交互）、能把用户的一连串操作学成 skill、下次独立完成。

分四个阶段，后一阶段依赖前一阶段。每项完成后在这里打勾，并写明提交。

## 现状缺口（改造前）

| 缺口 | 位置 |
|---|---|
| 只能发 `命令\n`，发不了控制字符 / 不带回车的输入 | `act.rs` `reject_command`、`run_one` |
| 运行中读不到当前屏幕（只在开始时传一次 `console_text`） | `plan_message`、`AgentRunRequest` |
| 完成判断靠提示符 + 静默，换提示符的命令要等满 120s | `executor.rs` |
| 无交互提示识别、无密码通道 | — |
| Agent 运行路径**没有脱敏**，输出原样发给模型 | `act::run` → `llm::complete_reply` |
| 用户与 Agent 同写一个 pty，无互斥；停止不发 Ctrl-C | `ActRuntime`、`AgentPanel` |
| 后端只有原始按键，没有「用户提交了哪条命令」 | `cmd.rs` ssh_write/local_write |
| 无记忆、无 skill；宏无参数、会录下密码 | `macro.ts`、`macros` 表 |

---

## 阶段 1 · 手和眼（对终端的操作原语）

- [x] **1.1 脱敏补洞**：agentic 路径上，`plan_message` / `console_text` / 命令输出反馈 / 错误，凡发往 LLM 的内容在 `acct.redact` 为真时过 `redact::redact`。加测试。
- [x] **1.2 `nx_send_keys`**：原始按键与控制字符（`\x03` Ctrl-C、`\x04`、`\x1b`、方向键、Tab、不带回车的文本）。新 `AgentAction::Keys`；与 `nx_exec` 区分——Keys 不等命令结束，只写入并返回「已发送」。过 danger / scope；`reject_command` 的单行限制只对 Exec 保留。
- [x] **1.3 `nx_wait`**：等某段文字出现（子串或 `/regex/`）或输出静止，带超时，返回命中与否 + 此刻屏幕尾部。
- [x] **1.4 `nx_screen`**：运行中随时读取目标会话的当前 xterm 屏幕（含 alt screen）。复用 `request_tab` + `nx://agent/tab` + `agent_tab_ack` 的前端往返模式，前端用 `snapshotConsole`。
- [x] **1.5 交互提示分类器**（纯函数，`agent/prompt.rs`）：把屏幕尾部 / 最后一行分类为 `Password | YesNo | Pager | Menu | Repl | ShellPrompt | Unknown`，并给出建议应答。大量单测。
- [x] **1.6 接入分类器**：`nx_exec` / `nx_wait` / `nx_screen` 的返回里带 `prompt_kind`；`wait_for_end` 在检出非 ShellPrompt 的交互提示时提前返回，而不是等满超时。（`nx_send_keys` 同样带；输出静默 1s 后置信度 >= 0.8 的非 ShellPrompt 提示以 `awaiting_input` 提前返回，提示行 `prompt_kind= confidence= 提示：` 进反馈；面板步骤卡带提示标签。）
- [x] **1.7 密码通道**：Agent 只能发起 `nx_secret{reason}`，由用户在审批卡里输入（或选保险库条目），后端直接写入 pty；密文不进模型上下文、不进 trace、不进 side buffer 的 `$ ` 转录。（`agent/secret.rs` + `act.rs` 的 `nx_secret`：事件 `nx://agent/secret/{session}` → 面板掩码输入卡 → `agent_secret_ack` → 后端 `write_to` 写 pty；120s 超时、停止即取消。保险库条目选择暂未做，只支持手输 / 跳过。）
- [x] **1.8 系统提示更新**：告知新工具的用法与边界（何时 send_keys、何时 secret、何时 screen）。

## 阶段 2 · 共同在场（共享控制权）

- [x] **2.1 控制权状态机**：`UserDriving | AgentDriving | Paused`，按会话维护；前后端同步。（`agent/control.rs` 纯状态机 + 登记表，`ActRuntime.control`；事件 `nx://agent/control/{session}`（载荷 `{session,state}`，同时发给被驾驶会话与发起会话），查询 `agent_control_get`；运行开始即 AgentDriving，结束 / 出错 / 停止由 `ControlLease` Drop 还给用户。）
- [x] **2.2 用户输入仲裁**：Agent 运行期间用户一敲键盘 → 自动 Paused，面板出现「接管 / 交还」。Agent 写入前检查状态。（前端 `lib/control.ts` 键盘闸：闸关时每键仅一次布尔判断，Agent 驾驶时首次真键入才发一次 `agent_control_user_input`，xterm 自动应答不算；后端在每个动作开始前与 exec / keys / secret 写入前 `gate_drive`，Paused 轮询等待（可取消，上限 5 分钟，超时告知模型「用户接管」，累计 2 次收束本轮）；面板横幅：Paused 显示「交还」，Agent 驾驶中显示「接管」。键盘字节从不被拦、不被延迟。）
- [x] **2.3 停止语义**：「停止」可选同时发 Ctrl-C；停止后状态回到 UserDriving。（面板「同时发 Ctrl-C」复选框，localStorage 持久、默认关；`agent_run_cancel(sessionId, sendCtrlC)` → `act::stop_run`：置取消、可选向被驾驶会话写 `\x03`（绕过暂停闸）、立即释放控制权。）
- [x] **2.4 提交命令事件**：前端 `onCommandCommitted`（已有于 `TerminalView`）推给后端；后端 side buffer 增加分段后的 user command 记录，且不记录无回显（密码）输入。（回车时 `agent_user_command{sessionId,line,noEcho}` fire-and-forget；`agent/usercmd.rs` 的 `UserCommandEntry{seq,tsMs,actor,kind,line,truncated,at}` 环形 256 条；无回显（前端 `lineEchoedRef`）或后端 `prompt::classify` 判 Password >= 0.8 → 只存 `secret_input` 占位；读取走 `AgentManager::user_commands / _redacted / _since`，给模型必须用脱敏版。）
- [x] **2.5 统一时间线**：终端会话的事件流 `{actor: user|agent, kind: command|keys|prompt|output, ts}`；面板展示，并标明谁做的。（`agent/timeline.rs`：每会话环形 512 条的只追加事件流 `{seq,tsMs,actor,kind,text,note,refSeq,promptKind,confidence,truncated}`，kind = command / keys / prompt / output / secret / wait / screen；合并 2.4 的用户命令与 `act.rs` 的 Agent 动作（exec / keys / wait / screen / secret，secret 只存占位）、非 ShellPrompt 的提示检出、每条命令的输出摘要（`SessionBuffer::out_since` 按 `at` 切出 Out 段，尾 20 行 / 2KB、脱敏，作为独立 `output` 事件 `refSeq` 回指）；用户命令的输出等 800ms 静默后补一条；`agent_timeline(sessionId,sinceSeq,limit)` + 事件 `nx://agent/timeline/{session}`（载荷为新增事件数组）；面板「对话 / 时间线」切换，`components/Timeline.tsx`（memo 行、贴底才自动滚动、输出折叠）+ `lib/timeline.ts`；`plan_message` 附「用户最近操作」摘要（≤8 条 / 1.2KB / 30 分钟内，按账户脱敏开关脱敏）。）
- [x] **2.6 对话与运行持久化**：跨重载保留对话；`act::run` 能带上上一轮摘要。（范围=全局一份：面板对话本就全局唯一，标签 id 重启即变，按标签分桶无意义。新表 `agent_conversations(scope,messages JSON,updated_at)` + `agent_run_summaries(id,scope,ts,summary JSON)`，`CREATE IF NOT EXISTS` 纯增量；对话上限 200 条 / 1MB / 单字段 8000 字，摘要留 20 条。落盘前后端 `agent/history.rs::sanitize_messages` 统一脱敏（不看账号开关，落盘即脱敏）、丢 value/password 键。`StepPayload` 经 `emit_step` 进 `RunLogs`，`RunLogGuard` Drop 时收成 `RunSummary{goal,actions,outcome,finalState}`；`act::run` 把最近 3 条（<=3000 字、再过 `scrub`）拼在第一条 user 消息前。前端 `lib/agentHistory.ts`（纯逻辑）+ `lib/useAgentHistory.ts`，挂载读回、1.5s 防抖写盘；读回时把未收尾的审批 / 密码卡 / 进行中步骤收尾；面板新增「清空对话」真删库。）

## 阶段 3 · 学习（操作 → skill）

- [ ] **3.1 Skill 数据模型与存储**：`skills` 表（名称、描述、参数、步骤、验证条件、风险等级、适用主机范围、创建/更新时间）；步骤 = `exec | keys | expect+respond | secret | screen_check`，含 `on_unexpected: ask|stop|llm`。CRUD 命令。
- [ ] **3.2 录制器**：基于 2.5 时间线录制一段操作（开始/结束），密码步骤只记 `secret` 占位。替代旧宏录制中「录下密码」的问题。
- [ ] **3.3 Agent 整理 skill**：录制 → LLM 提取参数、前置条件、每步期望、验证 → 草稿 skill，用户可编辑后保存。
- [ ] **3.4 Skill 管理界面**：列表 / 查看 / 编辑 / 删除 / 导入导出（文本文件）。
- [ ] **3.5 旧宏迁移**：`macros` 表数据可一键转成 skill。

## 阶段 4 · 独立执行（混合回放 + 记忆）

- [ ] **4.1 `nx_skill` 工具**：`list | run | show`，Agent 可按描述检索并运行 skill。
- [ ] **4.2 确定性回放引擎**：按步骤执行，每步按「期望」检查屏幕；偏离则按 `on_unexpected` 处理。
- [ ] **4.3 混合回放**：偏离时转交 LLM 判断（继续 / 改做法 / 停下问用户），并把结果回写建议（可更新 skill）。
- [ ] **4.4 信任分级**：首次每步确认 → 成功 N 次后只读 skill 可免确认；高风险步骤始终确认；skill 只能作用于其适用主机范围。
- [ ] **4.5 主机记忆**：每台主机的备忘（提示符、sudo 习惯、常用路径），Agent 可读写，用户可见可删。
- [ ] **4.6 运行预算与可恢复**：取代硬编码 `MAX_STEPS=8`；长任务可暂停/恢复。

---

## 约定

- 分支：`bugfix/nexus-agent`；每个子任务由 agent 在独立 worktree 完成，`--no-ff` 合入，附测试。
- 验证：`cargo test --lib`（`src-tauri`）、`npx tsc --noEmit`、`npm test`。
- 安全底线不放松：scope 强制、危险命令审批、脱敏、模型永不接触密码。
