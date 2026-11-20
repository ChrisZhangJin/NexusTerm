//! 托管改文件前的原文备份（「还原」按钮用）。
//!
//! 托管经 nx_write / nx_edit 覆盖或修改一个**已存在**的文件时，把改之前的原文存到
//! `<app_data>/host-backups/<contract_id>/<id>.bak`，旁边一份 `index.json`（路径、终端、时间、大小、sha256）。
//! - 同一次托管里同一个文件只留**第一次**改之前的原文（多次修改后还原回托管开始前的样子）；
//! - 单个文件最多 [`FILE_CAP`]、一次托管合计最多 [`RUN_CAP`]，超出的不备份；二进制 / 非 UTF-8 不备份；
//! - 只是本机文件，不进日志、不给模型；Unix 上目录 0700、文件 0600；
//! - 托管行被历史清理删掉后，对应的备份目录由 [`prune`] 一并删掉。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 单个文件最多备份多少字节
pub const FILE_CAP: usize = 1024 * 1024;
/// 一次托管合计最多备份多少字节
pub const RUN_CAP: u64 = 20 * 1024 * 1024;
const INDEX: &str = "index.json";
const DIR: &str = "host-backups";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupEntry {
    /// 本次托管内的序号（文件名）
    pub id: String,
    /// 解析后的绝对路径（`~` 已展开）
    pub path: String,
    /// 写入时所在终端的会话 id（还原走同一个终端的文件通道）
    pub console_id: String,
    pub ts_ms: i64,
    pub size: u64,
    pub sha256: String,
    /// 已经还原过的时间
    #[serde(default)]
    pub restored_at: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Index {
    contract_id: String,
    entries: Vec<BackupEntry>,
}

pub fn root(app_data: &Path) -> PathBuf {
    app_data.join(DIR)
}

/// 契约 id 当目录名：只留安全字符
fn dir_of(root: &Path, contract_id: &str) -> PathBuf {
    let safe: String = contract_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    root.join(if safe.is_empty() { "_".to_string() } else { safe })
}

fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

/// 值得备份：不超过单文件上限、不像二进制、是合法 UTF-8
pub fn worth_backing_up(bytes: &[u8]) -> bool {
    bytes.len() <= FILE_CAP && !bytes[..bytes.len().min(8192)].contains(&0) && std::str::from_utf8(bytes).is_ok()
}

fn read_index(dir: &Path) -> Option<Index> {
    let s = fs::read_to_string(dir.join(INDEX)).ok()?;
    serde_json::from_str(&s).ok()
}

#[cfg(unix)]
fn private(p: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(p, fs::Permissions::from_mode(mode));
}
#[cfg(not(unix))]
fn private(_p: &Path, _mode: u32) {}

fn write_private(p: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = p.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    private(&tmp, 0o600);
    fs::rename(&tmp, p).map_err(|e| e.to_string())
}

fn write_index(dir: &Path, idx: &Index) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(idx).map_err(|e| e.to_string())?;
    write_private(&dir.join(INDEX), &json)
}

/// 记下一份原文。不值得备份 / 这个文件本次已备份过 / 超出合计上限时返回 `Ok(None)`。
pub fn save(
    root: &Path,
    contract_id: &str,
    console_id: &str,
    path: &str,
    original: &[u8],
    now_ms: i64,
) -> Result<Option<BackupEntry>, String> {
    if !worth_backing_up(original) {
        return Ok(None);
    }
    let dir = dir_of(root, contract_id);
    let mut idx = read_index(&dir).unwrap_or_else(|| Index { contract_id: contract_id.to_string(), entries: Vec::new() });
    if idx.entries.iter().any(|e| e.path == path && e.console_id == console_id) {
        return Ok(None);
    }
    let total: u64 = idx.entries.iter().map(|e| e.size).sum();
    if total + original.len() as u64 > RUN_CAP {
        return Ok(None);
    }
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    private(root, 0o700);
    private(&dir, 0o700);
    let id = format!("{:04}", idx.entries.len() + 1);
    write_private(&dir.join(format!("{id}.bak")), original)?;
    let entry = BackupEntry {
        id,
        path: path.to_string(),
        console_id: console_id.to_string(),
        ts_ms: now_ms,
        size: original.len() as u64,
        sha256: sha256_hex(original),
        restored_at: None,
    };
    idx.entries.push(entry.clone());
    write_index(&dir, &idx)?;
    Ok(Some(entry))
}

/// 一次托管的全部备份（没有就是空）
pub fn list(root: &Path, contract_id: &str) -> Vec<BackupEntry> {
    read_index(&dir_of(root, contract_id)).map(|i| i.entries).unwrap_or_default()
}

