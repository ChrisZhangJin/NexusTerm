use tauri::{AppHandle, State};

use crate::agent::StreamKind;
use crate::keys;
use crate::sftp::{RemoteEntry, SftpError, SftpManager};
use crate::ssh::session::{ConnectOutcome, ConnectRequest, SessionError, SessionManager};
use crate::store::{self, Store, SweepReport};
use zeroize::Zeroizing;

pub struct AppState {
    pub sessions: SessionManager,
    pub sftp: SftpManager,
    pub store: Store,
    pub local: crate::local::LocalManager,
    /// AI agent 的旁路缓冲。会话数据流在完成一次 emit 之后顺手喂一份给它。
    pub agent: crate::agent::AgentManager,
    /// AI agent 的出网层：流式提问与取消。
    pub llm: crate::agent::llm::LlmRuntime,
    /// AI agent 的动作层运行时：一轮 Agentic 运行的取消标记 + 人工审批登记表。
    pub act: crate::agent::act::ActRuntime,
    /// 二次验证（keyboard-interactive）的问答登记表。
    /// 与 `sessions` / `sftp` 内部持有的是同一个 Arc。
    pub auth_prompts: std::sync::Arc<crate::ssh::prompt::AuthPrompts>,
    /// 用户密钥库目录（`<app_data>/keys`）。私钥文件落在这里，
    /// 不碰 `~/.ssh` —— 那是 ssh / git 自己的资产。
    pub keys_dir: std::path::PathBuf,
}

// ——— 本地 shell ———

/// 通用落盘（ZMODEM 下载用）：数据由前端攒好后一次写入。
#[tauri::command]
pub fn write_file(path: String, data: Vec<u8>) -> Result<(), String> {
    std::fs::write(&path, &data).map_err(|e| format!("写入失败：{}", e))
}

/// 大文件落盘（ZMODEM 下载）。前端把字节编成 base64 传来：
/// 走 `number[]` 时一个 600MB 文件会变成 6 亿个 JSON 数字，内存和解析都撑不住。
#[tauri::command]
pub fn write_file_b64(path: String, data: String) -> Result<(), String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .map_err(|e| format!("base64 解码失败：{}", e))?;
    std::fs::write(&path, &bytes).map_err(|e| format!("写入失败：{}", e))
}

/// 输入诊断日志落盘（排查 macOS 中文输入法用）。
///
/// 不复用 `append_file_b64`：那条要前端给绝对路径，而前端拿不到 home
/// （`path` 插件没装）。后端自己知道 app_data 在哪，直接写
/// `<app_data>/logs/input.log`，和 `nexus.log` 放一起方便现场排查。
///
/// 内容由 `lib/macInput` 产出：每个输入事件的 keyCode / isComposing / data /
/// textarea，以及每个真正发往 pty 的字节的来源 —— 输入法那几类症状（丢字 /
/// 多空格 / 标点重复）在用户机器上转瞬即逝，光靠描述猜不出来。
#[tauri::command]
pub fn append_input_log(app: AppHandle, lines: String) -> Result<(), String> {
    use std::io::Write as _;
    let dir = crate::app_data_dir(&app).join("logs");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建日志目录失败：{e}"))?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("input.log"))
        .map_err(|e| format!("打开 input.log 失败：{e}"))?;
    f.write_all(lines.as_bytes())
        .map_err(|e| format!("写入失败：{e}"))?;
    Ok(())
}

/// 追加写（ZMODEM 下载流式落盘）：每来一 MB 就写一次，内存不随文件大小增长。
/// 每次重新 open 而不长驻句柄：系统调用开销可忽略，换来的是进程被强杀时不留未 flush 的句柄。
#[tauri::command]
pub fn append_file_b64(path: String, data: String, truncate: bool) -> Result<(), String> {
    use base64::Engine as _;
    use std::io::Write as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .map_err(|e| format!("base64 解码失败：{}", e))?;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true);
    // append 与 truncate 互斥，不能同时设（Windows 上会直接报错）
    if truncate {
        opts.truncate(true);
    } else {
        opts.append(true);
    }
    let mut f = opts
        .open(&path)
        .map_err(|e| format!("打开文件失败：{}", e))?;
    f.write_all(&bytes)
        .map_err(|e| format!("写入失败：{}", e))?;
    Ok(())
}

/// 开一个本地 PTY。id 由前端生成，与标签页一致。
#[tauri::command]
pub fn local_spawn(
    id: String,
    cols: u16,
    rows: u16,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    state.local.spawn(&id, cols, rows, &app)
}

