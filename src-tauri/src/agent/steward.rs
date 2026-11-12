//! 「会话织物」：会话枚举 + 后端屏幕状态 + 托管契约 + 意向栈顶。
//!
//! 这一层是 NexusAgent v2（主动托管 Console）的地基，设计见
//! `docs/nexus-agent-steward-design.md`。P0 只把「有哪些终端、每个终端此刻长什么样、
//! 哪个终端被托管了」变成后端随时可读的事实；P1 加了常驻只读循环（`steward_loop.rs`）；
//! P2 起，签契约会同时把用户那句原话编译成意向栈 I0（`compile_intent_input`），
//! 决策编译在 `decision.rs`，依据在 `profile.rs`。
//!
//! 为什么要把屏幕搬回后端：现状只有「一轮工具调用时前端请求-回执抓一次屏」，
//! Agent 跑完那一轮就再也看不到屏幕了。屏幕状态常驻后端，才能让以后的事件驱动
//! 循环在面板关闭、窗口最小化、甚至跨窗口时依然成立。
//!
//! 与旁路缓冲的分工：旁路缓冲是「字节流」（给模型看历史），这里是「帧」（给状态机看局面）。
//! 两者互不替代，也不共用锁。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::State;

use crate::cmd::AppState;

use super::policy::Authority;
use super::profile::{CtxInput, StewardBrain, KIND_INTENT};
use super::steward_core::StewardCore;
use super::steward_loop::StewardBus;

