//! 回放夹具：真 Tauri 应用（无窗口）+ 真本地 PTY + 帧泵 + 假模型 + 事件录制。见 `e2e/mod.rs` 的说明。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine;
use parking_lot::Mutex;
use serde_json::Value;
use tauri::{AppHandle, EventId, Listener, Manager};

use super::fake_llm::{FakeLlm, Rule, Turn};
use super::vt::Vt;
use crate::agent::host::{CardReply, HostPhase, HostShared};
use crate::agent::steward::{self, ScreenDelta, ScreenLine, ScreenStore};
use crate::cmd::AppState;

pub const COLS: usize = 100;
pub const ROWS: usize = 30;
/// 帧泵心跳：前端是 2s 一帧 + 变化即推；这里更勤一些，免得「帧不新鲜」退回旁路缓冲
const HEARTBEAT: Duration = Duration::from_millis(250);

/// 场景串行跑：整个进程只能有一个 tao 事件循环（= 一个 App），配置 / 设置表是全局的。
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
static SEQ: AtomicU64 = AtomicU64::new(0);

fn root_dir() -> PathBuf {
    std::env::temp_dir().join(format!("nexus-e2e-{}", std::process::id()))
}

/// 有显示就用；没有就起一个 Xvfb（`-terminate`：最后一个客户端 —— 测试进程 —— 断开后自行退出）。
/// 两样都没有 → false，场景跳过。
fn ensure_display() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        let has = |k: &str| std::env::var(k).is_ok_and(|v| !v.trim().is_empty());
        if has("DISPLAY") || has("WAYLAND_DISPLAY") {
            return true;
        }
        for n in 90..140u32 {
            let sock = format!("/tmp/.X11-unix/X{n}");
            if Path::new(&sock).exists() || Path::new(&format!("/tmp/.X{n}-lock")).exists() {
                continue;
            }
            let child = Command::new("Xvfb")
                .args([&format!(":{n}"), "-nolisten", "tcp", "-terminate", "-screen", "0", "1024x768x24"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            let Ok(child) = child else {
                eprintln!("[e2e] 没有 DISPLAY 也没有 Xvfb，托管回放场景跳过");
                return false;
            };
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(5) {
                if Path::new(&sock).exists() {
                    std::mem::forget(child);
                    std::env::set_var("DISPLAY", format!(":{n}"));
                    return true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        eprintln!("[e2e] Xvfb 起不来，托管回放场景跳过");
        false
    })
}

/// 进程内唯一的应用：`lib.rs::run` 的 setup 照抄一份（不建窗口、不注册命令 / 插件），
/// 事件循环在后台线程里跑（`any_thread`），AppHandle 交给各场景共用。
fn app() -> Option<AppHandle> {
    static APP: OnceLock<Option<AppHandle>> = OnceLock::new();
    APP.get_or_init(|| {
        if !ensure_display() {
            return None;
        }
        let root = root_dir();
        let _ = std::fs::create_dir_all(&root);
        // 不碰真实的应用数据目录（误读样本 / trace 都按 app_data_dir 落盘）
        std::env::set_var("XDG_DATA_HOME", root.join("xdg"));
        // 假模型在 127.0.0.1：别让代理环境变量把它绕出去
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("no_proxy", "127.0.0.1,localhost");
        if std::env::var("SHELL").map_or(true, |s| s.trim().is_empty()) {
            std::env::set_var("SHELL", "/bin/bash");
        }
        let data = root.join("data");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("e2e-tauri".into())
            .spawn(move || {
                let built = tauri::Builder::<tauri::Wry>::default()
                    .any_thread()
                    .build(tauri::test::mock_context(tauri::test::noop_assets()));
                let app = match built {
                    Ok(a) => a,
                    Err(e) => {
                        eprintln!("[e2e] Tauri 应用起不来：{e}");
                        let _ = tx.send(None);
                        return;
                    }
                };
                manage_like_lib_rs(app.handle(), data);
                let _ = tx.send(Some(app.handle().clone()));
                app.run(|_, _| {});
            })
            .ok()?;
        rx.recv_timeout(Duration::from_secs(30)).ok().flatten()
    })
    .clone()
}

fn manage_like_lib_rs(app: &AppHandle, dir: PathBuf) {
    use crate::agent;
    let keys_dir = dir.join(crate::keys::VAULT_DIR);
    let store = crate::store::Store::open(dir).expect("e2e：SQLite 初始化失败");
    let sessions = crate::ssh::session::SessionManager::new();
    let auth_prompts = sessions.auth_prompts();
    let sftp = crate::sftp::SftpManager::new(sessions.known_hosts(), auth_prompts.clone(), sessions.shared());
    let mgr = agent::AgentManager::new();
    let screen = agent::steward::ScreenStore::new();
    let registry = agent::steward::StewardRegistry::new();
    let events = agent::steward_loop::EventLog::new();
    let bus = agent::steward_loop::StewardBus::default();
    let runtime = agent::steward_loop::StewardRuntime::new();
    let config = agent::steward_loop::StewardConfigStore::new();
    let brain = agent::profile::StewardBrain::load(&store);
    // 系统通知 / 任务栏闪烁在无窗口的测试应用里没有意义（也没注册通知插件）
    let _ = store.set_setting("host_notify", r#"{"system":false,"taskbar":false,"webhook":""}"#);
    app.manage(crate::viewer::DocStore::default());
    app.manage(screen.clone());
    app.manage(registry.clone());
    app.manage(events.clone());
    app.manage(bus.clone());
    app.manage(runtime.clone());
    app.manage(config.clone());
    app.manage(brain);
    app.manage(agent::steward_core::StewardCore::new());
    app.manage(AppState {
        sessions,
        sftp,
        store,
        local: crate::local::LocalManager::new(),
        agent: mgr,
        llm: agent::llm::LlmRuntime::new(),
        act: agent::act::ActRuntime::new(),
        auth_prompts,
        keys_dir,
    });
    // 常驻观察循环也照真实应用起上（它只读屏幕，但和托管任务共用 ScreenStore / 契约表）
    agent::steward_loop::spawn_daemon(app.clone(), screen, registry, events, bus, runtime, config);
}

/// 录下来的一条事件。
#[derive(Debug, Clone)]
pub struct Captured {
    /// 距场景开始多久收到的
    pub at: Duration,
    pub channel: String,
    pub payload: Value,
}

/// 一个场景：独占应用、一个本地 bash（cwd = 临时目录）、一个假模型。
pub struct Harness {
    pub app: AppHandle,
    pub console: String,
    pub dir: PathBuf,
    pub llm: FakeLlm,
    pub contract: Option<String>,
    vt: Arc<Mutex<Vt>>,
    events: Arc<Mutex<Vec<Captured>>>,
    listeners: Vec<EventId>,
    stop: Arc<AtomicBool>,
    /// 多终端场景里另起的终端（[`add_console`](Self::add_console)）
    extras: Vec<Rig>,
    _serial: std::sync::MutexGuard<'static, ()>,
}

/// 一个真 bash + 它自己的模拟器与帧泵（多终端场景的其余终端）。
struct Rig {
    id: String,
    dir: PathBuf,
    vt: Arc<Mutex<Vt>>,
    listener: EventId,
    stop: Arc<AtomicBool>,
}

/// 帧泵：PTY 字节 → 模拟器 → 全量帧，外加心跳帧。返回 (监听, 停止旗)。
fn pump(app: &AppHandle, console: &str, vt: &Arc<Mutex<Vt>>) -> (EventId, Arc<AtomicBool>) {
    let frame_id = Arc::new(AtomicU64::new(1));
    let listener = {
        let (vt, fid, a, c) = (Arc::clone(vt), Arc::clone(&frame_id), app.clone(), console.to_string());
        app.listen_any(format!("nx://local/data/{console}"), move |e| {
            let raw: String = serde_json::from_str(e.payload()).unwrap_or_default();
            let bytes = base64::engine::general_purpose::STANDARD.decode(raw).unwrap_or_default();
            let mut g = vt.lock();
            g.feed(&bytes);
            push_frame(&a, &c, &g, fid.fetch_add(1, Ordering::Relaxed));
        })
    };
    let stop = Arc::new(AtomicBool::new(false));
    {
        let (vt, fid, a, c, stop) = (Arc::clone(vt), frame_id, app.clone(), console.to_string(), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(HEARTBEAT);
                let g = vt.lock();
                push_frame(&a, &c, &g, fid.fetch_add(1, Ordering::Relaxed));
            }
        });
    }
    (listener, stop)
}

/// 起本地 bash，固定提示符、进 `dir`、清屏，等提示符那一帧落进 ScreenStore。
fn boot_shell(app: &AppHandle, console: &str, dir: &Path, vt: &Arc<Mutex<Vt>>) {
    let state = app.state::<AppState>();
    state.local.spawn(console, COLS as u16, ROWS as u16, app).expect("起本地 shell");
    let cmd = format!(" unset HISTFILE PROMPT_COMMAND; export PS1='\\u@e2e:\\w\\$ '; cd '{}'; clear\n", dir.display());
    state.local.write(console, cmd.as_bytes()).expect("写 PTY");
    let t0 = Instant::now();
    while !vt.lock().text().lines().next().is_some_and(|l| l.contains("@e2e:")) {
        assert!(t0.elapsed() < Duration::from_secs(10), "等提示符超时，当前屏幕：\n{}", vt.lock().text());
        std::thread::sleep(Duration::from_millis(30));
    }
    // 清屏后再等一个心跳，让提示符那一帧落进 ScreenStore。
    // 旁路缓冲**不清**：nx_exec 拿它的最后一行当「提示符基线」判命令结束（清了就只剩静默判定）
    std::thread::sleep(HEARTBEAT * 2);
}

/// 把模拟器当前这一屏作为全量帧推进 `ScreenStore`（等同前端 `steward_screen_push`）。
fn push_frame(app: &AppHandle, console: &str, vt: &Vt, frame_id: u64) {
    let lines = vt.lines();
    let (cx, cy) = (vt.cx.min(COLS - 1) as u16, vt.cy as u16);
    let hash = steward::frame_hash(&lines, vt.alt, COLS as u16, ROWS as u16, cx, cy);
    let d = ScreenDelta {
        console_id: console.to_string(),
        frame_id,
        window_start: vt.scrolled,
        total: ROWS as u32,
        cols: COLS as u16,
        rows: ROWS as u16,
        alt: vt.alt,
        cursor_x: cx,
        cursor_y: cy,
        app_cursor: vt.app_cursor,
        hash,
        full: true,
        lines: lines.into_iter().enumerate().map(|(y, text)| ScreenLine { y: y as u32, text }).collect(),
        ts_ms: steward::now_ms(),
    };
    if let Some(s) = app.try_state::<ScreenStore>() {
        s.apply(&d);
    }
}

impl Harness {
    /// 起场景。没有可用的显示环境时返回 None（调用方打印跳过并 return）。
    pub fn new(turns: Vec<Turn>, rules: Vec<Rule>, criteria: Value) -> Option<Self> {
        let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let app = app()?;
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let console = format!("e2e-{n}");
        let dir = root_dir().join(format!("work-{n}"));
        std::fs::create_dir_all(&dir).expect("建工作目录");
        let dir = dir.canonicalize().unwrap();
        let criteria = substitute(&criteria, &dir);
        let llm = FakeLlm::start(turns.into_iter().map(|t| t.with_dir(&dir)).collect(), rules, criteria);

        let state = app.state::<AppState>();
        // 上一场景的运行摘要 / 模型历史别漏进来（托管的会话 key 为空 = 全局作用域）
        let _ = state.store.agent_history_clear();
        let _ = state.store.agent_llm_clear();
        let mut cfg = crate::agent::AgentConfig::default();
        cfg.enabled = true;
        cfg.accounts[0].provider = "openai".into();
        cfg.accounts[0].base_url = llm.base_url();
        cfg.accounts[0].model = "e2e-fake".into();
        cfg.accounts[0].api_key = "sk-e2e".into();
        crate::agent::save_config(&state.store, &cfg).expect("写 Agent 配置");
        state.agent.apply_config(&cfg);

        let vt = Arc::new(Mutex::new(Vt::new(COLS, ROWS)));
        let events: Arc<Mutex<Vec<Captured>>> = Arc::default();
        let t0 = Instant::now();
        let mut listeners = Vec::new();
        let (pl, stop) = pump(&app, &console, &vt);
        listeners.push(pl);
        for ch in [format!("nx://agent/step/{console}"), crate::agent::host_cmd::HOST_ALL_EVENT.to_string()] {
            let (ev, chan) = (Arc::clone(&events), ch.clone());
            listeners.push(app.listen_any(ch, move |e| {
                let payload = serde_json::from_str(e.payload()).unwrap_or(Value::Null);
                ev.lock().push(Captured { at: t0.elapsed(), channel: chan.clone(), payload });
            }));
        }
        boot_shell(&app, &console, &dir, &vt);
        let h = Self {
            app: app.clone(),
            console,
            dir,
            llm,
            contract: None,
            vt,
            events,
            listeners,
            stop,
            extras: Vec::new(),
            _serial: serial,
        };
        Some(h)
    }

    /// 多终端场景：再起一个本地 bash（cwd = 工作目录下的子目录 `name`），返回它的 console id。
    pub fn add_console(&mut self, name: &str) -> String {
        let id = format!("{}-{name}", self.console);
        let dir = self.dir.join(name);
        std::fs::create_dir_all(&dir).expect("建子目录");
        let vt = Arc::new(Mutex::new(Vt::new(COLS, ROWS)));
        let (listener, stop) = pump(&self.app, &id, &vt);
        boot_shell(&self.app, &id, &dir, &vt);
        self.extras.push(Rig { id: id.clone(), dir, vt, listener, stop });
        id
    }

    fn rig(&self, id: &str) -> &Rig {
        self.extras.iter().find(|r| r.id == id).unwrap_or_else(|| panic!("没有终端 {id}"))
    }

    /// 终端的工作目录（主终端 = `dir`）。
    pub fn dir_of(&self, id: &str) -> PathBuf {
        if id == self.console {
            return self.dir.clone();
        }
        self.rig(id).dir.clone()
    }

    /// 终端的当前屏（主终端或其余终端之一）。
    pub fn screen_of(&self, id: &str) -> String {
        if id == self.console {
            return self.screen();
        }
        self.rig(id).vt.lock().text()
    }

    /// 绕过托管直接往某个终端的 PTY 写。
    pub fn type_raw_on(&self, id: &str, s: &str) {
        self.state().local.write(id, s.as_bytes()).expect("写 PTY");
    }

    /// 同 [`paint`](Self::paint)，画在终端 `id` 上（主终端或其余终端之一）。
    pub fn paint_on(&self, id: &str, lines: &[String]) {
        let fixture = self.dir_of(id).join(".nx-screen");
        std::fs::write(&fixture, lines.join("\n")).expect("写屏幕夹具");
        let last = lines.last().cloned().unwrap_or_default();
        self.type_raw_on(id, " clear; printf '%s' \"$(cat .nx-screen)\"; read -r _nx_answer\n");
        let t0 = Instant::now();
        loop {
            let s = self.screen_of(id);
            if !s.contains("_nx_answer") && s.lines().any(|l| l.trim_end() == last.trim_end()) {
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "等屏幕超时，当前屏幕：\n{s}");
            std::thread::sleep(Duration::from_millis(30));
        }
        std::thread::sleep(HEARTBEAT * 2);
    }

    pub fn state(&self) -> tauri::State<'_, AppState> {
        self.app.state::<AppState>()
    }

    /// 绕过托管直接往 PTY 写（布置现场 / 模拟用户敲键用）。
    pub fn type_raw(&self, s: &str) {
        self.state().local.write(&self.console, s.as_bytes()).expect("写 PTY");
    }

    pub fn screen(&self) -> String {
        self.vt.lock().text()
    }

    /// 把一帧屏幕（逐行文本，如误读样本的 `screen.lines`）原样「画」到真终端上，光标停在最后一行行尾，
    /// 终端此刻在 `read` 里等输入 —— 和真程序停在提示符上一样。模型随后发的按键进 `read`，回车后 shell 提示符回来。
    /// 只是**文本**还原：颜色 / 光标位置 / alt screen 不还原（见 `vt.rs` 的局限）。
    pub fn paint(&self, lines: &[String]) {
        self.paint_on(&self.console, lines);
    }

    pub fn wait_screen(&self, timeout: Duration, cond: impl Fn(&str) -> bool) {
        let t0 = Instant::now();
        while !cond(&self.screen()) {
            assert!(t0.elapsed() < timeout, "等屏幕超时，当前屏幕：\n{}", self.screen());
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    /// 走真实入口 `host_cmd::host_start`（YOLO：判据草稿直接采用，不弹确认卡）。
    pub fn start(&mut self, goal: &str, tier: &str) -> String {
        self.start_with(goal, tier, &[])
    }

    /// 多终端托管：主终端 = 本场景的 console，`extra` = [`add_console`](Self::add_console) 起的其余终端。
    pub fn start_with(&mut self, goal: &str, tier: &str, extra: &[&str]) -> String {
        let app = self.app.clone();
        let extra = (!extra.is_empty()).then(|| {
            extra
                .iter()
                .map(|id| crate::agent::host_cmd::ConsoleHint {
                    console_id: id.to_string(),
                    label: Some(format!("e2e-{id}")),
                    ..Default::default()
                })
                .collect::<Vec<_>>()
        });
        let fut = crate::agent::host_cmd::host_start(
            self.console.clone(),
            goal.to_string(),
            Some(tier.to_string()),
            Some("e2e".to_string()),
            None,
            None,
            Some(false),
            Some(true),
            None,
            None,
            None,
            extra,
            app.clone(),
            app.state(),
            app.state(),
            app.state(),
            app.state(),
            app.state(),
        );
        let id = tauri::async_runtime::block_on(fut).expect("host_start");
        self.contract = Some(id.clone());
        id
    }

    pub fn host(&self) -> Arc<HostShared> {
        let id = self.contract.as_deref().expect("还没 start");
        self.state().act.host.get(id).expect("托管任务在登记表里")
    }

    pub fn phase(&self) -> HostPhase {
        self.host().phase()
    }

    /// 等相位满足条件；超时 panic 并打出现场（屏幕 + 步骤 + 模型看到的反馈）。
    pub fn wait_phase(&self, timeout: Duration, what: &str, cond: impl Fn(&HostPhase) -> bool) -> HostPhase {
        let t0 = Instant::now();
        loop {
            let p = self.phase();
            if cond(&p) {
                return p;
            }
            if t0.elapsed() > timeout {
                panic!("等「{what}」超时（{timeout:?}），当前相位 {p:?}\n{}", self.dump());
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    /// 通用等待：条件满足就返回，超时 panic 并打出现场。
    pub fn wait_until(&self, timeout: Duration, what: &str, cond: impl Fn(&Self) -> bool) {
        let t0 = Instant::now();
        while !cond(self) {
            if t0.elapsed() > timeout {
                panic!("等「{what}」超时（{timeout:?}）\n{}", self.dump());
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    pub fn wait_terminal(&self, timeout: Duration) -> HostPhase {
        self.wait_phase(timeout, "终态", |p| p.is_terminal())
    }

    /// 答复当前等人的卡（走 `host_cmd::host_answer`，与面板点按钮同一条路）。
    pub fn answer(&self, text: &str) {
        let card = match self.phase().base() {
            HostPhase::WaitingHuman { card } => card.id.clone(),
            p => panic!("没有等人的卡：{p:?}"),
        };
        let reply = CardReply { card_id: card, text: text.to_string(), approved: None };
        let fut = crate::agent::host_cmd::host_answer(
            self.contract.clone().unwrap(),
            reply,
            self.app.clone(),
            self.app.state(),
        );
        tauri::async_runtime::block_on(fut).expect("host_answer");
    }

    pub fn events(&self) -> Vec<Captured> {
        self.events.lock().clone()
    }

    /// 发给面板的步骤（`nx://agent/step/{console}`）。
    pub fn steps(&self) -> Vec<Value> {
        let ch = format!("nx://agent/step/{}", self.console);
        self.events().into_iter().filter(|e| e.channel == ch).map(|e| e.payload).collect()
    }

    pub fn steps_of(&self, kind: &str) -> Vec<Value> {
        self.steps().into_iter().filter(|s| s["kind"] == kind).collect()
    }

    /// 本任务推过的托管状态（`nx://host/all` 里属于本 console 的）。
    pub fn statuses(&self) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|e| e.channel == crate::agent::host_cmd::HOST_ALL_EVENT && e.payload["consoleId"] == self.console.as_str())
            .map(|e| e.payload)
            .collect()
    }

    /// 剧本断言全部通过。
    pub fn assert_script_ok(&self) {
        let m = self.llm.mismatches();
        assert!(m.is_empty(), "剧本断言失败：\n{}\n{}", m.join("\n"), self.dump());
    }

    pub fn dump(&self) -> String {
        let ch = format!("nx://agent/step/{}", self.console);
        let steps: Vec<String> = self
            .events()
            .iter()
            .filter(|e| e.channel == ch)
            .map(|e| {
                let s = &e.payload;
                let pick = |k: &str| s[k].as_str().map(|v| v.chars().take(300).collect::<String>()).unwrap_or_default();
                format!("  {:>6.2}s [{}] {} {} {}", e.at.as_secs_f64(), pick("kind"), pick("tool"), pick("command"), pick("text"))
            })
            .collect();
        format!(
            "—— 屏幕 ——\n{}\n—— 步骤 ——\n{}\n{}",
            self.screen(),
            steps.join("\n"),
            self.llm.transcript()
        )
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(id) = self.contract.clone() {
            let still = self.state().act.host.get(&id).is_some_and(|h| !h.is_terminal());
            if still {
                let fut = crate::agent::host_cmd::host_stop(id, Some(true), self.app.clone(), self.app.state());
                let _ = tauri::async_runtime::block_on(fut);
            }
        }
        self.stop.store(true, Ordering::Relaxed);
        for l in self.listeners.drain(..) {
            self.app.unlisten(l);
        }
        let _ = self.state().local.close(&self.console);
        for r in self.extras.drain(..) {
            r.stop.store(true, Ordering::Relaxed);
            self.app.unlisten(r.listener);
            let _ = self.app.state::<AppState>().local.close(&r.id);
            if let Some(s) = self.app.try_state::<ScreenStore>() {
                s.drop(&r.id);
            }
        }
        if let Some(s) = self.app.try_state::<ScreenStore>() {
            s.drop(&self.console);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 误读样本（应用数据目录 `misreads/*.json`，格式见 `host::misread::MisreadReport`）里的那一屏。
pub fn misread_lines(json: &str) -> Vec<String> {
    let r: crate::agent::host::misread::MisreadReport = serde_json::from_str(json).expect("不是误读样本 JSON");
    r.evidence.screen.lines
}

/// 剧本 / 判据里的 `{dir}` 换成本场景的工作目录。
pub fn substitute(v: &Value, dir: &Path) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace("{dir}", &dir.display().to_string())),
        Value::Array(a) => Value::Array(a.iter().map(|x| substitute(x, dir)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), substitute(x, dir))).collect()),
        other => other.clone(),
    }
}

impl Turn {
    fn with_dir(self, dir: &Path) -> Self {
        use super::fake_llm::Reply;
        let reply = match self.reply {
            Reply::Tools(calls) => Reply::Tools(calls.into_iter().map(|(n, a)| (n, substitute(&a, dir))).collect()),
            r => r,
        };
        Turn { expect: self.expect, reply }
    }
}
