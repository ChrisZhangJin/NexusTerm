//! 托管回放场景。每个场景：剧本（假模型）→ `host_start` → 真 bash → 断言相位 / 步骤 / 模型看到的反馈。

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::fake_llm::{Reply, Turn};
use super::harness::Harness;
use crate::agent::host::{CardKind, HostPhase};
use tauri::Manager;

/// 剧本的一步：模型调用 `name(args)`；`expect` = 这一步之前模型看到的反馈里必须含的字（None 不查）。
pub fn turn(expect: Option<&str>, name: &str, args: Value) -> Turn {
    Turn { expect: expect.map(str::to_string), reply: Reply::tool(name, args) }
}

pub fn complete(expect: Option<&str>) -> Turn {
    turn(expect, "nx_complete", json!({ "summary": "完成", "evidence": ["e2e"] }))
}

/// 判据：一条永远满足的只读命令（不想让判据成为场景重点时用）。
fn always_ok() -> Value {
    json!([{ "kind": "commandOk", "cmd": "true" }])
}

macro_rules! harness {
    ($turns:expr, $rules:expr, $criteria:expr) => {
        match Harness::new($turns, $rules, $criteria) {
            Some(h) => h,
            None => {
                eprintln!("[e2e] 没有显示环境（DISPLAY / Xvfb），跳过");
                return;
            }
        }
    };
}

fn assert_completed(h: &Harness, p: &HostPhase) {
    match p {
        HostPhase::Completed { checks, .. } => {
            assert!(!checks.is_empty() && checks.iter().all(|c| c.passed), "判据没全过：{checks:?}\n{}", h.dump())
        }
        other => panic!("没有完成：{other:?}\n{}", h.dump()),
    }
    h.assert_script_ok();
}

fn refused_gates(h: &Harness) -> Vec<Value> {
    h.steps_of("gate").into_iter().filter(|g| g["gateVerdict"] == "refused").collect()
}

/// 1. 正常路径：nx_exec 写文件 → nx_complete → 判据（文件存在）在另开的 shell 里核验通过。
#[test]
fn happy_path_exec_then_complete() {
    let mut h = harness!(
        vec![
            turn(None, "nx_exec", json!({ "command": "echo hi > out.txt" })),
            // 提示符回来 = 确认执行完毕（不是靠静默猜的）
            complete(Some("已确认执行完毕")),
        ],
        vec![],
        json!([{ "kind": "fileExists", "path": "{dir}/out.txt" }])
    );
    h.start("在当前目录写一个 out.txt，内容 hi", "auto");
    let p = h.wait_terminal(Duration::from_secs(30));
    assert_completed(&h, &p);
    assert_eq!(std::fs::read_to_string(h.dir.join("out.txt")).unwrap(), "hi\n");
    // 相位先落、副作用（落盘 / 推送）随后：等推送到了再断言
    h.wait_until(Duration::from_secs(5), "面板收到完成状态", |h| h.statuses().iter().any(|s| s["state"] == "completed"));
}

/// 2. heredoc 改道：在可见终端里敲 `cat > f.py <<'EOF'` 不执行（门禁卡 refused + nx_write 提示），改用 nx_write 后完成。
#[test]
fn heredoc_is_redirected_to_nx_write() {
    let mut h = harness!(
        vec![
            turn(None, "nx_exec", json!({ "command": "cat > f.py <<'EOF'\ndef gen_password():\n    return 'x'\nEOF" })),
            turn(Some("nx_write"), "nx_write", json!({ "path": "{dir}/f.py", "content": "def gen_password():\n    return 'x'\n" })),
            complete(None),
        ],
        vec![],
        json!([{ "kind": "fileExists", "path": "{dir}/f.py" }])
    );
    h.start("写一个 f.py", "auto");
    let p = h.wait_terminal(Duration::from_secs(30));
    assert_completed(&h, &p);
    let gates = refused_gates(&h);
    assert_eq!(gates.len(), 1, "heredoc 一张拒绝卡：{gates:?}");
    assert!(gates[0]["command"].as_str().unwrap_or("").contains("cat > f.py"));
    assert!(!h.screen().contains("EOF"), "heredoc 没敲进终端：\n{}", h.screen());
    assert_eq!(std::fs::read_to_string(h.dir.join("f.py")).unwrap(), "def gen_password():\n    return 'x'\n");
}

