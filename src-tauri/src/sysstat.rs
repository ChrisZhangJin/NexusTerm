//! 本机资源快照（CPU / 内存 / 磁盘），供本地终端标签的状态栏使用。
//! 输出与远端 STATS_SCRIPT 相同的 KEY=VALUE 结构，前端共用一套解析与渲染。

use std::collections::HashMap;
use std::time::Duration;

pub type Stats = HashMap<String, String>;

/// CPU 占用需要两次采样求差，间隔与远端脚本一致（0.5s）
const SAMPLE: Duration = Duration::from_millis(500);

pub async fn collect() -> Result<Stats, String> {
    tokio::task::spawn_blocking(collect_blocking)
        .await
        .map_err(|e| format!("资源采集失败：{e}"))?
}

#[cfg(target_os = "windows")]
fn collect_blocking() -> Result<Stats, String> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use windows_sys::Win32::System::SystemInformation::{
        GetSystemInfo, GetSystemWindowsDirectoryW, GlobalMemoryStatusEx, MEMORYSTATUSEX,
        SYSTEM_INFO,
    };
    use windows_sys::Win32::System::Threading::GetSystemTimes;

    fn ft(f: &FILETIME) -> u64 {
        ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
    }
    /// (idle, total)。GetSystemTimes 的 kernel 时间已包含 idle
    fn times() -> Option<(u64, u64)> {
        let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut i, mut k, mut u) = (z, z, z);
        let ok = unsafe { GetSystemTimes(&mut i, &mut k, &mut u) };
        (ok != 0).then(|| (ft(&i), ft(&k) + ft(&u)))
    }

    let mut out = Stats::new();

    if let Some((i1, t1)) = times() {
        std::thread::sleep(SAMPLE);
        if let Some((i2, t2)) = times() {
            let dt = t2.saturating_sub(t1);
            let di = i2.saturating_sub(i1);
            if dt > 0 {
                let pct = (dt.saturating_sub(di)) as f64 * 100.0 / dt as f64;
                out.insert("CPU_PCT".into(), format!("{pct:.1}"));
            }
        }
    }

    unsafe {
        let mut si: SYSTEM_INFO = std::mem::zeroed();
        GetSystemInfo(&mut si);
        out.insert("CPUS".into(), si.dwNumberOfProcessors.to_string());

        let mut ms: MEMORYSTATUSEX = std::mem::zeroed();
        ms.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut ms) != 0 {
            out.insert("MEM_TOTAL".into(), ms.ullTotalPhys.to_string());
            out.insert("MEM_AVAIL".into(), ms.ullAvailPhys.to_string());
        }

        // 系统盘：取 Windows 目录所在盘符根，如 C:\
        let mut buf = [0u16; 260];
        let n = GetSystemWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) as usize;
        let root: Vec<u16> = if n >= 3 && n < buf.len() {
            buf[..3].iter().copied().chain(std::iter::once(0)).collect()
        } else {
            "C:\\\0".encode_utf16().collect()
        };
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        if GetDiskFreeSpaceExW(root.as_ptr(), &mut avail, &mut total, &mut free) != 0 && total > 0 {
            out.insert("DISK_TOTAL".into(), total.to_string());
            out.insert("DISK_USED".into(), total.saturating_sub(free).to_string());
            let mount = String::from_utf16_lossy(&root[..root.len() - 1]);
            out.insert("DISK_MOUNT".into(), mount);
        }
    }
    Ok(out)
}

// ── macOS 采集 ──────────────────────────────────────────────────────────
// macOS 没有 /proc，只能借系统自带命令。用 `top -l 2` 取第二次采样的 CPU 占用
// （第一次是从开机到现在的累计值，不具代表性），`vm_stat` + `sysctl` 取内存。
// 这些命令都是只读、无需特权，失败时优雅缺项而不是让状态栏报错。

#[cfg(target_os = "macos")]
fn sysctl_value(key: &str) -> Option<String> {
    let o = std::process::Command::new("sysctl")
        .args(["-n", key])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(target_os = "macos")]