#[tauri::command]
pub fn local_write(id: String, data: Vec<u8>, state: State<'_, AppState>) -> Result<(), String> {
    state.local.write(&id, &data)?;
    // 写成功才旁路：没真正送进 PTY 的输入不该出现在 agent 的上下文里
    state.agent.feed(&id, &data, StreamKind::In);
    Ok(())
}

#[tauri::command]
pub fn local_write_b64(id: String, data: String, state: State<'_, AppState>) -> Result<(), String> {
    use base64::Engine as _;
    // 先解码再喂：缓冲里必须存原始字节，存 base64 的话快照全是乱码
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .map_err(|e| format!("base64 解码失败：{}", e))?;
    state.local.write(&id, &bytes)?;
    state.agent.feed(&id, &bytes, StreamKind::In);
    Ok(())
}

#[tauri::command]
pub fn local_resize(
    id: String,
    cols: u16,
    rows: u16,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.local.resize(&id, cols, rows)
}

#[tauri::command]
pub fn local_close(id: String, state: State<'_, AppState>) -> Result<(), String> {
    let res = state.local.close(&id);
    // 无论关闭是否报错（会话可能根本不存在），agent 侧都要清干净
    state.agent.remove(&id);
    res
}

#[tauri::command]
pub async fn ssh_connect(
    id: String,
    req: ConnectRequest,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<ConnectOutcome, SessionError> {
    state.sessions.connect(app, id, req).await
}

/// 连接编辑弹窗的「测试连接」：只验证能不能握手+认证，不开通道/PTY，
/// 不在连接列表或标签栏留下任何痕迹。
#[tauri::command]
pub async fn ssh_test_connect(
    req: ConnectRequest,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<ConnectOutcome, SessionError> {
    let id = format!("test-{}", uuid::Uuid::new_v4().simple());
    state.sessions.test_connect(app, id, req).await
}

#[tauri::command]
pub async fn ssh_write(
    id: String,
    data: Vec<u8>,
    state: State<'_, AppState>,
) -> Result<(), SessionError> {
    // 写失败时不进 agent 缓冲：没真正执行过的命令不该出现在「人干过什么」里
    let res = state.sessions.write(&id, data.clone()).await;
    if res.is_ok() {
        state.agent.feed(&id, &data, StreamKind::In);
    }
    res
}

/// 大块二进制写入（ZMODEM 上传）：base64 比 JSON 数字数组小 ~3 倍、解析快得多。
/// async 是必须的：它 await 发送队列，队列满时前端的读盘循环会被网速卡住，而不是把整个文件堆进内存。
#[tauri::command]
pub async fn ssh_write_b64(
    id: String,
    data: String,
    state: State<'_, AppState>,
) -> Result<(), SessionError> {
    use base64::Engine as _;
    // 解码后再喂：缓冲里存原始字节，base64 正文对模型毫无意义
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .map_err(|e| SessionError::Channel(format!("base64 解码失败：{}", e)))?;
    let res = state.sessions.write(&id, bytes.clone()).await;
    if res.is_ok() {
        state.agent.feed(&id, &bytes, StreamKind::In);
    }
    res
}

#[tauri::command]
pub fn ssh_resize(
    id: String,
    cols: u16,
    rows: u16,
    state: State<'_, AppState>,
) -> Result<(), SessionError> {
    state.sessions.resize(&id, cols, rows)
}

#[tauri::command]
pub fn ssh_close(id: String, state: State<'_, AppState>) {
    state.sessions.close(&id);
    // 会话没了，旁路缓冲也要跟着放掉：AppState 活到应用退出，
    // 不清理的话每个关掉的标签都会留下最多 256KB 常驻内存
    state.agent.remove(&id);
}

/// 回填二次验证（keyboard-interactive）的答案。
///
/// `answers` 按问题顺序排列；传 `null` 表示用户取消，后端会立刻中止认证，
/// 不会傻等到超时。返回 true 表示确实有一个等待中的认证收到了答案。
#[tauri::command]
pub fn ssh_auth_respond(
    id: String,
    answers: Option<Vec<String>>,
    state: State<'_, AppState>,
) -> bool {
    state.auth_prompts.respond(&id, answers)
}

/// 探测标签当前的工作目录 / 前台进程 / tmux 会话（用于复制会话、断线恢复）
#[tauri::command]
pub async fn ssh_probe(
    id: String,
    state: State<'_, AppState>,
) -> Result<crate::ssh::session::ProbeInfo, SessionError> {
    state.sessions.probe(&id).await
}

/// 远端资源快照（CPU / 内存 / 磁盘），状态栏轮询用
#[tauri::command]
pub async fn ssh_stats(
    id: String,
    state: State<'_, AppState>,
) -> Result<crate::ssh::session::ProbeInfo, SessionError> {
    state.sessions.stats(&id).await
}

/// 本机资源快照（本地终端标签的状态栏用），KEY 与 ssh_stats 一致
#[tauri::command]
pub async fn local_stats() -> Result<crate::sysstat::Stats, String> {
    crate::sysstat::collect().await
}

// ——— Console 自动补全：session 级命令历史 ———
//
// 按会话身份（已保存连接配置的 id，本地固定 'local'）持久化而非临时标签页 id，
// 这样重连甚至重启应用后新开的标签都能接着用之前敲过的命令补全。

#[tauri::command]
pub fn history_record(key: String, command: String, state: State<'_, AppState>) -> Result<(), String> {
    state.store.record_history(&key, &command)
}

#[tauri::command]
pub fn history_list(key: String, state: State<'_, AppState>) -> Result<Vec<String>, String> {
    state.store.list_history(&key)
}

// ——— 会话附加状态（跳板机登录路径 / 兼容模式）———

/// 一次取回所有会话的附加状态。前端启动时缓存，`connect` 时按 configId 查，
/// 免掉「每次连接多一次 IPC」和在连接流程里插 await。
#[tauri::command]
pub fn session_extras(
    state: State<'_, AppState>,
) -> std::collections::HashMap<String, store::SessionExtras> {
    state.store.all_extras()
}

#[tauri::command]
pub fn set_session_extras(
    id: String,
    extras: store::SessionExtras,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.store.set_extras(&id, &extras)
}

/// 运行中切换某个标签的旁路通道开关（前端发现堡垒机拒绝并行通道时调用）。
#[tauri::command]
pub fn ssh_set_sidecar(id: String, on: bool, state: State<'_, AppState>) {
    state.sessions.set_sidecar(&id, on);
}

// ——— 系统剪贴板 ———
// WebView2 里 navigator.clipboard.readText 会被权限策略拦截（静默失败），
// 统一走后端读写系统剪贴板。

/// 剪贴板里是位图（截图工具「复制」放的就是这种）时编码成 PNG 字节，落盘位置由调用方决定。
fn encode_clipboard_image_to_png() -> Result<Vec<u8>, String> {
    let img = arboard::Clipboard::new()
        .and_then(|mut c| c.get_image())
        .map_err(|e| e.to_string())?;
    let buf = image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned())
        .ok_or_else(|| "剪贴板图片数据尺寸不对，无法解码".to_string())?;
    let mut bytes: Vec<u8> = Vec::new();
    buf.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .map_err(|e| format!("PNG 编码失败：{e}"))?;
    Ok(bytes)
}

/// 落盘成本地临时文件，返回路径当文本粘贴进终端——pty 只能传字节流，这是让
/// 终端里的程序认出「刚粘贴的是一张图」的唯一办法（iTerm2 等终端同样如此）。
fn write_png_bytes_to_local_temp(bytes: &[u8]) -> Result<String, String> {
    let path = std::env::temp_dir().join(format!("nexusterm-paste-{}.png", uuid::Uuid::new_v4().simple()));
    std::fs::write(&path, bytes).map_err(|e| format!("无法写入临时文件：{e}"))?;
    Ok(path.display().to_string())
}

#[tauri::command]
pub async fn clipboard_read(tab_id: String, state: State<'_, AppState>) -> Result<String, String> {
    match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
        Ok(t) if !t.is_empty() => return Ok(t),
        _ => {}
    }
    let png = encode_clipboard_image_to_png()?;
    // 远程 tab：直接写到远端 /tmp，省掉手动 scp。写入失败（堡垒机/兼容模式等）静默回退本地。
    if !tab_id.is_empty() && state.sessions.has(&tab_id) {
        let remote_path = format!("/tmp/nexusterm-paste-{}.png", uuid::Uuid::new_v4().simple());
        if state.sessions.write_remote_file(&tab_id, &remote_path, &png).await.is_ok() {
            return Ok(remote_path);
        }
    }
    write_png_bytes_to_local_temp(&png)
}