/// 3. 屏幕上的代码不是密码提示：`cat` 出含 `def gen_password():` 的文件，再在 Python REPL 里逐行敲代码 ——
/// 屏幕上全是「含 password、以冒号结尾」的行，按键一次都不能被当成「替你输密码」拦下。
#[test]
fn code_on_screen_is_not_a_password_prompt() {
    let keys = |k: &str| json!({ "keys": k });
    let mut h = harness!(
        vec![
            turn(None, "nx_exec", json!({ "command": "cat gen.py" })),
            turn(Some("def gen_password():"), "nx_exec", json!({ "command": "PYTHON_BASIC_REPL=1 python3 -q" })),
            turn(None, "nx_send_keys", keys("def gen_password():<Enter>")),
            turn(None, "nx_send_keys", keys("    return 'sekrit'<Enter>")),
            turn(None, "nx_send_keys", keys("<Enter>")),
            turn(None, "nx_send_keys", keys("if not gen_password(): print('no password:')<Enter>")),
            turn(None, "nx_send_keys", keys("print(gen_password())<Enter>")),
            turn(None, "nx_send_keys", keys("exit()<Enter>")),
            complete(None),
        ],
        vec![],
        always_ok()
    );
    std::fs::write(
        h.dir.join("gen.py"),
        "import secrets\n\ndef gen_password():\n    password = secrets.token_hex(8)\n    if not password:\n        raise SystemExit('Password:')\n    return password\n",
    )
    .unwrap();
    h.start("看看 gen.py 并在 REPL 里试一下", "auto");
    let p = h.wait_terminal(Duration::from_secs(60));
    let gates = refused_gates(&h);
    assert!(gates.is_empty(), "按键被门禁拦下：{gates:?}\n{}", h.dump());
    assert!(h.llm.feedback().iter().all(|f| !f.contains("密码提示")), "反馈里出现了密码提示误判\n{}", h.dump());
    assert!(h.screen().contains("sekrit"), "REPL 真的执行了按键：\n{}", h.screen());
    assert_completed(&h, &p);
}

/// 4. 新 shell 接手：nx_exec 起一个嵌套 shell（原提示符不会回来）要尽快返回 Handover，不能等满 120s。
#[test]
fn nested_shell_hands_over_quickly() {
    let mut h = harness!(
        vec![
            turn(None, "nx_exec", json!({ "command": "PS1='root@c0ffee:/app# ' bash --norc --noprofile" })),
            turn(Some("交给了另一个程序"), "nx_exec", json!({ "command": "echo nested-ok" })),
            turn(Some("nested-ok"), "nx_exec", json!({ "command": "exit" })),
            complete(None),
        ],
        vec![],
        always_ok()
    );
    let t0 = Instant::now();
    h.start("进一个子 shell 看看再出来", "auto");
    // 第二步被请求 = 第一步的 nx_exec 已经返回
    let t1 = wait_cursor(&h, 2, Duration::from_secs(40));
    let first = t1 - t0;
    assert!(first < Duration::from_secs(30), "嵌套 shell 的 nx_exec 过了 {first:?} 才返回");
    let p = h.wait_terminal(Duration::from_secs(60));
    assert_completed(&h, &p);
}

/// 等假模型被请求到第 `n` 步（剧本游标 ≥ n），返回那一刻。
fn wait_cursor(h: &Harness, n: usize, timeout: Duration) -> Instant {
    let t0 = Instant::now();
    while h.llm.cursor() < n {
        assert!(t0.elapsed() < timeout, "剧本停在第 {} 步\n{}", h.llm.cursor(), h.dump());
        std::thread::sleep(Duration::from_millis(20));
    }
    Instant::now()
}

