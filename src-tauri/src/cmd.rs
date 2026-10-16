use std::os::unix::fs::PermissionsExt;

use tauri::State;

use crate::keys;
use crate::sftp::{RemoteEntry, SftpError, SftpManager};
use crate::ssh::session::{ConnectRequest, SessionError, SessionManager};
use crate::store::{self, Store};

pub struct AppState {
    pub sessions: SessionManager,
    pub sftp: SftpManager,
    pub store: Store,
    pub local: crate::local::LocalManager,
}

// ——— 本地 shell ———

/// 通用落盘（ZMODEM 下载用）：数据由前端攒好后一次写入。
#[tauri::command]
pub fn write_file(path: String, data: Vec<u8>) -> Result<(), String> {
    std::fs::write(&path, &data).map_err(|e| format!("写入失败：{}", e))
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
    state.local.write(&id, &data)
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
    state.local.close(&id)
}

#[tauri::command]
pub async fn ssh_connect(
    id: String,
    req: ConnectRequest,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), SessionError> {
    state.sessions.connect(app, id, req).await
}

#[tauri::command]
pub fn ssh_write(id: String, data: Vec<u8>, state: State<'_, AppState>) -> Result<(), SessionError> {
    state.sessions.write(&id, data)
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
    state.store.delete(&id)?;
    store::secrets::delete(&id);
    Ok(())
}

// ——— 凭据（Keychain，不入库）———

#[tauri::command]
pub fn save_password(id: String, password: String) -> Result<(), String> {
    store::secrets::save(&id, &password)
}

#[tauri::command]
pub fn load_password(id: String) -> Option<String> {
    store::secrets::load(&id)
}

// ——— Xshell .xsh 导入 / 导出 ———

#[derive(serde::Serialize)]
pub struct ImportedFile {
    pub label: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// 批量导入 .xsh。密码无法从加密串还原，导入后需用户重填。
#[tauri::command]
pub fn import_xsh(paths: Vec<String>, state: State<AppState>) -> Result<Vec<ImportedFile>, String> {
    let mut out = Vec::new();
    for parsed in crate::xsh::parse_files(&paths) {
        let (ok, error) = match &parsed.result {
            Ok(new) => match state.store.create(new.clone()) {
                Ok(_) => (true, None),
                Err(e) => (false, Some(format!("入库失败：{}", e))),
            },
            Err(e) => (false, Some(e.clone())),
        };
        out.push(ImportedFile {
            label: parsed.label,
            ok,
            error,
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
        let safe = sanitize_filename(&s.name);
        let mut target = format!("{}/{}.xsh", dir.trim_end_matches('/'), safe);
        let mut n = 1;
        while std::path::Path::new(&target).exists() {
            n += 1;
            target = format!("{}/{} ({}).xsh", dir.trim_end_matches('/'), safe, n);
        }
        std::fs::write(&target, bytes).map_err(|e| format!("写入 {} 失败：{}", target, e))?;
        count += 1;
    }
    Ok(count)
}

fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | ':' | '\0' => '-',
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

/// 浏览本机目录 —— 文件管理器的本地面板。
#[tauri::command]
pub async fn list_local_dir(path: String) -> Result<Vec<RemoteEntry>, String> {
    let expanded = crate::keys::expand_tilde(&path);
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
        let full = if expanded.ends_with('/') {
            format!("{}{}", expanded, name)
        } else {
            format!("{}/{}", expanded, name)
        };
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
            permissions: Some(meta.permissions().mode()),
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
) -> Result<(), SftpError> {
    state.sftp.connect(id, req).await
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

#[tauri::command]
pub fn health() -> String {
    "nexus-term backend ok".into()
}
