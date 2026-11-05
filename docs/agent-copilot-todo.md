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

- [x] **3.1 Skill 数据模型与存储**：`skills` 表（名称、描述、参数、步骤、验证条件、风险等级、适用主机范围、创建/更新时间）；步骤 = `exec | keys | expect+respond | secret | screen_check`，含 `on_unexpected: ask|stop|llm`。CRUD 命令。（`agent/skill.rs` 纯模型 + 校验 + `render_step` + `assess_risk` + `export_text / import_text`，顶部注释即模型文档；`store/skills.rs` 的 `skills` 表（`CREATE IF NOT EXISTS` 纯增量，params / steps / verify / host_scope 为 JSON 列，上限 200 条 / 每条 100 步）与 `skill_create / update / delete / get / list(query) / record_run_result`；命令 `agent/skill_cmd.rs`：`skill_list / get / create / update / delete / validate / export / import_preview / import`，**不暴露** `record_run_result`；前端类型在 `types.ts`、封装在 `api.ts`、辅助在 `lib/skill.ts`。skill 里永远没有密码值：secret 步骤只有 reason + 参数引用名，结构 `deny_unknown_fields`，secret 参数无默认值且只能用于 secret 通道；导入是不受信输入，新 id、来源 imported、计数清零、风险重评；编辑回放相关内容会清零计数。）
- [x] **3.2 录制器**：基于 2.5 时间线录制一段操作（开始/结束），密码步骤只记 `secret` 占位。替代旧宏录制中「录下密码」的问题。（`agent/recorder.rs`：纯函数 `build_draft(events, meta) -> RecordingDraft`，顶部注释即草稿 JSON 与转换规则文档——用户命令 → `exec`，回答确认 / 分页器 / 菜单 / REPL 提示 → `expect_respond`（`expect` 取自紧挨着的提示事件），Ctrl-C / 空回车等 → `keys`，密码 → 只有 `secret` 占位（reason 取自提示文字）；去噪（空行 / clear / 注释）、续行合并、连续重复合并、步数 / 长度上限；疑似写死口令的命令整条丢弃并告警；每步输出尾部作为 `hints`（脱敏、末 3 行、限长）不进步骤；对整段与逐步 `assess_risk` 告警；时间线环溢出、Agent 动作混入、无主输出、终端输入段回车数与捕获数对不上 → 「录制期间可能有未捕获的操作」。状态 + 命令 `agent/recorder_cmd.rs`：`agent_record_start / stop / status / cancel`、`agent_user_keys`，状态事件 `nx://agent/recorder/{session}`；草稿只经 stop 的返回值交给前端，后端不留存。录制期间后端额外往时间线写用户侧 `prompt`（回车前分类屏幕尾部）与 `keys` 事件，不影响给模型的 digest。前端：`lib/recorder.ts` 纯逻辑、`lib/termLine.ts`（↑ 历史 / Tab 补全后从屏幕读回整行，只在光标位于行尾时采信）、`lib/useRecorder.ts` 模块级 store、`components/RecorderBar.tsx`（时间线头部按钮 + 草稿预览、面板头部 🔴 指示）；`TerminalView` 仅在录制中补报按键 / 空回车 / 读回的整行。）
- [x] **3.3 Agent 整理 skill**：录制 → LLM 提取参数、前置条件、每步期望、验证 → 草稿 skill，用户可编辑后保存。（`agent/skill_refine.rs` 纯逻辑 + `skill_cmd::skill_refine_draft(draft, hint?)`：草稿按账号 `redact` 标志逐字符串脱敏后发给模型，模型回严格 JSON（SkillInput 形状 + `notes`），宽松解析（去代码围栏 / 前后废话 / `{"skill":…}` 包装）；一律过 `finalize`（夹紧 → `validate` → `normalize` → `assess_risk`），不过则把问题清单反馈给模型重试一次，仍不过回退到机械草稿并带警告，机械草稿也不合格才返回错误；返回 `{draft, notes, warnings, fallback, attempts}`。夹紧：来源恒为 `recorded`；`hostScope` 忽略模型输出，默认 `list [录制主机名]`，无可用主机名才 `any` + 警告；风险只升不降；secret 参数清默认值、口令类参数名强制 secret；非只读步骤的 `onUnexpected` 不许是 `llm`；不许丢 secret 步骤、不许凭空新增命令；`[REDACTED]` 占位符回流视为错误；`validate` 未放宽。前置条件写在 `description` 的「前置条件：」段。前端：`RecorderDraftCard` 增加「整理为技能」（进度 / 失败提示 / 结果卡片显示 notes 与 warnings，用户点「打开编辑器」）与「直接编辑」（机械草稿），经 `Timeline` → `AgentPanel` 的 `onOpenSkillDraft` 回调打开 `SkillManager initialDraft`，保存始终由用户在编辑器里确认；纯逻辑在 `lib/skillRefine.ts`，自测 `tests/fe/skillrefine.test.ts`。）
- [x] **3.4 Skill 管理界面**：列表 / 查看 / 编辑 / 删除 / 导入导出（文本文件）。（`components/SkillManager.tsx` 列表 + 搜索 + 删除确认 + 导入（文件 → `skill_import_preview` → 确认 → `skill_import`）/ 导出（`skill_export` → 保存对话框 → `write_file`）；`SkillEditor.tsx` 查看 / 编辑（参数编辑器、按种类的步骤编辑器含增删 / 上下移 / 复制、verify、主机范围、占位符高亮、后端校验问题按步骤 / 参数逐条显示）；入口为 AgentPanel 头部「技能」按钮；风险只读（后端评估只会抬高）；secret 参数在 UI 里不能填默认值；`SkillManager` 接受 `initialDraft?: SkillInput` 供 3.2 / 3.3 / 3.5 预填新建编辑器；纯逻辑在 `lib/skillForm.ts`，自测 `tests/fe/skillform.test.ts`。）
- [x] **3.5 旧宏迁移**：`macros` 表数据可一键转成 skill。（`agent/macro_migrate.rs`：纯函数转换器 `convert_macro`（打字片段合并成行；整行命令 → `exec`，Ctrl-C/D/Z 单独 `keys`，Tab/方向键等混进行里则整行记 `keys`；ssh / su / 数据库客户端等交互会话内的行一律 `keys`；`sleep` / 静默 `wait` 丢弃，仅 ≥2s 的作为上一步 `timeoutMs` 提示，带 pattern 的 `wait` 变成上一步 `expectAfter`）；来源 `migrated_macro`、主机范围不限、风险经 `assess_risk`、计数为 0、重名加 ` (2)` 后缀。**录下的密码一律不复制**：口令提示之后的行、ssh/sudo/su/mysql -p 等登录命令之后的第一行非空内容、命令行里写死的凭据（`password=` / `sshpass -p` / `echo x | sudo -S` / URL 内嵌凭据 / `curl -u` 等）、形似口令的单词、口令后的 4-8 位数字验证码，全部换成 `secret` 占位（或 `expect_respond{secret:true}`），警告只写步骤序号与类别、不引用原文；转换后过 `validate`，因凭据类问题失败的步骤同样被换成占位并报告。幂等：`store/macro_migration.rs` 的 `macro_migrations` 表（`CREATE IF NOT EXISTS` 纯增量）；不删除、不修改原宏（报告里提醒其中仍有明文）。命令 `macros_migrate_preview(ids?)`（不写库）/ `macros_migrate(ids?)`；前端 `api.macrosMigratePreview / macrosMigrate`、`lib/macroMigrate.ts`，宏管理器里「转为技能」「全部迁移」按钮 + `MacroMigrateDialog` 预览 / 结果窗口。）

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