#[tauri::command]
pub fn clipboard_write(text: String) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.set_text(text))
        .map_err(|e| e.to_string())
}

// ——— 会话持久化 ———

#[tauri::command]
pub fn list_sessions(state: State<'_, AppState>) -> Result<Vec<store::SessionRecord>, String> {
    state.store.list()
}

#[tauri::command]
pub fn create_session(
    session: store::NewSession,
    state: State<'_, AppState>,
) -> Result<store::SessionRecord, String> {
    state.store.create(session)
}

#[tauri::command]
pub fn update_session(
    id: String,
    session: store::NewSession,
    state: State<'_, AppState>,
) -> Result<store::SessionRecord, String> {
    state.store.update(&id, session)
}

#[tauri::command]
pub fn delete_session(id: String, state: State<'_, AppState>) -> Result<(), String> {
    // 密码随 sessions 行一起删除，无需单独清理
    state.store.delete(&id)
}

// ——— 宏 ———
//
// 执行完全在前端（需要读终端屏幕、按输出节奏决定何时发下一步），后端只负责持久化。

#[tauri::command]
pub fn list_macros(state: State<'_, AppState>) -> Result<Vec<store::MacroRecord>, String> {
    state.store.list_macros()
}

#[tauri::command]
pub fn create_macro(
    name: String,
    steps: Vec<store::MacroStep>,
    state: State<'_, AppState>,
) -> Result<store::MacroRecord, String> {
    state.store.create_macro(&name, &steps)
}

