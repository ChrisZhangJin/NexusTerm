//! 托管改文件前的原文备份（「还原」按钮用）。
//!
//! 托管经 nx_write / nx_edit **第一次**碰一个文件（同一终端上的同一路径）时，在
//! `<app_data>/host-backups/<contract_id>/index.json` 记一条（路径、终端、时间、状态），之后这一对不再记、不再备份：
//! - `backedUp`：改之前的原文存成旁边的 `<id>.bak`（带大小、sha256），可以「还原」；
//! - `created`：文件是这次托管新建的 —— 没有「原文」，也就不会把托管自己写的第一版当原文还原回去；
//! - `skipped`：原文没备份（超过 [`FILE_CAP`] / 二进制或非 UTF-8 / 本次合计超过 [`RUN_CAP`] / 原文件过大没读），`note` 写明原因。
//!
//! 只是本机文件，不进日志、不给模型；Unix 上目录一创建就是 0700、文件 0600。`index.json` 的读改写
//! 由进程内一把锁串行化。
//!
//! 保留：托管行被历史清理删掉后，对应的备份目录由 [`prune`] 一并删掉；此外只留最新 [`KEEP_RUNS`] 次托管、
//! 合计不超过 [`KEEP_BYTES`] 的备份（按最近一次记录时间从新到旧，超出的整次删掉；仍在进行的托管不删）。
//! 启动时执行。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 单个文件最多备份多少字节
pub const FILE_CAP: usize = 1024 * 1024;
/// 一次托管合计最多备份多少字节
pub const RUN_CAP: u64 = 20 * 1024 * 1024;
/// 最多保留多少次托管的备份
pub const KEEP_RUNS: usize = 30;
/// 全部备份合计最多占多少字节
pub const KEEP_BYTES: u64 = 500 * 1024 * 1024;
const INDEX: &str = "index.json";
const DIR: &str = "host-backups";

/// `index.json` 读改写的进程内锁（同一托管的多个终端可能并发写文件）
static INDEX_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn index_lock() -> std::sync::MutexGuard<'static, ()> {
    INDEX_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 第一次碰这个文件时的情况
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BackupStatus {
    /// 原文已备份（旧索引没有这个字段，都是这种）
    #[default]
    BackedUp,
    /// 托管新建的文件
    Created,
    /// 原文没备份，原因见 `note`
    Skipped,
}

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
    #[serde(default)]
    pub status: BackupStatus,
    /// `skipped` 的原因
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 托管第一次写一个文件时，它原来是什么样
#[derive(Debug, Clone, Copy)]
pub enum Touch<'a> {
    /// 文件原来不存在
    Created,
    /// 改之前的原文
    Original(&'a [u8]),
    /// 文件原来就有，但原文件过大没读
    Unread,
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

/// 不值得备份的原因：超过单文件上限 / 像二进制 / 不是合法 UTF-8。值得备份返回 None
pub fn skip_reason(bytes: &[u8]) -> Option<String> {
    if bytes.len() > FILE_CAP {
        Some(crate::tr!("原文件超过 1 MB，没有备份", "Original file is over 1 MB, not backed up"))
    } else if bytes[..bytes.len().min(8192)].contains(&0) || std::str::from_utf8(bytes).is_err() {
        Some(crate::tr!("原文件是二进制 / 不是 UTF-8 文本，没有备份", "Original file is binary / not UTF-8 text, not backed up"))
    } else {
        None
    }
}

/// 值得备份：不超过单文件上限、不像二进制、是合法 UTF-8
pub fn worth_backing_up(bytes: &[u8]) -> bool {
    skip_reason(bytes).is_none()
}

fn read_index(dir: &Path) -> Option<Index> {
    let s = fs::read_to_string(dir.join(INDEX)).ok()?;
    serde_json::from_str(&s).ok()
}

/// 目录一创建就是 0700（Unix）
fn create_private_dirs(dir: &Path) -> Result<(), String> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir).map_err(|e| e.to_string())
}

/// 先写临时文件（一创建就是 0600）再改名
fn write_private(p: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = p.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(&tmp).map_err(|e| e.to_string())?;
    f.write_all(bytes).map_err(|e| e.to_string())?;
    drop(f);
    fs::rename(&tmp, p).map_err(|e| e.to_string())
}

