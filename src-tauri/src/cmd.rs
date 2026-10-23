use tauri::State;

use crate::agent::StreamKind;
use crate::keys;
use crate::sftp::{RemoteEntry, SftpError, SftpManager};
use crate::ssh::session::{ConnectOutcome, ConnectRequest, SessionError, SessionManager};
use crate::store::{self, Store};

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

/// 追加写（ZMODEM 下载流式落盘）：每来一 MB 就写一次，内存不随文件大小增长。
///
/// `truncate = true` 时用 `create`（清空重建），之后一律 `append`。
/// 每次重新 open 而不长驻句柄：一 MB 一次的系统调用开销可以忽略，换来的是
/// 进程被强杀时不会留下未 flush 的句柄。
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

#[tauri::command]
pub async fn ssh_write(
    id: String,
    data: Vec<u8>,
    state: State<'_, AppState>,
) -> Result<(), SessionError> {
    // 多一次拷贝换来的是严格的顺序：只有真正发出去的输入才进 agent 上下文。
    // 写失败（会话已关）时缓冲里不该留下这一段 —— 命令没执行过，却出现在
    // 「人干过什么」里，比少记一条的误导性大得多。
    let res = state.sessions.write(&id, data.clone()).await;
    if res.is_ok() {
        state.agent.feed(&id, &data, StreamKind::In);
    }
    res
}

/// 大块二进制写入（ZMODEM 上传）：base64 字符串比 JSON 数字数组小 ~3 倍、解析快得多。
///
/// **async 是必须的**：它 await 发送队列，队列满时 IPC 才不返回。
/// 这样前端「读盘 → 编码 → 写」的循环会被网速卡住，而不是把整个文件堆进内存。
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
// 按会话身份（已保存连接配置的 id，本地固定 'local'）持久化，而不是按
// 临时的标签页 id —— 这样同一台服务器下次重连、甚至重启应用后新开的
// 标签，都能接着用之前在这台机器上敲过的命令做补全。

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

#[tauri::command]
pub fn clipboard_read() -> Result<String, String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.get_text())
        .map_err(|e| e.to_string())
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
// 宏的执行完全在前端（需要读终端屏幕、按输出节奏决定何时发下一步），
// 后端只负责持久化 —— 这样宏的定义与执行不会出现两套解析逻辑。

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

// ——— 凭据（明文随 db 保存，便于跨机器迁移）———

#[tauri::command]
pub fn save_password(id: String, password: String, state: State<'_, AppState>) -> Result<(), String> {
    state.store.set_password(&id, &password)
}

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
    let mut out = Vec::new();
    for parsed in crate::xsh::parse_files(&paths) {
        let (ok, error, password_imported) = match &parsed.result {
            Ok(new) => match state.store.create(new.clone()) {
                Ok(created) => {
                    // 如果密码解密成功，自动写入 db 明文密码列
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
    state.store.set_passphrase(&id, &passphrase)
}

#[tauri::command]
pub fn load_passphrase(id: String, state: State<'_, AppState>) -> Option<String> {
    state.store.get_passphrase(&id)
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
    state: State<'_, AppState>,
) -> Result<u64, SftpError> {
    state.sftp.download(&id, &remote, &local).await
}

#[tauri::command]
pub async fn sftp_upload(
    id: String,
    local: String,
    remote: String,
    state: State<'_, AppState>,
) -> Result<u64, SftpError> {
    state.sftp.upload(&id, &local, &remote).await
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