#[tauri::command]
pub fn update_macro(
    id: String,
    name: String,
    steps: Vec<store::MacroStep>,
    state: State<'_, AppState>,
) -> Result<store::MacroRecord, String> {
    state.store.update_macro(&id, &name, &steps)
}

#[tauri::command]
pub fn delete_macro(id: String, state: State<'_, AppState>) -> Result<(), String> {
    state.store.delete_macro(&id)
}

/// 拖拽后批量持久化排序与分组归属。
/// items: [(id, sortOrder, folder), ...]
#[tauri::command]
pub fn reorder_sessions(
    items: Vec<(String, i32, String)>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.store.reorder(&items)
}

// ——— 凭据 ———
//
// 默认随 db 明文保存（为了「拷到另一台机器就能接着用」）；用户在设置里启用
// 主密码之后，同样这两条路径会把值加密成 `v2:` 再落盘。见 `vault.rs`。

#[tauri::command]
pub fn save_password(id: String, password: String, state: State<'_, AppState>) -> Result<(), String> {
    let password = Zeroizing::new(password);
    state.store.set_password(&id, &password)
}

/// 读密码。
///
/// 返回 `Option` 而不是 `Result`：保险库锁着的时候这里给的是 `None`，和「没存过」
/// 一个样子。看着像是在掩盖错误，但对调用方来说是安全的 —— 锁定态下
/// `save_password` 一定失败，所以「读出空 → 用户保存 → 把真密码覆盖掉」这条
/// 丢数据的路走不通。真正该拦住用户的是界面上的解锁闸门。
#[tauri::command]
pub fn load_password(id: String, state: State<'_, AppState>) -> Option<String> {
    state.store.get_password(&id)
}

// ——— Xshell .xsh 导入 / 导出 ———

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedFile {
    pub label: String,
    pub ok: bool,
    pub error: Option<String>,
    /// 是否成功解密并导入了密码
    pub password_imported: bool,
}

/// 批量导入 .xsh。在同一台 Windows 机器上可自动解密密码并存入系统凭据服务。
#[tauri::command]
pub fn import_xsh(paths: Vec<String>, state: State<AppState>) -> Result<Vec<ImportedFile>, String> {
    // 整件事挡在门外，而不是「会话建好、密码一条都没进去」。后者更难收拾：
    // 用户看到一屏新连接，以为导完了，实际每一条都要重新填密码。
    if state.store.vault_locked() {
        return Err("保险库已锁定，请先输入主密码再导入".into());
    }
    let mut out = Vec::new();
    for parsed in crate::xsh::parse_files(&paths) {
        let (ok, error, password_imported) = match &parsed.result {
            Ok(new) => match state.store.create(new.clone()) {
                Ok(created) => {
                    let pw_ok = if let Some(ref pw) = parsed.password {
                        state.store.set_password(&created.id, pw).is_ok()
                    } else {
                        false
                    };
                    (true, None, pw_ok)
                }
                Err(e) => (false, Some(format!("入库失败：{}", e)), false),
            },
            Err(e) => (false, Some(e.clone()), false),
        };
        out.push(ImportedFile {
            label: parsed.label,
            ok,
            error,
            password_imported,
        });
    }
    Ok(out)
}

