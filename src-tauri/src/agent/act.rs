//! Nexus AI Agent 的动作层：让模型不只是「读」终端，而是能真的「动手」。
//!
//! `llm.rs` 解决「怎么把答案一个字一个字推回来」，`executor.rs` 解决
//! 「一条命令怎么算跑完」，这一层解决的是中间那段编排：
//!
//! ```text
//! 提问 → 模型决定做点什么 → 真的去做 → 把终端回了什么续喂回去 → 再决定 → … → 给结论
//! ```
//!
//! ## 动作协议：工具调用优先，围栏兜底
//!
//! **首选路径是原生 function calling**。请求里带上工具声明，模型要动手就发
//! `tool_calls`，名字与参数都由 JSON Schema 约束 —— 它没法凭空编出一个
//! `new-tab`，也不能把界面动作当成 shell 命令。
//!
//! 围栏代码块是**兜底**，两条理由：
//!
//! 1. 大量自建网关/兼容层只转发 messages，收到 `tools` 直接 400。
//!    `llm::complete_reply` 会识别并自动降级，本层随即改用围栏解析。
//! 2. 部分模型即便给了工具，仍习惯把 JSON 写进代码块。围栏解析照接不误。
//!
//! ````text
//! ```nx-exec
//! {"target":"<会话 id>","command":"df -h"}
//! ```
//! ````
//!
//! 一个块里也可以给 `targets` + `commands` 两个数组，一次对多台机器下发同一批命令。
//! 不写 `target` 就默认落到「当前会话」。
//!
//! ## 伪命令护栏
//!
//! 模型在终端场景下有个顽固的先验：「想做什么就写个命令」。于是它会写出
//! `new-tab`、`reconnect` 这种**根本不存在的命令** —— 敲进终端只是换来一句
//! `command not found`，还白白浪费一轮。
//!
//! 这些是界面动作。本层在**下发到终端之前**把它们拦下来，转成一张说明卡片 +
//! 一句纠正喂回模型，而不是等 shell 报错再说。
//!
//! ## 三条不可妥协的约束
//!
//! 1. **命令必须是单行的**。换行意味着「一次注入一串互不相干的操作」，
//!    那样既没法逐条审批，也彻底失去了「哪一步炸了」的可追溯性。要分几步请拆成几轮。
//! 2. **人在回路是默认值**（`ExecMode::Approval`）：被 `danger::assess` 判为危险的
//!    命令先弹 UI 等人点头。审批有超时 —— 人去接杯咖啡回来发现流程卡死，
//!    比「超时拒绝」糟糕得多。
//! 3. **任何一步失败都不能让整轮蒸发**。执行报错会当成一条反馈续喂给模型，
//!    让它自己决定重试、换命令还是收手；命令的真实输出原样喂回去 ——
//!    没有退出码这种参考信息，模型得照着输出文本自己判断成败，这与人操作时一致。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use super::capture;
use super::danger::assess;
use super::executor::{
    wait_for_end, ExecEnd, ExecOutcome, WaitConfig, CLASSIFY_TAIL_BYTES,
};
use super::interact::{self, Pattern, WaitEnd, WaitSpec};
use super::llm::{self, ChatMessage, CONTEXT_BEGIN, CONTEXT_END};
use super::prompt::{self, PromptGuess};
use super::secret::{self, SecretAnswer};
use super::trace;
use super::{AgentConfig, AgentError};
use crate::agent::StreamKind;
use crate::cmd::AppState;
use crate::ssh::session::{AuthMethod, ConnectRequest};
use crate::store;

/// 一次 run 最多允许多少轮「思考 → 动作 → 再思考」。
///
/// 取 8：真正在排查问题时通常 3~5 步就收敛了；再多说明模型在打转，
/// 让它继续跑只会烧钱，不如停下来把现状交给人。
const MAX_STEPS: usize = 8;

/// 单一步骤里最多允许多少个动作（防止模型一口气吐出五十条命令）。
const MAX_ACTIONS: usize = 12;

/// 动作块解析不出来时，最多再让模型改几次。
/// 一次足够它纠正格式；再多就是拿步数上限当儿戏了。
const MAX_MISFIRE_RETRY: usize = 1;

/// 提示词里最多列出多少条已保存的主机。
///
/// 这清单的作用是让模型能挑出 `sessionRecordId`，不是让它背通讯录。
/// 保存上百条的人并不罕见，全列出去纯属浪费上下文预算。
const MAX_SAVED_HOST_HINTS: usize = 40;

/// 人工审批的最长等待。超时按「拒绝」处理。
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// 喂回给模型的单条执行反馈的字符上限。
///
/// 一条 `tail -f` 或 `dmesg` 轻松上千行，全都续喂回去第二轮就爆上下文了。
/// 保留头部（最有价值的通常在前面）而不是尾部。
const MAX_FEEDBACK_CHARS: usize = 6000;

// ——————————————————————————————— 协议类型 ———————————————————————————————

/// 一个可操作目标：**会话 id + 给人看的名字**。
///
/// 名字由前端给：后端只知道一串 uuid，把 uuid 摆给人看毫无意义。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTarget {
    pub id: String,
    pub label: String,
}

/// 一次 Agentic 运行的请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunRequest {
    /// 面板当前绑定的会话（同时是省略 target 时的默认目标）
    pub session_id: String,
    pub question: String,
    /// 允许操作的会话。为空时退回 `[session_id]`。
    #[serde(default)]
    pub targets: Vec<AgentTarget>,
    /// 前端从 xterm 缓冲区抓的当前屏幕快照（含 scrollback）。
    ///
    /// 有它就用它当**当前会话**的上下文：旁路缓冲只记「开旁路之后」的字节，
    /// 用户刚连上就问问题时它往往是空的，而屏幕上才是完整的现场。
    #[serde(default)]
    pub console_text: Option<String>,
    /// 当前会话是否停在全屏程序（vim / less / tmux / docker attach）里。
    /// 这类程序自己接管了屏幕，旁路抓不到真实输出，要提示模型改用抓取 skill。
    #[serde(default)]
    pub console_alt: bool,
}

/// 模型下发的一个动作。九选一：
///
/// - `Exec`：在某个会话上跑一条命令。
/// - `Export`：把某个会话迄今为止的全部输出要回来（`agent::capture`）。
/// - `Open`：开一个新会话（本地或某个已保存的 SSH 主机），可选顺手发一条命令。
/// - `Tab`：操作窗口上的标签页（列出 / 切换 / 关闭 / 改名 / 重连）。
/// - `Keys` / `Wait` / `Screen`：对终端的「手和眼」——发原始按键、等文字出现、读当前屏幕。
/// - `Secret`：请用户输入一个密码（密码通道，见 `secret.rs`）。模型只能「请求」，永远拿不到值。
/// - `Reject`：模型把界面动作写成了 shell 命令，被拦下来了（见 `pseudo_command_hint`）。
///
/// `Tab` 与 `Exec` 有个本质区别：**它不由后端执行**。tab 是前端 React state，
/// 后端连「用户开了几个标签页」都不知道；这里只把动作转成一个事件，等前端回执。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentAction {
    Exec { target: Option<String>, command: String },
    Export { target: Option<String>, deep: bool },
    Open { kind: OpenKind, label: Option<String>, command: Option<String> },
    Tab { op: TabOp, target: Option<String>, name: Option<String> },
    /// 原始按键：写进 pty 就算完，**不追加换行、不等命令结束**。
    /// `keys` 保留模型写的转义串原文（语法见 `interact::parse_keys`），执行时才解码，
    /// 这样审批卡 / trace 里能原样展示它「想发什么」。
    Keys { target: Option<String>, keys: String },
    /// 等某段文字出现（子串或 `/regex/`）或输出静止。只读旁路缓冲，不往终端写任何东西。
    Wait {
        target: Option<String>,
        pattern: Option<String>,
        timeout_ms: Option<u64>,
        quiet_ms: Option<u64>,
    },
    /// 读目标会话此刻的 xterm 屏幕（含 alt screen）。
    Screen { target: Option<String> },
    /// 请用户在面板里输入一个密码，由后端直接写进目标会话的 pty。
    /// 动作里**只有 reason**：没有、也不接受任何「值」字段 —— 模型不知道密码，也不该经手。
    Secret { target: Option<String>, reason: Option<String> },
    /// 不是「命令有风险」，而是「这条命令根本不该出现」。
    ///
    /// 它照样占一个动作位、照样例行走反馈链路 —— 让模型看到自己错在哪，
    /// 而不是让这轮静悄悄什么都不发生。
    Reject { command: String, hint: String },
}

/// 对标签页能做些什么。
///
/// `List` 是其余几个的前提：模型得先知道有哪些 tab、各自 id 是什么，
/// 才谈得上「切到那个跑着 vim 的」或「把卡住的那个关掉」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabOp {
    List,
    Focus,
    Close,
    Rename,
    Reconnect,
}

impl TabOp {
    fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Focus => "focus",
            Self::Close => "close",
            Self::Rename => "rename",
            Self::Reconnect => "reconnect",
        }
    }

    /// 给人看的中文说法，出现在步骤卡与喂回模型的反馈里。
    fn label(self) -> &'static str {
        match self {
            Self::List => "列出标签页",
            Self::Focus => "切换标签页",
            Self::Close => "关闭标签页",
            Self::Rename => "重命名标签页",
            Self::Reconnect => "重连标签页",
        }
    }
}

/// `Open` 动作的目标：本地新开一个 shell，还是连到一个已保存的 SSH 主机。
///
/// **刻意不接受模型直接给的 host/用户名/密码**：新会话只能指向前端侧栏里
/// 已经存在的 `SessionRecord`，引用方式是它的 id。这样即便终端输出里被注入了
/// 恶意指令，模型也没有办法凭空编一个主机把数据连出去 —— 能连的，都是用户自己
/// 已经配置、已经信任过的目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenKind {
    Local,
    Ssh { session_record_id: String },
}

/// 围栏代码块内部那一坨 JSON 的形状。字段全可选，是为了让模型怎么写都能接住。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecBlock {
    /// `exec`（默认，兼容没有这个字段的旧式调用） | `export` | `open`
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    targets: Vec<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    commands: Vec<String>,
    /// `export` 专用：是否额外探一遍 tmux / docker 的现状。
    #[serde(default)]
    deep: bool,
    /// `open` 专用：`local`（默认） | `ssh`
    #[serde(default)]
    kind: Option<String>,
    /// `open` + `kind:"ssh"` 专用：引用一个已保存的会话配置 id。
    #[serde(default)]
    session_record_id: Option<String>,
    /// `open` 专用：新标签页的展示名。
    #[serde(default)]
    pub label: Option<String>,
    /// `tab` 专用：`list` | `focus` | `close` | `rename` | `reconnect`
    #[serde(default)]
    op: Option<String>,
    /// `tab` + `op:"rename"` 专用：新名字。
    #[serde(default)]
    name: Option<String>,
    /// `secret` 专用：为什么要用户输入密码（给人看的一句话）。
    #[serde(default)]
    reason: Option<String>,
    /// `keys` 专用：按键串（转义语法见 `interact::parse_keys`）。
    #[serde(default)]
    keys: Option<String>,
    /// `wait` 专用：子串，或 `/regex/`。
    #[serde(default)]
    pattern: Option<String>,
    /// `wait` 专用：最长等多久（毫秒），后端会封顶。
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// `wait` 专用：连续多久没有新输出就返回（毫秒）。
    #[serde(default)]
    quiet_ms: Option<u64>,
}

// ——————————————————————————————— 工具声明 ———————————————————————————————

/// 工具名。
///
/// 刻意与围栏协议的 `action` 取值一一对应（去掉 `nx_` 前缀就是）：
/// 工具参数因此可以直接补一层 `action` 后喂给同一套 `ExecBlock` 解析，
/// 不用为工具再维护一份字段表 —— 那两份迟早会走散。
pub const TOOL_EXEC: &str = "nx_exec";
pub const TOOL_EXPORT: &str = "nx_export";
pub const TOOL_OPEN: &str = "nx_open_tab";
pub const TOOL_TAB: &str = "nx_tab";
pub const TOOL_KEYS: &str = "nx_send_keys";
pub const TOOL_WAIT: &str = "nx_wait";
pub const TOOL_SCREEN: &str = "nx_screen";
pub const TOOL_SECRET: &str = "nx_secret";

/// 声明给模型的全部工具。
///
/// 名字带 `nx_` 前缀：模型可能在别的上下文里见过 `exec` / `open` 这类通用名，
/// 用带前缀的名字能把它从「通用先验」里拽出来，照我们的语义用。
pub fn tool_specs() -> Vec<llm::ToolSpec> {
    vec![
        llm::ToolSpec {
            name: TOOL_EXEC,
            description: "在一个终端会话里执行一条 shell 命令，并拿到它的真实输出。".into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "目标会话 id。省略表示落到用户当前所在的会话。"
                    },
                    "command": {
                        "type": "string",
                        "description": "单条单行命令。不要换行串联，也不要用 && 串起互不相关的操作。"
                    }
                },
                "required": ["command"]
            }),
        },
        llm::ToolSpec {
            name: TOOL_EXPORT,
            description: "导出一个会话迄今为止的全部输出记录（不受喂给你的上下文长度限制）。".into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "目标会话 id。省略表示当前会话。"
                    },
                    "deep": {
                        "type": "boolean",
                        "description": "true 时额外探一遍该会话里的 tmux pane 与 docker 容器日志，更全但更慢。"
                    }
                }
            }),
        },
        llm::ToolSpec {
            name: TOOL_OPEN,
            description: "开一个新的标签页（本地 shell，或连到一个用户已保存的 SSH 主机）。".into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["local", "ssh"],
                        "description": "local = 本机 shell；ssh = 连到远程主机（此时必须给 sessionRecordId）。"
                    },
                    "sessionRecordId": {
                        "type": "string",
                        "description": "kind=ssh 时必填：用户在侧栏里已保存的会话配置 id。\
                                        只能引用它，**绝不能自己编造主机名/用户名/密码**。"
                    },
                    "label": { "type": "string", "description": "新标签页的展示名，可省略。" },
                    "command": { "type": "string", "description": "开好之后顺手执行的命令，可省略。" }
                },
                "required": ["kind"]
            }),
        },
        llm::ToolSpec {
            name: TOOL_TAB,
            description: "操作界面上的标签页。标签页是界面元素，\
                          没有任何 shell 命令能操作它 —— 不要试图用命令代替这个工具。".into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["list", "focus", "close", "rename", "reconnect"],
                        "description": "list = 列出作用范围内的标签页（**先做这个拿到 id**）；\
                                        focus = 切过去；close = 关掉；\
                                        rename = 改名；reconnect = SSH 断线后重连。"
                    },
                    "target": {
                        "type": "string",
                        "description": "标签页 id（与 nx_exec 的 target 是同一套会话 id）。\
                                        除 list 外必填。"
                    },
                    "name": { "type": "string", "description": "op=rename 时的新名字。" }
                },
                "required": ["op"]
            }),
        },
        llm::ToolSpec {
            name: TOOL_KEYS,
            description: "向终端发送原始按键：**不追加换行、不等命令结束**，写进去就返回。\
                          用于 nx_exec 做不到的事：vim 里按 Esc 与 :wq、回应 [Y/n] 之类的交互提示、\
                          Ctrl-C 打断卡住的进程、Ctrl-D 退出 REPL、方向键 / Tab 补全、\
                          在 python / mysql 等 REPL 里输入。普通的「跑一条命令看输出」仍用 nx_exec。\
                          **绝不要用它输入密码**：遇到密码提示请改用 nx_secret，让用户本人输入。".into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "目标会话 id。省略表示用户当前所在的会话。"
                    },
                    "keys": {
                        "type": "string",
                        "description": "按键串，解码后最长 256 字节。普通字符原样发送；\
                                        转义：\\r 回车、\\n 换行、\\t 制表、\\e ESC、\\\\ 反斜杠、\\xNN 任意字节（\\x03 = Ctrl-C）；\
                                        具名键：<Enter> <Tab> <Esc> <Space> <BS> <Del> <Up> <Down> <Left> <Right> \
                                        <Home> <End> <PgUp> <PgDn> <F1>…<F12>、<C-a>…<C-z>（Ctrl 组合，如 <C-c> <C-d>）、\
                                        <M-x>（Alt + 字符）、<lt>（字面的 <）。\
                                        例：\"<C-c>\" 打断；\"y<Enter>\" 回答 y 并回车；\"<Esc>:wq<Enter>\" 保存退出 vim。"
                    }
                },
                "required": ["keys"]
            }),
        },
        llm::ToolSpec {
            name: TOOL_WAIT,
            description: "等终端里出现某段文字（或输出静止），不往终端写任何东西。\
                          适合发完按键 / 启动了长任务之后，等一个提示或结束标志出现。\
                          返回是否命中，以及等待期间输出的尾部。".into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "目标会话 id。省略表示当前会话。"
                    },
                    "pattern": {
                        "type": "string",
                        "description": "要等的文字：直接写就是子串匹配；写成 /正则/（可加 i 标志，如 /error|fail/i）则按正则匹配。\
                                        只在**新输出**里找。省略则只等输出静止。"
                    },
                    "timeoutMs": {
                        "type": "integer",
                        "description": "最长等多久（毫秒），默认 15000，上限 60000。"
                    },
                    "quietMs": {
                        "type": "integer",
                        "description": "连续这么久（毫秒）没有新输出就算静止并返回。\
                                        没给 pattern 时默认 1500；给了 pattern 时不设，只等命中或超时。"
                    }
                }
            }),
        },
        llm::ToolSpec {
            name: TOOL_SCREEN,
            description: "读取目标会话此刻的终端屏幕文字（含 vim / less / tmux 等全屏程序的当前画面）。\
                          想确认「现在屏幕上到底是什么」「它在等我输入什么」时用。".into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "目标会话 id。省略表示当前会话。"
                    }
                }
            }),
        },
        llm::ToolSpec {
            name: TOOL_SECRET,
            description: "请用户本人输入一个密码 / 口令 / 验证码：面板会弹出一个带掩码输入框的卡片，\
                          用户输入后由后端**直接写进目标终端并回车**，输入内容不会经过你、也不会回显。\
                          只在终端正停在密码类提示（`Password:` / `[sudo] password` / `passphrase` / 验证码）时用。\
                          你只能「请求」：没有任何参数能让你提供或看到密码；\
                          用户可能拒绝或超时，结果会告诉你。**绝不要用 nx_send_keys / nx_exec 输入密码**。".into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "要输入密码的会话 id。省略表示当前会话。"
                    },
                    "reason": {
                        "type": "string",
                        "description": "一句话告诉用户为什么需要密码（会显示在卡片上），\
                                        例如「sudo 需要你的登录密码才能安装 nginx」。"
                    }
                },
                "required": ["reason"]
            }),
        },
    ]
}

/// 模型在终端语境下习惯写 `{"command":["uptime","df -h"]}`（一排命令），
/// 而工具的 schema 里 `command` 是字符串。
///
/// 这里把数组形态搬到 `commands` / `targets` 上，让围栏协议那套批量下发
/// 照样能用 —— 否则一次类型不符就要白耗一整轮去重发。
fn promote_arrays(obj: &mut serde_json::Map<String, serde_json::Value>) {
    for (single, plural) in [("command", "commands"), ("target", "targets")] {
        if obj.get(single).map(|v| v.is_array()).unwrap_or(false) {
            let arr = obj.remove(single).unwrap_or(serde_json::Value::Null);
            obj.insert(plural.to_string(), arr);
        }
    }
}

/// 工具名 → 围栏协议的 `action` 取值。
fn tool_action_name(name: &str) -> Option<&'static str> {
    match name {
        TOOL_EXEC => Some("exec"),
        TOOL_EXPORT => Some("export"),
        TOOL_OPEN => Some("open"),
        TOOL_TAB => Some("tab"),
        TOOL_KEYS => Some("keys"),
        TOOL_WAIT => Some("wait"),
        TOOL_SCREEN => Some("screen"),
        TOOL_SECRET => Some("secret"),
        _ => None,
    }
}

/// 把模型的一个工具调用翻译成动作。
///
/// 工具参数是 `{"op":"list"}` 这种扁平对象，围栏协议则在同层还要一个
/// `{"action":"tab"}`。这里把 `action` 补进去，就能完全复用围栏侧的解析 ——
/// 包括「缺字段给默认值」「`switchTo` 等同 `focus`」这些已有的宽容处理。
///
/// 动作序列刻意覆写 `action` 字段：万一模型在参数里自己写了个 `action`，
/// 也该以工具名为准（工具名是它选这条路时真正的意图）。
pub fn actions_from_tool_call(call: &llm::ToolCall) -> Vec<AgentAction> {
    let action = match tool_action_name(call.name.trim()) {
        Some(a) => a,
        // 未知工具：宁可什么都不做，也不要猜。
        None => return Vec::new(),
    };
    let mut args: serde_json::Value = match serde_json::from_str(&call.arguments) {
        Ok(serde_json::Value::Object(m)) => serde_json::Value::Object(m),
        // 工具参数偶有空串（模型没传参就直接调了），按空对象继续，
        // 后面 parse_* 会因为缺必填字段自行丢弃。
        _ => serde_json::Value::Object(serde_json::Map::new()),
    };
    if let Some(obj) = args.as_object_mut() {
        obj.insert("action".into(), serde_json::Value::String(action.into()));
        promote_arrays(obj);
    }
    let block: ExecBlock = match serde_json::from_value(args) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    match action {
        "export" => parse_export(block),
        "open" => parse_open(block).into_iter().collect(),
        "tab" => parse_tab(block).into_iter().collect(),
        "keys" => parse_keys_action(block),
        "wait" => parse_wait(block),
        "screen" => parse_screen(block),
        "secret" => parse_secret(block),
        _ => parse_exec(block),
    }
}

/// 工具调用在历史里的文本形态。
///
/// 不能原样塞回带 `tool_calls` 的 assistant 消息：OpenAI 系接口要求它后面紧跟
/// `role:"tool"` 回执，而我们的回执是「终端回了什么」这条普通 user 文本，下一轮会 400。
/// 写成人话记进去，历史就仍是干净的 user/assistant 交替，对任何兼容层都成立。
fn describe_tool_calls(reply: &llm::LlmReply) -> String {
    let mut out = reply.text.clone();
    for c in &reply.tool_calls {
        if !out.trim().is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("（调用工具 {}：{}）", c.name, c.arguments));
    }
    out
}

/// 工具名（+ 参数）在界面上给人看的说法。
pub fn tool_label(name: &str, args: &str) -> &'static str {
    match name.trim() {
        TOOL_EXEC => "执行命令",
        TOOL_EXPORT => "导出记录",
        TOOL_OPEN => "新建标签页",
        TOOL_KEYS => "发送按键",
        TOOL_WAIT => "等待输出",
        TOOL_SCREEN => "读取屏幕",
        TOOL_SECRET => "请求输入密码",
        TOOL_TAB => match serde_json::from_str::<serde_json::Value>(args)
            .ok()
            .and_then(|v| v.get("op").and_then(|o| o.as_str()).map(|s| s.to_string()))
            .as_deref()
        {
            Some("list") => "列出标签页",
            Some("focus" | "switch" | "switchTo") => "切换标签页",
            Some("close") => "关闭标签页",
            Some("rename") => "重命名标签页",
            Some("reconnect" | "reopen") => "重连标签页",
            _ => "操作标签页",
        },
        _ => "工具调用",
    }
}

// ——————————————————————————————— 事件载荷 ———————————————————————————————

/// `nx://agent/step/{session}` 的载荷：一轮里发生的每一件事。
///
/// 刻意做成「一个扁平结构 + kind」而不是 tagged enum：TS 侧判别联合要写模板代码，
/// 而这里的多数字段本来就是可选的，扁平结构反而更好渲染。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepPayload {
    /// `plan` | `tool` | `exec` | `result` | `skip` | `note` | `answer`
    pub kind: &'static str,
    pub step: usize,
    /// `kind:"tool"` 专用：模型调用的工具名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// `kind:"tool"` 专用：给人看的工具名（中文）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_label: Option<String>,
    /// `kind:"tool"` 专用：原始参数 JSON。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_args: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasons: Option<Vec<String>>,
    /// `kind:"waiting"` 专用：命令发出去之后已经等了多少秒，还没收到结果。
    /// 纯展示用，让面板上的「执行中」卡片能看出还活着、不是卡死。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_secs: Option<u64>,
    /// 终端此刻停在什么上（`prompt::PromptKind` 的 snake_case 名字）。
    /// 给 exec / wait / screen / keys 的结果卡打标签用，与喂给模型的 `prompt_kind` 同源。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_kind: Option<String>,
    /// `kind:"secret"` 专用：这次密码请求的结局（`secret::SecretStatus::code`）。
    /// **只有状态，绝无密码**。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_status: Option<String>,
}