/// 5a. auto 档下带推荐的问题：直接按推荐继续，不挂起等人。
#[test]
fn question_with_recommendation_is_self_answered() {
    let mut h = harness!(
        vec![
            turn(None, "nx_ask_user", json!({ "question": "用 A 还是 B？", "recommendation": "用 A：更稳" })),
            complete(Some("用 A：更稳")),
        ],
        vec![],
        always_ok()
    );
    h.start("选个方案", "auto");
    let p = h.wait_terminal(Duration::from_secs(30));
    assert_completed(&h, &p);
    assert!(h.statuses().iter().all(|s| s["state"] != "waiting_human"), "不该挂起等人：{:?}", h.statuses());
}

/// 5b. 不带推荐：挂起 WaitingHuman（Ask 卡），用户经 `host_answer` 作答后恢复，模型拿到答复。
#[test]
fn question_without_recommendation_waits_for_human() {
    let mut h = harness!(
        vec![turn(None, "nx_ask_user", json!({ "question": "部署到哪个环境？" })), complete(Some("staging-7"))],
        vec![],
        always_ok()
    );
    h.start("部署", "auto");
    let p = h.wait_phase(Duration::from_secs(30), "等人", |p| matches!(p, HostPhase::WaitingHuman { .. }));
    match p {
        HostPhase::WaitingHuman { card } => {
            assert_eq!(card.kind, CardKind::Ask);
            assert!(card.question.contains("部署到哪个环境"));
        }
        _ => unreachable!(),
    }
    // 挂起期间零 LLM 调用
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(h.llm.cursor(), 1, "挂起时不该再请求模型");
    h.answer("staging-7");
    let p = h.wait_terminal(Duration::from_secs(30));
    assert_completed(&h, &p);
}

/// 6. 状态推送不死锁：任务挂在等人卡上时，多线程狂按接管 / 交还（每次迁移都走 `AppHostIo::emit`
/// → `status_of` + 控制权同步 + Tauri emit），同时读状态。曾经 `status_of` 里对同一把锁加两次锁，
/// 第一次推送就把调用线程（`host_start` 所在的主线程）永远卡住。
#[test]
fn rapid_phase_changes_do_not_deadlock() {
    let mut h = harness!(
        vec![turn(None, "nx_ask_user", json!({ "question": "继续吗？" })), complete(Some("继续"))],
        vec![],
        always_ok()
    );
    h.start("压一压状态推送", "auto");
    h.wait_phase(Duration::from_secs(30), "等人", |p| matches!(p, HostPhase::WaitingHuman { .. }));
    let contract = h.contract.clone().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut workers = Vec::new();
    for w in 0..4 {
        let (app, contract, tx) = (h.app.clone(), contract.clone(), tx.clone());
        workers.push(std::thread::spawn(move || {
            use tauri::Manager;
            for i in 0..10 {
                let st = app.state::<crate::cmd::AppState>();
                let host = st.act.host.get(&contract).unwrap();
                if (i + w) % 2 == 0 {
                    let _ = tauri::async_runtime::block_on(crate::agent::host_cmd::host_takeover(
                        contract.clone(),
                        app.clone(),
                        app.state(),
                    ));
                } else {
                    let _ = tauri::async_runtime::block_on(crate::agent::host_cmd::host_handback(
                        contract.clone(),
                        app.clone(),
                        app.state(),
                    ));
                }
                // 直接迁移 + 推送（不经收件箱），与运行循环的 drain 并发
                let _ = host.apply(crate::agent::host::HostEvent::Takeover);
                let _ = host.apply(crate::agent::host::HostEvent::Handback);
                let _ = host.phase();
                let _ = host.checkpoint();
            }
            let _ = tx.send(w);
        }));
    }
    drop(tx);
    for _ in 0..4 {
        rx.recv_timeout(Duration::from_secs(60)).unwrap_or_else(|_| panic!("状态推送卡死了\n{}", h.dump()));
    }
    for w in workers {
        w.join().unwrap();
    }
    // 交还到底：回到那张卡，再作答收尾
    let _ = tauri::async_runtime::block_on(crate::agent::host_cmd::host_handback(contract, h.app.clone(), h.app.state()));
    h.wait_phase(Duration::from_secs(10), "回到等人卡", |p| matches!(p, HostPhase::WaitingHuman { .. }));
    h.answer("继续");
    let p = h.wait_terminal(Duration::from_secs(30));
    assert_completed(&h, &p);
    assert!(h.statuses().len() >= 10, "推送了大量状态：{}", h.statuses().len());
}