/// 导出单个会话为 .xsh（UTF-16LE+BOM，Xshell 原生格式）。密码不导出。
#[tauri::command]
pub fn export_xsh(id: String, path: String, state: State<AppState>) -> Result<(), String> {
    let s = state.store.get(&id)?;
    let bytes = crate::xsh::encode_xsh(
        &s.name,
        &s.host,
        s.port,
        &s.username,
        &s.auth_type,
        s.key_path.as_deref(),
    );
    std::fs::write(&path, bytes).map_err(|e| format!("写入失败：{}", e))
}

/// 导出指定 id 列表的会话到目录（分组导出用）。
#[tauri::command]
pub fn export_sessions_xsh(
    ids: Vec<String>,
    dir: String,
    state: State<AppState>,
) -> Result<usize, String> {
    let all = state.store.list()?;
    let selected: Vec<_> = all.into_iter().filter(|s| ids.contains(&s.id)).collect();
    let mut count = 0usize;
    for s in &selected {
        let bytes = crate::xsh::encode_xsh(
            &s.name,
            &s.host,
            s.port,
            &s.username,
            &s.auth_type,
            s.key_path.as_deref(),
        );
        let base = std::path::Path::new(&dir);
        let safe = sanitize_filename(&s.name);
        let mut target = base.join(format!("{}.xsh", safe));
        let mut n = 1;
        while target.exists() {
            n += 1;
            target = base.join(format!("{} ({}).xsh", safe, n));
        }
        std::fs::write(&target, bytes)
            .map_err(|e| format!("写入 {} 失败：{}", target.display(), e))?;
        count += 1;
    }
    Ok(count)
}

/// 导出全部会话到目录（每个会话一个 .xsh，重名自动加序号）。
#[tauri::command]
pub fn export_all_xsh(dir: String, state: State<AppState>) -> Result<usize, String> {
    let sessions = state.store.list()?;
    let mut count = 0usize;
    for s in &sessions {
        let bytes = crate::xsh::encode_xsh(
            &s.name,
            &s.host,
            s.port,
            &s.username,
            &s.auth_type,
            s.key_path.as_deref(),
        );
        let base = std::path::Path::new(&dir);
        let safe = sanitize_filename(&s.name);
        let mut target = base.join(format!("{}.xsh", safe));
        let mut n = 1;
        while target.exists() {
            n += 1;
            target = base.join(format!("{} ({}).xsh", safe, n));
        }
        std::fs::write(&target, bytes)
            .map_err(|e| format!("写入 {} 失败：{}", target.display(), e))?;
        count += 1;
    }
    Ok(count)
}

/// 会话名 → 合法文件名。Windows 额外保留 `\ * ? " < > |`，
/// 这些字符在 NTFS 上根本无法落盘，不过滤会让整个导出失败。
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '-',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "session".to_string()
    } else {
        trimmed.to_string()
    }
}

// ——— 密钥 ———

#[tauri::command]
pub async fn list_ssh_keys() -> Vec<keys::DiscoveredKey> {
    keys::discover_keys().await
}

#[tauri::command]
pub async fn verify_key(
    path: String,
    passphrase: Option<String>,
) -> Result<String, String> {
    let _key = keys::load_private_key(&path, passphrase).await?;
    Ok("ok".into())
}

// ——— 用户密钥库 ———

/// 全部可用密钥：库内的 + `~/.ssh` 里发现的（只读）。
#[tauri::command]
pub async fn list_keys(state: State<'_, AppState>) -> Result<Vec<keys::KeyInfo>, String> {
    Ok(keys::list_all(&state.store, &state.keys_dir).await)
}

#[tauri::command]
pub async fn import_key(
    src: String,
    name: Option<String>,
    passphrase: Option<String>,
    state: State<'_, AppState>,
) -> Result<keys::KeyInfo, String> {
    keys::import(&state.store, &state.keys_dir, &src, name, passphrase).await
}

/// 生成密钥可能要几秒（RSA 4096），前端应显示忙碌态。
#[tauri::command]
pub async fn generate_key(
    opts: keys::GenerateOptions,
    state: State<'_, AppState>,
) -> Result<keys::KeyInfo, String> {
    keys::generate(&state.store, &state.keys_dir, opts).await
}