impl StepPayload {
    fn new(kind: &'static str, step: usize) -> Self {
        Self {
            kind,
            step,
            tool: None,
            tool_label: None,
            tool_args: None,
            target: None,
            target_label: None,
            command: None,
            output: None,
            text: None,
            reasons: None,
            elapsed_secs: None,
            prompt_kind: None,
            secret_status: None,
        }
    }
}

/// `nx://agent/approve/{session}` 的载荷：请人点头。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovePayload {
    pub request_id: String,
    /// `danger`（危险命令，受 exec mode 约束） | `scope`（超出作用范围，**不受** exec mode 影响）。
    /// 前端据此换标题与按钮文案：两种批准的含义不同，不能让人一眼分不清自己在批什么。
    pub kind: &'static str,
    pub command: String,
    pub target: String,
    pub target_label: String,
    pub reasons: Vec<String>,
}

/// 给 UI 展示的单次执行结果（手动执行按钮也用它）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecResult {
    pub output: String,
    pub reason: ExecEnd,
    /// 收尾时终端停在什么上（`prompt::PromptKind` 的 snake_case 名字）；取消时没有。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_kind: Option<String>,
}

/// `nx://agent/tab` 的载荷：请前端动一下标签页。
///
/// 后端**没有** tab 这个概念，所以这不是「执行」，是「转达」。
/// `request_id` 是回执的凭据：前端办完（或办不成）都要带着它回一句话，
/// 否则后端永远分不清「办好了」和「消息丢了」。
///
/// 事件名是**全局**的（不带 session 后缀）：标签页属于整个窗口，
/// 不是某个会话的私产；而 tab 的创建 / 关闭又只发生在前端，
/// 让每个会话各挂一个监听反而更容易漏。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TabRequest {
    pub request_id: String,
    /// 发起这次操作的会话（只为定位来源，执行时不看它）
    pub session_id: String,
    pub op: &'static str,
    pub op_label: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// 前端回执。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TabReply {
    pub ok: bool,
    /// 一句人话：办成了什么 / 为什么没办成。
    #[serde(default)]
    pub detail: String,
    /// 仅 `list` 有意义：当前所有标签页。
    #[serde(default)]
    pub tabs: Vec<TabInfo>,
}

/// 一个标签页的对外视图。给模型看，也给用户看。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TabInfo {
    pub id: String,
    pub title: String,
    /// `local` | `ssh`
    #[serde(default)]
    pub kind: String,
    /// `connected` | `connecting` | `closed` | `error` …
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub active: bool,
}

/// `nx://agent/screen` 的载荷：请前端把某个会话此刻的 xterm 屏幕读出来。
///
/// 与 `TabRequest` 同一套往返：xterm 实例只在前端，后端没有「屏幕」这个东西
/// （旁路缓冲是字节流转录，不是渲染后的画面，vim 之类的全屏程序在里面是一团转义序列）。
/// 全局事件、不带 session 后缀，理由同 `TabRequest`。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenRequest {
    pub request_id: String,
    /// 发起这次读取的会话（只为定位来源）
    pub session_id: String,
    /// 要读哪个会话的屏幕
    pub target: String,
    /// 前端最多回多少字符（取末尾）
    pub max_chars: usize,
}

/// 前端对 `ScreenRequest` 的回执。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenReply {
    pub ok: bool,
    /// 屏幕文字（已摊平成纯文本）
    #[serde(default)]
    pub text: String,
    /// 是否停在 alternate screen（vim / less / tmux …）
    #[serde(default)]
    pub alt: bool,
    /// `ok=false` 时的原因
    #[serde(default)]
    pub detail: String,
}

/// `nx://agent/secret/{session}` 的载荷：请用户输入一个密码。
///
/// 与审批同一个思路（per-session 事件，面板里渲染成卡片），区别是答复里带一个值，
/// 且这个值**只走 `agent_secret_ack` → 后端 → pty**。这里没有、也不能有任何值字段。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretRequest {
    pub request_id: String,
    /// 要往哪个会话输入
    pub target: String,
    pub target_label: String,
    /// 模型给的理由（已清洗成单行、限长）
    pub reason: String,
    /// 请求发出时对目标终端的分类。不是 `password` 时卡片要提醒用户：
    /// 终端当前似乎并没在要密码，输进去可能会被回显。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_kind: Option<String>,
    /// 多少秒内必须答复（卡片上显示倒计时用）
    pub timeout_secs: u64,
}

fn emit_step(app: &AppHandle, session: &str, payload: StepPayload) {
    let _ = app.emit(&format!("nx://agent/step/{session}"), payload);
}

// ——————————————————————————————— 动作解析 ———————————————————————————————

/// 围栏块的开始标记。认它时也接受模型自作主张加的语言后缀（```nx-exec json）。
const FENCE_OPEN: &str = "```nx-exec";

/// 从一段模型回复里抽出所有动作。
///
/// 只认 ```nx-exec 围栏：普通代码块（常常是模型给用户抄的命令）一律不动，
/// 否则「请把这段命令贴进终端」这类善意的回复会被误当成「请帮我执行」。
pub fn extract_actions(text: &str) -> Vec<AgentAction> {
    let mut out = Vec::new();
    let mut inside = false;
    let mut buf = String::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if !inside {
            // 围栏开放行。模型常在语言位多写 ``json`` / 空格，starts_with 天然容得下。
            if trimmed.starts_with(FENCE_OPEN) {
                inside = true;
                buf.clear();
            }
            continue;
        }
        if trimmed.starts_with("```") {
            inside = false;
            out.extend(parse_block(&buf));
            continue;
        }
        buf.push_str(line);
        buf.push('\n');
    }
    // 围栏没收尾（模型被 max_tokens 截断）时，块里已有的东西也值得一试：
    // 一条残缺的 JSON 解不出来会被静默丢弃，不会因此误执行任何东西。
    if inside {
        out.extend(parse_block(&buf));
    }

    out.truncate(MAX_ACTIONS);
    out
}

/// 解析一个块。
///
/// 先按约定试 JSON；不是 JSON 就走 shell 兜底（见 `parse_raw_shell`）。
/// 以前这里 JSON 一失败就返回空，结果是最糟的一种失败：围栏明明写着 `nx-exec`，
/// 界面上却什么都没发生 —— 用户只看到模型自说自话，以为它卡住了。
fn parse_block(raw: &str) -> Vec<AgentAction> {
    match serde_json::from_str::<ExecBlock>(raw.trim()) {
        Ok(block) => match block.action.as_deref().unwrap_or("exec") {
            "export" => parse_export(block),
            "open" => parse_open(block).into_iter().collect(),
            "tab" => parse_tab(block).into_iter().collect(),
            "keys" => parse_keys_action(block),
            "wait" => parse_wait(block),
            "screen" => parse_screen(block),
            "secret" => parse_secret(block),
            _ => parse_exec(block),
        },
        Err(_) => parse_raw_shell(raw),
    }
}

/// 围栏里直接写了一段 shell —— 模型最常犯的协议错误（终端场景下它的先验就是
/// 「命令写进代码块」）。
///
/// 围栏写成 `nx-exec` 本身就是「请执行」的明确信号，所以照执行；但以 `{` / `[`
/// 开头说明它是想写 JSON 却写坏了，这时**绝不**退化成 shell：否则
/// `{"target":"s1","command": "ls` 这半截会被原样敲进终端。
fn parse_raw_shell(raw: &str) -> Vec<AgentAction> {
    let head = raw.trim_start();
    if head.starts_with('{') || head.starts_with('[') {
        return Vec::new();
    }
    raw.lines()
        .filter_map(|line| {
            let cmd = line.trim();
            // 模型常顺手把提示符一起抄进来（`$ uptime`），那不是命令的一部分
            let cmd = cmd.strip_prefix("$ ").unwrap_or(cmd);
            // 空行和注释不是动作；注释尤其不能发 —— 敲进终端只是白占一行
            if cmd.is_empty() || cmd.starts_with('#') {
                return None;
            }
            Some(action_for_command(None, cmd))
        })
        .collect()
}

/// `action:"exec"`（或没写 action，向后兼容旧协议）。
fn parse_exec(block: ExecBlock) -> Vec<AgentAction> {
    let targets = if block.targets.is_empty() {
        block.target.into_iter().collect::<Vec<_>>()
    } else {
        block.targets
    };
    let commands = if block.commands.is_empty() {
        block.command.into_iter().collect::<Vec<_>>()
    } else {
        block.commands
    };
    if commands.is_empty() {
        return Vec::new();
    }

    // target 为空 → None（走默认会话）；否则逐个铺开。
    // 顺序是「目标在外、命令在内」：同一台机器上的几条命令连着跑，
    // 比来回切换目标更少打乱 shell 状态。
    let mut out = Vec::new();
    if targets.is_empty() {
        for c in &commands {
            out.push(action_for_command(None, c));
        }
    } else {
        for t in &targets {
            for c in &commands {
                out.push(action_for_command(Some(t.clone()), c));
            }
        }
    }
    out
}

/// `action:"export"`：导出一个（或多个）会话迄今为止的全部输出。
fn parse_export(block: ExecBlock) -> Vec<AgentAction> {
    let targets = if block.targets.is_empty() {
        block.target.into_iter().collect::<Vec<_>>()
    } else {
        block.targets
    };
    if targets.is_empty() {
        vec![AgentAction::Export { target: None, deep: block.deep }]
    } else {
        targets
            .into_iter()
            .map(|t| AgentAction::Export { target: Some(t), deep: block.deep })
            .collect()
    }
}

/// 把 `target` / `targets` 摊成一串目标；都没写就是 `[None]`（落到当前会话）。
fn fan_targets(single: Option<String>, many: Vec<String>) -> Vec<Option<String>> {
    let ts: Vec<String> = if many.is_empty() { single.into_iter().collect() } else { many };
    let ts: Vec<String> =
        ts.into_iter().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect();
    if ts.is_empty() {
        vec![None]
    } else {
        ts.into_iter().map(Some).collect()
    }
}

/// `action:"keys"`：原始按键。`keys` 为空就不产生动作（由主循环给「缺参数」提示）。
///
/// 只存原文、不在这里解码：解码失败要给人看的卡片与喂回模型的原因，那是执行层的事。
fn parse_keys_action(block: ExecBlock) -> Vec<AgentAction> {
    let Some(keys) = block.keys.filter(|k| !k.is_empty()) else {
        return Vec::new();
    };
    fan_targets(block.target, block.targets)
        .into_iter()
        .map(|target| AgentAction::Keys { target, keys: keys.clone() })
        .collect()
}

/// `action:"wait"`：参数全可选（pattern / quietMs 都省略 = 等输出静止）。
fn parse_wait(block: ExecBlock) -> Vec<AgentAction> {
    let pattern = block.pattern.filter(|p| !p.is_empty());
    fan_targets(block.target, block.targets)
        .into_iter()
        .map(|target| AgentAction::Wait {
            target,
            pattern: pattern.clone(),
            timeout_ms: block.timeout_ms,
            quiet_ms: block.quiet_ms,
        })
        .collect()
}

/// `action:"screen"`：读屏幕。
fn parse_screen(block: ExecBlock) -> Vec<AgentAction> {
    fan_targets(block.target, block.targets)
        .into_iter()
        .map(|target| AgentAction::Screen { target })
        .collect()
}

/// `action:"secret"`：请用户输入密码。只认 `target` / `reason`；
/// 一次只对一个会话发起（多目标没有意义：一张卡片对应一次输入）。
fn parse_secret(block: ExecBlock) -> Vec<AgentAction> {
    let target = fan_targets(block.target, block.targets).into_iter().next().flatten();
    vec![AgentAction::Secret { target, reason: block.reason }]
}

/// `action:"open"`：开一个新会话。`kind:"ssh"` 必须带 `sessionRecordId`，
/// 否则宁可什么都不做（也不要凭空连一个模型编出来的主机）。
fn parse_open(block: ExecBlock) -> Option<AgentAction> {
    let kind = match block.kind.as_deref() {
        Some("ssh") => OpenKind::Ssh { session_record_id: block.session_record_id? },
        _ => OpenKind::Local,
    };
    Some(AgentAction::Open { kind, label: block.label, command: block.command })
}

/// `action:"tab"`：操作标签页。
///
/// 参数不全就**不产生动作**，而不是产出一个半成品让执行层去兜 ——
/// 缺 target 的 close 是没法「尽量做」的。丢弃后主循环会因为「有围栏却 0 动作」
/// 给出一张可见的提示卡，不会静默。
fn parse_tab(block: ExecBlock) -> Option<AgentAction> {
    let op = match block.op.as_deref()?.trim() {
        "list" => TabOp::List,
        "focus" | "switch" | "switchTo" => TabOp::Focus,
        "close" => TabOp::Close,
        "rename" => TabOp::Rename,
        "reconnect" | "reopen" => TabOp::Reconnect,
        _ => return None,
    };
    let target = block.target.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    // 只有 list 不需要指名道姓
    if op != TabOp::List && target.is_none() {
        return None;
    }
    let name = block.name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    if op == TabOp::Rename && name.is_none() {
        return None;
    }
    Some(AgentAction::Tab { op, target, name })
}

/// 「看起来像命令、其实是界面动作」的伪命令 → 纠正话术。
///
/// 见模块头「伪命令护栏」。
///
/// 只比对第一个 token：`new-tab --ssh foo` 带参数也拦得住，而 `ls | grep tab` 不受影响。
fn pseudo_command_hint(cmd: &str) -> Option<&'static str> {
    let head = cmd
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_start_matches("./")
        .to_ascii_lowercase();
    if head.is_empty() {
        return None;
    }
    match head.as_str() {
        h if matches!(
            h,
            "new-tab" | "newtab" | "new_tab" | "open-tab" | "opentab" | "open_tab"
            | "tab-new" | "tabnew" | "create-tab" | "add-tab" | "tab-create" | "newtabpage"
        ) =>
        {
            Some("「新建标签页」是界面动作，终端里没有这条命令。请改用 nx_open_tab 工具")
        }
        h if matches!(
            h,
            "reconnect" | "reconnect-tab" | "reconnecttab" | "reconnect_tab" | "re-tab"
            | "tab-reconnect"
        ) => Some("「重连标签页」是界面动作，终端里没有这条命令。请改用 nx_tab 工具（op=reconnect）"),
        h if matches!(h, "close-tab" | "closetab" | "close_tab" | "tab-close" | "kill-tab") => {
            Some("「关闭标签页」是界面动作，终端里没有这条命令。请改用 nx_tab 工具（op=close）")
        }
        h if matches!(
            h,
            "switch-tab" | "switchtab" | "switch_tab" | "tab-switch" | "focus-tab" | "tab-focus"
        ) => Some("「切换标签页」是界面动作，终端里没有这条命令。请改用 nx_tab 工具（op=focus）"),
        h if matches!(h, "list-tabs" | "listtab" | "tab-list" | "tabs") => {
            Some("「查看标签页」是界面动作，终端里没有这条命令。请改用 nx_tab 工具（op=list）")
        }
        h if matches!(h, "rename-tab" | "renametab" | "rename_tab" | "tab-rename") => {
            Some("「标签页改名」是界面动作，终端里没有这条命令。请改用 nx_tab 工具（op=rename）")
        }
        _ => None,
    }
}

/// 把一条命令变成动作：被判为伪命令时转成 `Reject`，否则就是普通的 `Exec`。
fn action_for_command(target: Option<String>, command: &str) -> AgentAction {
    match pseudo_command_hint(command) {
        Some(hint) => AgentAction::Reject { command: command.trim().into(), hint: hint.into() },
        None => AgentAction::Exec { target, command: command.to_string() },
    }
}

/// 校验一条命令能不能下发。返回可读的拒绝理由。
///
/// 拒绝换行是这里最重要的一条：一次注入一串操作会让「逐步审批」彻底失效 ——
/// 人看到的永远只是一句话，而实际执行的可能有五条。
pub fn reject_command(cmd: &str) -> Option<&'static str> {
    // 只按空格与制表符判空：`\n` / `\r` 也要算「有东西」，否则会被当成空命令放过
    if cmd.trim_matches(|c: char| c == ' ' || c == '\t').is_empty() {
        return Some("命令为空");
    }
    // ESC / BEL / NUL 同样要挡：它们能改写命令在屏幕上的显示，
    // 让人看到 `ls` 而实际跑的是别的。
    if cmd.chars().any(|c| c.is_control() && c != '\t') {
        return Some("命令含换行或控制字符，必须拆成单条执行");
    }
    None
}

// ——————————————————————————————— 系统提示词 ———————————————————————————————

/// Nexus AI Agent 的系统提示词。
///
/// 只有一个 Agent：问答和执行是它自己判断的事，不再由用户先选模式。
/// 这里必须把协议、边界、「什么时候该停」以及**抓取 skill** 讲得毫不含糊 ——
/// 说不清楚的话，模型要么在「再查一条」上无限打转，要么隔着全屏程序瞎猜。
pub const AGENTIC_SYSTEM_PROMPT_INTRO: &str = concat!(
    "你是 NexusTerm 的终端 Agent，可以直接在用户的终端会话里执行命令、",
    "导出会话的完整记录、甚至开一个新会话。",
    "用户会给出若干个会话及其最近输出记录，其中 `$ ` 开头的是人敲进去的命令，",
    "其余是程序输出（已剥离 ANSI 转义）。"
);

/// 动作协议本身。
///
/// 刻意抽成独立常量并被**两条路径共用**（内置提示词 / 用户自定义提示词）：
/// 之前自定义路径用的是压缩版说明，结果用户的提示词里写着「只给结论与必要命令」，
/// 模型就真把 shell 直接抄进围栏 —— 协议写歪了，界面上什么都不发生。
pub const ACTION_PROTOCOL: &str = concat!(
    "【优先用工具】你有 nx_exec / nx_send_keys / nx_wait / nx_screen / nx_secret / nx_export / nx_open_tab / nx_tab 八个工具。",
    "能用工具调用时**直接用工具**，不要写成下面的围栏文本；",
    "只有当你确认自己无法发起工具调用时，才退化成围栏代码块。\n",
    "**绝不要把界面动作当成 shell 命令**：`new-tab`、`reconnect`、`close-tab`、",
    "`switch-tab`、`list-tabs`、`rename-tab` 这些在终端里根本不存在，",
    "敲过去只会得到 command not found。它们对应的都是 nx_tab / nx_open_tab 工具。\n\n",
    "【要执行命令时】在回复里写且只写这样一个围栏代码块（不写 action 等同 \"exec\"）：\n",
    "```nx-exec\n{\"action\":\"exec\",\"target\":\"<会话id>\",\"command\":\"<命令>\"}\n```\n",
    "一次要对多台机器发同一批命令时，用 targets / commands 两个数组：\n",
    "```nx-exec\n{\"action\":\"exec\",\"targets\":[\"id1\",\"id2\"],\"commands\":[\"uptime\",\"df -h\"]}\n```\n",
    "不写 target 表示落到用户当前所在的会话。",
    "围栏里**只放那一行 JSON**：直接抄一段 shell 虽然也能跑，但 target 就丢了。\n\n",
    "【要导出一个会话的完整记录时】用 export 动作，而不是自己拼 tmux/docker 命令去凑：\n",
    "```nx-exec\n{\"action\":\"export\",\"target\":\"<会话id>\",\"deep\":true}\n```\n",
    "`deep:true` 会额外探一遍该会话里的 tmux pane 与 docker 容器日志，",
    "`deep:false`（默认）只给旁路缓冲里已经记下的内容，更快但可能够不到更早的历史。\n\n",
    "【要开一个新会话时】本地 shell：\n",
    "```nx-exec\n{\"action\":\"open\",\"kind\":\"local\",\"label\":\"排查用\",\"command\":\"uptime\"}\n```\n",
    "连到一个用户已经保存好的 SSH 主机（必须给出已存在的会话配置 id，",
    "**不能自己编一个主机/用户名/密码**，你也看不到密码本身）：\n",
    "```nx-exec\n{\"action\":\"open\",\"kind\":\"ssh\",\"sessionRecordId\":\"<已保存的会话配置id>\",\"label\":\"db-01\"}\n```\n",
    "`command` 可省略（只开不发），开成功后用户的界面上会真的多出一个标签页。\n",
    "**「开一个窗口」就是指这个 open 动作**：不要用 `open -a Terminal`、`osascript`、",
    "`gnome-terminal`、`wt` 之类的命令去启动系统终端应用 —— 那会在本应用之外弹出",
    "一个与 NexusTerm 毫无关系的窗口，用户要的并不是它。open 失败就如实说明原因，",
    "不要换成系统命令硬绕，也不要在没执行的情况下说「已打开」。\n\n",
    "【要操作标签页（tab）时】用 tab 动作 —— 标签页是界面上的东西，", 
    "**没有任何 shell 命令能操作它**，别试着用命令绕：\n",
    "```nx-exec\n{\"action\":\"tab\",\"op\":\"list\"}\n```\n",
    "`op` 取值：`list`（列出作用范围内的标签页，**先做这个拿到 id**）、`focus`（切过去）、",
    "`close`（关掉，里面的程序会被结束）、`rename`（配 `name` 改名）、",
    "`reconnect`（SSH 断线后重连）。后四个都要带 `target`：\n",
    "```nx-exec\n{\"action\":\"tab\",\"op\":\"focus\",\"target\":\"<标签页id>\"}\n```\n",
    "```nx-exec\n{\"action\":\"tab\",\"op\":\"rename\",\"target\":\"<标签页id>\",\"name\":\"<新名字>\"}\n```\n",
    "标签页 id 与 exec 的 target 是同一套会话 id。关掉自己当前所在的那个标签页也是允许的，\n",
    "但注意：关掉之后它就不在了，这一轮里后面再有 exec 动作省略 target、想隐式落到",
    "这个会话上会失败，需要的话换一个明确的 target。\n\n",
    "【手和眼：按键 / 等待 / 读屏】nx_exec 是「敲一行命令、等它跑完」；",
    "终端里**正停在一个交互程序上**时它不适用，改用下面三个（围栏写法分别是 ",
    "`{\"action\":\"keys\",\"keys\":\"…\"}` / `{\"action\":\"wait\",\"pattern\":\"…\"}` / `{\"action\":\"screen\"}`，同样可带 target）：\n",
    "· **nx_send_keys{keys}**：写入原始按键，**不追加回车、不等命令结束**，约 0.3 秒后回一段新输出。",
    "用于 vim（`<Esc>:wq<Enter>`）、`[Y/n]` 之类的确认（`y<Enter>`）、`python` / `mysql` 等 REPL 里输入、",
    "方向键与 Tab 补全，以及 **Ctrl-C 打断卡住的进程（`<C-c>`）**、Ctrl-D 退出 REPL（`<C-d>`）。",
    "按键串语法：普通字符原样发；转义 `\\r` 回车 `\\n` 换行 `\\t` 制表 `\\e` ESC `\\\\` 反斜杠 `\\xNN` 任意字节；",
    "具名键 `<Enter> <Tab> <Esc> <Space> <BS> <Del> <Up> <Down> <Left> <Right> <Home> <End> <PgUp> <PgDn> <F1>…<F12>`、",
    "`<C-a>`…`<C-z>`、`<M-x>`、`<lt>`（字面 `<`）。解码后最长 256 字节，超了请拆开。",
    "要跑一条普通命令并拿它的输出，仍用 nx_exec —— 它会替你等命令结束；send_keys 不会。\n",
    "· **nx_wait{pattern?, timeoutMs?, quietMs?}**：等新输出里出现某段文字（直接写是子串，`/正则/` 加 i 可忽略大小写），",
    "或等输出静止；不往终端写任何东西。发完按键、启动了长任务后用它等一个提示 / 结束标志，",
    "返回命中与否 + 期间输出的尾部。超时默认 15 秒、最多 60 秒，没命中就如实说没等到，不要假装成功。\n",
    "· **nx_screen{}**：读目标会话**此刻的屏幕**（含 vim / less / tmux 等全屏程序的当前画面）。",
    "拿不准「现在屏幕上是什么 / 它在等我输入什么」时先读屏，不要凭记忆盲敲按键。\n",
    "· **nx_secret{target?, reason}**：**密码通道**。看到 `Password:` / `passphrase` / `[sudo] password` / 验证码 这类提示，",
    "（nx_exec / nx_wait / nx_screen / nx_send_keys 的结果里 `prompt_kind=password` 就是这个信号）调用它：",
    "面板会弹出一张带掩码输入框的卡片，用户本人输入后由后端**直接写进终端并回车**，",
    "输入内容不会经过你、不会回显、也不会进任何记录；你只会收到「已提供 / 已拒绝 / 超时」和之后的一段新输出。",
    "`reason` 用一句话说清为什么需要它（显示在卡片上）。围栏写法 `{\"action\":\"secret\",\"reason\":\"…\"}`。",
    "用户拒绝或超时就是拒绝：不要再次索要同一个密码，也不要换别的办法绕过，在结论里说明这一步需要他自己完成；",
    "提示再次出现（输错了）可以再请求一次，但同一个任务里别反复纠缠。\n",
    "**绝不要用 send_keys / exec 输入密码、口令、验证码、私钥口令，也不要把密码写进任何命令或参数里**；",
    "也不要尝试猜密码、读取用户的密码文件来代填。\n",
    "· 上述工具的结果里都带 `prompt_kind`（password / yes_no / pager / menu / repl / shell_prompt / unknown）、",
    "`confidence` 与一句 `提示`：它是对终端尾部的启发式判断，拿来当线索，拿不准就 nx_screen 读屏确认。",
    "nx_exec 还可能返回「状态：终端停在等待输入的提示上」——命令**没有**结束，是卡在了交互提示，",
    "按 `prompt_kind` 与 `提示` 处理（yes_no → send_keys 回答；password → nx_secret；pager → `q`；repl → 在里面输入或 `<C-d>`）。\n",
    "send_keys 与 exec 一样受作用范围与审批约束；审批模式下含回车的按键会弹窗让人确认。\n\n",
    "【作用范围】你只能操作上面列出的「作用范围内」会话（含省略 target 时落到的当前会话）。",
    "对范围外会话的 exec / send_keys / wait / screen / secret / export / tab 动作，以及任何 open 新会话的动作，都会被拦截或弹窗交给用户确认，",
    "用户拒绝就是拒绝：不要重试、不要换个说法或换条路径绕过去，改为在结论里告诉用户",
    "需要把哪个会话加入作用范围，或请他自己操作。tab 的 list 也只会列出范围内的标签页。\n\n",
    "【规则】\n",
    "1) exec 的 command 必须是**单条单行**命令，不许用换行或 && 串联互不相关的操作；",
    "需要分几步就分几轮，每轮都会收到上一条的真实输出；\n",
    "2) 一次最多给 12 条动作，够用为止，不要贪多；按键类动作尤其如此 ——",
    "每敲一下屏幕都会变，要看了效果再决定下一步的，就分轮，别在同一轮里盲发一长串；\n",
    "3) 优先用只读命令把情况看清楚：POSIX 用 ls / cat / grep / ss / df / ps / journalctl；",
    "**Windows 本地会话（提示符形如 `C:\\Users\\…>`）是 cmd.exe，必须换成 dir / type /",
    " findstr / ipconfig / tasklist / systeminfo 等 cmd 语法** —— ls、cat、grep 在 cmd 里",
    "根本不存在，发过去只会得到「不是内部或外部命令」；",
    "确有必要才做写操作，并在下达前用一句话说明为什么要做、影响面多大；\n",
    "4) 命令的真实输出会原样回给你，**没有退出码这种参考信息**——",
    "跟真人盯着终端一样，自己照着输出文本判断这条命令到底成没成功，不要假装没看到；\n",
    "5) 不要执行不可逆操作（rm -rf、mkfs、DROP、chmod -R 777、把未知脚本管道给 shell 等），",
    "除非用户在这一轮对话里明确要求；开新会话同理要有明确理由，不要为了好奇心乱开；\n",
    "6) 已经查清楚就不要再查了：直接用中文给出结论，说明依据与必要的后续建议；\n",
    "7) 记录缺失或被截断就直说看不到，不要臆测终端里发生过什么。"
);