/// 单帧最多保留多少行。前端抓的是「视口 + 尾部 scrollback」而不是全量历史，
/// 200 行足够覆盖一次问答和绝大多数全屏程序的一屏。
pub const MAX_LINES: usize = 200;

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn fnv1a(bytes: &[u8]) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in bytes {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

/// 帧指纹。**必须与前端 `frameHash`（`src/lib/termRegistry.ts`）逐字节一致**：
/// 行以 `\n` 连接，追加 `\n#alt:cols:rows:cursorX:cursorY`，对 UTF-8 字节做 FNV-1a。
/// 后端重算一遍是为了发现「前端发了增量、后端窗口却已经滑动」这类漂移，
/// 一旦不一致就要求下一帧发全量，绝不带着错位的状态继续判断。
pub fn frame_hash(
    lines: &[String],
    alt: bool,
    cols: u16,
    rows: u16,
    cursor_x: u16,
    cursor_y: u16,
) -> String {
    let mut payload = lines.join("\n");
    payload.push_str(&format!(
        "\n#{}:{}:{}:{}:{}",
        u8::from(alt),
        cols,
        rows,
        cursor_x,
        cursor_y
    ));
    fnv1a(payload.as_bytes())
}

/// 一行的增量。`y` 是抓屏窗口内的下标（0 = 窗口最旧一行），不是终端绝对行号。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenLine {
    pub y: u32,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenDelta {
    pub console_id: String,
    /// 前端自增的帧号；后端用它丢掉乱序 / 重复帧
    pub frame_id: u64,
    /// 抓屏窗口首行的绝对行号。窗口一滑动，增量下标就对不上了 → 必须发全量
    pub window_start: u64,
    pub total: u32,
    pub cols: u16,
    pub rows: u16,
    /// true 表示当前是 alt screen（tmux / vim / claude code 全屏态）
    pub alt: bool,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub hash: String,
    /// 全量帧（首帧 / 重连 / 窗口滑动）；此时 `lines` 覆盖窗口内每一行
    pub full: bool,
    pub lines: Vec<ScreenLine>,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenDeltaAck {
    pub applied: bool,
    /// 后端与前端不同步（缺基线 / 窗口滑动 / 哈希漂移），下一帧请发全量
    pub need_full: bool,
    /// 内容相对上一帧是否真的变了（光标动、哈希相同 → false）
    pub changed: bool,
    pub frame_id: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenSnapshot {
    pub console_id: String,
    pub frame_id: u64,
    pub window_start: u64,
    pub hash: String,
    pub alt: bool,
    pub cols: u16,
    pub rows: u16,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub lines: Vec<String>,
    pub updated_at: i64,
    /// 连续多少帧内容没变。P1 的 Watchdog 用它做空转判定，不必自己再数一遍
    pub idle_frames: u32,
}

#[derive(Clone, Debug)]
struct Frame {
    snap: ScreenSnapshot,
}

#[derive(Clone, Default)]
pub struct ScreenStore(Arc<Mutex<HashMap<String, Frame>>>);

impl ScreenStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 合并一帧。返回是否落地、是否要求重发全量、局面是否变化。
    ///
    /// 关键取舍：任何「说不清」的情况（缺基线、窗口滑动、哈希对不上）都**拒绝落地并要求全量**，
    /// 而不是猜着合并。屏幕状态错一帧，后面的局面分类就会全错。
    pub fn apply(&self, d: &ScreenDelta) -> ScreenDeltaAck {
        let mut map = self.0.lock();
        // 先把上一帧拆成所有权数据，避免后面 insert 时与不可变借用打架
        let previous: Option<(u64, u64, Vec<String>, String, u32)> = map.get(&d.console_id).map(|f| {
            (
                f.snap.frame_id,
                f.snap.window_start,
                f.snap.lines.clone(),
                f.snap.hash.clone(),
                f.snap.idle_frames,
            )
        });

        if !d.full {
            let Some((prev_frame, prev_start, _, _, _)) = previous.as_ref() else {
                return ScreenDeltaAck {
                    applied: false,
                    need_full: true,
                    changed: false,
                    frame_id: 0,
                };
            };
            // 乱序 / 重复：静默忽略即可。要求重发全量只会引发来回风暴
            if d.frame_id <= *prev_frame {
                return ScreenDeltaAck {
                    applied: true,
                    need_full: false,
                    changed: false,
                    frame_id: *prev_frame,
                };
            }
            if *prev_start != d.window_start {
                return ScreenDeltaAck {
                    applied: false,
                    need_full: true,
                    changed: false,
                    frame_id: *prev_frame,
                };
            }
        }

        let mut lines: Vec<String> = match (&previous, d.full) {
            (_, true) => vec![String::new(); (d.total as usize).min(MAX_LINES)],
            (Some((_, _, prev_lines, _, _)), false) => prev_lines.clone(),
            (None, false) => {
                return ScreenDeltaAck {
                    applied: false,
                    need_full: true,
                    changed: false,
                    frame_id: 0,
                }
            }
        };
        // 增量帧的行数变了（resize / 抓取窗口截断）同样对不上
        if !d.full && lines.len() != d.total as usize {
            return ScreenDeltaAck {
                applied: false,
                need_full: true,
                changed: false,
                frame_id: previous.map(|p| p.0).unwrap_or(0),
            };
        }
        for l in &d.lines {
            let i = l.y as usize;
            if i < lines.len() {
                lines[i] = l.text.clone();
            }
        }

        let computed = frame_hash(&lines, d.alt, d.cols, d.rows, d.cursor_x, d.cursor_y);
        if computed != d.hash {
            return ScreenDeltaAck {
                applied: false,
                need_full: true,
                changed: false,
                frame_id: previous.as_ref().map(|p| p.0).unwrap_or(0),
            };
        }

        let (changed, idle_frames, prev_frame) = match previous.as_ref() {
            Some((f, _, _, h, idle)) => (
                h != &computed,
                if h == &computed {
                    (*idle).saturating_add(1)
                } else {
                    0
                },
                *f,
            ),
            None => (true, 0, 0),
        };
        let _ = prev_frame;
        let snap = ScreenSnapshot {
            console_id: d.console_id.clone(),
            frame_id: d.frame_id,
            window_start: d.window_start,
            hash: computed,
            alt: d.alt,
            cols: d.cols,
            rows: d.rows,
            cursor_x: d.cursor_x,
            cursor_y: d.cursor_y,
            lines,
            updated_at: if d.ts_ms > 0 { d.ts_ms } else { now_ms() },
            idle_frames,
        };
        map.insert(d.console_id.clone(), Frame { snap });
        ScreenDeltaAck {
            applied: true,
            need_full: false,
            changed,
            frame_id: d.frame_id,
        }
    }

    pub fn get(&self, id: &str) -> Option<ScreenSnapshot> {
        self.0.lock().get(id).map(|f| f.snap.clone())
    }

    pub fn ids(&self) -> Vec<String> {
        self.0.lock().keys().cloned().collect()
    }

    /// 关闭标签时由 `cmd::local_close` / `cmd::ssh_close` 调用，避免帧无限堆积。
    pub fn drop(&self, id: &str) {
        self.0.lock().remove(id);
    }
}

/// 一个 console 的对外视图：活着没有、有没有屏幕状态、被谁托管。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleInfo {
    pub id: String,
    /// local | ssh
    pub kind: String,
    pub alive: bool,
    pub has_screen: bool,
    pub frame_id: u64,
    pub screen_hash: String,
    pub alt: bool,
    pub idle_frames: u32,
    pub updated_at: i64,
    pub contract_id: Option<String>,
    /// 有契约是 hosted，没有是 none。托管相位（运行中 / 等你回答 / 你在驾驶…）看 `host_list`
    pub status: String,
}

/// 托管契约。P0 只登记，不驱动循环；`status` 恒为 `observing`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StewardContract {
    pub id: String,
    pub console_id: String,
    /// P0 用 kind:id 占位；P2 换成 hostmem::host_key 的真实主机键
    pub host_key: String,
    pub goal: String,
    /// auto | notify | approve
    pub authority_tier: String,
    pub status: String,
    /// 预留给 P2 的意向栈；P0 恒为空
    pub intent_ids: Vec<String>,
    pub timebox_ms: u64,
    pub created_at: i64,
    /// 完成判据：由模型起草、用户确认（统一托管 §5）。旧数据没有该字段时为空
    #[serde(default)]
    pub success_criteria: Vec<super::host::Check>,
    /// YOLO：直接采用起草的判据、不经确认就开始托管。只跳过确认，不改授权档位与 deny
    #[serde(default)]
    pub yolo: bool,
    /// 判据经用户确认过（草稿卡上接受 / 编辑过）。确认过的 CommandOk 按普通托管写入过
    /// `gate::authorize` 执行；没确认过（YOLO）的只允许只读命令。旧数据没有该字段 → false（保守）
    #[serde(default)]
    pub criteria_confirmed: bool,
}

