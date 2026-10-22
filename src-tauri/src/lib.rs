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
mod console;
mod keys;
mod local;
mod reader;
mod sftp;
mod ssh;
mod store;
mod sysstat;
mod xsh;

fn app_data_dir(app: &tauri::AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .expect("无法定位应用数据目录")
}

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        // 阅读窗口里 HTML iframe 的页面来源（带独立 CSP，默认禁脚本）
        .register_uri_scheme_protocol("nxdoc", reader::protocol)
        .setup(|app| {
            let dir = app_data_dir(app.handle());
            let keys_dir = dir.join(crate::keys::VAULT_DIR);
            let store = Store::open(dir).expect("SQLite 初始化失败");
            // 把旧版存在系统凭据服务里的密码迁移到 db 明文列
            store.migrate_legacy_secrets();
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
            app.manage(reader::DocStore::default());
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
            tracing::info!("nexus-term 后端就绪");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            cmd::health,
            cmd::host_info,
            cmd::ssh_connect,
            cmd::ssh_write,
            cmd::ssh_resize,
            cmd::ssh_close,
            cmd::ssh_auth_respond,
            cmd::ssh_probe,
            cmd::ssh_write_b64,
            cmd::local_write_b64,
            cmd::ssh_stats,
            cmd::local_stats,
            cmd::console_command_options,
            cmd::session_extras,
            cmd::set_session_extras,
            cmd::ssh_set_sidecar,
            cmd::clipboard_read,
            cmd::clipboard_write,
            cmd::list_sessions,
            cmd::create_session,
            cmd::update_session,
            cmd::delete_session,
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
            cmd::list_local_dir,
            cmd::sftp_connect,
            cmd::sftp_list,
            cmd::sftp_home,
            cmd::sftp_download,
            cmd::sftp_upload,
            cmd::sftp_mkdir,
            cmd::sftp_remove,
            cmd::sftp_rename,
            cmd::sftp_close,
            cmd::write_file,
            cmd::write_file_b64,
            cmd::append_file_b64,
            cmd::local_spawn,
            cmd::local_write,
            cmd::local_resize,
            cmd::local_close,
            reader::reader_open_sftp,
            reader::reader_open_ssh,
            reader::reader_open_local,
            reader::reader_doc,
            agent::cmd::agent_get_config,
            agent::cmd::agent_set_config,
            agent::cmd::agent_set_session,
            agent::cmd::agent_get_session,
            agent::cmd::agent_context,
            agent::cmd::agent_clear,
            agent::cmd::agent_stats,
            agent::cmd::agent_ask,
            agent::cmd::agent_cancel,
            agent::cmd::agent_test,
            agent::cmd::agent_run,
            agent::cmd::agent_run_cancel,
            agent::cmd::agent_approve,
            agent::cmd::agent_tab_ack,
            agent::cmd::agent_exec,
            agent::cmd::agent_export_session,
            agent::cmd::agent_open_session,
            agent::cmd::agent_open_window,
            agent::cmd::agent_close_window,
            agent::cmd::agent_window_open,
        ])
        .run(tauri::generate_context!())
        .expect("启动 Tauri 应用失败");
}