/// 全屏程序（tmux / docker / 分页器）里抓真实输出时的退路。
const OUTPUT_SKILL_TIPS: &str = concat!(
    "【输出抓取 skill（export 工具的退路）】当记录看起来被全屏程序接管、看不到真实输出时",
    "（tmux / screen / docker attach / less / vim 等），想知道**此刻屏幕上是什么**先用 nx_screen；",
    "想要更早的历史再用上面的 export + deep:true；",
    "如果它抓到的 pane 不是你想要的那个，再退回手动只读命令自己定位：\n",
    "· tmux：先 `tmux list-panes -a -F '#{session_name}:#{window_index}.#{pane_index} #{pane_current_command}'` 找到 pane，",
    "再 `tmux capture-pane -p -t <pane> -S -200` 抓最近 200 行；\n",
    "· docker：先 `docker ps --format '{{.ID}} {{.Names}} {{.Status}}'` 找到容器，",
    "再 `docker logs --tail 200 <容器>`；\n",
    "· 被 less / more / man 这类分页器截住：那是**屏幕停在半路**，不是命令没跑完 ——",
    "让它重跑一遍并落到文件（`<命令> > /tmp/nx.out 2>&1; tail -200 /tmp/nx.out`）比试图翻页可靠。\n",
    "抓到的真实内容才是判断依据。"
);

/// 内置的 agentic 系统提示词：介绍 + 完整协议 + 抓取技巧。
///
/// 用函数而不是 `const`：`concat!` 只吃字面量，拼不进上面两个常量；
/// 而**共用同一份协议文本**才是这里真正重要的事。
pub fn agentic_system_prompt() -> String {
    format!(
        "{AGENTIC_SYSTEM_PROMPT_INTRO}\n\n{ACTION_PROTOCOL}\n\n{OUTPUT_SKILL_TIPS}"
    )
}

// ——————————————————————————————— 上下文组装 ———————————————————————————————

/// 第一轮 user 消息：把所有目标会话的上下文与问题拼在一起。
fn plan_message(
    state: &AppState,
    req: &AgentRunRequest,
    targets: &[AgentTarget],
    max_bytes: usize,
    capture: Option<&str>,
    redact: bool,
) -> String {
    let blocks: Vec<(&AgentTarget, String)> = targets
        .iter()
        .map(|t| {
            // 当前会话优先用前端抓的屏幕快照：它含 scrollback，且不依赖旁路是否开启。
            // 别的会话前端拿不到屏幕，只能退回旁路缓冲。
            let screen = req
                .console_text
                .as_deref()
                .filter(|s| !s.trim().is_empty() && t.id == req.session_id);
            let raw = match screen {
                // 屏幕快照同样受上下文预算约束：前端按 24KB 抓的，配置里可能更小。
                // 砍头不砍尾 —— 要问的是「刚才发生了什么」。
                Some(s) => tail_chars(s, max_bytes),
                None => state.agent.context(&t.id, max_bytes).unwrap_or_default(),
            };
            (t, raw)
        })
        .collect();
    build_plan_message(req, &blocks, capture, &saved_hosts_hint(state), redact)
}

/// 出网前的脱敏闸门：`redact` 为真时过 `redact::redact`。
///
/// 对话循环里**所有**发往模型的终端内容（初始上下文、命令输出、屏幕、错误）
/// 都必须经过它 —— `llm::complete_reply` 本身不做脱敏，漏一处就是一次凭据外泄。
fn scrub(redact: bool, text: &str) -> String {
    if redact {
        super::redact::redact(text)
    } else {
        text.to_string()
    }
}

/// `plan_message` 的纯拼装部分（不碰 `AppState`，便于测试）。
///
/// 终端内容（每个会话的记录、全屏抓取）在这里脱敏；用户自己写的问题与已保存主机清单不脱敏：
/// 前者是用户明确交给模型的话，后者是模型挑 `sessionRecordId` 必需的信息（含 host）。
fn build_plan_message(
    req: &AgentRunRequest,
    blocks: &[(&AgentTarget, String)],
    capture: Option<&str>,
    hosts_hint: &str,
    redact: bool,
) -> String {
    let mut msg = String::from(
        "以下是你可以操作的终端会话及其最近输出记录（`$ ` 开头是命令，其余是输出）：\n",
    );
    for (t, raw) in blocks {
        let ctx = scrub(redact, raw);
        msg.push_str(&format!(
            "\n===== 会话 {} ({}) =====\n{}{}\n{}\n",
            t.label,
            t.id,
            CONTEXT_BEGIN,
            if ctx.trim().is_empty() {
                "（暂无可用记录：会话旁路未开启或已被清空）".to_string()
            } else {
                ctx
            },
            CONTEXT_END,
        ));
    }
    // 全屏程序会把真实输出挡在 alternate buffer 里，旁路抓不到；
    // 明确点出来，模型才会去用上面那套抓取 skill 而不是瞎猜。
    if req.console_alt {
        msg.push_str(
            "\n注意：当前会话正停在全屏程序（alternate buffer）里，上面的记录可能不是真实输出。\n",
        );
        match capture {
            // 已经替它抓好了：直接给内容，省掉一轮「先想命令、再执行、再读」的往返。
            Some(text) if !text.trim().is_empty() => {
                msg.push_str("我已通过旁路通道自动抓取了该程序的真实内容（只读，未碰你的屏幕）：\n");
                msg.push_str(CONTEXT_BEGIN);
                msg.push('\n');
                msg.push_str(&scrub(redact, text));
                msg.push('\n');
                msg.push_str(CONTEXT_END);
                msg.push('\n');
            }
            // 没抓到（非 tmux、旁路通道被堡垒机拒绝、超时）：让它自己去抓。
            _ => msg.push_str("必要时先用「输出抓取 skill」拿到内容再判断。\n"),
        }
    }
    msg.push_str(hosts_hint);
    msg.push_str(&format!(
        "\n目标：{}\n\n需要执行操作就调用工具，已经查清就直接给结论。",
        req.question.trim()
    ));
    msg
}

/// 用户已保存的主机清单。
///
/// 没有这一段，`nx_open_tab(kind=ssh)` 是死功能：协议要模型填「已存在的会话配置 id」，
/// 却从不告诉它有哪些 —— 模型只能瞎编、被「未找到会话配置」打回，最后放弃工具改写
/// `new-tab` 伪命令。
///
/// 只给 id / 名称 / host:port，**不给用户名与认证方式**：开新连接用不到，不多发凭据相关数据。
fn saved_hosts_hint(state: &AppState) -> String {
    let records = match state.store.list() {
        Ok(v) => v,
        // 库读取失败不该把整轮问答拖垮：少这一段，模型最多是开不了 SSH 标签页
        Err(_) => return String::new(),
    };
    let total = records.len();
    if total == 0 {
        return String::new();
    }
    let mut out = String::from(
        "\n【已保存的主机】下面是用户在侧栏里存过的连接目标。\
         要开一个连到其中某台的标签页，用 nx_open_tab 工具、把这一行的 id 填进 sessionRecordId。\
         **只能引用这些 id，绝不能自己编造主机**；列表里没有的目标，如实告诉用户你连不了：\n",
    );
    for r in records.into_iter().take(MAX_SAVED_HOST_HINTS) {
        let name = if r.name.trim().is_empty() { &r.host } else { &r.name };
        out.push_str(&format!("· id={} 「{}」 {}:{}\n", r.id, name, r.host, r.port));
    }
    if total > MAX_SAVED_HOST_HINTS {
        out.push_str(&format!("……（还有 {} 条未列出）\n", total - MAX_SAVED_HOST_HINTS));
    }
    out
}

/// 全屏程序（tmux / screen / less / vim …）把自己的画面画在 alternate buffer 上，
/// 远端的真实输出既不进旁路缓冲，也不在前端抓的屏幕快照里。
///
/// 走**旁路通道**跑只读抓取：用户屏幕上不多一个字符，也省掉一次 LLM 往返。
///
/// 只做 tmux：docker / kubectl 要么得猜容器名、要么常要 sudo，替用户跑一条猜出来的
/// 命令代价太大，留给模型按系统提示里的抓取 skill 自己决定。
///
/// 任何一步不成立（不是 tmux、旁路被堡垒机拒、超时）一律 `None`，由调用方退化成
/// 「让模型自己抓」。
async fn capture_fullscreen(state: &AppState, session_id: &str) -> Option<String> {
    // 先定位「用户此刻在看哪个 pane」：
    //   1) display-message 在没有任何客户端上下文时会失败，但成功时最准；
    //   2) 退回「第一个 session 里 active window 的 active pane」。
    // 整条是一条 sh 行，`2>/dev/null` 把「tmux 不存在」的噪声吞掉。
    const CMD: &str = concat!(
        "p=$(tmux display-message -p '#{session_name}:#{window_index}.#{pane_index}' 2>/dev/null); ",
        "[ -n \"$p\" ] || p=$(tmux list-panes -a -F ",
        "'#{session_name}:#{window_index}.#{pane_index} #{?window_active,1,0} #{?pane_active,1,0}' ",
        "2>/dev/null | awk '$2==1 && $3==1 {print $1; exit}'); ",
        "[ -n \"$p\" ] && tmux capture-pane -p -t \"$p\" -S -200"
    );
    let raw = state
        .sessions
        .run_sidecar(session_id, CMD, None, 4, "抓取全屏程序内容超时")
        .await
        .ok()?;
    let text = raw.trim().to_string();
    if text.is_empty() {
        return None;
    }
    // 200 行 tmux 历史足够判断「刚才发生了什么」，再多只是挤占上下文预算
    Some(tail_chars(&text, 8 * 1024))
}

/// 取末尾 `max` 个字节（切点对齐到字符边界）。抓屏的价值集中在最近几屏，砍头不砍尾。
fn tail_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let cut = s.len() - max;
    // 别切在多字节字符中间
    let start = (cut..s.len()).find(|i| s.is_char_boundary(*i)).unwrap_or(cut);
    format!("（前略）\n{}", &s[start..])
}

/// 把一次执行的原文结果变成喂回模型的反馈。
///
/// 没有退出码这种参考信息了——模型得照着下面的真实输出自己判断这条命令是否成功，
/// 就像人盯着终端看一样。
fn feedback_for(command: &str, label: &str, outcome: &ExecOutcome) -> String {
    let mut s = String::new();
    s.push_str(&format!("在会话「{label}」执行：`{}`\n", command.trim()));
    match outcome.reason {
        ExecEnd::Confirmed => s.push_str("状态：已确认执行完毕（提示符已回到终端）\n"),
        ExecEnd::Silence => s.push_str("状态：长时间没有新输出，按执行完毕处理（未能确认是否真的结束）\n"),
        ExecEnd::Timeout => s.push_str("状态：等待超时，命令可能仍在运行\n"),
        ExecEnd::AwaitingInput => s.push_str(AWAITING_INPUT_STATUS),
        ExecEnd::Cancelled => return format!("{s}结果：已被用户取消，不要重试这条命令。\n"),
    }
    // 保留头部：报错与关键信息通常在最前面，尾部往往是无关的重复
    s.push_str(&format!("输出：\n{}\n", head_chars(&outcome.output, MAX_FEEDBACK_CHARS)));
    push_guess(&mut s, outcome.prompt.as_ref());
    s
}

/// `ExecEnd::AwaitingInput` 的状态行：必须说清「命令没结束」，免得模型当成跑完了。
const AWAITING_INPUT_STATUS: &str =
    "状态：命令**还没有结束**——终端停在一个等待输入的交互提示上（详见下方 prompt_kind / 提示）\n";

/// 把终端分类（`prompt_kind` / `confidence` / `提示`）附到反馈末尾。没有分类就什么都不加。
fn push_guess(s: &mut String, guess: Option<&PromptGuess>) {
    if let Some(g) = guess {
        s.push_str(&g.feedback());
    }
}

/// 对一个会话此刻的缓冲尾部分类。旁路缓冲没开 / 没内容时按空串分类（得到 Unknown）。
fn classify_session(state: &AppState, id: &str, alt: bool) -> PromptGuess {
    let tail = state.agent.context(id, CLASSIFY_TAIL_BYTES).unwrap_or_default();
    prompt::classify(&tail, alt)
}

/// 写入超时的提示文案。独立于 `is_channel_closed`：只是一次写超时，
/// 命令可能根本没发出去也可能发出去了但没收到确认，不等于连接已经报废，
/// 不升级成「别再重试」那套更严厉的措辞。
const WRITE_TIMEOUT_MSG: &str = "写入超时（15 秒内未发出）：连接可能暂时不稳定";

/// 也有可能执行本身就没跑起来（会话关了、拒绝-ID 不存在）。
fn failure_feedback(command: &str, label: &str, why: &str) -> String {
    let mut out = format!(
        "在会话「{label}」执行 `{}` 失败：{}\n请据此调整：换一条命令、换个会话，或直接给结论。\n",
        command.trim(),
        why
    );
    // 断线的会话敲不进任何东西。不点破的话模型只会在这里空转 ——
    // 实测过它连着报两次「输入通道已关闭」还在下发第三条命令。
    if is_channel_closed(why) {
        out.push_str(&format!(
            "注意：这个会话的通道已经关闭，**再往里发命令也不会有结果**，不要重试。\n\
             要恢复它请用 {TOOL_TAB} 工具（op=reconnect，target 填这个会话的 id）；\
             拿不到工具就用围栏写 {{\"action\":\"tab\",\"op\":\"reconnect\",\"target\":\"<该会话id>\"}}。\n\
             不需要它了就直接给结论：标签页本身还在，用户随时能自己点右键重连。\n"
        ));
    } else if is_write_timeout(why) {
        out.push_str(
            "注意：不确定这条命令有没有真正发出去。可以先换一条无副作用的命令\
             （比如 `pwd`）确认连接还活着，再决定要不要重试；\
             如果连续超时，说明连接确实有问题，请用上面说的 tab 工具 reconnect，不要反复硬发。\n",
        );
    }
    out
}

/// 这条失败是不是「会话通道已关闭」。
fn is_channel_closed(why: &str) -> bool {
    why.contains("通道") && (why.contains("关闭") || why.contains("失效") || why.contains("断开"))
}

/// 这条失败是不是「写入超时」（见 `WRITE_TIMEOUT_MSG`）。
fn is_write_timeout(why: &str) -> bool {
    why.contains("写入超时")
}

/// 按**字符**截断并给出提示。按 `char` 而不是字节，否则中文会切出半个字。
fn head_chars(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}\n……（输出过长，已截断 {} / {total} 字符）", total - max)
}

// ——————————————————————————————— 写入通道 ———————————————————————————————

/// 会话的写入通道：本地 PTY 还是 SSH。
enum Sink {
    Local,
    Ssh,
}

/// 判断一个 id 指向哪种会话。**先本地后 SSH**：两者的 id 都由前端生成且互不重叠，
/// 本地是更常见的那一种（启动就是一个本地标签页）。
fn sink_of(state: &AppState, id: &str) -> Option<Sink> {
    if state.local.contains(id) {
        return Some(Sink::Local);
    }
    if state.sessions.contains(id) {
        return Some(Sink::Ssh);
    }
    None
}

/// `SessionManager::write` 本身没有超时（队列满了就老实等，这是正常键入/粘贴
/// 需要的背压）。但 Agent 这里是「发一条命令、等它跑完」，连接已经建立，
/// 正常几毫秒就该写完——如果迟迟写不进去，十有八九是连接半死，不该无限期挂起
/// 等一次重连才可能解套（且重连也未必来得及，见 `SessionRuntime.task`）。
const AGENT_WRITE_TIMEOUT: Duration = Duration::from_secs(15);

/// 提示符基线的长度上限（字符）。超过这么长的末行多半不是提示符，而是一行正文
/// （上一条命令的输出残留），拿它当基线只会永远对不上。
const MAX_PROMPT_CHARS: usize = 200;

/// 往任意一种会话写字节。会话不存在时才报错。
async fn write_to(state: &AppState, id: &str, data: &[u8]) -> Result<(), String> {
    match sink_of(state, id) {
        Some(Sink::Local) => state.local.write(id, data),
        Some(Sink::Ssh) => tokio::time::timeout(
            AGENT_WRITE_TIMEOUT,
            state.sessions.write(id, data.to_vec()),
        )
        .await
        .map_err(|_| WRITE_TIMEOUT_MSG.to_string())?
        .map_err(|e| e.to_string()),
        None => Err(format!("会话不存在或已关闭：{id}")),
    }
}

// ——————————————————————————————— 活体确认 ———————————————————————————————

// ——————————————————————————————— 单条命令执行 ———————————————————————————————

/// 在目标会话上跑一条命令，返回输出与结束原因。
///
/// **命令会原样旁路进 agent 缓冲**：既让 `pending_echo` 抵消 pty 回显，也留下审计。
///
/// 发出去的就是 `cmd\n`，跟人手敲一模一样，屏幕上不多一个字符；结束判定靠
/// 「静默 + 活体确认」（见 `executor.rs` 顶部）。
pub async fn run_one(
    state: &AppState,
    app: &AppHandle,
    session_id: &str,
    command: &str,
    step: usize,
    label: &str,
    target_id: &str,
    cancel: Option<&AtomicBool>,
    announce: bool,
) -> Result<ExecOutcome, String> {
    if let Some(why) = reject_command(command) {
        return Err(why.to_string());
    }
    let cmd = command.trim();

    // 要让这条命令的输出进得了旁路，缓冲必须是开着的。
    // 这里是「Agent 主动要用某个会话」，与全局 autoAttach 无关，故显式打开。
    state.agent.set_enabled(session_id, true);

    // 发命令之前记下提示符：此刻 shell 正停在它那儿等输入，缓冲末行就是它。
    // 命令跑完提示符会重新出现，这就是判定结束的依据（见 `executor.rs` 顶部）。
    let prompt = state
        .agent
        .last_line(session_id)
        .filter(|l| !l.is_empty() && l.chars().count() <= MAX_PROMPT_CHARS);
    tracing::debug!(session_id, command = cmd, prompt = ?prompt, "agent exec: 提示符基线");

    let payload = format!("{cmd}\n");
    let write_started = std::time::Instant::now();
    write_to(state, session_id, payload.as_bytes()).await.inspect_err(|e| {
        tracing::debug!(session_id, command = cmd, error = %e, elapsed_ms = write_started.elapsed().as_millis() as u64, "agent exec: 写入失败");
    })?;
    tracing::debug!(session_id, command = cmd, elapsed_ms = write_started.elapsed().as_millis() as u64, "agent exec: 写入完成，开始等待输出");
    // 喂的是实际发出去的原始字节：这样 pty 的回显才能被 pending_echo 逐字节抵消干净
    state.agent.feed(session_id, payload.as_bytes(), StreamKind::In);

    if announce {
        emit_step(
            app,
            session_id,
            StepPayload {
                target: Some(target_id.into()),
                target_label: Some(label.into()),
                command: Some(cmd.into()),
                ..StepPayload::new("exec", step)
            },
        );
    }

    // 命令本身刚刚喂进缓冲，从这里往后的才是它的输出
    let mark = state.agent.pushed(session_id).unwrap_or_default();
    let wait_started = std::time::Instant::now();
    let mut wait_fut = Box::pin(wait_for_end(
        WaitConfig::default(),
        prompt.as_deref(),
        || state.agent.pushed(session_id).unwrap_or_default(),
        || state.agent.last_line(session_id).unwrap_or_default(),
        || state.agent.context(session_id, CLASSIFY_TAIL_BYTES).unwrap_or_default(),
        || state.agent.since(session_id, mark).unwrap_or_default(),
        || cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false),
    ));
    // 定期报一下「还在等」，而不是让面板上的卡片一动不动——用户没法分清
    // 「正常在跑」和「卡死了」。`wait_for_end` 本身保持纯逻辑不碰 Tauri
    // （便于单测），进度提示在这里用 select! 旁路加，不改它的签名。
    const WAITING_TICK: Duration = Duration::from_secs(5);
    let mut ticker = tokio::time::interval(WAITING_TICK);
    ticker.tick().await; // 吃掉 interval 立即触发的第一下
    let mut elapsed_secs = 0u64;
    let mut outcome = loop {
        tokio::select! {
            o = &mut wait_fut => break o,
            _ = ticker.tick() => {
                elapsed_secs += WAITING_TICK.as_secs();
                if announce {
                    emit_step(
                        app,
                        session_id,
                        StepPayload {
                            target: Some(target_id.into()),
                            target_label: Some(label.into()),
                            command: Some(cmd.into()),
                            elapsed_secs: Some(elapsed_secs),
                            ..StepPayload::new("waiting", step)
                        },
                    );
                }
            }
        }
    };
    outcome.output = outcome.output.trim().to_string();
    tracing::debug!(
        session_id, command = cmd, reason = ?outcome.reason,
        elapsed_ms = wait_started.elapsed().as_millis() as u64,
        "agent exec: 等待结束"
    );

    if announce {
        emit_step(
            app,
            session_id,
            StepPayload {
                target: Some(target_id.into()),
                target_label: Some(label.into()),
                command: Some(cmd.into()),
                output: Some(outcome.output.clone()),
                prompt_kind: outcome.prompt.as_ref().map(|g| g.kind.as_str().to_string()),
                ..StepPayload::new("result", step)
            },
        );
    }
    Ok(outcome)
}