/// 新契约的 id。
pub fn new_contract_id(console_id: &str) -> String {
    format!("steward-{console_id}-{}", now_ms())
}

#[derive(Clone, Default)]
pub struct StewardRegistry(Arc<Mutex<HashMap<String, StewardContract>>>);

impl StewardRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 一个 console 同时只允许一份契约：sign 新的会顶掉旧的。
    pub fn start(
        &self,
        console_id: &str,
        kind: &str,
        goal: &str,
        tier: &str,
    ) -> StewardContract {
        self.start_with_id(&new_contract_id(console_id), console_id, kind, goal, tier)
    }

    /// 用事先定好的 id 签约：`host_start` 要先以 `host:{id}` 拿到租约，再让契约对守护进程可见。
    pub fn start_with_id(
        &self,
        id: &str,
        console_id: &str,
        kind: &str,
        goal: &str,
        tier: &str,
    ) -> StewardContract {
        let mut map = self.0.lock();
        map.retain(|_, c| c.console_id != console_id);
        let ts = now_ms();
        let contract = StewardContract {
            id: id.to_string(),
            console_id: console_id.to_string(),
            host_key: format!("{kind}:{console_id}"),
            goal: goal.to_string(),
            authority_tier: normalize_tier(tier),
            status: "observing".to_string(),
            intent_ids: Vec::new(),
            timebox_ms: 0,
            created_at: ts,
            success_criteria: Vec::new(), yolo: false, criteria_confirmed: false,
        };
        map.insert(contract.id.clone(), contract.clone());
        contract
    }

    /// 写入（用户确认过的）完成判据。
    pub fn set_criteria(&self, id: &str, checks: Vec<super::host::Check>, yolo: bool) -> bool {
        match self.0.lock().get_mut(id) {
            Some(c) => {
                c.success_criteria = checks;
                c.yolo = yolo;
                c.criteria_confirmed = !yolo;
                true
            }
            None => false,
        }
    }

        pub fn stop(&self, id: &str) -> bool {
        self.0.lock().remove(id).is_some()
    }

    /// P4：启动时从 Checkpoint 重建未完成契约。一个 console 仍只允许一份。
    pub fn restore(&self, c: StewardContract) {
        let mut map = self.0.lock();
        map.retain(|_, x| x.console_id != c.console_id);
        map.insert(c.id.clone(), c);
    }

    /// P2：把编译出来的意向栈挂到契约上，`intentIds` 不再是空数组。
    pub fn attach_intent(&self, contract_id: &str, intent_id: &str) {
        let mut map = self.0.lock();
        if let Some(c) = map.get_mut(contract_id) {
            if !c.intent_ids.contains(&intent_id.to_string()) {
                c.intent_ids.push(intent_id.to_string());
            }
        }
    }

    pub fn all(&self) -> Vec<StewardContract> {
        let mut v: Vec<_> = self.0.lock().values().cloned().collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        v
    }

    pub fn get(&self, id: &str) -> Option<StewardContract> {
        self.0.lock().get(id).cloned()
    }

    pub fn amend(
        &self,
        id: &str,
        goal: Option<String>,
        tier: Option<String>,
    ) -> Option<StewardContract> {
        let mut map = self.0.lock();
        let c = map.get_mut(id)?;
        if let Some(g) = goal {
            c.goal = g;
        }
        if let Some(t) = tier {
            c.authority_tier = normalize_tier(&t);
        }
        Some(c.clone())
    }

    pub fn for_console(&self, console_id: &str) -> Option<StewardContract> {
        self.0
            .lock()
            .values()
            .find(|c| c.console_id == console_id)
            .cloned()
    }
}