/// 取回一份原文，校验 sha256。
pub fn load(root: &Path, contract_id: &str, id: &str) -> Result<(BackupEntry, Vec<u8>), String> {
    let dir = dir_of(root, contract_id);
    let idx = read_index(&dir).ok_or_else(|| "这次托管没有留下备份".to_string())?;
    let e = idx.entries.into_iter().find(|e| e.id == id).ok_or_else(|| format!("找不到备份 {id}"))?;
    let bytes = fs::read(dir.join(format!("{}.bak", e.id))).map_err(|err| format!("备份文件读不出来：{err}"))?;
    if sha256_hex(&bytes) != e.sha256 {
        return Err("备份文件已损坏（校验和对不上），不还原".into());
    }
    Ok((e, bytes))
}

/// 标记已还原
pub fn mark_restored(root: &Path, contract_id: &str, id: &str, now_ms: i64) -> Result<(), String> {
    let dir = dir_of(root, contract_id);
    let mut idx = read_index(&dir).ok_or_else(|| "这次托管没有留下备份".to_string())?;
    if let Some(e) = idx.entries.iter_mut().find(|e| e.id == id) {
        e.restored_at = Some(now_ms);
    }
    write_index(&dir, &idx)
}

/// 删掉不再保留的托管的备份目录（`keep(contract_id)` 为 false 的；索引读不出来的也删）。返回删了几个。
pub fn prune(root: &Path, keep: impl Fn(&str) -> bool) -> usize {
    let Ok(rd) = fs::read_dir(root) else { return 0 };
    let mut n = 0;
    for ent in rd.flatten() {
        let p = ent.path();
        if !p.is_dir() {
            continue;
        }
        let kept = read_index(&p).is_some_and(|i| keep(&i.contract_id));
        if !kept && fs::remove_dir_all(&p).is_ok() {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nx-host-backup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn save_keeps_first_original_and_loads_with_checksum() {
        let root = tmp("first");
        let e = save(&root, "c-1", "t1", "/etc/app.conf", b"port=80\n", 10).unwrap().unwrap();
        assert_eq!((e.id.as_str(), e.size), ("0001", 8));
        // 同一文件第二次改：不覆盖最初的原文
        assert!(save(&root, "c-1", "t1", "/etc/app.conf", b"port=81\n", 20).unwrap().is_none());
        // 另一台机器上的同名文件另算
        assert!(save(&root, "c-1", "t2", "/etc/app.conf", b"x", 30).unwrap().is_some());
        let all = list(&root, "c-1");
        assert_eq!(all.len(), 2);
        let (e, bytes) = load(&root, "c-1", "0001").unwrap();
        assert_eq!((e.path.as_str(), bytes.as_slice()), ("/etc/app.conf", b"port=80\n".as_slice()));
        mark_restored(&root, "c-1", "0001", 99).unwrap();
        assert_eq!(list(&root, "c-1")[0].restored_at, Some(99));
        // 篡改后拒绝
        fs::write(dir_of(&root, "c-1").join("0001.bak"), b"evil").unwrap();
        assert!(load(&root, "c-1", "0001").unwrap_err().contains("损坏"));
        assert!(load(&root, "c-1", "9999").is_err());
        assert!(list(&root, "nobody").is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn skips_binary_oversized_and_over_run_cap() {
        let root = tmp("caps");
        assert!(save(&root, "c", "t", "/bin/x", b"\x7fELF\0\0", 1).unwrap().is_none());
        assert!(save(&root, "c", "t", "/x", &[0xff, 0xfe, b'a'], 1).unwrap().is_none(), "非 UTF-8 不备份");
        let big = vec![b'a'; FILE_CAP + 1];
        assert!(save(&root, "c", "t", "/big", &big, 1).unwrap().is_none());
        let one = vec![b'a'; FILE_CAP];
        for i in 0..20 {
            assert!(save(&root, "c", "t", &format!("/f{i}"), &one, 1).unwrap().is_some());
        }
        assert!(save(&root, "c", "t", "/f20", &one, 1).unwrap().is_none(), "合计超过 20 MB");
        assert!(save(&root, "c", "t", "/small", b"", 1).unwrap().is_some(), "空文件也算原文");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_drops_unkept_and_unsafe_ids_stay_inside_root() {
        let root = tmp("prune");
        save(&root, "keep", "t", "/a", b"a", 1).unwrap();
        save(&root, "drop", "t", "/b", b"b", 1).unwrap();
        save(&root, "../../evil", "t", "/c", b"c", 1).unwrap();
        assert!(dir_of(&root, "../../evil").starts_with(&root));
        fs::create_dir_all(root.join("junk")).unwrap();
        assert_eq!(prune(&root, |id| id == "keep"), 3);
        assert_eq!(list(&root, "keep").len(), 1);
        assert!(list(&root, "drop").is_empty());
        let _ = fs::remove_dir_all(&root);
    }
}
