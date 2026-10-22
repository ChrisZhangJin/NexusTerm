//! 本地 shell（PTY）。
//!
//! 没有 SSH 连接时也能当终端用 —— 启动时自动开一个本地 shell 标签页。
//! 与 SSH 通道共用同一套事件协议（`…/data` 发 base64、`…/closed` 发退出码），
//! 前端因此只需要按 kind 换 API，不需要两套渲染逻辑。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use base64::Engine;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tauri::{AppHandle, Emitter, Manager};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// 子进程要被读线程 wait()、被 close() kill()，两边都要拿到，故用 Arc<Mutex>。
type SharedChild = Arc<Mutex<Box<dyn Child + Send + Sync>>>;

struct LocalSession {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: SharedChild,
}

pub struct LocalManager {
    sessions: Mutex<HashMap<String, LocalSession>>,
}

impl LocalManager {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// 开一个本地 shell。可执行文件由 [`default_login_shell`] 按平台决定。
    pub fn spawn(
        &self,
        id: &str,
        cols: u16,
        rows: u16,
        app: &AppHandle,
    ) -> Result<(), String> {
        // 登录 shell 比非登录 shell 环境变量完整，更接近日常终端体感
        let mut cmd = default_login_shell();

        // Windows 上 $HOME 大多不存在（或为 Git Bash 风格的 /c/Users/xxx），
        // 交给 keys::home_dir() 按平台解析，否则 PTY 会因 cwd 非法直接启动失败
        let home = crate::keys::home_dir();
        cmd.cwd(&home);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: rows.max(1),
                cols: cols.max(20),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("创建伪终端失败：{}", e))?;

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("启动本地 shell 失败：{}", e))?;

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("获取 PTY 读端失败：{}", e))?;
        // writer 必须在 slave spawn 之后取，否则写入会落到错误的 fd
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("获取 PTY 写端失败：{}", e))?;

        let child = Arc::new(Mutex::new(child));
        {
            let mut guard = self.sessions.lock().unwrap();
            guard.insert(
                id.to_string(),
                LocalSession {
                    master: pair.master,
                    writer,
                    child: Arc::clone(&child),
                },
            );
        }

        // 读取是阻塞的 std::io::Read，放进独立线程，避免卡住 tokio 的 worker
        let app_handle = app.clone();
        let session_id = id.to_string();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let payload = B64.encode(&buf[..n]);
                        let _ = app_handle.emit(&format!("nx://local/data/{}", session_id), payload);
                        // 旁路。这里是 std::thread（不是 tokio task），只能同步取状态，
                        // 故用 try_state 而不是 tauri 的异步 State；取不到就静默跳过。
                        if let Some(st) = app_handle.try_state::<crate::cmd::AppState>() {
                            st.agent
                                .feed(&session_id, &buf[..n], crate::agent::StreamKind::Out);
                        }
                    }
                    Err(_) => break,
                }
            }
            // 流结束 → 子进程已退出，wait 拿真实退出码
            let code = match child.lock().unwrap().wait() {
                Ok(st) => st.exit_code() as i32,
                Err(_) => 0,
            };
            let _ = app_handle.emit(&format!("nx://local/closed/{}", session_id), code);
        });

        Ok(())
    }

    /// 该本地会话是否存在。**轻量且无副作用**（不取写锁、不碰 pty），
    /// 供需要按 id 判断「到底是本地 shell 还是 SSH 会话」的调用方使用。
    pub fn contains(&self, id: &str) -> bool {
        self.sessions.lock().unwrap().contains_key(id)
    }

    pub fn write(&self, id: &str, data: &[u8]) -> Result<(), String> {
        let mut guard = self.sessions.lock().unwrap();
        let s = guard.get_mut(id).ok_or("本地会话不存在")?;
        s.writer
            .write_all(data)
            .map_err(|e| format!("写入 PTY 失败：{}", e))?;
        s.writer.flush().map_err(|e| format!("刷新 PTY 失败：{}", e))
    }

    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), String> {
        let guard = self.sessions.lock().unwrap();
        let s = guard.get(id).ok_or("本地会话不存在")?;
        s.master
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(20),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("调整 PTY 尺寸失败：{}", e))
    }

    /// 关闭：先杀进程，再从表里移除。
    ///
    /// 这里只管 PTY 本体：agent 的旁路缓冲拿不到 AppState（本结构体不持有它），
    /// 清理统一由命令层 `cmd::local_close` 在调用本方法后做。
    pub fn close(&self, id: &str) -> Result<(), String> {
        let mut guard = self.sessions.lock().unwrap();
        let Some(s) = guard.remove(id) else {
            return Ok(());
        };
        let _ = s.child.lock().unwrap().kill();
        Ok(())
    }
}

/// 各平台的默认登录 shell。
///
/// 这里必须按平台分支：`$SHELL` 和 `/bin/zsh` 在 Windows 上都不存在，
/// 不做分支的话那边一开标签就启动失败。
#[cfg(windows)]
fn default_login_shell() -> CommandBuilder {
    let shell = std::env::var("COMSPEC")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "cmd.exe".to_string());
    let mut cmd = CommandBuilder::new(shell);
    // ConPTY 下 cmd.exe 按系统 ANSI 代码页输出（简体中文环境是 GBK），
    // 而 xterm.js 一律按 UTF-8 解码 —— 不切页的话所有中文都是乱码。
    // `/K` 执行完这条命令后回到交互提示符，输出被重定向掉以免首行出现噪音。
    // 代价：`chcp 65001` 下少数老式控制台程序（如 ping）的进度重绘会错位。
    cmd.args(["/K", "chcp 65001 >nul"]);
    cmd
}

#[cfg(unix)]
fn default_login_shell() -> CommandBuilder {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/zsh".to_string());
    // `-l` 让它成为登录 shell：环境变量与用户日常打开终端时一致
    let mut cmd = CommandBuilder::new(shell);
    cmd.arg("-l");
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 可执行文件在 PATH 里时是裸名字（如 `cmd.exe`），要按平台规则找一遍。
    fn exists_in_path(exe: &str) -> bool {
        let p = std::path::Path::new(exe);
        if p.is_absolute() || exe.contains('/') || exe.contains('\\') {
            return p.exists();
        }
        let exts: &[&str] = if cfg!(windows) {
            &["", ".exe", ".cmd", ".bat"]
        } else {
            &[""]
        };
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
            .any(|dir| exts.iter().any(|e| dir.join(format!("{exe}{e}")).exists()))
    }

    /// 本地终端是启动即开的，shell 路径解析错就是 100% 复现的故障。
    /// Windows 上曾因 `Result::filter` 编译不过而根本没跑到这一步。
    #[test]
    fn default_shell_is_resolvable() {
        let cmd = default_login_shell();
        let argv = cmd.get_argv();
        assert!(!argv.is_empty(), "shell 可执行文件为空");
        let exe = argv[0].to_string_lossy().to_string();
        assert!(!exe.is_empty(), "shell 可执行文件为空字符串");
        assert!(exists_in_path(&exe), "默认 shell 找不到：{}", exe);
    }

    /// Windows 上必须带上切 UTF-8 的参数，否则本地终端中文全是乱码。
    #[test]
    #[cfg(windows)]
    fn windows_shell_switches_to_utf8() {
        let cmd = default_login_shell();
        let joined = cmd
            .get_argv()
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("65001"),
            "缺少 chcp 65001，中文会乱码：{}",
            joined
        );
    }
}