// ——————————————————————————————— 手和眼：按键 / 等待 / 读屏 ———————————————————————————————

/// 按键写入之后，等多久再取一次输出尾部给模型看。
///
/// 太短看不到回显（`:wq` 之后 vim 还没退出），太长拖慢一轮；300ms 是人眼「敲完看一眼」的量级，
/// 要看更晚的结果由模型接一个 `nx_wait`。
const KEYS_SETTLE: Duration = Duration::from_millis(300);

/// 反馈里「按键之后的输出尾部」最多保留多少字节。
const KEYS_TAIL_BYTES: usize = 2000;

/// 读屏结果 / 等待输出尾部的字节上限（喂回模型与向前端索取的都是它）。
const SCREEN_MAX_BYTES: usize = 6 * 1024;

/// 往目标会话写原始按键。**不追加换行、不等命令结束**，只在 ~300ms 后回一段输出尾部。
///
/// 与 `run_one` 一样把发出去的字节喂进旁路缓冲（`In`），审计与 `since(mark)` 才对得上；
/// 区别是这里没有命令边界，缓冲里这一段只是一串按键。
pub async fn send_keys(state: &AppState, session_id: &str, data: &[u8]) -> Result<String, String> {
    state.agent.set_enabled(session_id, true);
    write_to(state, session_id, data).await?;
    state.agent.feed(session_id, data, StreamKind::In);
    // mark 取在 feed 之后：之后的才是这些按键引出的输出
    let mark = state.agent.pushed(session_id).unwrap_or_default();
    tokio::time::sleep(KEYS_SETTLE).await;
    let out = state.agent.since(session_id, mark).unwrap_or_default();
    Ok(out.trim().to_string())
}

/// 按键串会不会「提交一行」，以及这样做的风险原因。`None` = 没有回车，只是往行里加字，无需把关。
///
/// 为什么不直接对键入文本做 `assess`：被提交的那一行**不止是这次键入的** ——
/// 它可能是之前几次按键拼起来的、用户自己敲了一半的，或是 `<Up>` 从历史里翻出来、
/// Tab 补全展开的，这些我们都看不见。只评估本次文本会给人「已经把过关」的错觉。
/// 所以凡含回车一律给出原因（审批模式下由此触发人工批准）；
/// 本次键入的各行若本身命中危险规则，再把具体原因一并列出，方便人判断。
pub fn keys_risk(bytes: &[u8]) -> Option<Vec<String>> {
    if !interact::contains_enter(bytes) {
        return None;
    }
    let mut reasons = vec![
        "按键序列含回车，会提交终端当前行（那一行也可能是之前输入或历史里翻出来的）".to_string(),
    ];
    let text = String::from_utf8_lossy(bytes);
    for line in text.split(['\r', '\n']).map(str::trim).filter(|l| !l.is_empty()) {
        // 控制字符（ESC 序列等）不是命令的一部分，只评估可见文本
        let visible: String = line.chars().filter(|c| !c.is_control()).collect();
        let risk = assess(&visible);
        if risk.dangerous {
            for r in risk.reasons {
                reasons.push(format!("`{visible}`：{r}"));
            }
        }
    }
    Some(reasons)
}

/// 发按键的反馈。
fn keys_feedback(label: &str, shown: &str, tail: &str, guess: Option<&PromptGuess>) -> String {
    let tail = tail.trim();
    let mut s = format!(
        "已向会话「{label}」发送按键：`{shown}`（只写入，不等命令结束，也不保证对方已处理）。\n\
         约 300ms 后的新输出：\n{}\n",
        if tail.is_empty() { "（暂无新输出）".to_string() } else { tail_chars(tail, KEYS_TAIL_BYTES) }
    );
    push_guess(&mut s, guess);
    s
}

/// 等待的结论（给人看，也给模型看）。
fn wait_feedback(
    label: &str,
    pattern: Option<&str>,
    end: WaitEnd,
    output: &str,
    guess: Option<&PromptGuess>,
) -> String {
    let what = pattern.map(|p| format!("「{p}」")).unwrap_or_else(|| "输出静止".to_string());
    let verdict = match end {
        WaitEnd::Matched => format!("命中：新输出里出现了 {what}"),
        WaitEnd::Quiet => match pattern {
            Some(_) => format!("未命中 {what}：输出已静止"),
            None => "输出已静止".to_string(),
        },
        WaitEnd::Timeout => format!("未命中：等 {what} 超时，条件仍未满足"),
        WaitEnd::Cancelled => return format!("等待会话「{label}」的输出时被用户取消。\n"),
    };
    let out = output.trim();
    let mut s = format!(
        "等待会话「{label}」：{verdict}。\n等待期间的新输出（尾部）：\n{}\n",
        if out.is_empty() { "（无）".to_string() } else { tail_chars(out, SCREEN_MAX_BYTES) }
    );
    push_guess(&mut s, guess);
    s
}

/// 读屏的反馈。`rendered=true` 是前端 xterm 渲染后的画面，否则是旁路缓冲的转录（全屏程序里不可信）。
fn screen_feedback(
    label: &str,
    text: &str,
    alt: bool,
    rendered: bool,
    guess: Option<&PromptGuess>,
) -> String {
    let body = text.trim_matches('\n');
    let mut s = if rendered {
        format!(
            "会话「{label}」当前屏幕{}：\n",
            if alt { "（正停在全屏程序 / alternate screen 里）" } else { "" }
        )
    } else {
        format!(
            "界面没有回应读屏请求，以下是会话「{label}」旁路缓冲里的最近记录\
             （是字节流转录而不是渲染后的画面，若在全屏程序里请谨慎参考）：\n"
        )
    };
    s.push_str(&if body.trim().is_empty() {
        "（屏幕上没有文字）".to_string()
    } else {
        tail_chars(body, SCREEN_MAX_BYTES)
    });
    s.push('\n');
    push_guess(&mut s, guess);
    s
}

/// 一张「手和眼」类步骤卡：`output` 为 `None` 是开始（执行中），`Some` 是收尾。
/// 前端按 (kind, target, command) 把收尾合并回开始那张卡上。
#[allow(clippy::too_many_arguments)]
fn emit_interact(
    app: &AppHandle,
    session: &str,
    kind: &'static str,
    step: usize,
    target_id: &str,
    label: &str,
    command: Option<&str>,
    text: Option<String>,
    output: Option<String>,
    guess: Option<&PromptGuess>,
) {
    emit_step(
        app,
        session,
        StepPayload {
            target: Some(target_id.into()),
            target_label: Some(label.into()),
            command: command.map(str::to_string),
            text,
            output,
            prompt_kind: guess.map(|g| g.kind.as_str().to_string()),
            ..StepPayload::new(kind, step)
        },
    );
}

/// 范围闸门（Keys / Wait / Screen 共用）：把 `target` 落到真实会话，越界则先要人批准。
///
/// 返回 `Some((id, 展示名, 是否越过了界))`；`None` = 已落好「未执行」卡并写好反馈，调用方 `continue`。
/// `what` 是越界弹窗里「要做什么」（拿展示名拼）；`skip_cmd` 是未执行卡上的命令位文本；
/// `reasons` 并进越界弹窗（越界又危险只弹一次，同 Exec）。
#[allow(clippy::too_many_arguments)]
async fn gate_target(
    app: &AppHandle,
    state: &AppState,
    runtime: &ActRuntime,
    session: &str,
    step: usize,
    targets: &mut Vec<AgentTarget>,
    target: Option<&str>,
    what: impl Fn(&str) -> String,
    skip_cmd: &str,
    reasons: Vec<String>,
    feedback: &mut String,
) -> Option<(String, String, bool)> {
    match locate(target, targets, session) {
        Ok((id, label)) => Some((id, label, false)),
        Err(id) => {
            let label = out_of_scope_label(&id);
            if !session_alive(state, &id) {
                emit_step(
                    app,
                    session,
                    StepPayload {
                        target: Some(id.clone()),
                        command: Some(skip_cmd.to_string()),
                        reasons: Some(vec!["会话不存在，且不在作用范围内".into()]),
                        ..StepPayload::new("skip", step)
                    },
                );
                feedback.push_str(&format!(
                    "会话 {id} 不在作用范围内，也不存在，「{skip_cmd}」没有执行。\
                     只能操作作用范围内的会话。\n"
                ));
                return None;
            }
            if !ask_beyond_scope(
                app,
                runtime,
                session,
                step,
                Some((&id, &label)),
                &what(&label),
                reasons,
                feedback,
            )
            .await
            {
                return None;
            }
            admit(targets, &id, &label);
            Some((id, label, true))
        }
    }
}

// ——————————————————————————————— 开新会话 ———————————————————————————————

/// 一次「开新会话」请求。
pub struct OpenSpec {
    pub kind: OpenKind,
    pub label: String,
    /// 开好之后顺手跑的第一条命令，可选。
    pub command: Option<String>,
}

/// 开新会话的结果：新会话 id，以及（如果带了 `command`）那条命令的执行结果。
pub struct OpenOutcome {
    pub session_id: String,
    pub label: String,
    pub command_result: Option<Result<ExecOutcome, String>>,
}

fn default_cols() -> u16 {
    120
}
fn default_rows() -> u16 {
    32
}

/// 按 `SessionRecord.authType` 规整出一份 `ConnectRequest` 能用的 `AuthMethod`。
///
/// 与前端 `lib/auth.ts::resolveAuth` 是同一套规则 —— 两边都要能从同一份
/// `SessionRecord` 连上，规则漂移就是「前端能连、agent 连不上」之类的怪异故障。
fn resolve_auth(state: &AppState, record: &store::SessionRecord) -> Result<AuthMethod, String> {
    match record.auth_type.as_str() {
        "password" => {
            // 锁定态下 `get_password` 返回的也是 None，和「没存过」长得一样。
            // 不区分的话，用户会得到一句与事实相反的「未保存密码」，然后去重填
            // 一个其实已经存在的密码 —— 而那一步同样会失败。
            if state.store.vault_locked() {
                return Err("保险库已锁定，请先输入主密码".into());
            }
            let password = state
                .store
                .get_password(&record.id)
                .ok_or_else(|| "未保存密码，请先在该会话里补填密码".to_string())?;
            Ok(AuthMethod::Password { password })
        }
        "key" => {
            let path = record
                .key_path
                .clone()
                .ok_or_else(|| "该会话未配置私钥路径".to_string())?;
            // 口令是「有则用，没有就试试看」：很多私钥本就没加口令，
            // 真要口令而又没有的话，连接会在握手阶段报出明确的认证错误。
            let passphrase = state.store.get_passphrase(&record.id);
            Ok(AuthMethod::PrivateKey { path, passphrase })
        }
        _ => Ok(AuthMethod::Agent),
    }
}

/// 开一个新会话（本地 shell，或连到一个已保存的 SSH 主机），可选顺手跑一条命令。
///
/// 新会话 id 由后端自己铸造（迄今为止所有活会话 id 都来自前端，这是第一个
/// 反过来的方向），随后用 `nx://agent/session-opened`（**全局**主题，不像
/// `nx://agent/step/{id}` 那样按会话分流）通知前端把它接成一个真正的标签页 ——
/// 否则这条会话虽然在后端活着，用户却永远看不到它。全局主题是刻意的：
/// 这条新会话此刻还没有任何标签页在监听它专属的频道，只有前端的顶层
/// 标签管理器会长期订阅，按 payload 里的 `originSessionId` 自行关联。
pub async fn open_session(
    app: &AppHandle,
    state: &AppState,
    origin_session: &str,
    spec: OpenSpec,
) -> Result<OpenOutcome, AgentError> {
    let id = uuid::Uuid::new_v4().to_string();

    match &spec.kind {
        OpenKind::Local => {
            state
                .local
                .spawn(&id, default_cols(), default_rows(), app)
                .map_err(AgentError::BadRequest)?;
        }
        OpenKind::Ssh { session_record_id } => {
            let record = state
                .store
                .get(session_record_id)
                .map_err(|_| AgentError::BadRequest(format!("未找到会话配置：{session_record_id}")))?;
            let auth = resolve_auth(state, &record).map_err(AgentError::BadRequest)?;
            let req = ConnectRequest {
                host: record.host.clone(),
                port: Some(record.port),
                username: record.username.clone(),
                auth: Some(auth),
                cols: default_cols(),
                rows: default_rows(),
                connect_timeout_secs: record.connect_timeout,
                legacy_crypto: None,
                config_id: Some(record.id.clone()),
                sidecar: None,
            };
            state
                .sessions
                .connect(app.clone(), id.clone(), req)
                .await
                .map_err(|e| AgentError::BadRequest(e.to_string()))?;
        }
    }

    let payload = SessionOpenedPayload {
        id: id.clone(),
        origin_session_id: origin_session.to_string(),
        kind: match spec.kind {
            OpenKind::Local => "local",
            OpenKind::Ssh { .. } => "ssh",
        },
        label: spec.label.clone(),
    };
    let _ = app.emit("nx://agent/session-opened", payload);

    let command_result = match spec.command.as_deref() {
        Some(cmd) if !cmd.trim().is_empty() => {
            Some(run_one(state, app, &id, cmd, 0, &spec.label, &id, None, false).await)
        }
        _ => None,
    };

    Ok(OpenOutcome { session_id: id, label: spec.label, command_result })
}

/// `nx://agent/session-opened` 的载荷：通知前端「有一个后端自己开的
/// 新会话，请接一个标签页上去」。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOpenedPayload {
    pub id: String,
    /// 发起这次「开新会话」的那个会话 —— 多半是触发 agent 的那个标签。
    pub origin_session_id: String,
    /// `local` | `ssh`
    pub kind: &'static str,
    pub label: String,
}

/// 把 `open_session` 的结果变成喂回模型的反馈。
fn open_feedback(outcome: &OpenOutcome) -> String {
    let mut s = format!(
        "已打开新会话「{}」（id：{}），已在前端创建对应标签页。\n",
        outcome.label, outcome.session_id
    );
    match &outcome.command_result {
        Some(Ok(exec)) => {
            s.push_str(&feedback_for_open_command(exec));
        }
        Some(Err(e)) => {
            s.push_str(&format!("顺带执行的命令未能跑起来：{e}\n"));
        }
        None => {}
    }
    s
}

fn feedback_for_open_command(outcome: &ExecOutcome) -> String {
    let status = match outcome.reason {
        ExecEnd::Confirmed => "已确认执行完毕（该会话已没有前台进程在跑）".to_string(),
        ExecEnd::Silence => "长时间没有新输出，按执行完毕处理（未能确认是否真的结束）".into(),
        ExecEnd::Timeout => "等待超时，命令可能仍在运行".into(),
        ExecEnd::AwaitingInput => "命令还没有结束——终端停在一个等待输入的交互提示上".into(),
        ExecEnd::Cancelled => return "该命令已被用户取消。\n".into(),
    };
    let mut s = format!(
        "状态：{status}\n输出：\n{}\n",
        head_chars(&outcome.output, MAX_FEEDBACK_CHARS)
    );
    push_guess(&mut s, outcome.prompt.as_ref());
    s
}

// ——————————————————————————————— 审批 ———————————————————————————————

/// Agentic 运行的运行时：一个会话一轮访问的取消标记，以及等待人工批准的登记表。
pub struct ActRuntime {
    inflight: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// request_id → 谁来收「批准 / 拒绝」这个答复
    approvals: Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>,
    /// request_id → 谁来收前端对 tab 动作的回执
    tab_acks: Mutex<HashMap<String, tokio::sync::oneshot::Sender<TabReply>>>,
    /// request_id → 谁来收前端对读屏请求的回执
    screen_acks: Mutex<HashMap<String, tokio::sync::oneshot::Sender<ScreenReply>>>,
    /// request_id → 谁来收用户对密码请求的答复（值经它直达 `run`，再写进 pty）
    secret_acks: Mutex<HashMap<String, tokio::sync::oneshot::Sender<SecretAnswer>>>,
}

impl ActRuntime {
    pub fn new() -> Self {
        Self {
            inflight: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            tab_acks: Mutex::new(HashMap::new()),
            screen_acks: Mutex::new(HashMap::new()),
            secret_acks: Mutex::new(HashMap::new()),
        }
    }

    /// 登记一轮运行。返回 `None` 表示该会话已经有一轮在跑。
    fn begin(&self, session_id: &str) -> Option<Arc<AtomicBool>> {
        let mut guard = self.inflight.lock();
        if guard.contains_key(session_id) {
            return None;
        }
        let flag = Arc::new(AtomicBool::new(false));
        guard.insert(session_id.to_string(), Arc::clone(&flag));
        Some(flag)
    }

    fn finish(&self, session_id: &str) {
        self.inflight.lock().remove(session_id);
    }

    /// 取消某会话正在进行的一轮。返回是否确实有流被置位。
    pub fn cancel(&self, session_id: &str) -> bool {
        match self.inflight.lock().get(session_id) {
            Some(flag) => {
                flag.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    #[allow(dead_code)]
    pub fn is_running(&self, session_id: &str) -> bool {
        self.inflight.lock().contains_key(session_id)
    }

    /// 收下前端的审批答复。**找不到请求说明已经超时**，返回 false 让调用方如实告知。
    pub fn answer(&self, request_id: &str, approved: bool) -> bool {
        let Some(tx) = self.approvals.lock().remove(request_id) else {
            return false;
        };
        // 接收端已经因为超时被 drop 掉了 → send 会失败，同样是「没人接了」
        tx.send(approved).is_ok()
    }

    /// 收前端对 tab 动作的回执。返回 false 表示这个请求已经超时、没人接了。
    pub fn answer_tab(&self, request_id: &str, reply: TabReply) -> bool {
        let Some(tx) = self.tab_acks.lock().remove(request_id) else {
            return false;
        };
        tx.send(reply).is_ok()
    }

    /// 挂一个等 tab 回执的接收端。
    fn register_tab(&self, request_id: &str) -> tokio::sync::oneshot::Receiver<TabReply> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tab_acks.lock().insert(request_id.to_string(), tx);
        rx
    }

    /// 超时或提前放弃时把登记清掉，别让 map 随着轮次慢慢涨。
    fn unregister_tab(&self, request_id: &str) {
        self.tab_acks.lock().remove(request_id);
    }

    /// 收前端对读屏请求的回执。返回 false 表示请求已经超时、没人接了。
    pub fn answer_screen(&self, request_id: &str, reply: ScreenReply) -> bool {
        let Some(tx) = self.screen_acks.lock().remove(request_id) else {
            return false;
        };
        tx.send(reply).is_ok()
    }

    fn register_screen(&self, request_id: &str) -> tokio::sync::oneshot::Receiver<ScreenReply> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.screen_acks.lock().insert(request_id.to_string(), tx);
        rx
    }

    fn unregister_screen(&self, request_id: &str) {
        self.screen_acks.lock().remove(request_id);
    }

    /// 收用户对密码请求的答复。`value = None` 是「跳过」。
    ///
    /// 返回 false = 这个请求已经没人接了（超时 / 本轮已被取消 / id 不对）。
    /// 值在这里就被 `sanitize_secret` 装进 `Zeroizing`，之后不论走哪条分支（校验失败、没人接）
    /// 都会在 drop 时被擦掉。校验失败返回 `Err(原因)` 且**不消耗**登记：让用户改了再提交一次。
    pub fn answer_secret(
        &self,
        request_id: &str,
        value: Option<String>,
    ) -> Result<bool, &'static str> {
        let answer = match value {
            Some(raw) => SecretAnswer::Provided(secret::sanitize_secret(raw)?),
            None => SecretAnswer::Declined,
        };
        let Some(tx) = self.secret_acks.lock().remove(request_id) else {
            return Ok(false);
        };
        // 接收端已经因为超时 / 取消被 drop → send 把答复原样退回 Err，随即 drop 擦掉
        Ok(tx.send(answer).is_ok())
    }

    fn register_secret(&self, request_id: &str) -> tokio::sync::oneshot::Receiver<SecretAnswer> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.secret_acks.lock().insert(request_id.to_string(), tx);
        rx
    }

    fn unregister_secret(&self, request_id: &str) {
        self.secret_acks.lock().remove(request_id);
    }

    /// 挂一个待批请求，返回用来等答复的接收端。
    fn register(&self, request_id: &str) -> tokio::sync::oneshot::Receiver<bool> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.approvals.lock().insert(request_id.to_string(), tx);
        rx
    }

    /// 请求被应答或超时后，把登记清掉（对着这一次 request_id，不影响别的请求）。
    fn unregister(&self, request_id: &str) {
        self.approvals.lock().remove(request_id);
    }
}

impl Default for ActRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// 请人点头。超时（或 UI 崩了导致发送端被 drop）一律按**拒绝**处理。
async fn request_approval(
    app: &AppHandle,
    runtime: &ActRuntime,
    session_id: &str,
    payload: &ApprovePayload,
) -> bool {
    let rx = runtime.register(&payload.request_id);
    let _ = app.emit(&format!("nx://agent/approve/{session_id}"), payload.clone());

    let decision = match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
        Ok(Ok(approved)) => approved,
        // Err = 超时；Ok(Err) = UI 那一头先没了（窗口关了 / 页面重载）
        _ => {
            tracing::warn!(
                "审批请求无人应答，按拒绝处理：{} {}",
                payload.request_id,
                payload.command
            );
            false
        }
    };
    runtime.unregister(&payload.request_id);
    decision
}

/// tab 动作等前端回执的上限。
///
/// 比审批短得多：前端只是改一下 React state，正常几毫秒就回来了。
/// 3 秒还没动静，基本是「没人挂监听」或「主窗口在忙」——
/// 那不是再等等能好的事，得如实告诉用户「这次没确认到」。
const TAB_ACK_TIMEOUT: Duration = Duration::from_secs(3);

/// 把 tab 动作转给前端，并等它回一句话。
///
/// 返回 `None` = 超时或前端没接。**不能把这个当成成功**：
/// 上一轮「说了却没动静」的毛病就是由「发出去就当做到了」养出来的。
async fn request_tab(
    app: &AppHandle,
    runtime: &ActRuntime,
    session_id: &str,
    payload: &TabRequest,
) -> Option<TabReply> {
    let rx = runtime.register_tab(&payload.request_id);
    let _ = app.emit("nx://agent/tab", payload.clone());
    let got = tokio::time::timeout(TAB_ACK_TIMEOUT, rx).await;
    runtime.unregister_tab(&payload.request_id);
    match got {
        Ok(Ok(reply)) => Some(reply),
        _ => {
            tracing::warn!(
                "标签页动作无人回执（来源会话 {session_id}）：{} {}",
                payload.request_id,
                payload.op
            );
            None
        }
    }
}

/// 读屏等前端回执的上限。同 `TAB_ACK_TIMEOUT`：前端只是读一下 xterm 缓冲，几毫秒的事。
const SCREEN_ACK_TIMEOUT: Duration = Duration::from_secs(3);

/// 请前端读一个会话的屏幕。`None` = 超时或前端没接，调用方退回旁路缓冲。
async fn request_screen(
    app: &AppHandle,
    runtime: &ActRuntime,
    session_id: &str,
    payload: &ScreenRequest,
) -> Option<ScreenReply> {
    let rx = runtime.register_screen(&payload.request_id);
    let _ = app.emit("nx://agent/screen", payload.clone());
    let got = tokio::time::timeout(SCREEN_ACK_TIMEOUT, rx).await;
    runtime.unregister_screen(&payload.request_id);
    match got {
        Ok(Ok(reply)) => Some(reply),
        _ => {
            tracing::warn!(
                "读屏请求无人回执（来源会话 {session_id}）：{} → {}",
                payload.request_id,
                payload.target
            );
            None
        }
    }
}

// ——————————————————————————————— Agentic 主循环 ———————————————————————————————

/// 目标列表的兜底：前端一个都没传时，用当前会话。
fn resolve_targets(req: &AgentRunRequest) -> Vec<AgentTarget> {
    if req.targets.is_empty() {
        vec![AgentTarget {
            id: req.session_id.clone(),
            label: "当前会话".into(),
        }]
    } else {
        req.targets.clone()
    }
}