/// 3b. 误读样本回放：把一份 `misreads/*.json` 的屏幕画到真终端上（光标停在最后一行行尾），
/// 模型对着这一屏发按键 —— 断言门禁不再按「密码提示」拦下。新的误判样本照这个抄一个场景即可。
#[test]
fn misread_sample_replays_as_screen_fixture() {
    // 「这是误判」存下来的样本（`host::misread::MisreadReport`），这里内联一份；也可以 include_str! 一个文件
    const SAMPLE: &str = r#"{
        "version": 1, "reportedAtMs": 1763600000000, "note": "这是代码，不是密码提示", "fromGate": true,
        "contractId": "c-x", "consoleId": "local-1", "tsMs": 1763600000000,
        "action": "发送按键（屏幕被认成密码提示，按键内容不显示）", "reason": "替你输密码",
        "screen": { "lines": ["root@web:~/app# python3", ">>> import secrets", ">>> def gen_password():"], "alt": false, "rendered": true },
        "classified": { "kind": "password", "program": "unknown", "label": "屏幕被识别为：密码提示", "evidence": [], "confidence": 0.9 },
        "screenHash": ""
    }"#;
    let mut h = harness!(
        vec![
            turn(None, "nx_send_keys", json!({ "keys": "<Enter>" })),
            complete(None),
        ],
        vec![],
        always_ok()
    );
    h.paint(&super::harness::misread_lines(SAMPLE));
    h.start("在 REPL 里把函数写完", "auto");
    let p = h.wait_terminal(Duration::from_secs(30));
    let gates = refused_gates(&h);
    assert!(gates.is_empty(), "误读样本那一屏的按键被拦下：{gates:?}\n{}", h.dump());
    assert_completed(&h, &p);
}

/// 4b. 自定义的短提示符（`inner$ `）：分类器认不出是 shell 提示符（置信度不到 Handover 的门槛），
/// 原提示符又不会回来 —— 曾经让 nx_exec 等满 120s 才 Timeout。现在按提示符形状（`executor::looks_like_custom_prompt`）交接。
#[test]
fn nested_shell_with_custom_prompt_hands_over() {
    let mut h = harness!(
        vec![
            turn(None, "nx_exec", json!({ "command": "PS1='inner$ ' bash --norc --noprofile" })),
            turn(Some("交给了另一个程序"), "nx_exec", json!({ "command": "exit" })),
            complete(None),
        ],
        vec![],
        always_ok()
    );
    let t0 = Instant::now();
    h.start("进一个子 shell 看看再出来", "auto");
    let t1 = wait_cursor(&h, 2, Duration::from_secs(40));
    assert!(t1 - t0 < Duration::from_secs(30), "嵌套 shell 的 nx_exec 过了 {:?} 才返回", t1 - t0);
    let p = h.wait_terminal(Duration::from_secs(30));
    assert_completed(&h, &p);
}

