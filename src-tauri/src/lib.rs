use std::path::PathBuf;

use tauri::Manager;

use cmd::AppState;
use local::LocalManager;
use sftp::SftpManager;
use ssh::session::SessionManager;
use store::Store;

pub mod errors;
mod agent;
mod cmd;
mod histfile;
mod keys;
mod local;
mod migrate;
mod viewer;
mod secret;
mod sftp;
mod ssh;
mod store;
mod sysstat;
mod totp;
mod vault;
mod xsh;

pub(crate) fn app_data_dir(app: &tauri::AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .expect("无法定位应用数据目录")
}

/// 日志写入器：所有事件共用同一个文件句柄。
///
/// 为什么需要它：release 版是 `windows_subsystem = "windows"` 的 GUI 程序，
/// 根本没有 stderr 可看，`tracing_subscriber::fmt()` 的默认输出等于扔进黑洞。
/// 现场问题（尤其是「连上十几秒自己断」这种）没有日志就完全没法查。
#[derive(Clone)]
struct LogFile(std::sync::Arc<std::sync::Mutex<std::fs::File>>);

impl std::io::Write for LogFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut f = self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("日志文件锁已中毒"))?;
        std::io::Write::write_all(&mut *f, buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.0.lock() {
            Ok(mut f) => std::io::Write::flush(&mut *f),
            Err(_) => Ok(()),
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogFile {
    type Writer = LogFile;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// 日志过滤规则。
///
/// - `RUST_LOG` 优先（临时精细控制用）
/// - `NEXUS_DEBUG=1`：本项目 + russh 全开 debug。断链、握手、算法协商这类问题
///   基本都要靠它，`info` 级别看不到库内部发生了什么。
/// - 默认 `info`：但「服务端主动断开」那条是 `warn!`，所以**不开任何环境变量也能看到断链原因**。
fn log_filter() -> tracing_subscriber::EnvFilter {
    use tracing_subscriber::EnvFilter;
    if let Ok(f) = EnvFilter::try_from_default_env() {
        return f;
    }
    if std::env::var("NEXUS_DEBUG")
        .map(|v| v != "0")
        .unwrap_or(false)
    {
        return EnvFilter::new("info,nexus_term=debug,russh=debug,russh_sftp=debug");
    }
    EnvFilter::new("info")
}

/// 初始化日志：stderr（开发时看）+ `<app_data>/logs/nexus.log`（现场排查用）。
fn init_logging(data_dir: &std::path::Path) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // 把 `log` 宏接进 tracing。russh / russh-sftp 全部用 `log`，不装这道桥，
    // 它们内部的探活、kex、断开诊断全都会被丢掉（表现为「开了 debug 也什么都看不到」）。
    // 重复调用（例如测试里多次初始化）会返回 Err，忽略即可。
    let _ = tracing_log::LogTracer::init();

    let filter = log_filter();
    let log_dir = data_dir.join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let path = log_dir.join("nexus.log");

    // 超过 8MB 滚一份旧的，避免长期运行把用户磁盘写满
    if std::fs::metadata(&path)
        .map(|m| m.len() > 8 * 1024 * 1024)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&path, log_dir.join("nexus.log.1"));
    }

    let registry = tracing_subscriber::registry().with(filter);
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => {
            let _ = registry
                .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(LogFile(std::sync::Arc::new(std::sync::Mutex::new(f))))
                        .with_ansi(false),
                )
                .try_init();
            tracing::info!(path = %path.display(), "日志已就绪（文件 + stderr）");
        }
        Err(e) => {
            let _ = registry.with(tracing_subscriber::fmt::layer()).try_init();
            tracing::warn!(error = %e, "日志文件打不开，只输出到 stderr");
        }
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        // 浏览窗口里 HTML iframe 的页面来源（带独立 CSP，默认禁脚本）
        .register_uri_scheme_protocol("nxdoc", viewer::protocol)
        .setup(|app| {
            let dir = app_data_dir(app.handle());
            // 先起日志：下面每一步（建库、恢复会话）出错都需要留下记录
            init_logging(&dir);
            // Agent 过程日志（默认不写，用户勾选「记录推理过程」后才会落到 agent.log）
            agent::trace::init(&dir);
            let keys_dir = dir.join(crate::keys::VAULT_DIR);
            let store = Store::open(dir).expect("SQLite 初始化失败");
            // 把旧版存在系统凭据服务里的密码迁移到 db 的凭据列。
            //
            // 保险库锁着的时候**必须跳过**：那时写不进去，而迁移的收尾动作是删掉
            // 凭据服务里的源头 —— 仅存的那一份。解锁成功后 `after_unlock` 会补跑。
            if !store.vault_locked() {
                store.migrate_legacy_secrets();
            }
            let sessions = SessionManager::new();
            // SFTP 复用同一份主机指纹表，保证 TOFU 策略在两个通道上一致；
            // 二次验证登记表同理 —— 一个 SFTP 面板在等验证码时，
            // 命令层要能通过同一个表把答案送回去。
            let auth_prompts = sessions.auth_prompts();
            // 还要共享「已认证连接」表：文件面板在同一条连接上开 sftp 子系统，
            // 就不必重新认证（对 OTP / 堡垒机服务器等于省掉一次验证码）。
            let sftp = SftpManager::new(
                sessions.known_hosts(),
                auth_prompts.clone(),
                sessions.shared(),
            );
            // AI 旁路：启动时把 db 里的配置灌进内存里的原子开关，
            // 否则设施齐全但全局默认关闭，feed 会一路短路
            let agent = agent::AgentManager::new();
            agent.apply_config(&agent::load_config(&store));
            app.manage(viewer::DocStore::default());
            // P0 会话织物 + P1 常驻循环：屏幕帧状态、契约表、事件流、时钟统计、配置
            // 都独立于 AppState，避免给热路径上的 AppState 再添锁；命令层通过 State<T> 直接取。
            // 每份状态 clone 一份给常驻循环（都是 Arc），循环本身只读屏幕、不写终端。
            let steward_screen = agent::steward::ScreenStore::new();
            let steward_registry = agent::steward::StewardRegistry::new();
            let steward_events = agent::steward_loop::EventLog::new();
            let steward_bus = agent::steward_loop::StewardBus::default();
            let steward_runtime = agent::steward_loop::StewardRuntime::new();
            let steward_config = agent::steward_loop::StewardConfigStore::new();
            // P2：决策依据是 db 的内存镜像，启动时载入并写默认种子；
            // 升级卡裁决队列由命令层投递、循环统一执行，保证注入只有一个所有者。
            let steward_brain = agent::profile::StewardBrain::load(&store);
            let steward_resolutions = agent::steward_loop::StewardResolutions::new();
            // P3–P5：一个共享状态承载所有权 / 授权 / Checkpoint / Timebox / 预算 /
            // 撤销窗口 / Planner 开关 / 写锁 / LLM 并发闸 / 恢复队列。
            let steward_core = agent::steward_core::StewardCore::new();
            app.manage(steward_screen.clone());
            app.manage(steward_registry.clone());
            app.manage(steward_events.clone());
            app.manage(steward_bus.clone());
            app.manage(steward_runtime.clone());
            app.manage(steward_config.clone());
            app.manage(steward_brain.clone());
            app.manage(steward_resolutions.clone());
            app.manage(steward_core.clone());
            // P4：启动扫描未完成契约。契约本身只在内存里，靠 Checkpoint 重建；
            // 循环启动后会 drain 恢复队列、等首帧读屏对齐，hash 不符就降为「确认」档。
            agent::steward::restore_checkpoints(&store, &steward_registry, &steward_core);
            app.manage(AppState {
                sessions,
                sftp,
                store,
                local: LocalManager::new(),
                agent,
                llm: agent::llm::LlmRuntime::new(),
                act: agent::act::ActRuntime::new(),
                auth_prompts,
                keys_dir,
            });
            // 统一托管的启动扫描：未结束的托管一律挂起（AppRestart）待用户确认接驳，不写任何终端
            agent::host_cmd::boot_scan(app.handle());
            // P1：一个常驻 Supervisor 管所有被托管 console 的状态槽，
            // 面板关闭 / 窗口最小化都不影响它按心跳观察。全程只读、零 LLM。
            agent::steward_loop::spawn_daemon(
                app.handle().clone(),
                steward_screen,
                steward_registry,
                steward_events,
                steward_bus,
                steward_runtime,
                steward_config,
                steward_brain,
                steward_resolutions,
                steward_core,
            );
            tracing::info!("nexus-term 后端就绪");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            cmd::health,
            cmd::host_info,
            cmd::ssh_connect,
            cmd::ssh_test_connect,
            cmd::ssh_write,
            cmd::ssh_resize,
            cmd::ssh_close,
            cmd::ssh_auth_respond,
            cmd::ssh_probe,
            cmd::ssh_write_b64,
            cmd::local_write_b64,
            cmd::ssh_stats,
            cmd::local_stats,
            cmd::history_load_local,
            cmd::history_load_remote,
            cmd::session_extras,
            cmd::set_session_extras,
            cmd::ssh_set_sidecar,
            cmd::clipboard_read,
            cmd::clipboard_write,
            cmd::list_sessions,
            cmd::create_session,
            cmd::update_session,
            cmd::delete_session,
            cmd::list_macros,
            cmd::create_macro,
            cmd::update_macro,
            cmd::delete_macro,
            cmd::save_password,
            cmd::load_password,
            cmd::reorder_sessions,
            cmd::import_xsh,
            cmd::export_xsh,
            cmd::export_sessions_xsh,
            cmd::export_all_xsh,
            cmd::list_ssh_keys,
            cmd::verify_key,
            cmd::list_keys,
            cmd::import_key,
            cmd::generate_key,
            cmd::delete_key,
            cmd::export_key,
            cmd::update_key_meta,
            cmd::key_public,
            cmd::save_passphrase,
            cmd::load_passphrase,
            cmd::save_totp_seed,
            cmd::has_totp_seed,
            cmd::totp_preview,
            cmd::list_local_dir,
            cmd::sftp_connect,
            cmd::sftp_list,
            cmd::sftp_home,
            cmd::sftp_download,
            cmd::sftp_upload,
            cmd::sftp_cancel_transfer,
            cmd::sftp_mkdir,
            cmd::sftp_remove,
            cmd::sftp_rename,
            cmd::sftp_close,
            cmd::write_file,
            cmd::write_file_b64,
            cmd::append_file_b64,
            cmd::append_input_log,
            cmd::local_spawn,
            cmd::local_write,
            cmd::local_resize,
            cmd::local_close,
            cmd::vault_status,
            cmd::vault_is_remembered,
            cmd::vault_try_remembered,
            cmd::vault_enable,
            cmd::vault_unlock,
            cmd::vault_lock,
            cmd::vault_forget,
            cmd::vault_change_password,
            cmd::vault_disable,
            cmd::connections_export,
            cmd::connections_import,
            viewer::viewer_open_sftp,
            viewer::viewer_open_ssh,
            viewer::viewer_open_local,
            viewer::viewer_doc,
            agent::cmd::agent_get_config,
            agent::cmd::agent_set_config,
            agent::cmd::agent_set_session,
            agent::cmd::agent_get_session,
            agent::cmd::agent_context,
            agent::cmd::agent_clear,
            agent::cmd::agent_history_load,
            agent::cmd::agent_history_save,
            agent::cmd::agent_history_clear,
            agent::cmd::agent_conversation_reset,
            agent::cmd::agent_stats,
            agent::cmd::agent_user_command,
            agent::cmd::agent_timeline,
            agent::recorder_cmd::agent_record_start,
            agent::recorder_cmd::agent_record_stop,
            agent::recorder_cmd::agent_record_status,
            agent::recorder_cmd::agent_record_cancel,
            agent::recorder_cmd::agent_user_keys,
            agent::cmd::agent_ask,
            agent::cmd::agent_cancel,
            agent::cmd::agent_test,
            agent::cmd::agent_run,
            agent::cmd::agent_run_cancel,
            agent::cmd::agent_run_pause,
            agent::cmd::agent_run_resume,
            agent::cmd::agent_control_get,
            agent::cmd::agent_control_user_input,
            agent::cmd::agent_control_takeover,
            agent::cmd::agent_control_handback,
            agent::cmd::agent_approve,
            agent::cmd::agent_tab_ack,
            agent::cmd::agent_screen_ack,
            agent::cmd::agent_secret_ack,
            agent::cmd::agent_exec,
            agent::cmd::agent_export_session,
            agent::cmd::agent_open_session,
            agent::cmd::agent_open_window,
            agent::cmd::agent_close_window,
            agent::cmd::agent_window_open,
            agent::skill_cmd::skill_list,
            agent::skill_cmd::skill_get,
            agent::skill_cmd::skill_create,
            agent::skill_cmd::skill_update,
            agent::skill_cmd::skill_delete,
            agent::skill_cmd::skill_validate,
            agent::skill_cmd::skill_export,
            agent::skill_cmd::skill_import_preview,
            agent::skill_cmd::skill_import,
            agent::skill_cmd::skill_refine_draft,
            agent::skill_cmd::skill_run,
            agent::skill_cmd::skill_trust,
            agent::macro_migrate::macros_migrate_preview,
            agent::macro_migrate::macros_migrate,
            agent::hostmem_cmd::host_memory_list,
            agent::hostmem_cmd::host_memory_add,
            agent::hostmem_cmd::host_memory_update,
            agent::hostmem_cmd::host_memory_delete,
            agent::hostmem_cmd::host_memory_clear,
            agent::steward::steward_list_consoles,
            agent::steward::steward_screen_delta,
            agent::steward::steward_screen_get,
            agent::steward::steward_start,
            agent::host_cmd::host_start,
            agent::host_cmd::host_stop,
            agent::host_cmd::host_answer,
            agent::host_cmd::host_takeover,
            agent::host_cmd::host_handback,
            agent::host_cmd::host_pause,
            agent::host_cmd::host_resume,
            agent::host_cmd::host_status,
            agent::host_cmd::host_list,
            agent::host_cmd::host_confirm_criteria,
            agent::host_cmd::host_amend,
            agent::host_cmd::host_correct,
            agent::host_cmd::host_reattach,
            agent::host_cmd::host_reattach_candidates,
            agent::steward::steward_stop,
            agent::steward::steward_status,
            agent::steward::steward_amend,
            agent::steward_loop::steward_events,
            agent::steward_loop::steward_runtime_info,
            agent::steward_loop::steward_classify_now,
            agent::steward_loop::steward_clear_events,
            agent::steward_loop::steward_config,
            agent::steward_loop::steward_set_config,
            agent::steward_loop::steward_wake,
            agent::steward_cmd::steward_brain,
            agent::steward_cmd::steward_ctx_upsert,
            agent::steward_cmd::steward_ctx_delete,
            agent::steward_cmd::steward_intent_push,
            agent::steward_cmd::steward_decisions,
            agent::steward_cmd::steward_decide_now,
            agent::steward_cmd::steward_execution,
            agent::steward_cmd::steward_set_execution,
            agent::steward_cmd::steward_approve,
            agent::steward_cmd::steward_reject,
            agent::steward_cmd::steward_clear_decisions,
            agent::steward_cmd::steward_policy,
            // P3 授权与协作
            agent::steward_cmd::steward_core_view,
            agent::steward_cmd::steward_user_key,
            agent::steward_cmd::steward_takeover,
            agent::steward_cmd::steward_handback,
            agent::steward_cmd::steward_undo,
            agent::steward_cmd::steward_policy_v2,
            agent::steward_cmd::steward_set_authority,
            // P4 长运行与恢复
            agent::steward_cmd::steward_timebox,
            agent::steward_cmd::steward_set_timebox,
            agent::steward_cmd::steward_checkpoints,
            agent::steward_cmd::steward_resume,
            // P5 学习闭环 / 统一审计 / LLM 兜底
            agent::steward_cmd::steward_explain,
            agent::steward_cmd::steward_review,
            agent::steward_cmd::steward_audit,
            agent::steward_cmd::steward_distill,
            agent::steward_cmd::steward_planner,
            agent::steward_cmd::steward_set_planner,
            agent::steward_cmd::steward_plan_now,
        ])
        .run(tauri::generate_context!())
        .expect("启动 Tauri 应用失败");
}