fn write_index(dir: &Path, idx: &Index) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(idx).map_err(|e| e.to_string())?;
    write_private(&dir.join(INDEX), &json)
}

/// 托管第一次写 `(path, console_id)` 时记一条：原文能备份就备份（`backedUp`），新建的记 `created`，
/// 备份不了的记 `skipped` 和原因。这一对本次托管已经记过时返回 `Ok(None)`（不再备份：
/// 后来的「原文」其实是托管自己写的）。
pub fn record(
    root: &Path,
    contract_id: &str,
    console_id: &str,
    path: &str,
    touch: Touch<'_>,
    now_ms: i64,
) -> Result<Option<BackupEntry>, String> {
    let _g = index_lock();
    let dir = dir_of(root, contract_id);
    let mut idx = read_index(&dir).unwrap_or_else(|| Index { contract_id: contract_id.to_string(), entries: Vec::new() });
    if idx.entries.iter().any(|e| e.path == path && e.console_id == console_id) {
        return Ok(None);
    }
    create_private_dirs(&dir)?;
    let id = format!("{:04}", idx.entries.len() + 1);
    let mut entry = BackupEntry {
        id,
        path: path.to_string(),
        console_id: console_id.to_string(),
        ts_ms: now_ms,
        size: 0,
        sha256: String::new(),
        restored_at: None,
        status: BackupStatus::Skipped,
        note: None,
    };
    match touch {
        Touch::Created => entry.status = BackupStatus::Created,
        Touch::Unread => {
            entry.note = Some(crate::tr!("原文件过大，没有读取，也就没有备份", "Original file too large to read, so not backed up"))
        }
        Touch::Original(original) => {
            let total: u64 = idx.entries.iter().filter(|e| e.status == BackupStatus::BackedUp).map(|e| e.size).sum();
            if let Some(why) = skip_reason(original) {
                entry.note = Some(why);
            } else if total + original.len() as u64 > RUN_CAP {
                entry.note = Some(crate::tr!(
                    "这次托管的备份已满 20 MB，没有再备份",
                    "This autopilot run's backups hit the 20 MB limit, not backed up"
                ));
            } else {
                write_private(&dir.join(format!("{}.bak", entry.id)), original)?;
                entry.status = BackupStatus::BackedUp;
                entry.size = original.len() as u64;
                entry.sha256 = sha256_hex(original);
            }
        }
    }
    idx.entries.push(entry.clone());
    write_index(&dir, &idx)?;
    Ok(Some(entry))
}

/// 记下一份原文（[`record`] 的 `Touch::Original`）。备份成功才返回 `Some`。
#[cfg(test)]
pub fn save(
    root: &Path,
    contract_id: &str,
    console_id: &str,
    path: &str,
    original: &[u8],
    now_ms: i64,
) -> Result<Option<BackupEntry>, String> {
    Ok(record(root, contract_id, console_id, path, Touch::Original(original), now_ms)?.filter(|e| e.status == BackupStatus::BackedUp))
}

/// 一次托管的全部记录（没有就是空）
pub fn list(root: &Path, contract_id: &str) -> Vec<BackupEntry> {
    read_index(&dir_of(root, contract_id)).map(|i| i.entries).unwrap_or_default()
}

/// 索引条目像不像真的：序号只有数字、路径不空
fn entry_valid(e: &BackupEntry) -> bool {
    !e.id.is_empty() && e.id.bytes().all(|b| b.is_ascii_digit()) && !e.path.trim().is_empty()
}