/// 范围外会话在卡片里的占位展示名：后端只认 uuid，范围外的会话前端没有交过名字。
const OUT_OF_SCOPE_LABEL: &str = "范围外会话";

/// `id` 是否在本轮作用范围内。
///
/// 范围只有一个来源：前端 `targets`（用户用 `@` / 选择器勾的），外加用户在越界弹窗里
/// 点过头的会话。模型自己说「用户同意了」不算数 —— 它的话和终端输出里被注入的话没有区别。
fn in_scope(id: &str, targets: &[AgentTarget]) -> bool {
    targets.iter().any(|t| t.id == id)
}

/// 把 `target` 落到真实会话 id 与展示名上。
///
/// 省略 `target` 时落到 `fallback`（用户当前会话），**但它也必须在范围内**：
/// 前端允许把当前会话移出范围，那时「默认当前会话」同样算越界。
/// `Err(id)` = 这个 id 不在范围内，调用方必须先过越界批准才能动它。
fn locate(
    id: Option<&str>,
    targets: &[AgentTarget],
    fallback: &str,
) -> Result<(String, String), String> {
    let want = id.unwrap_or(fallback);
    match targets.iter().find(|t| t.id == want) {
        Some(t) => Ok((t.id.clone(), t.label.clone())),
        None => Err(want.to_string()),
    }
}

/// 越界批准通过后把会话并入范围：同一会话本轮之内不再重复弹窗。
fn admit(targets: &mut Vec<AgentTarget>, id: &str, label: &str) {
    if !in_scope(id, targets) {
        targets.push(AgentTarget { id: id.to_string(), label: label.to_string() });
    }
}

/// 范围外会话的展示名：带上 id 前 8 位，让人在弹窗里至少能和标签页对上号。
fn out_of_scope_label(id: &str) -> String {
    let short: String = id.chars().take(8).collect();
    format!("{OUT_OF_SCOPE_LABEL}（{short}）")
}

/// 只留范围内的标签页，返回 `(范围内清单, 被藏起来的个数)`。
///
/// `TabInfo.id` 就是 `AgentTarget.id`（前端两边都用 tab.id）。
/// 范围外的 id 一个都不给模型：终端输出里被注入的指令正好能拿它当跳板。
fn scoped_tabs(tabs: Vec<TabInfo>, targets: &[AgentTarget]) -> (Vec<TabInfo>, usize) {
    let total = tabs.len();
    let kept: Vec<TabInfo> = tabs.into_iter().filter(|t| in_scope(&t.id, targets)).collect();
    let hidden = total - kept.len();
    (kept, hidden)
}

/// 越界弹窗里「要做什么」的一句话。给人看，所以要点名会话与命令。
fn describe_open(
    kind: &OpenKind,
    label: &str,
    command: Option<&str>,
    ssh_desc: Option<&str>,
) -> String {
    let what = match kind {
        OpenKind::Local => "本地终端".to_string(),
        OpenKind::Ssh { session_record_id } => ssh_desc
            .map(str::to_string)
            .unwrap_or_else(|| format!("SSH 配置 {session_record_id}")),
    };
    match command.map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) => format!("打开新会话「{label}」（{what}）并执行：{c}"),
        None => format!("打开新会话「{label}」（{what}）"),
    }
}

/// 会话是否真的还活着（本地 pty 或 SSH）。
///
/// 范围外且根本不存在的 id（模型编的 / 已关闭的）不值得打扰人去批。
fn session_alive(state: &AppState, id: &str) -> bool {
    state.sessions.contains(id) || state.local.contains(id)
}

/// 越界被拒后喂回模型的话。
fn beyond_scope_feedback(what: &str) -> String {
    format!(
        "「{what}」超出本轮作用范围，用户没有批准（或等待超时），没有执行。\
         请让用户把该会话加入作用范围，或由他自己操作；不要重试，也不要换个说法或换条路径绕过去。\n"
    )
}

/// 越界批准：动作超出了本轮作用范围，必须人点头。
///
/// 与 `danger` 那条批准**刻意分开**：danger 只在「审批」模式下才弹，而作用范围是
/// 用户明确划的边界，自动模式也不能替他越过去。拒绝 / 超时 / 窗口没了一律按拒绝，
/// 落一张 `skip` 卡，并告诉模型别重试、别绕。
///
/// `target` 为 `None` 表示动作的对象还不存在（`open`）。
#[allow(clippy::too_many_arguments)]
async fn ask_beyond_scope(
    app: &AppHandle,
    runtime: &ActRuntime,
    session: &str,
    step: usize,
    target: Option<(&str, &str)>,
    what: &str,
    mut reasons: Vec<String>,
    feedback: &mut String,
) -> bool {
    reasons.insert(0, "超出作用范围：用户没有把它加入本轮作用范围".into());
    let (target_id, target_label) = target.unwrap_or(("", "新会话"));
    let payload = ApprovePayload {
        request_id: uuid::Uuid::new_v4().simple().to_string(),
        kind: "scope",
        command: what.to_string(),
        target: target_id.to_string(),
        target_label: target_label.to_string(),
        reasons: reasons.clone(),
    };
    if request_approval(app, runtime, session, &payload).await {
        return true;
    }
    emit_step(
        app,
        session,
        StepPayload {
            target: (!target_id.is_empty()).then(|| target_id.to_string()),
            target_label: Some(target_label.to_string()),
            command: Some(what.to_string()),
            reasons: Some(reasons),
            ..StepPayload::new("skip", step)
        },
    );
    feedback.push_str(&beyond_scope_feedback(what));
    false
}

/// 把前端回执里的标签页清单整理成模型好读的一段。
///
/// 带上 id 是关键：模型下一步要拿它当 `target`，光有标题是点不中的。
fn tab_list_feedback(tabs: &[TabInfo]) -> String {
    let mut out = String::from(
        "当前标签页（`*` 是用户正在看的那个；下一步的 target 用第三列那个 id）：\n",
    );
    for t in tabs {
        out.push_str(&format!(
            "{}「{}」· {} · {} · {}\n",
            if t.active { "* " } else { "  " },
            t.title,
            t.id,
            if t.kind == "local" { "本地" } else { "SSH" },
            t.status
        ));
    }
    out
}

/// 过程日志：把每一轮的推理、模型原始回复和解析出的动作记到 `agent.log`。
///
/// 只在 `cfg.trace` 打开时由调用方调用（见 `agent::trace`，写入前会脱敏）。
///
/// 模型偶尔「只描述、不下令」，界面上一张卡片都没有 —— 光看界面分不清是它压根没写
/// ```nx-exec 块，还是写了被我们的解析/执行环节拒掉。记下原文和解析出的动作，一次就能定案。
fn trace_round(session: &str, step: usize, reply: &llm::LlmReply, actions: &[AgentAction]) {
    // 推理能否看到取决于服务方是否返回，没有就不记这一段，免得日志里一堆空标题
    if !reply.reasoning.trim().is_empty() {
        trace::record(session, step, "reasoning", &reply.reasoning);
    }
    trace::record(session, step, "reply", &reply.text);
    let mut parsed = String::new();
    for c in &reply.tool_calls {
        parsed.push_str(&format!("tool_call {}({})\n", c.name, c.arguments));
    }
    for a in actions {
        parsed.push_str(&format!("action {a:?}\n"));
    }
    if parsed.is_empty() {
        parsed.push_str("（没有解析出任何动作）\n");
    }
    trace::record(session, step, "actions", &parsed);
}