#[tauri::command]
pub async fn delete_key(id: String, state: State<'_, AppState>) -> Result<(), String> {
    if id.starts_with("file:") {
        return Err("这是 ~/.ssh 里的密钥，请在系统中管理，本应用不会删除它".into());
    }
    keys::delete(&state.store, &id).await
}

#[tauri::command]
pub async fn export_key(
    id: String,
    dest: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    keys::export(&state.store, &id, &dest).await
}

#[tauri::command]
pub fn update_key_meta(
    id: String,
    name: Option<String>,
    comment: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    keys::update_meta(&state.store, &id, name, comment)
}

#[tauri::command]
pub async fn key_public(id: String, state: State<'_, AppState>) -> Result<String, String> {
    keys::public_key_of(&state.store, &id).await
}

/// 私钥口令（passphrase）与会话一起保存，理由是它只用于解开**本地**私钥文件，
/// 不是服务器凭据；不存的话每次连接都要重输。
#[tauri::command]
pub fn save_passphrase(
    id: String,
    passphrase: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let passphrase = Zeroizing::new(passphrase);
    state.store.set_passphrase(&id, &passphrase)
}

#[tauri::command]
pub fn load_passphrase(id: String, state: State<'_, AppState>) -> Option<String> {
    state.store.get_passphrase(&id)
}

// ——— 二次验证（TOTP）———
//
// 注意这组命令和上面几组的不对称：**没有 load_totp_seed**。种子一旦存进去就
// 不再出后端，前端只问得到「有没有」。密码会读回前端是因为要能查看和修改，
// 而种子是第二因子，让它定期在 IPC 上走一趟、落进 JS 堆里没有收益 ——
// 要改就重填一次，反正也就那一次。

/// 保存 TOTP 种子。接受裸 base32 或 `otpauth://totp/...` 链接。
/// 先解析一遍再存：种子抄错的话，此刻报错远好过下次连服务器时才发现。
#[tauri::command]
pub fn save_totp_seed(id: String, seed: String, state: State<'_, AppState>) -> Result<(), String> {
    let seed = seed.trim();
    if seed.is_empty() {
        return state.store.set_totp_seed(&id, "");
    }
    crate::totp::parse_secret(seed)?;
    state.store.set_totp_seed(&id, seed)
}

/// 这条连接有没有存过种子。前端据此显示「已保存」而不是把种子回填进输入框。
#[tauri::command]
pub fn has_totp_seed(id: String, state: State<'_, AppState>) -> bool {
    state.store.has_totp_seed(&id)
}

/// 用户填的种子**现在**会算出什么码，以及这个码还剩几秒。
///
/// 让人在保存前跟手机上的 app 对一眼。种子抄错是这个功能最常见的失败方式，
/// 而它的症状（连接时验证码被拒）离病因隔着一次连接，很难归因。
#[tauri::command]
pub fn totp_preview(seed: String) -> Result<TotpPreview, String> {
    let cfg = crate::totp::parse_secret(&seed)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(TotpPreview {
        code: cfg.code_at(now),
        remaining_secs: cfg.period() - (now % cfg.period()),
    })
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TotpPreview {
    pub code: String,
    pub remaining_secs: u64,
}

/// Windows 的驱动器列表。只有真正存在的盘才列出（跳过空软驱 A: 等）。
#[cfg(windows)]
fn list_drives() -> Result<Vec<RemoteEntry>, String> {
    let drives = (b'A'..=b'Z')
        .map(|c| format!("{}:\\", c as char))
        .filter(|root| std::path::Path::new(root).exists())
        .map(|root| RemoteEntry {
            name: root.clone(),
            path: root,
            is_dir: true,
            size: 0,
            mtime: None,
            permissions: None,
        })
        .collect::<Vec<_>>();
    if drives.is_empty() {
        return Err("没有找到可用的磁盘驱动器".into());
    }
    Ok(drives)
}

/// 非 Windows 没有盘符概念，空路径等价于列根目录。
#[cfg(not(windows))]
async fn list_drives() -> Result<Vec<RemoteEntry>, String> {
    read_dir_entries("/".to_string()).await
}

/// 文件权限位。远端走 SFTP 的 POSIX mode，本地面板则由本函数按平台合成。
#[cfg(unix)]
fn permission_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}

/// Windows 没有 POSIX mode。文件管理器只用它做「是否只读」的展示，
/// 因此按只读位合成一个近似值即可，不必也不能照搬 Unix 语义。
#[cfg(windows)]
fn permission_mode(meta: &std::fs::Metadata) -> u32 {
    let ro = meta.permissions().readonly();
    match (meta.is_dir(), ro) {
        (true, true) => 0o555,
        (true, false) => 0o755,
        (false, true) => 0o444,
        (false, false) => 0o666,
    }
}