/// 7. 多终端托管：主终端 A + 其余终端 B 一份托管。两个终端都被租下（只读锁定）；模型用 `target` 在 B 上写文件，
/// 判据在主终端上核验；托管状态带两个终端；结束时两个终端的租约一起放掉。
#[test]
fn multi_console_writes_on_extra_via_target() {
    let mut h = harness!(
        vec![
            // 第 1 步之前模型就该知道有哪几个终端、怎么指定
            turn(None, "nx_exec", json!({ "target": "{b}", "command": "echo from-b > b.txt" })),
            complete(Some("已确认执行完毕")),
        ],
        vec![],
        json!([{ "kind": "fileExists", "path": "{dir}/b/b.txt" }])
    );
    let b = h.add_console("b");
    h.llm.substitute("{b}", &b);
    h.start_with("在 B 上写一个 b.txt", "auto", &[&b]);
    // 开跑后两个终端都在这份托管的租约下
    let run = h.host().run_id().to_string();
    for c in [h.console.clone(), b.clone()] {
        assert!(h.state().act.control.holders_of(&c).contains(&run), "{c} 没被租下");
    }
    assert_eq!(h.host().consoles(), vec![h.console.clone(), b.clone()]);
    let p = h.wait_terminal(Duration::from_secs(30));
    assert_completed(&h, &p);
    assert_eq!(std::fs::read_to_string(h.dir_of(&b).join("b.txt")).unwrap(), "from-b\n");
    assert!(!h.dir.join("b.txt").exists(), "写在了主终端上");
    assert!(h.screen_of(&b).contains("echo from-b"), "命令是在 B 的终端里敲的：\n{}", h.screen_of(&b));
    assert!(!h.screen().contains("echo from-b"), "主终端上不该出现：\n{}", h.screen());
    // 系统提示里有「本次托管的终端」一节
    assert!(h.llm.systems().iter().any(|s| s.contains("本次托管的终端") && s.contains(&b)), "{}", h.dump());
    // 状态里带两个终端（主终端在前）
    h.wait_until(Duration::from_secs(5), "面板收到完成状态", |h| h.statuses().iter().any(|s| s["state"] == "completed"));
    let st = h.statuses().last().cloned().unwrap();
    let ids: Vec<&str> = st["consoles"].as_array().unwrap().iter().map(|c| c["consoleId"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![h.console.as_str(), b.as_str()]);
    // 结束：两个终端的租约一起还给用户
    h.wait_until(Duration::from_secs(5), "租约放掉", |h| {
        h.state().act.control.holders_of(&h.console).is_empty() && h.state().act.control.holders_of(&b).is_empty()
    });
}

/// 8. 多终端托管：发给 B 的按键按 **B 的屏幕**判。B 停在密码提示上 → 门禁拒（替你输密码），按键没进 B；
/// 同样的按键发给主终端 A（普通 shell 提示符）照常执行。不在本次托管里的终端一律拒。
#[test]
fn multi_console_keys_judged_by_target_screen() {
    let mut h = harness!(
        vec![
            turn(None, "nx_send_keys", json!({ "target": "{b}", "keys": "hunter2<Enter>" })),
            turn(Some("密码"), "nx_send_keys", json!({ "keys": "echo a-ok-hunter2<Enter>" })),
            turn(None, "nx_exec", json!({ "target": "e2e-ghost", "command": "uptime" })),
            complete(Some("不在本次托管里")),
        ],
        vec![],
        always_ok()
    );
    let b = h.add_console("b");
    h.llm.substitute("{b}", &b);
    h.paint_on(&b, &["[sudo] password for e2e: ".to_string()]);
    h.start_with("在 A 上执行一下，B 先别动", "auto", &[&b]);
    let p = h.wait_terminal(Duration::from_secs(40));
    assert_completed(&h, &p);
    let refused = refused_gates(&h);
    assert_eq!(refused.len(), 2, "B 的密码提示一张 + 范围外终端一张：{refused:?}\n{}", h.dump());
    assert_eq!(refused[0]["target"], b.as_str(), "拒的是 B 上的按键：{refused:?}");
    assert!(refused[1]["target"] == "e2e-ghost", "{refused:?}");
    assert!(!h.screen_of(&b).contains("hunter2"), "按键不该进 B：\n{}", h.screen_of(&b));
    assert!(h.screen().contains("a-ok-hunter2"), "A 上照常执行：\n{}", h.screen());
}