/// 跑完一整轮 Agentic 问答。过程中只发事件，不通过返回值交付内容。
pub async fn run(
    app: &AppHandle,
    state: &AppState,
    runtime: &ActRuntime,
    cfg: &AgentConfig,
    req: &AgentRunRequest,
) -> Result<(), AgentError> {
    if req.session_id.trim().is_empty() {
        return Err(AgentError::NoSession(req.session_id.clone()));
    }
    if req.question.trim().is_empty() {
        return Err(AgentError::EmptyQuestion);
    }
    llm::validate(cfg)?;

    let Some(cancel) = runtime.begin(&req.session_id) else {
        return Err(AgentError::Busy);
    };
    let acct = cfg.active_account();
    let max_bytes = if acct.max_context_bytes == 0 {
        super::DEFAULT_MAX_CONTEXT_BYTES
    } else {
        acct.max_context_bytes
    } as usize;

    // mut：越界动作经人批准后，目标会并入范围（见 `admit`），本轮后续不再重复弹窗。
    let mut targets = resolve_targets(req);

    // 当前会话停在全屏程序里时，屏幕上只剩程序自己画的东西 —— 真实输出在
    // alternate buffer 里，旁路缓冲和前端快照都拿不到。动手规划之前先补一次
    // 只读抓取；失败就算了，提示词里会退化成「让模型自己抓」。
    let capture = if req.console_alt && state.sessions.contains(&req.session_id) {
        capture_fullscreen(state, &req.session_id).await
    } else {
        None
    };

    let system = IfAgentic::pick(&acct.system_prompt);
    let mut messages = vec![ChatMessage::user(plan_message(
        state,
        req,
        &targets,
        max_bytes,
        capture.as_deref(),
        acct.redact,
    ))];

    let session = req.session_id.clone();
    let trace_on = cfg.trace;
    if trace_on {
        trace::record(&session, 0, "question", &req.question);
    }
    let mut step = 0usize;
    let mut feedback = String::new();
    // 会话 id → 本轮里上一次动过它（exec / 按键 / 等待）之后缓冲的位置，供 `nx_wait` 当起点：
    // 模型思考的那几秒里到达的输出也该算「新输出」，否则等的提示早出现了反而永远等不到。
    let mut marks: HashMap<String, u64> = HashMap::new();
    // 动作块写歪的次数。允许模型改一次，但不许它拿这个把步数刷完。
    let mut misfires = 0usize;
    // 服务方不认 tools 时置 false：此后一律走围栏协议，不再每轮都白试一次。
    let mut use_tools = true;
    let specs = tool_specs();

    loop {
        if cancel.load(Ordering::Relaxed) {
            emit_step(app, &session, StepPayload { text: Some("已取消。".into()), ..StepPayload::new("note", step) });
            break;
        }
        if step >= MAX_STEPS {
            emit_step(
                app,
                &session,
                StepPayload {
                    text: Some(format!("已达到本轮步数上限（{MAX_STEPS} 步），先停下来：由你确认是否继续。")),
                    ..StepPayload::new("note", step)
                },
            );
            break;
        }
        step += 1;

        if !feedback.is_empty() {
            if trace_on {
                trace::record(&session, step, "feedback", &feedback);
            }
            // 所有喂回模型的终端内容（命令输出、屏幕、错误、命令原文）都在这唯一的入口脱敏：
            // 各个动作分支往 feedback 里拼的是原文，漏一处分支就是一次外泄，所以不在分支里逐个处理。
            messages.push(ChatMessage::user(scrub(acct.redact, &feedback)));
            feedback.clear();
        }

        let asked_tools = if use_tools { specs.as_slice() } else { &[] };
        let reply = match llm::complete_reply(acct, &system, &messages, asked_tools, Some(&cancel))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                if trace_on {
                    trace::record(&session, step, "error", &e.to_string());
                }
                // 先把自己这一轮从登记表里摘掉，再报错 —— 否则会话会被永久卡住「运行中」
                runtime.finish(&session);
                let _ = app.emit(&format!("nx://agent/error/{session}"), e.to_string());
                return Err(e);
            }
        };
        // 服务方/模型不认 tools 就永久退回围栏协议：每轮都白试一次既慢，
        // 又会让"带上 tool_choice 却拿不到调用"的怪状态一直挂着。
        if use_tools && !reply.tools_supported {
            use_tools = false;
        }

        // 工具优先：有结构化调用就不再看围栏 —— 模型常常两种都写，
        // 若两边都收，同一件事会被执行两遍。
        let actions: Vec<AgentAction> = if reply.tool_calls.is_empty() {
            extract_actions(&reply.text)
        } else {
            reply.tool_calls.iter().flat_map(actions_from_tool_call).collect()
        };
        if trace_on {
            trace_round(&session, step, &reply, &actions);
        }

        // 进了历史的这条 assistant 消息：带工具调用时不能原样存，
        // 否则下一轮会被接口以「tool_calls 后面必须跟 tool 回执」拒掉。
        messages.push(ChatMessage::assistant(describe_tool_calls(&reply)));

        if actions.is_empty() {
            // 调了工具但一个动作都没产出（多半是缺必填参数，比如 nx_tab 没给 op）。
            // 卡片已经摆出去了，后面却什么都没发生 —— 那正是以前最招骂的那种
            // 「它说要做，然后没有然后」。这里必须说清楚卡在哪。
            if !reply.tool_calls.is_empty() && misfires < MAX_MISFIRE_RETRY {
                misfires += 1;
                emit_step(
                    app,
                    &session,
                    StepPayload {
                        text: Some(
                            "工具调用缺少必要参数，未产生任何动作，已请模型重发。".into(),
                        ),
                        ..StepPayload::new("note", step)
                    },
                );
                feedback.push_str(
                    "你的工具调用没有产生任何动作，多半是缺了必填参数。\
                     nx_exec 必须有 command；nx_send_keys 必须有 keys；nx_tab 必须有 op，\
                     且除 list 外都要带 target；nx_open_tab 必须有 kind，\
                     kind=ssh 时还要带 sessionRecordId。请补齐后重新调用。\n",
                );
                continue;
            }
            // 回复里有围栏却抽不出任何动作 = 协议写歪了，不是「模型想收尾」。
            // 这种情况必须让人看见：以前直接 break，界面上只留下模型那段
            // 「我需要打开一个新的会话…」，看起来就像它答应了却没动手。
            if reply.text.contains(FENCE_OPEN) && misfires < MAX_MISFIRE_RETRY {
                misfires += 1;
                emit_step(
                    app,
                    &session,
                    StepPayload {
                        text: Some(
                            "这一轮的动作块没能解析成可执行指令（围栏里不是约定的 JSON），已请模型重发。".into(),
                        ),
                        ..StepPayload::new("note", step)
                    },
                );
                feedback.push_str(&format!(
                    "你上一条回复里的 {FENCE_OPEN} 块没有解析出任何动作。围栏里必须只有一行 JSON，\
                     例如 {{\"action\":\"exec\",\"command\":\"ls\"}}，不要直接写 shell 代码块。请重发。"
                ));
                continue;
            }
            // 没有动作 = 模型准备收尾了，这段纯文本就是给用户的结论
            let text = strip_action_blocks(&reply.text);
            if !text.trim().is_empty() {
                emit_step(app, &session, StepPayload { text: Some(text), ..StepPayload::new("answer", step) });
            }
            break;
        }

        // 先把「模型调用了什么」摆到界面上，再逐个展示执行过程。
        //
        // 这张卡是原生工具调用独有的：围栏路径只是一段文本，没有结构化的
        // 「调了哪个工具、传了什么参数」，自然也不该有一张凭空造出来的卡。
        for c in &reply.tool_calls {
            emit_step(
                app,
                &session,
                StepPayload {
                    tool: Some(c.name.clone()),
                    tool_label: Some(tool_label(&c.name, &c.arguments).into()),
                    tool_args: Some(c.arguments.clone()),
                    ..StepPayload::new("tool", step)
                },
            );
        }

        for action in actions {
            if cancel.load(Ordering::Relaxed) {
                feedback.push_str("（用户取消了后续动作，请直接给出结论。）\n");
                break;
            }
            // 一轮内的多个动作共用同一个 step 号：UI 按 step 折叠，同一步归在一张卡里
            match action {
                AgentAction::Reject { command, hint } => {
                    // 静默丢弃是最糟的：用户看到模型说「我要开个新标签页」，
                    // 终端里却只留下一行 command not found。这里把「为什么没下发」
                    // 落一张卡，同时把正确用法喂回模型。
                    emit_step(
                        app,
                        &session,
                        StepPayload {
                            command: Some(command.clone()),
                            reasons: Some(vec!["不是 shell 命令，已拦下".into(), hint.clone()]),
                            ..StepPayload::new("skip", step)
                        },
                    );
                    feedback.push_str(&format!(
                        "命令 `{}` 已被拦下、没有下发到终端：{hint}\n",
                        command
                    ));
                }
                AgentAction::Exec { target, command } => {
                    let located = locate(target.as_deref(), &targets, &session);
                    let out_of_scope = located.is_err();
                    let (target_id, label_owned) = located.unwrap_or_else(|id| {
                        let label = out_of_scope_label(&id);
                        (id, label)
                    });
                    let target_label = label_owned.as_str();

                    if let Some(why) = reject_command(&command) {
                        // 静默拒绝是最糟的失败方式：原因只喂回模型，界面上什么都没有，
                        // 用户看到的就是「它说了要做，然后没有然后」。必须落一张卡片。
                        emit_step(
                            app,
                            &session,
                            StepPayload {
                                target: Some(target_id.clone()),
                                target_label: Some(target_label.into()),
                                command: Some(command.trim().into()),
                                reasons: Some(vec![why.to_string()]),
                                ..StepPayload::new("skip", step)
                            },
                        );
                        feedback.push_str(&failure_feedback(&command, target_label, why));
                        continue;
                    }

                    let risk = assess(&command);
                    // 越界闸门：不看 exec mode，自动模式也要弹。
                    // 越界又危险的命令只弹**一次**：把风险原因并进同一张卡，
                    // 批准即同时覆盖两件事，别让人连点两下还分不清各批的什么。
                    let mut danger_cleared = false;
                    if out_of_scope {
                        if !session_alive(state, &target_id) {
                            emit_step(
                                app,
                                &session,
                                StepPayload {
                                    target: Some(target_id.clone()),
                                    command: Some(command.trim().into()),
                                    reasons: Some(vec!["会话不存在，且不在作用范围内".into()]),
                                    ..StepPayload::new("skip", step)
                                },
                            );
                            feedback.push_str(&format!(
                                "会话 {target_id} 不在作用范围内，也不存在，命令 `{}` 没有执行。\
                                 只能操作作用范围内的会话。\n",
                                command.trim()
                            ));
                            continue;
                        }
                        let what = format!("在「{target_label}」执行：{}", command.trim());
                        let reasons = if risk.dangerous { risk.reasons.clone() } else { Vec::new() };
                        if !ask_beyond_scope(
                            app,
                            runtime,
                            &session,
                            step,
                            Some((&target_id, target_label)),
                            &what,
                            reasons,
                            &mut feedback,
                        )
                        .await
                        {
                            continue;
                        }
                        admit(&mut targets, &target_id, target_label);
                        danger_cleared = risk.dangerous;
                    }

                    if cfg.exec_mode.is_approval() && risk.dangerous && !danger_cleared {
                        let request_id = uuid::Uuid::new_v4().simple().to_string();
                        let req_payload = ApprovePayload {
                            request_id: request_id.clone(),
                            kind: "danger",
                            command: command.trim().into(),
                            target: target_id.clone(),
                            target_label: target_label.into(),
                            reasons: risk.reasons.clone(),
                        };
                        let approved = request_approval(app, runtime, &session, &req_payload).await;
                        if trace_on {
                            trace::record(
                                &session,
                                step,
                                "approval",
                                &format!(
                                    "{} · 命令 `{}` · 原因 {:?}",
                                    if approved { "已批准" } else { "未批准（拒绝或超时）" },
                                    command.trim(),
                                    risk.reasons
                                ),
                            );
                        }
                        if !approved {
                            emit_step(
                                app,
                                &session,
                                StepPayload {
                                    target: Some(target_id.clone()),
                                    target_label: Some(target_label.into()),
                                    command: Some(command.trim().into()),
                                    reasons: Some(risk.reasons.clone()),
                                    ..StepPayload::new("skip", step)
                                },
                            );
                            feedback.push_str(&format!(
                                "命令 `{}` 未获执行（人没批准 / 等待超时）。这可能正是必要的一步：\
                                 请要么换个更安全的等价做法，要么在结论里明确告诉用户需要他本人执行这一步。\n",
                                command.trim()
                            ));
                            continue;
                        }
                    }

                    match run_one(
                        state,
                        app,
                        &target_id,
                        &command,
                        step,
                        target_label,
                        &target_id,
                        Some(&cancel),
                        true,
                    )
                    .await
                    {
                        Ok(outcome) => {
                            marks.insert(target_id.clone(), state.agent.pushed(&target_id).unwrap_or_default());
                            let text = feedback_for(&command, target_label, &outcome);
                            feedback.push_str(&text);
                            feedback.push('\n');
                        }
                        Err(e) => {
                            emit_step(
                                app,
                                &session,
                                StepPayload {
                                    target: Some(target_id.clone()),
                                    target_label: Some(target_label.into()),
                                    command: Some(command.trim().into()),
                                    text: Some(e.clone()),
                                    ..StepPayload::new("note", step)
                                },
                            );
                            feedback.push_str(&failure_feedback(&command, target_label, &e));
                        }
                    }
                }

                AgentAction::Tab { op, target, name } => {
                    // 越界闸门：除 list 外，目标不在范围内就要人批准。
                    // list 不指名道姓，改在拿到回执后按范围过滤（见下）。
                    if let Some(id) = target.as_deref() {
                        if !in_scope(id, &targets) {
                            let label = out_of_scope_label(id);
                            let what = match name.as_deref() {
                                Some(n) => format!("{}「{label}」→「{n}」", op.label()),
                                None => format!("{}「{label}」", op.label()),
                            };
                            if !ask_beyond_scope(
                                app,
                                runtime,
                                &session,
                                step,
                                Some((id, &label)),
                                &what,
                                Vec::new(),
                                &mut feedback,
                            )
                            .await
                            {
                                continue;
                            }
                            admit(&mut targets, id, &label);
                        }
                    }

                    // 走到这里 target 要么是 None（list），要么已在范围内。
                    let where_ = target
                        .as_deref()
                        .map(|id| {
                            targets
                                .iter()
                                .find(|t| t.id == id)
                                .map(|t| t.label.clone())
                                .unwrap_or_else(|| id.to_string())
                        })
                        .unwrap_or_else(|| "范围内的标签页".into());

                    emit_step(
                        app,
                        &session,
                        StepPayload {
                            target: target.clone(),
                            target_label: Some(where_.clone()),
                            text: Some(format!("正在{}……", op.label())),
                            ..StepPayload::new("tab", step)
                        },
                    );

                    let payload = TabRequest {
                        request_id: uuid::Uuid::new_v4().simple().to_string(),
                        session_id: session.clone(),
                        op: op.as_str(),
                        op_label: op.label(),
                        target: target.clone(),
                        name: name.clone(),
                    };

                    match request_tab(app, runtime, &session, &payload).await {
                        Some(reply) if reply.ok => {
                            // list 的结果既进卡片也喂回模型 —— 后者才是重点：
                            // 模型拿到清单才知道下一步该点哪个 id。
                            //
                            // list 要先按作用范围过滤，且 detail 也得重写：前端那句
                            // 「当前共 N 个标签页」带着全量个数，会把范围外的存在泄露出去。
                            let (tabs, hidden) = if op == TabOp::List {
                                scoped_tabs(reply.tabs.clone(), &targets)
                            } else {
                                (reply.tabs.clone(), 0)
                            };
                            let detail = if op == TabOp::List {
                                format!(
                                    "作用范围内共 {} 个标签页{}",
                                    tabs.len(),
                                    if hidden > 0 {
                                        format!("（另有 {hidden} 个在作用范围之外，不能操作，也不会列出）")
                                    } else {
                                        String::new()
                                    }
                                )
                            } else if reply.detail.trim().is_empty() {
                                format!("{}完成", op.label())
                            } else {
                                reply.detail.clone()
                            };
                            emit_step(
                                app,
                                &session,
                                StepPayload {
                                    target: target.clone(),
                                    target_label: Some(where_.clone()),
                                    text: Some(detail.clone()),
                                    ..StepPayload::new("tab", step)
                                },
                            );
                            feedback.push_str(&format!("{detail}\n"));
                            if !tabs.is_empty() {
                                feedback.push_str(&tab_list_feedback(&tabs));
                            }
                        }
                        Some(reply) => {
                            let why = if reply.detail.trim().is_empty() {
                                "前端拒绝了这个操作".to_string()
                            } else {
                                reply.detail.clone()
                            };
                            emit_step(
                                app,
                                &session,
                                StepPayload {
                                    target: target.clone(),
                                    target_label: Some(where_.clone()),
                                    command: Some(op.label().into()),
                                    reasons: Some(vec![why.clone()]),
                                    ..StepPayload::new("skip", step)
                                },
                            );
                            feedback.push_str(&format!(
                                "{}失败：{why}。不要重试同一个目标，要么换个 id，\
                                 要么如实告诉用户界面上的实际情况。\n",
                                op.label()
                            ));
                        }
                        None => {
                            // 前端没接（超时 / 主窗口不在）。说清楚，别装作办成了。
                            emit_step(
                                app,
                                &session,
                                StepPayload {
                                    target: target.clone(),
                                    target_label: Some(where_.clone()),
                                    text: Some(format!(
                                        "「{}」没有收到界面回执（主窗口可能没开或正忙），未能确认结果。",
                                        op.label()
                                    )),
                                    ..StepPayload::new("note", step)
                                },
                            );
                            feedback.push_str(&format!(
                                "{}没有收到界面回执，结果未知。不要断言已完成，也不要重复下发。\n",
                                op.label()
                            ));
                        }
                    }
                }

                AgentAction::Keys { target, keys } => {
                    let bytes = match interact::parse_keys(&keys) {
                        Ok(b) => b,
                        Err(why) => {
                            emit_step(
                                app,
                                &session,
                                StepPayload {
                                    command: Some(keys.clone()),
                                    reasons: Some(vec![format!("按键串无效：{why}")]),
                                    ..StepPayload::new("skip", step)
                                },
                            );
                            feedback.push_str(&format!(
                                "按键串 `{keys}` 无效，没有发送：{why}。请修正后重发。\n"
                            ));
                            continue;
                        }
                    };
                    let shown = interact::display_keys(&bytes);
                    // 审批模式下含回车的按键要人点头（原因见 `keys_risk`）；自动模式不把关，与 exec 一致。
                    let risk = if cfg.exec_mode.is_approval() { keys_risk(&bytes) } else { None };
                    let Some((target_id, label, crossed)) = gate_target(
                        app,
                        state,
                        runtime,
                        &session,
                        step,
                        &mut targets,
                        target.as_deref(),
                        |l| format!("向「{l}」发送按键：{shown}"),
                        &shown,
                        risk.clone().unwrap_or_default(),
                        &mut feedback,
                    )
                    .await
                    else {
                        continue;
                    };
                    // 越界弹窗已经把风险原因并在同一张卡里了，批准即覆盖两件事，不再弹第二次
                    if let (Some(reasons), false) = (&risk, crossed) {
                        let req_payload = ApprovePayload {
                            request_id: uuid::Uuid::new_v4().simple().to_string(),
                            kind: "danger",
                            command: format!("发送按键：{shown}"),
                            target: target_id.clone(),
                            target_label: label.clone(),
                            reasons: reasons.clone(),
                        };
                        let approved = request_approval(app, runtime, &session, &req_payload).await;
                        if trace_on {
                            trace::record(
                                &session,
                                step,
                                "approval",
                                &format!(
                                    "{} · 按键 `{shown}` · 原因 {reasons:?}",
                                    if approved { "已批准" } else { "未批准（拒绝或超时）" },
                                ),
                            );
                        }
                        if !approved {
                            emit_step(
                                app,
                                &session,
                                StepPayload {
                                    target: Some(target_id.clone()),
                                    target_label: Some(label.clone()),
                                    command: Some(shown.clone()),
                                    reasons: Some(reasons.clone()),
                                    ..StepPayload::new("skip", step)
                                },
                            );
                            feedback.push_str(&format!(
                                "按键 `{shown}` 未发送（人没批准 / 等待超时）。\
                                 请改成不含回车的输入，或在结论里告诉用户需要他本人按下回车。\n"
                            ));
                            continue;
                        }
                    }

                    emit_interact(app, &session, "keys", step, &target_id, &label, Some(&shown), None, None, None);
                    match send_keys(state, &target_id, &bytes).await {
                        Ok(tail) => {
                            if trace_on {
                                trace::record(&session, step, "keys", &format!("{label}（{target_id}）← {shown}\n{tail}"));
                            }
                            marks.insert(target_id.clone(), state.agent.pushed(&target_id).unwrap_or_default());
                            let guess = classify_session(state, &target_id, false);
                            emit_interact(
                                app, &session, "keys", step, &target_id, &label, Some(&shown),
                                Some("已发送".into()), Some(tail.clone()), Some(&guess),
                            );
                            feedback.push_str(&keys_feedback(&label, &shown, &tail, Some(&guess)));
                            feedback.push('\n');
                        }
                        Err(e) => {
                            emit_step(
                                app,
                                &session,
                                StepPayload {
                                    target: Some(target_id.clone()),
                                    target_label: Some(label.clone()),
                                    command: Some(shown.clone()),
                                    text: Some(e.clone()),
                                    ..StepPayload::new("note", step)
                                },
                            );
                            feedback.push_str(&failure_feedback(&shown, &label, &e));
                        }
                    }
                }

                AgentAction::Wait { target, pattern, timeout_ms, quiet_ms } => {
                    let pat = match pattern.as_deref().map(Pattern::parse).transpose() {
                        Ok(p) => p,
                        Err(why) => {
                            emit_step(
                                app,
                                &session,
                                StepPayload {
                                    command: pattern.clone(),
                                    reasons: Some(vec![format!("pattern 无效：{why}")]),
                                    ..StepPayload::new("skip", step)
                                },
                            );
                            feedback.push_str(&format!(
                                "nx_wait 的 pattern 无效：{why}。子串直接写；正则写成 /正则/（可加 i）。请修正后重发。\n"
                            ));
                            continue;
                        }
                    };
                    let shown = pattern.clone().unwrap_or_else(|| "（等输出静止）".into());
                    let Some((target_id, label, _)) = gate_target(
                        app,
                        state,
                        runtime,
                        &session,
                        step,
                        &mut targets,
                        target.as_deref(),
                        |l| format!("读取「{l}」的新输出（等待：{shown}）"),
                        &shown,
                        Vec::new(),
                        &mut feedback,
                    )
                    .await
                    else {
                        continue;
                    };
                    if !session_alive(state, &target_id) {
                        feedback.push_str(&format!("会话「{label}」不存在或已关闭，无法等待它的输出。\n"));
                        continue;
                    }
                    let spec = WaitSpec::from_args(pat, timeout_ms, quiet_ms);
                    state.agent.set_enabled(&target_id, true);
                    // 起点用「本轮上次动过这个会话之后」：模型思考的那几秒里到的输出也算新输出
                    let mark = marks
                        .get(&target_id)
                        .copied()
                        .or_else(|| state.agent.pushed(&target_id))
                        .unwrap_or_default();
                    emit_interact(
                        app, &session, "wait", step, &target_id, &label, Some(&shown),
                        Some(format!("等待中（最长 {} 秒）……", spec.timeout_ms / 1000)), None, None,
                    );
                    let res = interact::wait_for_pattern(
                        &spec,
                        || state.agent.pushed(&target_id).unwrap_or_default(),
                        || state.agent.since(&target_id, mark).unwrap_or_default(),
                        || cancel.load(Ordering::Relaxed),
                    )
                    .await;
                    marks.insert(target_id.clone(), state.agent.pushed(&target_id).unwrap_or_default());
                    let guess = classify_session(state, &target_id, false);
                    let fb = wait_feedback(&label, pattern.as_deref(), res.end, &res.output, Some(&guess));
                    if trace_on {
                        trace::record(&session, step, "wait", &format!("{label}（{target_id}）{:?} {shown}\n{}", res.end, res.output));
                    }
                    let verdict = match res.end {
                        WaitEnd::Matched => "已命中",
                        WaitEnd::Quiet => "输出已静止",
                        WaitEnd::Timeout => "超时未命中",
                        WaitEnd::Cancelled => "已取消",
                    };
                    emit_interact(
                        app, &session, "wait", step, &target_id, &label, Some(&shown),
                        Some(verdict.into()), Some(tail_chars(res.output.trim(), SCREEN_MAX_BYTES)),
                        Some(&guess),
                    );
                    feedback.push_str(&fb);
                    feedback.push('\n');
                }

                AgentAction::Screen { target } => {
                    let Some((target_id, label, _)) = gate_target(
                        app,
                        state,
                        runtime,
                        &session,
                        step,
                        &mut targets,
                        target.as_deref(),
                        |l| format!("读取「{l}」的当前屏幕"),
                        "screen",
                        Vec::new(),
                        &mut feedback,
                    )
                    .await
                    else {
                        continue;
                    };
                    emit_interact(app, &session, "screen", step, &target_id, &label, None, Some("正在读取屏幕……".into()), None, None);
                    let payload = ScreenRequest {
                        request_id: uuid::Uuid::new_v4().simple().to_string(),
                        session_id: session.clone(),
                        target: target_id.clone(),
                        max_chars: SCREEN_MAX_BYTES,
                    };
                    let reply = request_screen(app, runtime, &session, &payload).await;
                    if let Some(r) = reply.as_ref().filter(|r| !r.ok) {
                        tracing::debug!("读屏被前端拒绝，退回旁路缓冲：{}", r.detail);
                    }
                    let (text, alt, rendered) = match reply {
                        Some(r) if r.ok => (r.text, r.alt, true),
                        // 前端没回 / 没有这个终端：退回旁路缓冲，总比什么都不给强
                        _ => (
                            state
                                .agent
                                .context(&target_id, SCREEN_MAX_BYTES)
                                .or_else(|| state.agent.snapshot(&target_id))
                                .unwrap_or_default(),
                            false,
                            false,
                        ),
                    };
                    if trace_on {
                        trace::record(&session, step, "screen", &format!("{label}（{target_id}）{}\n{text}", if rendered { "xterm" } else { "旁路缓冲" }));
                    }
                    // 渲染后的屏幕才是真画面；退回旁路缓冲时同样按缓冲分类（全屏程序里不可信，置信度本就低）
                    let guess = prompt::classify(&text, alt);
                    emit_interact(
                        app, &session, "screen", step, &target_id, &label, None,
                        Some(match (rendered, alt) {
                            (true, true) => "已读取屏幕（全屏程序）".into(),
                            (true, false) => "已读取屏幕".into(),
                            (false, _) => "界面未回应，改读旁路缓冲".into(),
                        }),
                        Some(tail_chars(text.trim_matches('\n'), SCREEN_MAX_BYTES)),
                        Some(&guess),
                    );
                    feedback.push_str(&screen_feedback(&label, &text, alt, rendered, Some(&guess)));
                    feedback.push('\n');
                }

                AgentAction::Secret { target, reason } => {
                    let reason = secret::clean_reason(reason.as_deref());
                    // 与 Keys / Wait / Screen 同一道范围闸门：越界要先人批准。
                    // 批准的只是「往那个会话输入」；密码本身还要用户在卡片里亲手输。
                    let Some((target_id, label, _)) = gate_target(
                        app,
                        state,
                        runtime,
                        &session,
                        step,
                        &mut targets,
                        target.as_deref(),
                        |l| format!("请你向「{l}」输入密码（原因：{reason}）"),
                        "nx_secret",
                        Vec::new(),
                        &mut feedback,
                    )
                    .await
                    else {
                        continue;
                    };
                    if !session_alive(state, &target_id) {
                        feedback.push_str(&format!("会话「{label}」不存在或已关闭，无法输入密码。\n"));
                        continue;
                    }
                    state.agent.set_enabled(&target_id, true);

                    // 卡片上提示终端是否真的在要密码：不是时输入可能被回显。
                    let guess = classify_session(state, &target_id, false);
                    let request_id = uuid::Uuid::new_v4().simple().to_string();
                    let rx = runtime.register_secret(&request_id);
                    emit_interact(
                        app, &session, "secret", step, &target_id, &label, Some(&reason),
                        Some("等待用户输入密码……".into()), None, Some(&guess),
                    );
                    let _ = app.emit(
                        &format!("nx://agent/secret/{session}"),
                        SecretRequest {
                            request_id: request_id.clone(),
                            target: target_id.clone(),
                            target_label: label.clone(),
                            reason: reason.clone(),
                            prompt_kind: Some(guess.kind.as_str().to_string()),
                            timeout_secs: secret::SECRET_TIMEOUT.as_secs(),
                        },
                    );
                    let answered =
                        secret::await_secret(rx, secret::SECRET_TIMEOUT, || cancel.load(Ordering::Relaxed))
                            .await;
                    runtime.unregister_secret(&request_id);

                    // 写入前记起点：之后的才是这个密码引出的输出
                    let mark = state.agent.pushed(&target_id).unwrap_or_default();
                    let (status, tail) = secret::deliver(
                        answered,
                        |payload| {
                            let target_id = target_id.clone();
                            async move { write_to(state, &target_id, &payload).await }
                        },
                        secret::SECRET_SETTLE,
                        || state.agent.since(&target_id, mark).unwrap_or_default(),
                    )
                    .await;
                    marks.insert(target_id.clone(), state.agent.pushed(&target_id).unwrap_or_default());
                    let after = classify_session(state, &target_id, false);
                    // trace 里只记状态与脱敏后的尾部，**没有**密码（`trace::record` 还会再脱敏一遍）
                    if trace_on {
                        trace::record(
                            &session,
                            step,
                            "secret",
                            &format!("{label}（{target_id}）{}\n{tail}", status.code()),
                        );
                    }
                    emit_step(
                        app,
                        &session,
                        StepPayload {
                            target: Some(target_id.clone()),
                            target_label: Some(label.clone()),
                            command: Some(reason.clone()),
                            text: Some(status.label()),
                            output: if tail.trim().is_empty() {
                                None
                            } else {
                                Some(tail_chars(tail.trim(), SCREEN_MAX_BYTES))
                            },
                            prompt_kind: Some(after.kind.as_str().to_string()),
                            secret_status: Some(status.code().to_string()),
                            ..StepPayload::new("secret", step)
                        },
                    );
                    feedback.push_str(&secret::secret_feedback(&label, &status, &tail, Some(&after)));
                    feedback.push('\n');
                }

                AgentAction::Export { target, deep } => {
                    let located = locate(target.as_deref(), &targets, &session);
                    let out_of_scope = located.is_err();
                    let (target_id, label_owned) = located.unwrap_or_else(|id| {
                        let label = out_of_scope_label(&id);
                        (id, label)
                    });
                    let target_label = label_owned.as_str();

                    // 导出 = 把整份会话记录交给模型，比 exec 一条命令泄露得更彻底，同样要过越界闸门。
                    if out_of_scope {
                        if !session_alive(state, &target_id) {
                            emit_step(
                                app,
                                &session,
                                StepPayload {
                                    target: Some(target_id.clone()),
                                    command: Some("export".into()),
                                    reasons: Some(vec!["会话不存在，且不在作用范围内".into()]),
                                    ..StepPayload::new("skip", step)
                                },
                            );
                            feedback.push_str(&format!(
                                "会话 {target_id} 不在作用范围内，也不存在，没有导出。\
                                 只能操作作用范围内的会话。\n"
                            ));
                            continue;
                        }
                        let what = format!("导出「{target_label}」的完整会话记录");
                        if !ask_beyond_scope(
                            app,
                            runtime,
                            &session,
                            step,
                            Some((&target_id, target_label)),
                            &what,
                            Vec::new(),
                            &mut feedback,
                        )
                        .await
                        {
                            continue;
                        }
                        admit(&mut targets, &target_id, target_label);
                    }

                    emit_step(
                        app,
                        &session,
                        StepPayload {
                            target: Some(target_id.clone()),
                            target_label: Some(target_label.into()),
                            command: Some(if deep { "export --deep".into() } else { "export".into() }),
                            ..StepPayload::new("export", step)
                        },
                    );
                    match capture::export_session(state, &target_id, deep).await {
                        Some(text) => {
                            feedback.push_str(&format!(
                                "已导出会话「{target_label}」迄今为止的全部输出{}：\n{}\n\n",
                                if deep { "（含 tmux/docker 深度抓取）" } else { "" },
                                head_chars(&text, MAX_FEEDBACK_CHARS)
                            ));
                        }
                        None => {
                            feedback.push_str(&format!(
                                "导出会话「{target_label}」没有拿到任何内容：该会话不存在，\
                                 或从未开启旁路、也没有可供深度抓取的 tmux/docker 现场。\n"
                            ));
                        }
                    }
                }

                AgentAction::Open { kind, label, command } => {
                    let label = label.unwrap_or_else(|| "新会话".to_string());

                    // 前端对 session-opened 只负责接标签页、不做任何确认，所以这里是 open 唯一的
                    // 人工关口：新会话不在任何人划的范围里，开它（尤其带命令）必须先点头。
                    // 弹窗里写明会连到哪、跑什么；SSH 配置名从库里查，查不到才退回 id。
                    let ssh_desc = match &kind {
                        OpenKind::Ssh { session_record_id } => state
                            .store
                            .get(session_record_id)
                            .ok()
                            .map(|r| format!("SSH：{} {}@{}:{}", r.name, r.username, r.host, r.port)),
                        OpenKind::Local => None,
                    };
                    let what = describe_open(&kind, &label, command.as_deref(), ssh_desc.as_deref());
                    // 带的命令若本身危险，把原因一并摆在这张卡上
                    let reasons = command
                        .as_deref()
                        .map(assess)
                        .filter(|r| r.dangerous)
                        .map(|r| r.reasons)
                        .unwrap_or_default();
                    if !ask_beyond_scope(
                        app,
                        runtime,
                        &session,
                        step,
                        None,
                        &what,
                        reasons,
                        &mut feedback,
                    )
                    .await
                    {
                        continue;
                    }

                    emit_step(
                        app,
                        &session,
                        StepPayload {
                            text: Some(format!("正在打开新会话「{label}」……")),
                            ..StepPayload::new("open", step)
                        },
                    );
                    let spec = OpenSpec { kind, label, command };
                    match open_session(app, state, &session, spec).await {
                        Ok(outcome) => {
                            // 人点过头的新会话并入范围：之后对它 exec / tab 不再重复弹窗。
                            admit(&mut targets, &outcome.session_id, &outcome.label);
                            // 成功也要补一条：前端确实会接出标签页，但那张卡片
                            // 留在「正在打开」上会让人以为它还没完成。
                            emit_step(
                                app,
                                &session,
                                StepPayload {
                                    target: Some(outcome.session_id.clone()),
                                    target_label: Some(outcome.label.clone()),
                                    text: Some(format!(
                                        "已打开「{}」，标签页已就位（id：{}）",
                                        outcome.label, outcome.session_id
                                    )),
                                    ..StepPayload::new("open", step)
                                },
                            );
                            feedback.push_str(&open_feedback(&outcome));
                        }
                        Err(e) => {
                            // 失败必须在**界面上**说出来，不能只喂回模型：
                            // 模型收到「打不开」之后很可能改用 open -a Terminal
                            // 之类的命令绕过去，再宣称成功 —— 用户只会看到
                            // 应用外面莫名多了个终端窗口，而面板里什么线索都没有。
                            emit_step(
                                app,
                                &session,
                                StepPayload {
                                    text: Some(format!("打开新会话失败：{e}")),
                                    ..StepPayload::new("note", step)
                                },
                            );
                            feedback.push_str(&format!("打开新会话失败：{e}\n"));
                        }
                    }
                }
            }
        }
    }

    runtime.finish(&session);
    Ok(())
}

/// 挑这一轮该用哪份系统提示词。
///
/// 返回 owned String 而不是 `&str`：用户写了自定义提示词时，成品是
/// 「他的提示词 + 必需的动作协议」，拼接只能是我这边的活。
struct IfAgentic;

impl IfAgentic {
    fn pick(custom: &str) -> String {
        if custom.trim().is_empty() {
            agentic_system_prompt()
        } else {
            // 协议必须是**完整版**：曾经这里只挂一段压缩说明，用户若写着
            // 「只给结论与必要命令」，模型就退回往代码块里抄 shell，界面上什么都不发生。
            format!("{}\n\n{ACTION_PROTOCOL}\n\n{CUSTOM_TAIL}", custom.trim_end())
        }
    }
}

/// 挂在用户自定义提示词后面的补充说明。
///
/// 明确告诉模型：协议优先于用户提示词里关于输出格式的任何要求。
const CUSTOM_TAIL: &str = concat!(
    "以上动作协议优先于上文关于「输出格式 / 只给结论」的任何要求：",
    "要真的动手，就必须写成围栏里那一行 JSON，而不是把命令贴在回复正文里。",
    "不写 target 表示落到用户当前会话。执行结果会回给你，据此继续。",
    "不可逆操作（rm -rf / mkfs / DROP 等）除非用户明确要求，否则不要执行。",
    "查清了直接给中文结论，不要再追加无谓的探查。"
);