fn normalize_tier(t: &str) -> String {
    match t {
        "auto" | "approve" => t.to_string(),
        _ => "notify".to_string(),
    }
}

/// 把用户签契约时那句原话编译成意向栈 I0。
///
/// 设计 §3.1 写的是「一次 LLM 编译，结果可编辑」。P2 **有意改成确定性分句**：
/// 中文目标里「别… / 不要… / 必须… / 最小改动」这类分句就是约束，其余是目标主体。
/// 这样零模型调用、结果可复核，用户还能在面板里直接改；LLM 编译顺延到 P3，
/// 只在确定性规则抽不出约束时才兜底。
pub fn compile_intent_input(
    goal: &str,
    contract_id: &str,
    intent_id: Option<String>,
) -> CtxInput {
    const CONSTRAINT_HINTS: &[&str] = &[
        "别", "不要", "不改", "不得", "勿", "必须", "保持", "最小改动", "只改", "仅", "不要动",
    ];
    let mut constraints: Vec<String> = Vec::new();
    let mut main: Vec<String> = Vec::new();
    for seg in goal.split(['，', ',', '；', ';', '。', '\n']) {
        let s = seg.trim().to_string();
        if s.is_empty() {
            continue;
        }
        let is_constraint = s.chars().count() >= 3
            && CONSTRAINT_HINTS.iter().any(|k| s.contains(k));
        if is_constraint && constraints.len() < 8 {
            constraints.push(s);
        } else if !is_constraint && main.len() < 8 {
            main.push(s);
        }
    }
    let text = if main.is_empty() {
        goal.trim().to_string()
    } else {
        main.join("，")
    };
    CtxInput {
        id: intent_id,
        kind: KIND_INTENT.to_string(),
        text,
        strength: None,
        scope: None,
        source: Some("user".to_string()),
        evidence: Some("由托管目标确定性编译（P2 零 LLM，可在面板编辑）".to_string()),
        extra: Some(json!({
            "successCriteria": [],
            "constraints": constraints,
            "answerTemplate": null,
            "status": "active",
            "priority": 0
        })),
        contract_id: Some(contract_id.to_string()),
        position: Some(0),
    }
}

fn console_info(
    id: String,
    kind: &str,
    alive: bool,
    screen: &ScreenStore,
    registry: &StewardRegistry,
) -> ConsoleInfo {
    let snap = screen.get(&id);
    let contract = registry.for_console(&id);
    ConsoleInfo {
        id,
        kind: kind.to_string(),
        alive,
        has_screen: snap.is_some(),
        frame_id: snap.as_ref().map(|s| s.frame_id).unwrap_or(0),
        screen_hash: snap.as_ref().map(|s| s.hash.clone()).unwrap_or_default(),
        alt: snap.as_ref().map(|s| s.alt).unwrap_or(false),
        idle_frames: snap.as_ref().map(|s| s.idle_frames).unwrap_or(0),
        updated_at: snap.as_ref().map(|s| s.updated_at).unwrap_or(0),
        contract_id: contract.as_ref().map(|c| c.id.clone()),
        status: if contract.is_some() { "hosted" } else { "none" }.to_string(),
    }
}