/// 浏览本机目录 —— 文件管理器的本地面板。
///
/// 空路径是「列盘符」的约定：Windows 没有单一的 `/`，用户在 C 盘根目录再点
/// 一次「上级目录」时前端会传空串，此时列出所有可用驱动器供切换。
#[tauri::command]
pub async fn list_local_dir(path: String) -> Result<Vec<RemoteEntry>, String> {
    if path.is_empty() {
        #[cfg(windows)]
        {
            return list_drives();
        }
        #[cfg(not(windows))]
        {
            return list_drives().await;
        }
    }
    let expanded = crate::keys::expand_tilde(&path);
    read_dir_entries(expanded).await
}

/// 读取目录内容。`list_local_dir` 与非 Windows 的 `list_drives` 共用；
/// 独立成函数是为了避免两者互相调用形成 async 递归环（编译器报 E0733）。
async fn read_dir_entries(expanded: String) -> Result<Vec<RemoteEntry>, String> {
    let mut dir = tokio::fs::read_dir(&expanded)
        .await
        .map_err(|e| format!("无法读取目录 {}: {}", expanded, e))?;

    let mut entries = Vec::new();
    while let Some(entry) = dir
        .next_entry()
        .await
        .map_err(|e| format!("读取目录项失败: {}", e))?
    {
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = match entry.metadata().await {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_dir = meta.is_dir();
        // 用 PathBuf 而不是手拼分隔符：Windows 本地路径是 `C:\Users\…`，
        // 硬写 `/` 会让上级目录按钮与双击进入目录都算错。
        let full = std::path::Path::new(&expanded)
            .join(&name)
            .to_string_lossy()
            .to_string();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());

        entries.push(RemoteEntry {
            name,
            path: full,
            is_dir,
            size: meta.len(),
            mtime,
            permissions: Some(permission_mode(&meta)),
        });
    }

    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

// ——— SFTP ———

#[tauri::command]
pub async fn sftp_connect(
    id: String,
    req: ConnectRequest,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), SftpError> {
    state.sftp.connect(&app, id, req).await
}

#[tauri::command]
pub async fn sftp_list(
    id: String,
    path: String,
    state: State<'_, AppState>,
) -> Result<Vec<RemoteEntry>, SftpError> {
    state.sftp.list(&id, &path).await
}

#[tauri::command]
pub async fn sftp_home(id: String, state: State<'_, AppState>) -> Result<String, SftpError> {
    state.sftp.home(&id).await
}

#[tauri::command]
pub async fn sftp_download(
    id: String,
    remote: String,
    local: String,
    transfer_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<u64, SftpError> {
    state.sftp.download(&id, &remote, &local, &transfer_id, &app).await
}

#[tauri::command]
pub async fn sftp_upload(
    id: String,
    local: String,
    remote: String,
    transfer_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<u64, SftpError> {
    state.sftp.upload(&id, &local, &remote, &transfer_id, &app).await
}

#[tauri::command]
pub fn sftp_cancel_transfer(transfer_id: String, state: State<'_, AppState>) -> Result<(), String> {
    state.sftp.cancel_transfer(&transfer_id);
    Ok(())
}

#[tauri::command]
pub async fn sftp_mkdir(
    id: String,
    path: String,
    state: State<'_, AppState>,
) -> Result<(), SftpError> {
    state.sftp.mkdir(&id, &path).await
}

#[tauri::command]
pub async fn sftp_remove(
    id: String,
    path: String,
    is_dir: bool,
    state: State<'_, AppState>,
) -> Result<(), SftpError> {
    state.sftp.remove(&id, &path, is_dir).await
}

#[tauri::command]
pub async fn sftp_rename(
    id: String,
    from: String,
    to: String,
    state: State<'_, AppState>,
) -> Result<(), SftpError> {
    state.sftp.rename(&id, &from, &to).await
}

#[tauri::command]
pub async fn sftp_close(id: String, state: State<'_, AppState>) -> Result<(), SftpError> {
    state.sftp.close(&id).await;
    Ok(())
}

/// 宿主平台信息。前端的标题栏留白、窗口控制按钮、快捷键提示都要按它分支 ——
/// 用后端常量而不是 UA 嗅探，避免 WebView 版本差异导致误判。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostInfo {
    /// `windows` | `macos` | `linux`
    pub platform: String,
    pub arch: String,
    pub os_version: String,
}

#[tauri::command]
pub fn host_info() -> HostInfo {
    HostInfo {
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        os_version: os_version(),
    }
}

#[cfg(windows)]
fn os_version() -> String {
    // 只区分「Win10 之前 / 之后」即可：ConPTY 自 Win10 1809 起可用，
    // 本地终端在老系统上无法工作，值得在界面上说明。
    std::env::var("OS")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Windows".to_string())
}

#[cfg(target_os = "macos")]
fn os_version() -> String {
    std::env::var("SYSTEM_VERSION_COMPAT")
        .ok()
        .unwrap_or_else(|| "macOS".to_string())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn os_version() -> String {
    std::env::consts::OS.to_string()
}

#[tauri::command]
pub fn health() -> String {
    "nexus-term backend ok".into()
}

// ——— 保险库（主密码）———
//
// 后端是唯一真相。界面上的解锁闸门是 UX，不是安全边界 —— 浮动 Agent 窗口不挂
// `App.tsx`，而 `agent/act.rs` 的动作层能在完全没有前端参与的情况下去读密码。
// 真正拦得住的是 `Store` 这一层：锁着就写不进去。
//
// 主密码一进来就 move 进 `Zeroizing<String>`。管不到的副本要诚实标注：Tauri 的
// IPC 反序列化过程中 serde 已经产生过一份中间 String，webview 的 JS 堆里还有一份。

/// 保险库现状。**廉价**：只读一个内存里的枚举，不做 KDF、不碰系统凭据服务，
/// 所以前端可以随便调。
#[tauri::command]
pub fn vault_status(state: State<'_, AppState>) -> crate::vault::VaultStatus {
    state.store.vault().status()
}

/// 这个库的钥匙有没有记在本机。
///
/// **会访问系统凭据服务**，Linux 上没有 D-Bus 时可能卡住，所以它和
/// [`vault_status`] 是分开的两条命令：启动路径只调前者。
#[tauri::command]
pub fn vault_is_remembered(state: State<'_, AppState>) -> bool {
    state.store.vault_is_remembered()
}

/// 试着用本机记住的钥匙直接解锁。前端在**准备弹解锁框之前**调一次：
/// 返回 true 就不用弹了。没记住、或者记的是别的库的钥匙，都返回 false。
#[tauri::command]
pub fn vault_try_remembered(state: State<'_, AppState>) -> bool {
    state.store.vault_try_remembered()
}

/// 启用保险库：建密钥、落元数据、把现有凭据全部改写成密文。
#[tauri::command]
pub fn vault_enable(
    password: String,
    remember_here: bool,
    state: State<'_, AppState>,
) -> Result<SweepReport, String> {
    let password = Zeroizing::new(password);
    state.store.vault_enable(&password, remember_here)
}

#[tauri::command]
pub fn vault_unlock(
    password: String,
    remember_here: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let password = Zeroizing::new(password);
    state.store.vault_unlock(&password, remember_here)
}

/// 丢掉内存里的密钥。不影响「本机记住」—— 那是另一条命令。
#[tauri::command]
pub fn vault_lock(state: State<'_, AppState>) {
    state.store.vault_lock();
}

/// 忘掉本机记住的钥匙。当前这把锁不受影响，下次启动才要重新输主密码。
#[tauri::command]
pub fn vault_forget(state: State<'_, AppState>) {
    state.store.vault_forget();
}

/// 换主密码。只重新包装那把 DEK，一条凭据都不动，所以不管库里有多少条都是一瞬间。
#[tauri::command]
pub fn vault_change_password(
    old_password: String,
    new_password: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let old = Zeroizing::new(old_password);
    let new = Zeroizing::new(new_password);
    state.store.vault_change_password(&old, &new)
}

/// 关闭保险库，凭据还原成明文。要验主密码 —— 这一步比读一条密码危险得多。
#[tauri::command]
pub fn vault_disable(password: String, state: State<'_, AppState>) -> Result<SweepReport, String> {
    let password = Zeroizing::new(password);
    state.store.vault_disable(&password)
}

/// 忘了主密码时的逃生口：**销毁**全部加密凭据，退回未启用。
///
/// 返回被清掉的条数。这是破坏性重置不是恢复，界面上必须让用户先确认。
#[tauri::command]
pub fn vault_reset(state: State<'_, AppState>) -> Result<usize, String> {
    state.store.vault_reset()
}