/// 把回复里的 ```nx-exec 块摘掉，剩下就是给人看的结论。
///
/// 用于「模型一面下令一面顺手写了段说明」的情况：步数耗完之后这些块执行不了，
/// 留在答案里只是一堆噪音。
fn strip_action_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if !inside {
            if trimmed.starts_with(FENCE_OPEN) {
                inside = true;
                continue;
            }
            out.push_str(line);
            out.push('\n');
        } else if trimmed.starts_with("```") {
            inside = false;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(json: &str) -> String {
        format!("```nx-exec\n{json}\n```\n")
    }

    #[test]
    fn single_action_is_parsed() {
        let acts = extract_actions(&block(r#"{"target":"s1","command":"df -h"}"#));
        assert_eq!(
            acts,
            vec![AgentAction::Exec { target: Some("s1".into()), command: "df -h".into() }]
        );
    }

    #[test]
    fn missing_target_falls_back_to_current() {
        let acts = extract_actions(&block(r#"{"command":"uptime"}"#));
        assert_eq!(acts, vec![AgentAction::Exec { target: None, command: "uptime".into() }]);
    }

    /// 多主机 × 多命令：目标在外层，同一台机器上的命令连着跑
    #[test]
    fn multi_target_expands_target_outer() {
        let acts = extract_actions(&block(
            r#"{"targets":["a","b"],"commands":["uptime","whoami"]}"#,
        ));
        assert_eq!(acts.len(), 4);
        assert_eq!(acts[0], AgentAction::Exec { target: Some("a".into()), command: "uptime".into() });
        assert_eq!(acts[1], AgentAction::Exec { target: Some("a".into()), command: "whoami".into() });
        assert_eq!(acts[2], AgentAction::Exec { target: Some("b".into()), command: "uptime".into() });
        assert_eq!(acts[3], AgentAction::Exec { target: Some("b".into()), command: "whoami".into() });
    }

    /// 模型爱在围栏语言位写 json —— 不许因此整块失效
    #[test]
    fn fence_language_suffix_is_tolerated() {
        let acts = extract_actions("```nx-exec json\n{\"command\":\"ls\"}\n```\n");
        assert_eq!(acts, vec![AgentAction::Exec { target: None, command: "ls".into() }]);
    }

    // —————————————— export / open 动作 ——————————————

    #[test]
    fn export_action_is_parsed_with_default_shallow() {
        let acts = extract_actions(&block(r#"{"action":"export","target":"s1"}"#));
        assert_eq!(acts, vec![AgentAction::Export { target: Some("s1".into()), deep: false }]);
    }

    #[test]
    fn export_action_honors_deep_flag_and_missing_target() {
        let acts = extract_actions(&block(r#"{"action":"export","deep":true}"#));
        assert_eq!(acts, vec![AgentAction::Export { target: None, deep: true }]);
    }

    #[test]
    fn open_local_action_is_parsed() {
        let acts = extract_actions(&block(
            r#"{"action":"open","kind":"local","label":"调试","command":"uptime"}"#,
        ));
        assert_eq!(
            acts,
            vec![AgentAction::Open {
                kind: OpenKind::Local,
                label: Some("调试".into()),
                command: Some("uptime".into()),
            }]
        );
    }

    #[test]
    fn open_ssh_action_requires_session_record_id() {
        // 没给 sessionRecordId：宁可什么都不做，也不能凭空连一个主机
        let acts = extract_actions(&block(r#"{"action":"open","kind":"ssh"}"#));
        assert!(acts.is_empty());

        let acts = extract_actions(&block(
            r#"{"action":"open","kind":"ssh","sessionRecordId":"rec-1"}"#,
        ));
        assert_eq!(
            acts,
            vec![AgentAction::Open {
                kind: OpenKind::Ssh { session_record_id: "rec-1".into() },
                label: None,
                command: None,
            }]
        );
    }

    #[test]
    fn missing_action_field_defaults_to_exec() {
        // 老协议（没有 action 字段）必须照旧解析成 Exec —— 不能因为加了新字段就破坏兼容
        let acts = extract_actions(&block(r#"{"target":"s1","command":"whoami"}"#));
        assert_eq!(acts, vec![AgentAction::Exec { target: Some("s1".into()), command: "whoami".into() }]);
    }

    // ——————————————————— tab 动作（后端只转达，由前端执行） ———————————————————

    #[test]
    fn tab_list_needs_no_target() {
        let acts = extract_actions(&block(r#"{"action":"tab","op":"list"}"#));
        assert_eq!(acts, vec![AgentAction::Tab { op: TabOp::List, target: None, name: None }]);
    }

    #[test]
    fn tab_focus_close_reconnect_carry_target() {
        for (op, want) in [
            ("focus", TabOp::Focus),
            ("close", TabOp::Close),
            ("reconnect", TabOp::Reconnect),
        ] {
            let acts = extract_actions(&block(&format!(
                r#"{{"action":"tab","op":"{op}","target":"tab-1"}}"#
            )));
            assert_eq!(
                acts,
                vec![AgentAction::Tab { op: want, target: Some("tab-1".into()), name: None }],
                "op={op}"
            );
        }
    }

    #[test]
    fn tab_rename_requires_a_name() {
        let ok = extract_actions(&block(
            r#"{"action":"tab","op":"rename","target":"tab-1","name":"日志"}"#,
        ));
        assert_eq!(
            ok,
            vec![AgentAction::Tab {
                op: TabOp::Rename,
                target: Some("tab-1".into()),
                name: Some("日志".into())
            }]
        );
        // 没名字的改名毫无意义，且执行层没法「尽量做」，直接不产生动作
        assert!(extract_actions(&block(
            r#"{"action":"tab","op":"rename","target":"tab-1"}"#
        ))
        .is_empty());
    }

    #[test]
    fn tab_without_target_is_dropped() {
        assert!(extract_actions(&block(r#"{"action":"tab","op":"close"}"#)).is_empty());
        assert!(extract_actions(&block(r#"{"action":"tab"}"#)).is_empty(), "缺 op");
        // 不认识的 op 不能被当成 exec 去跑
        assert!(extract_actions(&block(r#"{"action":"tab","op":"explode","target":"t"}"#)).is_empty());
    }

    /// 回执通道：挂上 → 应答 → 拿到；重复应答第二次应返回 false（已超时/已取走）
    #[test]
    fn tab_ack_round_trip() {
        let rt = ActRuntime::new();
        let rx = rt.register_tab("rq1");
        let reply = TabReply {
            ok: true,
            detail: "已切到「日志」".into(),
            tabs: vec![TabInfo {
                id: "tab-1".into(),
                title: "日志".into(),
                kind: "ssh".into(),
                status: "connected".into(),
                active: true,
            }],
        };
        assert!(rt.answer_tab("rq1", reply));
        let got = rx.blocking_recv().expect("应收到回执");
        assert!(got.ok);
        assert_eq!(got.tabs.len(), 1);
        // 取走之后再应答 = 没人接了
        assert!(!rt.answer_tab("rq1", TabReply::default()));
    }

    #[test]
    fn tab_list_feedback_mentions_ids_and_active_mark() {
        let out = tab_list_feedback(&[
            TabInfo {
                id: "tab-1".into(),
                title: "日志".into(),
                kind: "ssh".into(),
                status: "connected".into(),
                active: true,
            },
            TabInfo {
                id: "tab-2".into(),
                title: "本地".into(),
                kind: "local".into(),
                status: "connected".into(),
                active: false,
            },
        ]);
        assert!(out.contains("tab-1") && out.contains("tab-2"), "必须带 id，否则模型点不中");
        assert!(out.contains("* 「日志」"), "当前标签页要标出来");
        assert!(out.contains("本地"));
    }

    #[test]
    fn plain_code_blocks_are_never_actions() {
        let text = "请先自己试一下：\n```bash\nrm -rf /\n```\n我再去帮你看日志。\n";
        assert!(extract_actions(text).is_empty(), "普通代码块不该被当成要执行的动作");
    }

    #[test]
    fn broken_json_yields_nothing() {
        assert!(extract_actions(&block("{not json")).is_empty());
        assert!(extract_actions(&block(r#"{"target":"s1"}"#)).is_empty(), "没有 command 就不算动作");
    }

    // —————————————— 围栏里是裸 shell（现场抓到的真实回复） ——————————————

    /// 用户上报的那次「说要开 session 但什么都没发生」的原始回复：
    /// 围栏写的是 nx-exec，里面却是裸 shell。以前 JSON 解析失败 → 0 个动作 → 静默，
    /// 界面上只留下模型那段自说自话。
    #[test]
    fn raw_shell_block_is_executed_instead_of_dropped() {
        let reply = concat!(
            "我需要打开一个新的终端会话，复制当前的本地终端。\n\n",
            "```nx-exec\necho \"Opening new session...\"\n```\n\n",
            "让我在当前会话中确认环境信息：\n\n",
            "```nx-exec\npwd && whoami\n```\n",
        );
        let acts = extract_actions(reply);
        assert_eq!(acts.len(), 2, "两个裸 shell 块都该被执行，实际：{acts:?}");
        assert_eq!(
            acts[0],
            AgentAction::Exec { target: None, command: "echo \"Opening new session...\"".into() }
        );
        assert_eq!(acts[1], AgentAction::Exec { target: None, command: "pwd && whoami".into() });
    }

    #[test]
    fn raw_shell_single_line_block_is_executed() {
        let acts = extract_actions("```nx-exec\nexit\n```\n");
        assert_eq!(acts, vec![AgentAction::Exec { target: None, command: "exit".into() }]);
    }

    /// 多行块按行拆：`reject_command` 拒收换行，整块塞进去只会被拒。
    /// 顺手抄来的 `$ ` 提示符、空行、注释都不是命令。
    #[test]
    fn raw_shell_block_splits_lines_and_drops_noise() {
        let acts = extract_actions("```nx-exec\n# 先看看目录\n$ ls -la\n\nuptime\n```\n");
        assert_eq!(acts.len(), 2, "只该剩 ls -la 与 uptime，实际：{acts:?}");
        assert_eq!(acts[0], AgentAction::Exec { target: None, command: "ls -la".into() });
        assert_eq!(acts[1], AgentAction::Exec { target: None, command: "uptime".into() });
    }

    /// 想写 JSON 却写坏了（以 `{` 开头）—— 绝不退化成 shell：
    /// 否则这半截会被原样敲进终端。
    #[test]
    fn truncated_json_is_not_echoed_into_the_shell() {
        let acts = extract_actions(&block(r#"{"target":"s1","command": "ls"#));
        assert!(acts.is_empty(), "半截 JSON 不能被当成命令执行");
    }

    #[test]
    fn unterminated_fence_is_still_attempted() {
        let acts = extract_actions("```nx-exec\n{\"command\":\"pwd\"}");
        assert_eq!(acts, vec![AgentAction::Exec { target: None, command: "pwd".into() }]);
    }

    #[test]
    fn action_count_is_capped() {
        let mut commands = Vec::new();
        for i in 0..50 {
            commands.push(format!("echo {i}"));
        }
        let json = serde_json::json!({ "commands": commands }).to_string();
        assert_eq!(extract_actions(&block(&json)).len(), MAX_ACTIONS);
    }

    #[test]
    fn multiline_and_blank_commands_are_rejected() {
        assert_eq!(reject_command(""), Some("命令为空"));
        assert_eq!(reject_command("   "), Some("命令为空"));
        assert!(reject_command("ls\nrm -rf /").is_some(), "换行必须拒绝");
        assert!(reject_command("ls\r").is_some(), "回车必须拒绝");
        assert!(reject_command("bad\0cmd").is_some());
        // ESC 能在屏幕上把这条命令画成别的东西 —— 必须拒
        assert!(reject_command("ls\u{1b}]0;ok\u{7}").is_some());
        assert_eq!(reject_command("df -h"), None);
    }

    /// rd 带头的一组合法命令不能被误杀（含管道、引号、中文场景）
    #[test]
    fn normal_commands_pass_validation() {
        for cmd in [
            "ls -la /var/log",
            "grep -R 'error' /var/log/nginx --include='*.log' | head -50",
            "ss -tlnp | grep :8080",
            "journalctl -u nginx --since '10 min ago' --no-pager",
        ] {
            assert_eq!(reject_command(cmd), None, "{cmd} 不该被拒绝");
        }
    }

    #[test]
    fn action_blocks_are_stripped_from_the_final_answer() {
        let reply = "先看一下磁盘：\n```nx-exec\n{\"command\":\"df -h\"}\n```\n\n已经很清楚了。\n";
        assert_eq!(strip_action_blocks(reply), "先看一下磁盘：\n\n已经很清楚了。");
    }

    #[test]
    fn head_chars_caps_and_marks() {
        let long = "x".repeat(10);
        assert_eq!(head_chars(&long, 20), long);
        let capped = head_chars(&long, 4);
        assert!(capped.starts_with("xxxx"));
        assert!(capped.contains("6 / 10"), "{capped}");
    }

    /// 中文截断不能切出半个字
    #[test]
    fn head_chars_respects_char_boundaries() {
        let s = "中文输出内容";
        assert_eq!(head_chars(s, 2), "中文\n……（输出过长，已截断 4 / 6 字符）");
    }

    #[test]
    fn feedback_reports_confirmed_status_and_output() {
        let out = ExecOutcome {
            output: "err\n".into(),
            reason: ExecEnd::Confirmed,
            prompt: None,
        };
        let text = feedback_for("false", "web-01", &out);
        assert!(text.contains("false"));
        assert!(text.contains("已确认执行完毕"));
        assert!(text.contains("err"));
        assert!(text.contains("web-01"));
    }

    #[test]
    fn cancellation_feedback_tells_model_not_to_retry() {
        let out = ExecOutcome { output: String::new(), reason: ExecEnd::Cancelled, prompt: None };
        assert!(feedback_for("sleep 100", "x", &out).contains("不要重试"));
    }

    #[test]
    fn silence_fallback_is_worded_as_unconfirmed() {
        let out = ExecOutcome { output: "alive".into(), reason: ExecEnd::Silence, prompt: None };
        let text = feedback_for("top", "x", &out);
        assert!(text.contains("未能确认"));
        assert!(text.contains("alive"));
    }

    fn scope_targets() -> Vec<AgentTarget> {
        vec![
            AgentTarget { id: "a".into(), label: "web-01".into() },
            AgentTarget { id: "b".into(), label: "db-01".into() },
        ]
    }

    fn tab(id: &str, active: bool) -> TabInfo {
        TabInfo {
            id: id.into(),
            title: format!("tab-{id}"),
            kind: "ssh".into(),
            status: "connected".into(),
            active,
        }
    }

    #[test]
    fn locate_passes_in_scope_targets() {
        let targets = scope_targets();
        assert_eq!(locate(Some("b"), &targets, "a"), Ok(("b".to_string(), "db-01".to_string())));
        assert!(in_scope("a", &targets) && in_scope("b", &targets));
    }

    /// 范围外的 id 不能再被原样放行成「未知会话」：这是越界的口子
    #[test]
    fn locate_rejects_out_of_scope_targets() {
        let targets = scope_targets();
        assert_eq!(locate(Some("ghost"), &targets, "a"), Err("ghost".to_string()));
        assert!(!in_scope("ghost", &targets));
    }

    #[test]
    fn locate_without_target_uses_current_session_only_if_in_scope() {
        let targets = scope_targets();
        assert_eq!(locate(None, &targets, "a"), Ok(("a".to_string(), "web-01".to_string())));
        // 当前会话被用户移出了范围：省略 target 的默认落点同样算越界
        assert_eq!(locate(None, &targets, "cur"), Err("cur".to_string()));
    }

    #[test]
    fn admitted_target_joins_scope_once() {
        let mut targets = scope_targets();
        assert!(locate(Some("c"), &targets, "a").is_err());
        admit(&mut targets, "c", "cache-01");
        assert_eq!(locate(Some("c"), &targets, "a"), Ok(("c".to_string(), "cache-01".to_string())));
        // 重复批准同一会话不产生重复项
        admit(&mut targets, "c", "cache-01");
        assert_eq!(targets.iter().filter(|t| t.id == "c").count(), 1);
        assert_eq!(targets.len(), 3);
    }

    #[test]
    fn scoped_tabs_hides_out_of_scope_ids() {
        let targets = scope_targets();
        let (kept, hidden) = scoped_tabs(vec![tab("a", true), tab("x", false), tab("b", false)], &targets);
        assert_eq!(kept.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(hidden, 1);
        // 范围外的 id 不会出现在喂回模型的文本里
        let text = tab_list_feedback(&kept);
        assert!(!text.contains("tab-x") && !text.contains("「tab-x」"));
    }

    #[test]
    fn scoped_tabs_follows_admitted_targets() {
        let mut targets = scope_targets();
        let tabs = vec![tab("a", true), tab("new", false)];
        assert_eq!(scoped_tabs(tabs.clone(), &targets).0.len(), 1);
        admit(&mut targets, "new", "新会话");
        assert_eq!(scoped_tabs(tabs, &targets).0.len(), 2);
    }

    #[test]
    fn describe_open_names_session_and_command() {
        let local = describe_open(&OpenKind::Local, "排查", Some(" uptime "), None);
        assert!(local.contains("排查") && local.contains("本地终端") && local.contains("uptime"));
        let ssh = OpenKind::Ssh { session_record_id: "rec-1".into() };
        let with_desc = describe_open(&ssh, "db", None, Some("SSH：db root@10.0.0.1:22"));
        assert!(with_desc.contains("10.0.0.1") && !with_desc.contains("并执行"));
        // 查不到配置名时退回 id，而不是空着
        assert!(describe_open(&ssh, "db", None, None).contains("rec-1"));
    }

    #[test]
    fn beyond_scope_feedback_forbids_retry_and_workarounds() {
        let text = beyond_scope_feedback("在「x」执行：ls");
        assert!(text.contains("超出本轮作用范围") && text.contains("不要重试"));
        assert!(text.contains("作用范围"));
    }

    #[test]
    fn prompt_tells_model_about_scope_enforcement() {
        for key in ["作用范围", "拦截", "不要重试"] {
            assert!(ACTION_PROTOCOL.contains(key), "协议缺少作用范围要点：{key}");
        }
    }

    #[test]
    fn empty_targets_fall_back_to_current_session() {
        let req = AgentRunRequest {
            session_id: "cur".into(),
            question: "q".into(),
            targets: vec![],
            console_text: None,
            console_alt: false,
        };
        let resolved = resolve_targets(&req);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].id, "cur");
    }

    #[test]
    fn concurrent_runs_on_one_session_are_rejected() {
        let rt = ActRuntime::new();
        assert!(rt.begin("s1").is_some());
        assert!(rt.begin("s1").is_none());
        assert!(rt.begin("s2").is_some());
        rt.finish("s1");
        assert!(rt.begin("s1").is_some(), "结束后应可再次开始");
    }

    #[test]
    fn answering_an_unknown_request_is_reported() {
        let rt = ActRuntime::new();
        assert!(!rt.answer("nope", true), "没人挂着的请求应答应返回 false");
    }

    #[tokio::test]
    async fn answering_a_pending_request_delivers_the_decision() {
        let rt = ActRuntime::new();
        let rx = rt.register("r1");
        assert!(rt.answer("r1", true));
        assert!(rx.await.unwrap());
    }

    /// 只允许一轮：UI 崩了导致 rx 被 drop 时，unregister 不能误伤别的请求
    #[test]
    fn unregister_only_touches_its_own_request() {
        let rt = ActRuntime::new();
        let _rx1 = rt.register("r1");
        let _rx2 = rt.register("r2");
        rt.unregister("r1");
        assert!(rt.answer("r2", true));
    }

    #[test]
    fn cancel_sets_the_flag() {
        let rt = ActRuntime::new();
        let flag = rt.begin("s1").expect("应登记成功");
        assert!(rt.is_running("s1"));
        assert!(rt.cancel("s1"));
        assert!(flag.load(Ordering::Relaxed));
        assert!(!rt.cancel("s2"), "没有在跑的会话取消应返回 false");
    }

    /// agentic 提示词必须把协议与两条硬边界写清楚，否则模型不会动手 / 停不下来
    #[test]
    fn agentic_prompt_states_the_protocol_and_limits() {
        let agentic = agentic_system_prompt();
        assert!(agentic.contains("nx-exec"));
        for key in ["targets", "commands", "单行", "不可逆", "不要再"] {
            assert!(agentic.contains(key), "提示词缺少要点：{key}");
        }
        assert!(ACTION_PROTOCOL.contains("nx-exec"));
        // 「开窗口」必须走 open 动作，不许拿系统终端应用充数 ——
        // 模型曾用 open -a Terminal 交差，用户看到的是应用外多出来的窗口
        assert!(agentic.contains("open -a Terminal"), "提示词要显式禁掉系统终端命令");
        assert!(agentic.contains("不要换成系统命令硬绕"));
        assert!(ACTION_PROTOCOL.contains("open -a Terminal"));
        assert!(ACTION_PROTOCOL.contains("不能自己编一个主机"));
    }

    #[test]
    fn prompt_pick_never_drops_the_protocol() {
        // 用户清空提示词 → 用 agentic 兜底
        assert_eq!(IfAgentic::pick("   "), agentic_system_prompt());
        // 用户写了自己的 → 他的话要留着，协议也要挂上
        let picked = IfAgentic::pick("你是我的运维顾问");
        assert!(picked.starts_with("你是我的运维顾问"), "用户的提示词被丢了");
        assert!(picked.contains("nx-exec"), "自定义模式也必须带上协议");
    }

    /// 用户账号里真实存在的那段自定义提示词（「只给结论与必要命令」）。
    /// 它把模型往「直接贴命令」上带，之前自定义路径只挂了一段压缩协议，
    /// 模型于是把 shell 抄进围栏 —— 动作解析不出来，界面上什么都不发生。
    #[test]
    fn custom_prompt_still_gets_the_full_protocol() {
        let custom = concat!(
            "你是 NexusTerm 的终端助手。用户会把终端会话的最近输出发给你，",
            "请结合会话上下文用中文简洁回答，只给结论与必要命令，不要复述已给出的输出。"
        );
        let picked = IfAgentic::pick(custom);
        // 三个动作类型都得能照抄
        for key in ["\"action\":\"exec\"", "\"action\":\"export\"", "\"action\":\"open\""] {
            assert!(picked.contains(key), "自定义模式缺少动作样例：{key}");
        }
        // 围栏里只放 JSON、以及「开窗口别拿系统终端充数」这两条硬边界不能少
        assert!(picked.contains("只放那一行 JSON"));
        assert!(picked.contains("open -a Terminal"));
        // 协议必须压过用户那句「只给结论与必要命令」
        assert!(picked.contains("优先于上文"), "自定义模式要先声明协议优先级");
    }
    /// 模型真实回复（用同一账号跑出来的原文）：开本地会话
    #[test]
    fn real_world_reply_open_local_is_extracted() {
        let reply = "\u{0060}\u{0060}\u{0060}nx-exec\n{\"action\":\"open\",\"kind\":\"local\",\"label\":\"新窗口\"}\n\u{0060}\u{0060}\u{0060}\n\n已为你开一个新的本地终端窗口。";
        let acts = extract_actions(&reply);
        assert_eq!(acts.len(), 1, "应当解析出 1 个 open 动作，实际：{acts:?}");
        match &acts[0] {
            AgentAction::Open { kind, label, command } => {
                assert_eq!(*kind, OpenKind::Local);
                assert_eq!(label.as_deref(), Some("新窗口"));
                assert!(command.is_none());
            }
            other => panic!("应解析成 Open，实际 {other:?}"),
        }
    }

    // ——————————————————— 原生工具调用 ———————————————————

    #[test]
    fn tool_call_maps_to_tab_action() {
        let call = llm::ToolCall {
            name: TOOL_TAB.into(),
            arguments: r#"{"op":"reconnect","target":"tab-9"}"#.into(),
        };
        assert_eq!(
            actions_from_tool_call(&call),
            vec![AgentAction::Tab {
                op: TabOp::Reconnect,
                target: Some("tab-9".into()),
                name: None
            }]
        );
    }

    #[test]
    fn tool_call_maps_to_open_ssh_action() {
        let call = llm::ToolCall {
            name: TOOL_OPEN.into(),
            arguments: r#"{"kind":"ssh","sessionRecordId":"rec-1","label":"db-01"}"#.into(),
        };
        assert_eq!(
            actions_from_tool_call(&call),
            vec![AgentAction::Open {
                kind: OpenKind::Ssh { session_record_id: "rec-1".into() },
                label: Some("db-01".into()),
                command: None,
            }]
        );
    }

    #[test]
    fn tool_call_maps_to_exec_action() {
        let call = llm::ToolCall {
            name: TOOL_EXEC.into(),
            arguments: r#"{"target":"s1","command":"uptime"}"#.into(),
        };
        assert_eq!(
            actions_from_tool_call(&call),
            vec![AgentAction::Exec { target: Some("s1".into()), command: "uptime".into() }]
        );
    }

    /// 工具名是意图的权威来源：模型在参数里自己塞了个 `action` 也不能信它，
    /// 否则 `nx_exec` 配 `{"action":"tab"}` 会绕过 exec 的校验直接去操作界面。
    #[test]
    fn tool_name_beats_a_smuggled_action_field() {
        let call = llm::ToolCall {
            name: TOOL_EXEC.into(),
            arguments: r#"{"action":"tab","command":"rm -rf /"}"#.into(),
        };
        assert_eq!(
            actions_from_tool_call(&call),
            vec![AgentAction::Exec { target: None, command: "rm -rf /".into() }]
        );
    }

    /// 模型习惯把一排命令写成数组，而工具 schema 里 `command` 是字符串。
    /// 搬到 commands 上，批量下发照样能用，省掉一整轮重发。
    #[test]
    fn tool_call_accepts_array_commands() {
        let call = llm::ToolCall {
            name: TOOL_EXEC.into(),
            arguments: r#"{"target":"s1","command":["uptime","df -h"]}"#.into(),
        };
        assert_eq!(
            actions_from_tool_call(&call),
            vec![
                AgentAction::Exec { target: Some("s1".into()), command: "uptime".into() },
                AgentAction::Exec { target: Some("s1".into()), command: "df -h".into() },
            ]
        );
    }

    /// 参数缺失必须在界面上说清楚：卡片已经摆出去了，
    /// 后面却什么都没发生，那就是最招骂的「它说要做，然后没有然后」。
    #[test]
    fn empty_tool_actions_produce_a_correction_hint() {
        let call = llm::ToolCall { name: TOOL_TAB.into(), arguments: r#"{"op":"bogus"}"#.into() };
        assert!(actions_from_tool_call(&call).is_empty());
    }

    #[test]
    fn unknown_tool_call_yields_nothing() {
        // 未知工具宁可什么都不做，也不要按名字去猜它想干什么
        let call = llm::ToolCall { name: "nx_destroy_everything".into(), arguments: "{}".into() };
        assert!(actions_from_tool_call(&call).is_empty());
        // 空参数（模型直接调了没传参）也不能崩，由后面的必填字段校验丢掉
        let call = llm::ToolCall { name: TOOL_TAB.into(), arguments: "".into() };
        assert!(actions_from_tool_call(&call).is_empty());
        let call = llm::ToolCall { name: TOOL_TAB.into(), arguments: "not json".into() };
        assert!(actions_from_tool_call(&call).is_empty());
    }

    // —————————————— 手和眼：工具声明 / 解析 ——————————————

    fn call(name: &str, args: &str) -> llm::ToolCall {
        llm::ToolCall { name: name.into(), arguments: args.into() }
    }

    #[test]
    fn new_tools_are_declared_with_unique_names() {
        let specs = tool_specs();
        for n in [TOOL_KEYS, TOOL_WAIT, TOOL_SCREEN, TOOL_SECRET] {
            assert!(specs.iter().any(|s| s.name == n), "缺少工具 {n}");
        }
        let mut names: Vec<_> = specs.iter().map(|s| s.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "工具名重复");
        let keys = specs.iter().find(|s| s.name == TOOL_KEYS).unwrap();
        assert_eq!(keys.schema["required"][0], "keys");
    }

    #[test]
    fn keys_tool_call_keeps_raw_text() {
        let acts = actions_from_tool_call(&call(TOOL_KEYS, r#"{"keys":"<C-c>","target":"s1"}"#));
        assert_eq!(acts, vec![AgentAction::Keys { target: Some("s1".into()), keys: "<C-c>".into() }]);
    }

    #[test]
    fn keys_without_keys_field_yields_nothing() {
        assert!(actions_from_tool_call(&call(TOOL_KEYS, r#"{"target":"s1"}"#)).is_empty());
        assert!(actions_from_tool_call(&call(TOOL_KEYS, r#"{"keys":""}"#)).is_empty());
        assert!(actions_from_tool_call(&call(TOOL_KEYS, "")).is_empty());
    }

    #[test]
    fn keys_array_target_fans_out() {
        let acts = actions_from_tool_call(&call(TOOL_KEYS, r#"{"keys":"q","target":["a","b"]}"#));
        assert_eq!(acts.len(), 2);
        assert_eq!(acts[1], AgentAction::Keys { target: Some("b".into()), keys: "q".into() });
    }

    #[test]
    fn wait_tool_call_parses_optional_fields() {
        let acts = actions_from_tool_call(&call(
            TOOL_WAIT,
            r#"{"pattern":"/done|fail/i","timeoutMs":5000,"quietMs":800}"#,
        ));
        assert_eq!(
            acts,
            vec![AgentAction::Wait {
                target: None,
                pattern: Some("/done|fail/i".into()),
                timeout_ms: Some(5000),
                quiet_ms: Some(800),
            }]
        );
        // 全省略也合法：等输出静止
        assert_eq!(
            actions_from_tool_call(&call(TOOL_WAIT, "{}")),
            vec![AgentAction::Wait { target: None, pattern: None, timeout_ms: None, quiet_ms: None }]
        );
    }

    #[test]
    fn screen_tool_call_parses() {
        assert_eq!(
            actions_from_tool_call(&call(TOOL_SCREEN, r#"{"target":"s2"}"#)),
            vec![AgentAction::Screen { target: Some("s2".into()) }]
        );
        assert_eq!(
            actions_from_tool_call(&call(TOOL_SCREEN, "")),
            vec![AgentAction::Screen { target: None }]
        );
    }

    #[test]
    fn fence_protocol_supports_new_actions() {
        assert_eq!(
            extract_actions(&block(r#"{"action":"keys","keys":"y<Enter>"}"#)),
            vec![AgentAction::Keys { target: None, keys: "y<Enter>".into() }]
        );
        assert_eq!(
            extract_actions(&block(r#"{"action":"wait","pattern":"login:"}"#)),
            vec![AgentAction::Wait {
                target: None,
                pattern: Some("login:".into()),
                timeout_ms: None,
                quiet_ms: None
            }]
        );
        assert_eq!(
            extract_actions(&block(r#"{"action":"screen","target":"s1"}"#)),
            vec![AgentAction::Screen { target: Some("s1".into()) }]
        );
    }

    #[test]
    fn tool_labels_cover_new_tools() {
        assert_eq!(tool_label(TOOL_KEYS, "{}"), "发送按键");
        assert_eq!(tool_label(TOOL_WAIT, "{}"), "等待输出");
        assert_eq!(tool_label(TOOL_SCREEN, "{}"), "读取屏幕");
    }

    /// exec 的单行铁律不能因为有了 keys 而松动：回车 / 控制字符只在 keys 里合法。
    #[test]
    fn exec_still_rejects_control_chars_while_keys_allow_them() {
        assert!(reject_command("ls\r").is_some());
        assert!(reject_command("ls\n").is_some());
        assert_eq!(interact::parse_keys("ls\\r").unwrap(), b"ls\r");
        assert_eq!(interact::parse_keys("<C-c>").unwrap(), vec![3]);
    }

    // —————————————— 手和眼：审批 / 反馈 / 回执 ——————————————

    #[test]
    fn keys_without_enter_need_no_approval() {
        assert!(keys_risk(b"\x03").is_none());
        assert!(keys_risk(b"ls -la").is_none());
        assert!(keys_risk(b"\x1b[A").is_none());
    }

    #[test]
    fn keys_with_enter_always_flag_risk() {
        let r = keys_risk(b"y\r").expect("含回车必须给出原因");
        assert_eq!(r.len(), 1, "安全文本只有『含回车』这一条原因：{r:?}");
        assert!(keys_risk(b"\r").is_some(), "单独的回车也会提交当前行");
        assert!(keys_risk(b"a\nb").is_some());
    }

    #[test]
    fn keys_with_dangerous_text_list_concrete_reasons() {
        let r = keys_risk(b"rm -rf /tmp/x\r").unwrap();
        assert!(r.len() > 1, "危险命令要列出具体原因：{r:?}");
        assert!(r.iter().any(|x| x.contains("rm -rf /tmp/x")));
    }

    #[test]
    fn keys_feedback_says_sent_and_shows_tail() {
        let f = keys_feedback("web-1", "<C-c>", "^C\n$ ", None);
        assert!(f.contains("已向会话「web-1」发送按键"));
        assert!(f.contains("^C"));
        assert!(keys_feedback("w", "x", "  ", None).contains("暂无新输出"));
    }

    #[test]
    fn wait_feedback_distinguishes_outcomes() {
        assert!(wait_feedback("w", Some("ok"), WaitEnd::Matched, "all ok", None).contains("命中"));
        let miss = wait_feedback("w", Some("ok"), WaitEnd::Timeout, "x", None);
        assert!(miss.contains("未命中") && miss.contains("超时"));
        assert!(wait_feedback("w", Some("ok"), WaitEnd::Quiet, "", None).contains("未命中"));
        assert!(wait_feedback("w", None, WaitEnd::Quiet, "x", None).contains("输出已静止"));
        assert!(wait_feedback("w", Some("ok"), WaitEnd::Cancelled, "", None).contains("取消"));
    }

    #[test]
    fn screen_feedback_marks_alt_and_fallback() {
        assert!(screen_feedback("w", "hello", true, true, None).contains("alternate screen"));
        let fb = screen_feedback("w", "hello", false, false, None);
        assert!(fb.contains("旁路缓冲") && fb.contains("hello"));
        assert!(screen_feedback("w", "\n\n", false, true, None).contains("没有文字"));
    }

    #[test]
    fn screen_ack_round_trip_and_cleanup() {
        let rt = ActRuntime::new();
        let mut rx = rt.register_screen("r1");
        assert!(rt.answer_screen(
            "r1",
            ScreenReply { ok: true, text: "hi".into(), alt: true, detail: String::new() }
        ));
        let got = rx.try_recv().unwrap();
        assert!(got.ok && got.alt && got.text == "hi");
        // 已取走：再答复一次是「没人接了」
        assert!(!rt.answer_screen("r1", ScreenReply::default()));
        // 超时路径：注销后迟到的回执同样无人接
        let _rx = rt.register_screen("r2");
        rt.unregister_screen("r2");
        assert!(!rt.answer_screen("r2", ScreenReply::default()));
    }

    #[test]
    fn protocol_teaches_new_tools_and_forbids_typing_passwords() {
        for key in ["nx_send_keys", "nx_wait", "nx_screen", "<C-c>", "256 字节"] {
            assert!(ACTION_PROTOCOL.contains(key), "协议缺少：{key}");
        }
        assert!(ACTION_PROTOCOL.contains("绝不要用 send_keys / exec 输入密码"));
        // 密码通道上线了：不再让用户「自己在终端里输入」，也不再说「目前没有」通道
        assert!(!ACTION_PROTOCOL.contains("目前没有"));
        assert!(!ACTION_PROTOCOL.contains("用户自己在终端里输入"));
        for key in ["nx_secret", "密码通道", "prompt_kind", "不会经过你", "不要再次索要"] {
            assert!(ACTION_PROTOCOL.contains(key), "协议缺少：{key}");
        }
        assert!(ACTION_PROTOCOL.contains(&format!("一次最多给 {MAX_ACTIONS} 条动作")));
        assert!(IfAgentic::pick("只给结论").contains("nx_send_keys"));
        assert!(IfAgentic::pick("只给结论").contains("nx_secret"));
    }

    // —————————————— 接入分类器（1.6） ——————————————

    fn guess_of(tail: &str) -> PromptGuess {
        prompt::classify(tail, false)
    }

    #[test]
    fn feedback_for_carries_prompt_kind_confidence_and_hint() {
        let g = guess_of("Do you want to continue? [Y/n]");
        assert_eq!(g.kind, prompt::PromptKind::YesNo);
        let out = ExecOutcome {
            output: "Do you want to continue? [Y/n]".into(),
            reason: ExecEnd::AwaitingInput,
            prompt: Some(g),
        };
        let text = feedback_for("apt install nginx", "web", &out);
        assert!(text.contains("还没有结束"), "{text}");
        assert!(text.contains("prompt_kind=yes_no"), "{text}");
        assert!(text.contains("confidence="), "{text}");
        assert!(text.contains("提示："), "{text}");
        // 已结束的命令同样带上分类
        let done = ExecOutcome {
            output: "ok".into(),
            reason: ExecEnd::Confirmed,
            prompt: Some(guess_of("chris@web:~$ ")),
        };
        assert!(feedback_for("true", "web", &done).contains("prompt_kind=shell_prompt"));
        // 取消不带分类
        let c = ExecOutcome { output: String::new(), reason: ExecEnd::Cancelled, prompt: None };
        assert!(!feedback_for("x", "web", &c).contains("prompt_kind"));
    }

    #[test]
    fn open_command_feedback_handles_awaiting_input() {
        let out = ExecOutcome {
            output: "[sudo] password for chris:".into(),
            reason: ExecEnd::AwaitingInput,
            prompt: Some(guess_of("[sudo] password for chris:")),
        };
        let t = feedback_for_open_command(&out);
        assert!(t.contains("还没有结束") && t.contains("prompt_kind=password"), "{t}");
        assert!(t.contains("nx_secret"), "密码提示的建议应当指向 nx_secret：{t}");
    }

    #[test]
    fn keys_wait_screen_feedback_include_prompt_line() {
        let g = guess_of("Password:");
        assert!(keys_feedback("w", "x", "Password:", Some(&g)).contains("prompt_kind=password"));
        assert!(wait_feedback("w", Some("a"), WaitEnd::Timeout, "", Some(&g)).contains("prompt_kind=password"));
        assert!(screen_feedback("w", "Password:", false, true, Some(&g)).contains("prompt_kind=password"));
        // 没有分类就不加这一行
        assert!(!keys_feedback("w", "x", "", None).contains("prompt_kind"));
    }

    // —————————————— 密码通道（1.7） ——————————————

    #[test]
    fn secret_tool_is_declared_and_takes_only_target_and_reason() {
        let spec = tool_specs().into_iter().find(|s| s.name == TOOL_SECRET).expect("缺少 nx_secret");
        let props = spec.schema["properties"].as_object().unwrap();
        let mut keys: Vec<_> = props.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["reason", "target"], "密码通道不许有任何能带值的参数");
        assert_eq!(tool_label(TOOL_SECRET, "{}"), "请求输入密码");
    }

    #[test]
    fn secret_tool_call_parses_reason_and_target() {
        let acts = actions_from_tool_call(&call(
            TOOL_SECRET,
            r#"{"target":"s1","reason":"  sudo 需要密码\n安装 nginx "}"#,
        ));
        assert_eq!(
            acts,
            vec![AgentAction::Secret {
                target: Some("s1".into()),
                reason: Some("  sudo 需要密码\n安装 nginx ".into())
            }]
        );
        // 缺 reason 也产生动作（卡片用默认说明），不会让这一轮白白蒸发
        let bare = actions_from_tool_call(&call(TOOL_SECRET, "{}"));
        assert_eq!(bare, vec![AgentAction::Secret { target: None, reason: None }]);
    }

    #[test]
    fn secret_never_picks_up_a_value_from_model_args() {
        // 模型就算塞了 value / password 字段，动作里也没有地方放它
        let acts = actions_from_tool_call(&call(
            TOOL_SECRET,
            r#"{"reason":"x","value":"hunter2hunter2","password":"hunter2hunter2"}"#,
        ));
        assert_eq!(acts.len(), 1);
        assert!(!format!("{acts:?}").contains("hunter2hunter2"));
    }

    #[test]
    fn fence_protocol_supports_secret_action() {
        let acts = extract_actions(&block(r#"{"action":"secret","target":"s2","reason":"ssh 口令"}"#));
        assert_eq!(
            acts,
            vec![AgentAction::Secret { target: Some("s2".into()), reason: Some("ssh 口令".into()) }]
        );
    }

    #[tokio::test]
    async fn secret_ack_round_trip_delivers_value_once() {
        let rt = ActRuntime::new();
        let rx = rt.register_secret("r1");
        assert_eq!(rt.answer_secret("r1", Some("hunter2hunter2\n".into())), Ok(true));
        match rx.await.unwrap() {
            SecretAnswer::Provided(v) => assert_eq!(v.as_str(), "hunter2hunter2"),
            other => panic!("{other:?}"),
        }
        // 登记已被消耗：同一个请求不能答两次
        assert_eq!(rt.answer_secret("r1", Some("again".into())), Ok(false));
    }

    #[tokio::test]
    async fn secret_ack_skip_is_declined() {
        let rt = ActRuntime::new();
        let rx = rt.register_secret("r2");
        assert_eq!(rt.answer_secret("r2", None), Ok(true));
        assert!(matches!(rx.await.unwrap(), SecretAnswer::Declined));
    }

    #[test]
    fn secret_ack_unknown_or_expired_request_reports_false() {
        let rt = ActRuntime::new();
        assert_eq!(rt.answer_secret("nope", Some("x".into())), Ok(false));
        // 接收端已因超时 / 取消被丢弃
        drop(rt.register_secret("gone"));
        assert_eq!(rt.answer_secret("gone", Some("x".into())), Ok(false));
    }

    #[test]
    fn secret_ack_invalid_input_keeps_request_alive_and_names_no_value() {
        let rt = ActRuntime::new();
        let _rx = rt.register_secret("r3");
        let err = rt.answer_secret("r3", Some("hunter2hunter2\nrm -rf /".into())).unwrap_err();
        assert!(!err.contains("hunter2hunter2"));
        // 校验失败不消耗登记：用户改完能再交一次
        assert_eq!(rt.answer_secret("r3", Some("fixedpass".into())), Ok(true));
    }

    #[test]
    fn secret_payloads_and_cards_carry_no_secret_field() {
        // 事件载荷与步骤卡只有状态，序列化出来没有任何 value / secret / password 键
        let req = serde_json::to_string(&SecretRequest {
            request_id: "r".into(),
            target: "t".into(),
            target_label: "web".into(),
            reason: "sudo".into(),
            prompt_kind: Some("password".into()),
            timeout_secs: 120,
        })
        .unwrap();
        let step = serde_json::to_string(&StepPayload {
            secret_status: Some("provided".into()),
            ..StepPayload::new("secret", 1)
        })
        .unwrap();
        for j in [req, step] {
            for bad in ["\"value\":", "\"password\":", "\"secret\":"] {
                assert!(!j.contains(bad), "{j}");
            }
        }
    }

    #[test]
    fn prompt_hint_for_password_points_at_nx_secret_not_send_keys() {
        let g = guess_of("[sudo] password for chris:");
        assert!(g.hint.contains("nx_secret"));
        assert!(g.hint.contains("不要用 nx_send_keys"));
    }

    // —————————————— 出网脱敏（1.1） ——————————————

    const SECRET_CTX: &str =
        "$ env\nopenai sk-ABCDEFGH12345678\nmail bob@example.com host 10.1.2.3\n";

    fn plain_req(question: &str, alt: bool) -> AgentRunRequest {
        AgentRunRequest {
            session_id: "s1".into(),
            question: question.into(),
            targets: Vec::new(),
            console_text: None,
            console_alt: alt,
        }
    }

    #[test]
    fn scrub_masks_only_when_enabled() {
        let on = scrub(true, SECRET_CTX);
        assert!(!on.contains("sk-ABCDEFGH12345678"));
        assert!(!on.contains("bob@example.com"));
        assert!(!on.contains("10.1.2.3"));
        assert!(on.contains(super::super::redact::PLACEHOLDER));
        assert_eq!(scrub(false, SECRET_CTX), SECRET_CTX);
    }

    #[test]
    fn plan_message_redacts_terminal_content_only_when_on() {
        let t = AgentTarget { id: "s1".into(), label: "本机".into() };
        let r = plain_req("帮我看看", false);
        let blocks = [(&t, SECRET_CTX.to_string())];
        let on = build_plan_message(&r, &blocks, None, "", true);
        assert!(!on.contains("sk-ABCDEFGH12345678"));
        assert!(!on.contains("bob@example.com"));
        assert!(on.contains("帮我看看"), "用户的问题不该被脱敏吃掉");
        let off = build_plan_message(&r, &blocks, None, "", false);
        assert!(off.contains("sk-ABCDEFGH12345678") && off.contains("bob@example.com"));
    }

    #[test]
    fn plan_message_redacts_fullscreen_capture() {
        let t = AgentTarget { id: "s1".into(), label: "本机".into() };
        let r = plain_req("q", true);
        let msg = build_plan_message(
            &r,
            &[(&t, "x".to_string())],
            Some("tmux pane password=hunter2hunter2"),
            "",
            true,
        );
        assert!(msg.contains("自动抓取"));
        assert!(!msg.contains("hunter2hunter2"));
    }

    /// 反馈由 `run` 在入口统一过 `scrub`；这里确认各类反馈经 scrub 后不含明文。
    #[test]
    fn every_feedback_kind_is_clean_after_scrub() {
        let all = [
            feedback_for(
                "echo hi",
                "web",
                &ExecOutcome { output: "key sk-ABCDEFGH12345678".into(), reason: ExecEnd::Confirmed, prompt: None },
            ),
            failure_feedback("echo bob@example.com", "web", "连接 10.1.2.3 失败"),
            keys_feedback("web", "x", "password=hunter2hunter2", None),
            wait_feedback("web", Some("p"), WaitEnd::Matched, "from 10.9.8.7", None),
        ];
        for f in all {
            let clean = scrub(true, &f);
            for needle in [
                "sk-ABCDEFGH12345678",
                "bob@example.com",
                "10.1.2.3",
                "hunter2hunter2",
                "10.9.8.7",
            ] {
                assert!(!clean.contains(needle), "脱敏后仍含 {needle}：{clean}");
            }
        }
    }

    #[test]
    fn tool_label_names_the_tab_op() {
        assert_eq!(tool_label(TOOL_TAB, r#"{"op":"list"}"#), "列出标签页");
        assert_eq!(tool_label(TOOL_TAB, r#"{"op":"reconnect"}"#), "重连标签页");
        assert_eq!(tool_label(TOOL_TAB, "{}"), "操作标签页");
        assert_eq!(tool_label(TOOL_EXEC, "{}"), "执行命令");
        assert_eq!(tool_label(TOOL_OPEN, "{}"), "新建标签页");
    }

    /// 历史里那条 assistant 消息必须是「人话」而不是原始 tool_calls：
    /// OpenAI 系接口要求 tool_calls 后面紧跟 role:tool 回执，否则下一轮直接 400。
    #[test]
    fn history_records_tool_calls_as_text() {
        let reply = llm::LlmReply {
            text: "先看看有哪些标签页。".into(),
            reasoning: String::new(),
            tool_calls: vec![llm::ToolCall {
                name: TOOL_TAB.into(),
                arguments: r#"{"op":"list"}"#.into(),
            }],
            tools_supported: true,
        };
        let text = describe_tool_calls(&reply);
        assert!(text.contains("nx_tab"), "工具名要留在历史里：{text}");
        assert!(text.contains("list"), "参数也要留着：{text}");
        assert!(text.contains("先看看有哪些标签页"), "正文不能被吃掉：{text}");
    }

    // ——————————————————— 伪命令护栏 ———————————————————

    /// 截图里的真实翻车：模型想开新标签页，写出 `new-tab` 当成命令。
    /// 这条必须被拦下，绝不能敲进终端换回一句 command not found。
    #[test]
    fn new_tab_is_not_a_command() {
        let acts = extract_actions(&block(r#"{"action":"exec","command":"new-tab"}"#));
        assert_eq!(acts.len(), 1);
        match &acts[0] {
            AgentAction::Reject { command, hint } => {
                assert_eq!(command, "new-tab");
                assert!(hint.contains("nx_open_tab"), "纠正话术要指到工具上：{hint}");
            }
            other => panic!("应拦成 Reject，实际 {other:?}"),
        }
        // 围栏里直接写 shell 的那条路径同样要拦
        let acts = extract_actions("```nx-exec\nnew-tab\n```\n");
        assert_eq!(acts.len(), 1);
        assert!(matches!(acts[0], AgentAction::Reject { .. }), "实际 {acts:?}");
    }

    #[test]
    fn pseudo_commands_are_all_caught() {
        for cmd in [
            "new-tab", "newtab", "new_tab", "open-tab", "opentab", "tab-new",
            "create-tab", "add-tab", "reconnect", "reconnect-tab", "re-tab",
            "close-tab", "closetab", "kill-tab", "switch-tab", "focus-tab",
            "list-tabs", "tabs", "rename-tab",
        ] {
            assert!(
                pseudo_command_hint(cmd).is_some(),
                "「{cmd}」应被判为伪命令"
            );
            // 带参数时也要拦
            assert!(
                pseudo_command_hint(&format!("{cmd} --host 1.2.3.4")).is_some(),
                "「{cmd} --host …」应被判为伪命令"
            );
        }
    }

    /// 护栏不能误伤真命令：这些词出现在管道/参数里都不算伪命令
    #[test]
    fn real_commands_pass_the_guard() {
        for cmd in ["ls", "uptime", "cat tabs.txt", "grep -r tab .", "tmux new-session", "df -h"] {
            assert!(pseudo_command_hint(cmd).is_none(), "「{cmd}」不该被拦");
        }
    }

    /// 断线的会话再发命令也不会有结果。不点破的话模型会一直空转 ——
    /// 实测过它连着报两次「输入通道已关闭」还在下发第三条。
    #[test]
    fn closed_channel_feedback_points_at_reconnect() {
        let out = failure_feedback("uptime", "机器A", "通道错误: 输入通道已关闭");
        assert!(out.contains("不要重试"), "必须明确叫它别重试：{out}");
        assert!(out.contains("reconnect"), "必须指到重连动作上：{out}");
        assert!(out.contains(TOOL_TAB), "必须给出正确工具：{out}");

        // 普通失败不该夹带这段，否则每次报错都让模型去重连
        let out = failure_feedback("uptime", "机器A", "命令超时");
        assert!(!out.contains("reconnect"), "不该误报：{out}");
    }
}

/// 端到端（缓冲 → 末行 → 提示符判定）：`executor` 的单测喂的是理想化的末行字符串，
/// 这里喂真实字节流——带 OSC 改标题、带色、回显用 `\r\n`——确认 ANSI 剥离和回显抵消
/// 之后，末行真的还是那个提示符。
#[cfg(test)]
mod prompt_e2e {
    use crate::agent::buffer::{SessionBuffer, StreamKind};
    use crate::agent::executor::same_prompt;

    /// Ubuntu 默认 PS1：先一条 OSC 改窗口标题，再带色的 user@host:cwd$
    fn ubuntu_prompt(cwd: &str) -> Vec<u8> {
        format!(
            "\x1b]0;chris@chris-Latitude-3490: {cwd}\x07\
             \x1b[01;32mchris@chris-Latitude-3490\x1b[00m:\x1b[01;34m{cwd}\x1b[00m$ "
        )
        .into_bytes()
    }

    #[test]
    fn cd_workspace_round_trip() {
        let mut b = SessionBuffer::new();
        b.feed(&ubuntu_prompt("~"), StreamKind::Out);
        let base = b.last_line();
        eprintln!("baseline = {base:?}");

        b.feed(b"cd workspace\n", StreamKind::In);
        b.feed(b"cd workspace\r\n", StreamKind::Out);
        b.feed(&ubuntu_prompt("~/workspace"), StreamKind::Out);
        let now = b.last_line();
        eprintln!("now      = {now:?}");
        eprintln!("transcript tail = {:?}", b.take_tail(300));
        assert!(same_prompt(&base, &now), "提示符没认出来");
    }

    /// 真实情况：`cd` 是瞬时的，远端把「回显 + 新提示符」放在同一个包里发回来。
    #[test]
    fn echo_and_new_prompt_in_one_packet() {
        let mut b = SessionBuffer::new();
        b.feed(&ubuntu_prompt("~"), StreamKind::Out);
        let base = b.last_line();

        // agent 写下去的原始字节（注意是 \n，不是 \r\n）
        b.feed(b"cd workspace\n", StreamKind::In);
        // 远端一个包：回显用 \r\n，紧跟着新提示符
        let mut packet = b"cd workspace\r\n".to_vec();
        packet.extend_from_slice(&ubuntu_prompt("~/workspace"));
        b.feed(&packet, StreamKind::Out);

        let now = b.last_line();
        eprintln!("base = {base:?}");
        eprintln!("now  = {now:?}");
        eprintln!("tail = {:?}", b.take_tail(300));
        assert!(same_prompt(&base, &now), "提示符没认出来: {now:?}");
    }
}