/// 枚举当前所有活着的终端。本地 PTY 与 SSH 会话各有一张表，按 id 去重后合并；
/// 屏幕帧与契约状态一起附上，前端不必再拼三处数据。
#[tauri::command]
pub fn steward_list_consoles(
    state: State<'_, AppState>,
    screen: State<'_, ScreenStore>,
    registry: State<'_, StewardRegistry>,
) -> Vec<ConsoleInfo> {
    let mut ids: Vec<(String, &str)> = state
        .local
        .ids()
        .into_iter()
        .map(|id| (id, "local"))
        .collect();
    for id in state.sessions.ids() {
        if !ids.iter().any(|(existing, _)| existing == &id) {
            ids.push((id, "ssh"));
        }
    }
    // 只剩屏幕帧、PTY 已经关掉的会话也列出来，标 alive=false：
    // 重连 / 恢复托管时至少要能看见「这里曾经有个现场」，而不是凭空消失。
    for id in screen.ids() {
        if !ids.iter().any(|(existing, _)| existing == &id) {
            ids.push((id, "stale"));
        }
    }
    ids.sort_by(|a, b| a.0.cmp(&b.0));
    ids.into_iter()
        .map(|(id, kind)| console_info(id, kind, kind != "stale", &screen, &registry))
        .collect()
}

#[tauri::command]
pub fn steward_screen_delta(
    delta: ScreenDelta,
    screen: State<'_, ScreenStore>,
    bus: State<'_, StewardBus>,
) -> ScreenDeltaAck {
    let ack = screen.apply(&delta);
    // 事件驱动：帧一落地就唤醒常驻循环，不必等下一次心跳。
    // 前端静默期发的同哈希心跳帧也要唤醒，daemon 自己按自适应节奏决定要不要分类。
    bus.wake();
    ack
}

#[tauri::command]
pub fn steward_screen_get(
    console_id: String,
    screen: State<'_, ScreenStore>,
) -> Option<ScreenSnapshot> {
    screen.get(&console_id)
}

/// 签一份托管契约（`host_start` 用）。`id` 为 `None` 时现取一个。
#[allow(clippy::too_many_arguments)]
pub(crate) fn sign_contract(
    id: Option<&str>,
    console_id: &str,
    goal: &str,
    authority_tier: &str,
    state: &AppState,
    registry: &StewardRegistry,
    brain: &StewardBrain,
    bus: &StewardBus,
    core: &StewardCore,
) -> Result<StewardContract, String> {
    let console_id = console_id.to_string();
    let goal = goal.trim().to_string();
    if goal.is_empty() {
        return Err("托管目标不能为空".to_string());
    }
    let kind = if state.local.contains(&console_id) {
        "local"
    } else if state.sessions.contains(&console_id) {
        "ssh"
    } else {
        return Err(format!("没有 id 为「{console_id}」的终端"));
    };
    // 一个 console 只有一份契约。旧契约不能只被 registry 顶掉：它的意向栈与授权都得一起收尾。
    if let Some(old) = registry.for_console(&console_id) {
        stop_contract(registry, brain, &state.store, core, &old.id);
    }
    let contract = match id {
        Some(id) => registry.start_with_id(id, &console_id, kind, &goal, authority_tier),
        None => registry.start(&console_id, kind, &goal, authority_tier),
    };
    // P2：一句话目标 → 意向栈 I0，落库并挂到契约上。没有 I0 时决策编译器
    // 会 fail-closed 到「先补意向」，不会凭空编一个回复。
    let input = compile_intent_input(&goal, &contract.id, None);
    if let Ok(item) = brain.upsert(input) {
        if state.store.steward_ctx_upsert(&item.to_row()).is_ok() {
            registry.attach_intent(&contract.id, &item.id);
        }
    }
    // 登记授权契约：托管任务起步时按契约 id 读它
    core.set_authority(
        &contract.id,
        Authority::default().with_tier(&contract.authority_tier),
    );
    let live = registry.get(&contract.id).unwrap_or(contract);
    bus.wake();
    Ok(live)
}

