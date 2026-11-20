//! # 托管端到端回放（e2e）
//!
//! 单测覆盖了每个零件，可真实 bug 几乎都出在**整条回路**上：模型 → 工具调用 → 门禁 → 真终端 →
//! 读屏分类 → 下一步。这里把整条回路原样跑起来，只把两头换成可控的：
//!
//! ```text
//! 剧本（FakeLlm, 127.0.0.1 上的 OpenAI 兼容服务）
//!    ↑ reqwest（llm.rs 原样）
//! host_cmd::host_start → act::run(RunMode::Hosted) → HostShared 相位机 / gate / heredoc / executor
//!    ↓ state.local.write                                  ↑ ScreenStore（帧）+ 旁路缓冲
//! 本地 PTY 里的真 bash（cwd = 临时目录） ── nx://local/data ──→ vt.rs 极简终端 → 全量帧（每 250ms 心跳）
//! ```
//!
//! - **假模型**（`fake_llm.rs`）：生产代码零接缝 —— 配置里的 `baseUrl` 指向本机端口而已，出网层
//!   （请求拼装、重试、工具配对修复、回复解析）全都真跑。剧本是有序的 [`fake_llm::Turn`]（可断言
//!   「上一条反馈里必须含 X」），外加一次性的 [`fake_llm::Rule`]（反馈含 X 就回 Y）。判据起草请求
//!   （系统提示词 = `criteria::DRAFT_SYSTEM`）回场景给的判据 JSON。剧本用完 → HTTP 400 → 运行 Failed。
//! - **终端**：`LocalManager::spawn` 起的真登录 bash，`PS1='\u@e2e:\w\$ '`、cd 进临时目录、清屏。
//!   前端 xterm 的角色由 `vt.rs` 扮演（局限见该文件开头：无宽字符 / 滚动区，全屏 TUI 可能错位）。
//! - **应用**：`AppHandle` 在代码里是写死的 `AppHandle<Wry>`，`tauri::test::MockRuntime` 换不进去；
//!   所以起一个**真的 Wry 应用**，但用 `tauri::test::mock_context`（不嵌前端资源、不建窗口）+
//!   `Builder::any_thread()`，事件循环跑在后台线程。Linux 上 GTK 需要显示：有 `DISPLAY` 就用，
//!   没有就自动起 `Xvfb`（`-terminate`，测试进程退出后自行结束）；两样都没有 → 场景打印「跳过」后直接通过。
//!   **CI 必须装 Xvfb（或给 DISPLAY）**，否则这些场景全部静默跳过、等于没跑。
//!   `manage` 的状态与 `lib.rs::run` 的 setup 一致（含常驻观察循环），数据目录在临时目录里。
//! - **串行**：一个进程只能有一个 tao 事件循环 → 所有场景共用一个应用，靠 `Harness` 里的全局锁串行跑。
//!
//! 跑：`cargo test --lib agent::e2e`（单个场景 2–10s，串行合计约 1 分钟；首个场景多付一次 Xvfb / SQLite 启动。
//! `-- --ignored` 跑已知问题的复现）。
//!
//! ## 加一个场景
//!
//! 在 `scenarios.rs` 里：
//!
//! ```ignore
//! #[test]
//! fn my_scenario() {
//!     let mut h = harness!(
//!         vec![
//!             // 模型第 1 步：调 nx_exec。`{dir}` 会换成本场景的临时工作目录
//!             turn(None, "nx_exec", json!({ "command": "ls" })),
//!             // 第 2 步：先断言它看到的反馈里有「已确认执行完毕」，再宣告完成
//!             complete(Some("已确认执行完毕")),
//!         ],
//!         vec![],                                     // 一次性规则：Rule { when, reply }
//!         json!([{ "kind": "commandOk", "cmd": "true" }])  // 判据草稿（YOLO 直接采用）
//!     );
//!     std::fs::write(h.dir.join("x.txt"), "...").unwrap(); // 布置现场（或 h.type_raw / h.paint）
//!     h.start("目标", "auto");                       // 走真实入口 host_cmd::host_start
//!     let p = h.wait_terminal(Duration::from_secs(30));
//!     assert_completed(&h, &p);                      // 判据全过 + 剧本断言全过
//! }
//! ```
//!
//! 可用的观察点：`h.phase()` / `h.wait_phase(..)`（相位）、`h.steps()` / `h.steps_of("gate")`（发给面板的
//! `nx://agent/step/*`，字段 camelCase，如 `gateVerdict`）、`h.statuses()`（`nx://host/all`）、
//! `h.screen()`（模拟器当前屏）、`h.llm.feedback()`（模型每一步看到的反馈）、`h.llm.cursor()`（剧本走到哪）。
//! 交互：`h.answer("...")`（= 面板答卡，走 `host_answer`）、`h.type_raw("...")`（= 用户直接敲终端）。
//! 多终端托管：`let b = h.add_console("b")` 再起一个 bash（cwd = `dir/b`），剧本里用占位符 `{b}` 写 target、
//! `h.llm.substitute("{b}", &b)` 换成真 id，`h.start_with(goal, tier, &[&b])` 开始；`h.screen_of(&b)` /
//! `h.paint_on(&b, ..)` / `h.type_raw_on(&b, ..)` 看 / 布置那个终端（见 `multi_console_*` 场景）。
//! 失败时断言信息里带 `h.dump()`：屏幕 + 步骤 + 模型每一步看到的反馈。
//!
//! ## 把误读样本变成屏幕夹具
//!
//! 用户在门禁卡上点「这是误判」，应用数据目录的 `misreads/misread-<时间>-<局面>.json` 里会存一份
//! `host::misread::MisreadReport`（`screen.lines` = 当时那一屏，已脱敏；`classified` = 当时的错误判断）。
//! 1. 把文件拷进仓库（或直接内联成字符串，见 `misread_sample_replays_as_screen_fixture`）；
//! 2. `h.paint(&harness::misread_lines(include_str!("…/misread-xxx.json")))`：把这一屏原样画到真终端上，
//!    光标停在最后一行行尾、终端停在 `read` 里等输入（模型的按键会被它吃掉，回车后 shell 提示符回来）；
//! 3. 剧本写当时模型想做的那一步（通常是 `nx_send_keys`），断言 `steps_of("gate")` 里没有 `refused`。
//!
//! 只还原**文本**：`screen.alt = true` 的样本（全屏程序）还原不了 alt screen，纯分类问题更适合写进
//! `situation.rs` 的单测（见 `host/misread.rs` 开头）；这里适合「分类对了但整条回路仍然卡住」的回归。

mod fake_llm;
mod harness;
mod scenarios;
mod vt;