/// 取回一份原文，校验索引与 sha256。
pub fn load(root: &Path, contract_id: &str, id: &str) -> Result<(BackupEntry, Vec<u8>), String> {
    let dir = dir_of(root, contract_id);
    let idx = read_index(&dir).ok_or_else(|| crate::tr!("这次托管没有留下备份", "This autopilot run left no backups"))?;
    if idx.contract_id != contract_id {
        return Err(crate::tr!("备份索引和这次托管对不上，不还原", "Backup index doesn't match this autopilot run, not restoring"));
    }
    let e = idx
        .entries
        .into_iter()
        .find(|e| e.id == id)
        .ok_or_else(|| crate::tr!("找不到备份 {id}", "Backup {id} not found"))?;
    if !entry_valid(&e) {
        return Err(crate::tr!("备份索引损坏，不还原", "Backup index is corrupted, not restoring"));
    }
    match e.status {
        BackupStatus::BackedUp => {}
        BackupStatus::Created => {
            return Err(crate::tr!(
                "「{}」是托管新建的，没有原文可还原",
                "\"{}\" was created by autopilot, there's no original to restore",
                e.path
            ))
        }
        BackupStatus::Skipped => {
            return Err(crate::tr!("「{}」没有备份原文，无法还原", "\"{}\" has no backup of the original, can't restore", e.path))
        }
    }
    let bytes = fs::read(dir.join(format!("{}.bak", e.id)))
        .map_err(|err| crate::tr!("备份文件读不出来：{err}", "Can't read the backup file: {err}"))?;
    if sha256_hex(&bytes) != e.sha256 {
        return Err(crate::tr!("备份文件已损坏（校验和对不上），不还原", "Backup file is corrupted (checksum mismatch), not restoring"));
    }
    Ok((e, bytes))
}

/// 标记已还原
pub fn mark_restored(root: &Path, contract_id: &str, id: &str, now_ms: i64) -> Result<(), String> {
    let _g = index_lock();
    let dir = dir_of(root, contract_id);
    let mut idx = read_index(&dir).ok_or_else(|| crate::tr!("这次托管没有留下备份", "This autopilot run left no backups"))?;
    if let Some(e) = idx.entries.iter_mut().find(|e| e.id == id) {
        e.restored_at = Some(now_ms);
    }
    write_index(&dir, &idx)
}

/// 删掉不再保留的托管的备份目录（`keep(contract_id)` 为 false 的；索引读不出来的也删），
/// 再按 [`KEEP_RUNS`] / [`KEEP_BYTES`] 从旧到新删掉超出的（`live(contract_id)` 为 true 的不删）。返回删了几个。
pub fn prune(root: &Path, keep: impl Fn(&str) -> bool, live: impl Fn(&str) -> bool) -> usize {
    prune_with(root, keep, live, KEEP_RUNS, KEEP_BYTES)
}