/// 停止一份契约的全部收尾。用户点停止、签新契约顶掉旧的、关掉 tab 三条路径共用，
/// 任何一条漏掉一步都会留下「下次启动被误恢复」或「新契约继承旧状态」的问题。
pub(crate) fn stop_contract(
    registry: &StewardRegistry,
    brain: &StewardBrain,
    store: &crate::store::Store,
    core: &StewardCore,
    contract_id: &str,
) -> bool {
    let old = registry.get(contract_id);
    let stopped = registry.stop(contract_id);
    // 意向栈跟着契约一起归档；决策记录留着做审计，不删。
    brain.drop_intents(contract_id);
    store.steward_ctx_delete_intents(contract_id).ok();
    if old.is_some() {
        core.drop_contract(contract_id);
    }
    stopped
}

/// 标签关闭时调用：这个 console 上的契约随之结束。PTY / SSH 已经没了，契约留着只会
/// 在下次启动时被当成「未完成任务」恢复到一个不存在的终端上。
pub(crate) fn release_console(
    registry: &StewardRegistry,
    brain: &StewardBrain,
    store: &crate::store::Store,
    core: &StewardCore,
    console_id: &str,
) -> bool {
    match registry.for_console(console_id) {
        Some(c) => stop_contract(registry, brain, store, core, &c.id),
        None => false,
    }
}

#[tauri::command]
pub fn steward_stop(
    contract_id: String,
    registry: State<'_, StewardRegistry>,
    brain: State<'_, StewardBrain>,
    state: State<'_, AppState>,
    bus: State<'_, StewardBus>,
    core: State<'_, StewardCore>,
) -> bool {
    let stopped = stop_contract(&registry, &brain, &state.store, &core, &contract_id);
    if stopped {
        bus.wake();
    }
    stopped
}

#[tauri::command]
pub fn steward_status(registry: State<'_, StewardRegistry>) -> Vec<StewardContract> {
    registry.all()
}