fn macos_cpu_pct() -> Option<f64> {
    // `top` 在受限环境（沙箱 / 无 TTY）里可能被系统拒绝，改用 iostat：
    // `iostat -c 2 -w 1` 的表头含 `us sy id`，取最后一次采样的 idle 反推占用。
    // 磁盘数量决定列数，所以按表头里 "id" 的 token 位置取列，不写死下标。
    let o = std::process::Command::new("iostat")
        .args(["-c", "2", "-w", "1"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&o.stdout);
    let id_idx = s
        .lines()
        .find_map(|l| l.split_whitespace().position(|t| t == "id"))?;
    let last = s.lines().rev().find(|l| {
        l.split_whitespace()
            .next()
            .and_then(|x| x.parse::<f64>().ok())
            .is_some()
    })?;
    let idle: f64 = last.split_whitespace().nth(id_idx)?.parse().ok()?;
    Some((100.0 - idle).clamp(0.0, 100.0))
}

#[cfg(target_os = "macos")]
fn macos_mem() -> Option<(u64, u64)> {
    let total = sysctl_value("hw.memsize")?.parse::<u64>().ok()?;
    let page_size = sysctl_value("hw.pagesize")?.parse::<u64>().ok()?;
    let o = std::process::Command::new("vm_stat").output().ok()?;
    let s = String::from_utf8_lossy(&o.stdout);
    let mut avail_pages: u64 = 0;
    for line in s.lines() {
        let wanted = ["Pages free:", "Pages inactive:", "Pages speculative:"]
            .iter()
            .any(|k| line.starts_with(k));
        if !wanted {
            continue;
        }
        if let Some(v) = line
            .split(':')
            .nth(1)
            .and_then(|x| x.trim().trim_end_matches('.').parse::<u64>().ok())
        {
            avail_pages = avail_pages.saturating_add(v);
        }
    }
    let avail = avail_pages.saturating_mul(page_size).min(total);
    Some((total, avail))
}

#[cfg(target_os = "macos")]
fn macos_load() -> Option<String> {
    let s = sysctl_value("vm.loadavg")?;
    let nums: Vec<&str> = s
        .trim_matches(|c: char| !c.is_ascii_digit() && c != '.' && c != ' ')
        .split_whitespace()
        .take(3)
        .collect();
    (nums.len() == 3).then(|| nums.join(" "))
}

#[cfg(not(target_os = "windows"))]
fn collect_blocking() -> Result<Stats, String> {
    // Linux 读 /proc；macOS 没有 /proc，走下面 cfg(macos) 的系统命令分支
    fn cpu() -> Option<(u64, u64)> {
        let s = std::fs::read_to_string("/proc/stat").ok()?;
        let line = s.lines().find(|l| l.starts_with("cpu "))?;
        let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
        if v.len() < 5 {
            return None;
        }
        let idle = v[3] + v[4];
        Some((idle, v.iter().sum()))
    }
    let mut out = Stats::new();
    if let Some((i1, t1)) = cpu() {
        std::thread::sleep(SAMPLE);
        if let Some((i2, t2)) = cpu() {
            let dt = t2.saturating_sub(t1);
            let di = i2.saturating_sub(i1);
            if dt > 0 {
                let pct = (dt.saturating_sub(di)) as f64 * 100.0 / dt as f64;
                out.insert("CPU_PCT".into(), format!("{pct:.1}"));
            }
        }
    }
    if let Ok(n) = std::thread::available_parallelism() {
        out.insert("CPUS".into(), n.get().to_string());
    }
    if let Ok(s) = std::fs::read_to_string("/proc/loadavg") {
        let v: Vec<&str> = s.split_whitespace().take(3).collect();
        if v.len() == 3 {
            out.insert("LOAD".into(), v.join(" "));
        }
    }
    if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
        let kv = |k: &str| -> Option<u64> {
            s.lines()
                .find(|l| l.starts_with(k))?
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()
        };
        if let Some(t) = kv("MemTotal:") {
            let a = kv("MemAvailable:").or_else(|| {
                Some(kv("MemFree:")? + kv("Buffers:").unwrap_or(0) + kv("Cached:").unwrap_or(0))
            });
            out.insert("MEM_TOTAL".into(), (t * 1024).to_string());
            if let Some(a) = a {
                out.insert("MEM_AVAIL".into(), (a * 1024).to_string());
            }
        }
    }
    if let Ok(o) = std::process::Command::new("df").args(["-kP", "/"]).output() {
        let s = String::from_utf8_lossy(&o.stdout);
        if let Some(l) = s.lines().nth(1) {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() >= 6 {
                if let (Ok(t), Ok(u)) = (f[1].parse::<u64>(), f[2].parse::<u64>()) {
                    out.insert("DISK_TOTAL".into(), (t * 1024).to_string());
                    out.insert("DISK_USED".into(), (u * 1024).to_string());
                    out.insert("DISK_MOUNT".into(), f[5].to_string());
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if !out.contains_key("CPU_PCT") {
            if let Some(pct) = macos_cpu_pct() {
                out.insert("CPU_PCT".into(), format!("{pct:.1}"));
            }
        }
        if let Some((total, avail)) = macos_mem() {
            out.insert("MEM_TOTAL".into(), total.to_string());
            out.insert("MEM_AVAIL".into(), avail.to_string());
        }
        if let Some(load) = macos_load() {
            out.insert("LOAD".into(), load);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn collect_has_core_keys() {
        let s = super::collect_blocking().unwrap();
        eprintln!("{s:?}");
        for k in ["CPU_PCT", "CPUS", "MEM_TOTAL", "MEM_AVAIL"] {
            assert!(s.contains_key(k), "missing {k}");
        }
    }
}