fn prune_with(root: &Path, keep: impl Fn(&str) -> bool, live: impl Fn(&str) -> bool, max_runs: usize, max_bytes: u64) -> usize {
    let _g = index_lock();
    let Ok(rd) = fs::read_dir(root) else { return 0 };
    let mut n = 0;
    // 留下的：(最近一次记录时间, 占用字节, 目录, 是否在进行)
    let mut kept: Vec<(i64, u64, PathBuf, bool)> = Vec::new();
    for ent in rd.flatten() {
        let p = ent.path();
        if !p.is_dir() {
            continue;
        }
        match read_index(&p).filter(|i| keep(&i.contract_id)) {
            Some(i) => {
                let newest = i.entries.iter().map(|e| e.ts_ms).max().unwrap_or(0);
                let bytes = i.entries.iter().filter(|e| e.status == BackupStatus::BackedUp).map(|e| e.size).sum();
                kept.push((newest, bytes, p, live(&i.contract_id)));
            }
            None => {
                if fs::remove_dir_all(&p).is_ok() {
                    n += 1;
                }
            }
        }
    }
    kept.sort_by(|a, b| b.0.cmp(&a.0));
    let (mut runs, mut bytes) = (0usize, 0u64);
    for (_, size, p, is_live) in kept {
        if is_live || (runs < max_runs && bytes + size <= max_bytes) {
            runs += 1;
            bytes += size;
        } else if fs::remove_dir_all(&p).is_ok() {
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
        assert_eq!(prune(&root, |id| id == "keep", |_| false), 3);
        assert_eq!(list(&root, "keep").len(), 1);
        assert!(list(&root, "drop").is_empty());
        let _ = fs::remove_dir_all(&root);
    }
    #[test]
    fn first_touch_wins_created_and_skipped_never_back_up_later_versions() {
        let root = tmp("touch");
        // 托管新建的文件：记 created，之后再改也不把托管自己写的第一版当原文
        let e = record(&root, "c", "t", "/srv/new.conf", Touch::Created, 1).unwrap().unwrap();
        assert_eq!((e.status, e.size), (BackupStatus::Created, 0));
        assert!(record(&root, "c", "t", "/srv/new.conf", Touch::Original(b"run v1"), 2).unwrap().is_none());
        assert!(load(&root, "c", &e.id).unwrap_err().contains("新建"));
        // 第一次备份不了（二进制）：记 skipped + 原因，之后的文本版本也不备份
        let e = record(&root, "c", "t", "/bin/tool", Touch::Original(b"\x7fELF\0"), 3).unwrap().unwrap();
        assert_eq!(e.status, BackupStatus::Skipped);
        assert!(e.note.as_deref().unwrap().contains("二进制"));
        assert!(record(&root, "c", "t", "/bin/tool", Touch::Original(b"text"), 4).unwrap().is_none());
        assert!(load(&root, "c", &e.id).unwrap_err().contains("没有备份"));
        let e = record(&root, "c", "t", "/var/huge.log", Touch::Unread, 5).unwrap().unwrap();
        assert!(e.note.as_deref().unwrap().contains("过大"));
        let all = list(&root, "c");
        assert_eq!(all.len(), 3);
        assert!(!dir_of(&root, "c").join("0001.bak").exists());
        // 旧索引（没有 status）当作已备份
        let old: BackupEntry = serde_json::from_str(r#"{"id":"0001","path":"/a","consoleId":"t","tsMs":1,"size":1,"sha256":"x"}"#).unwrap();
        assert_eq!(old.status, BackupStatus::BackedUp);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_validates_index_entries() {
        let root = tmp("validate");
        save(&root, "c-9", "t", "/etc/a", b"a", 1).unwrap().unwrap();
        let dir = dir_of(&root, "c-9");
        let idx = fs::read_to_string(dir.join(INDEX)).unwrap();
        // 目录名相同但索引里写的是别的托管
        fs::write(dir.join(INDEX), idx.replace("\"c-9\"", "\"c?9\"")).unwrap();
        assert!(load(&root, "c-9", "0001").unwrap_err().contains("对不上"));
        // 序号不是纯数字 / 路径为空
        fs::write(dir.join(INDEX), idx.replace("\"0001\"", "\"../x\"")).unwrap();
        assert!(load(&root, "c-9", "../x").unwrap_err().contains("损坏"));
        fs::write(dir.join(INDEX), idx.replace("\"/etc/a\"", "\" \"")).unwrap();
        assert!(load(&root, "c-9", "0001").unwrap_err().contains("损坏"));
        fs::write(dir.join(INDEX), &idx).unwrap();
        assert!(load(&root, "c-9", "0001").is_ok());
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn files_and_dirs_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("mode");
        save(&root, "c", "t", "/a", b"a", 1).unwrap().unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&dir_of(&root, "c")), 0o700);
        assert_eq!(mode(&dir_of(&root, "c").join("0001.bak")), 0o600);
        assert_eq!(mode(&dir_of(&root, "c").join(INDEX)), 0o600);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn retention_keeps_newest_runs_within_bytes_and_live_ones() {
        let root = tmp("retain");
        for i in 0..5 {
            save(&root, &format!("r{i}"), "t", "/a", &vec![b'a'; 100], i * 10).unwrap().unwrap();
        }
        // 最多 3 次：r4 r3 r2 留下；r0 仍在进行，也留
        assert_eq!(prune_with(&root, |_| true, |id| id == "r0", 3, u64::MAX), 1);
        assert!(list(&root, "r1").is_empty());
        assert_eq!(["r0", "r2", "r3", "r4"].map(|id| list(&root, id).len()), [1, 1, 1, 1]);
        // 合计上限 250 字节：只留最新两次（r0 在进行，不删）
        assert_eq!(prune_with(&root, |_| true, |id| id == "r0", 30, 250), 1);
        assert!(list(&root, "r2").is_empty());
        assert_eq!(list(&root, "r3").len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn concurrent_records_keep_every_entry() {
        let root = tmp("lock");
        let hs: Vec<_> = (0..8)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || save(&root, "c", "t", &format!("/f{i}"), b"x", 1).unwrap().unwrap())
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        let ids: std::collections::BTreeSet<_> = list(&root, "c").into_iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), 8);
        let _ = fs::remove_dir_all(&root);
    }
}