#[tauri::command]
pub fn steward_amend(
    contract_id: String,
    goal: Option<String>,
    authority_tier: Option<String>,
    registry: State<'_, StewardRegistry>,
    brain: State<'_, StewardBrain>,
    state: State<'_, AppState>,
    bus: State<'_, StewardBus>,
    core: State<'_, StewardCore>,
) -> Option<StewardContract> {
    // 改目标是热更新：不重启循环，只替换 I0 的文本与约束（设计 §3.1）。
    // 沿用已有的意向 id，保证 intentIds 不漂移。
    let amended = registry.amend(&contract_id, goal.clone(), authority_tier.clone());
    // P3：档位热更新同步到授权契约，PolicyEngine 下一次决策立刻生效。
    if amended.is_some() {
        if let Some(tier) = authority_tier.as_deref() {
            let auth = core.authority(&contract_id).with_tier(tier);
            core.set_authority(&contract_id, auth);
        }
    }
    if let (Some(new_goal), Some(c)) = (goal.as_deref(), amended.as_ref()) {
        let intent_id = c.intent_ids.first().cloned();
        let input = compile_intent_input(new_goal, &contract_id, intent_id);
        if let Ok(item) = brain.upsert(input) {
            state.store.steward_ctx_upsert(&item.to_row()).ok();
        }
    }
    if amended.is_some() {
        bus.wake();
    }
    amended
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(y: u32, text: &str) -> ScreenLine {
        ScreenLine {
            y,
            text: text.to_string(),
        }
    }

    fn frame(lines: &[&str], alt: bool, cols: u16, rows: u16, cx: u16, cy: u16) -> String {
        frame_hash(
            &lines.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            alt,
            cols,
            rows,
            cx,
            cy,
        )
    }

    /// 跨语言校验向量：`hello\n#0:80:24:0:0` 的 FNV-1a。
    /// 前端 `tests/fe/steward.test.ts` 用同一个值，改算法时两边一起红。
    #[test]
    fn hash_matches_shared_vector() {
        assert_eq!(frame(&["hello"], false, 80, 24, 0, 0), "ee7ae512");
        assert_eq!(frame(&["hello", "world"], true, 120, 40, 5, 9), "16952930");
    }

    fn full_delta(lines: &[&str], frame_id: u64, window_start: u64) -> ScreenDelta {
        let hash = frame(lines, false, 80, 24, 0, 0);
        ScreenDelta {
            console_id: "tab-1".to_string(),
            frame_id,
            window_start,
            total: lines.len() as u32,
            cols: 80,
            rows: 24,
            alt: false,
            cursor_x: 0,
            cursor_y: 0,
            hash,
            full: true,
            lines: lines
                .iter()
                .enumerate()
                .map(|(i, s)| line(i as u32, s))
                .collect(),
            ts_ms: 1,
        }
    }

    #[test]
    fn full_frame_lands_and_is_queryable() {
        let store = ScreenStore::new();
        let ack = store.apply(&full_delta(&["$ pwd", "/tmp"], 1, 0));
        assert!(ack.applied && !ack.need_full && ack.changed);
        let snap = store.get("tab-1").expect("帧应落地");
        assert_eq!(snap.lines, vec!["$ pwd", "/tmp"]);
        assert_eq!(snap.frame_id, 1);
        assert_eq!(snap.idle_frames, 0);
    }

    #[test]
    fn incremental_frame_only_patches_changed_lines() {
        let store = ScreenStore::new();
        store.apply(&full_delta(&["$ pwd", "/tmp"], 1, 0));
        let lines = vec!["$ pwd", "/var"];
        let mut d = full_delta(&[], 2, 0);
        d.full = false;
        d.total = 2;
        d.hash = frame(&lines, false, 80, 24, 0, 0);
        d.lines = vec![line(1, "/var")];
        let ack = store.apply(&d);
        assert!(ack.applied && !ack.need_full && ack.changed);
        assert_eq!(store.get("tab-1").unwrap().lines, vec!["$ pwd", "/var"]);
    }

    #[test]
    fn identical_frame_is_not_a_change_but_counts_idle() {
        let store = ScreenStore::new();
        let first = full_delta(&["$ pwd"], 1, 0);
        store.apply(&first);
        let mut second = first.clone();
        second.frame_id = 2;
        let ack = store.apply(&second);
        assert!(ack.applied && !ack.changed);
        assert_eq!(store.get("tab-1").unwrap().idle_frames, 1);
    }

    #[test]
    fn window_slide_or_hash_drift_asks_for_full_frame() {
        let store = ScreenStore::new();
        store.apply(&full_delta(&["a", "b"], 1, 0));
        let mut d = full_delta(&[], 2, 7);
        d.full = false;
        d.total = 2;
        d.lines = vec![line(1, "c")];
        let ack = store.apply(&d);
        assert!(!ack.applied && ack.need_full);
        assert_eq!(store.get("tab-1").unwrap().frame_id, 1, "错误帧不得落地");

        let mut drift = full_delta(&[], 2, 0);
        drift.full = false;
        drift.total = 2;
        drift.hash = "deadbeef".to_string();
        assert!(store.apply(&drift).need_full);
    }

    #[test]
    fn out_of_order_frame_is_ignored_without_full_resend() {
        let store = ScreenStore::new();
        store.apply(&full_delta(&["a"], 5, 0));
        let mut old = full_delta(&[], 4, 0);
        old.full = false;
        old.total = 1;
        let ack = store.apply(&old);
        assert!(ack.applied && !ack.need_full && !ack.changed);
        assert_eq!(ack.frame_id, 5);
    }

    #[test]
    fn incremental_without_baseline_requires_full() {
        let store = ScreenStore::new();
        let mut d = full_delta(&["a"], 1, 0);
        d.full = false;
        let ack = store.apply(&d);
        assert!(!ack.applied && ack.need_full);
    }

    #[test]
    fn one_contract_per_console_and_amend_keeps_id() {
        let reg = StewardRegistry::new();
        let a = reg.start("tab-1", "local", "目标 A", "auto");
        assert_eq!(a.authority_tier, "auto");
        assert_eq!(a.status, "observing");
        let b = reg.start("tab-1", "local", "目标 B", "weird");
        assert_eq!(b.authority_tier, "notify", "未知档位一律落到 notify");
        assert_eq!(reg.all().len(), 1, "同一 console 只保留一份契约");
        let amended = reg.amend(&b.id, Some("目标 C".to_string()), None).unwrap();
        assert_eq!(amended.id, b.id);
        assert_eq!(amended.goal, "目标 C");
        assert!(reg.stop(&b.id));
        assert!(reg.all().is_empty());
    }
}
