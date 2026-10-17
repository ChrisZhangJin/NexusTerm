use std::path::PathBuf;

use tauri::Manager;

use cmd::AppState;
use local::LocalManager;
use sftp::SftpManager;
use ssh::session::SessionManager;
use store::Store;

pub mod errors;
mod cmd;
mod keys;
mod local;
mod sftp;
mod ssh;
mod store;
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
        .setup(|app| {
            let dir = app_data_dir(app.handle());
            let store = Store::open(dir).expect("SQLite 初始化失败");
            let sessions = SessionManager::new();
            // SFTP 复用同一份主机指纹表，保证 TOFU 策略在两个通道上一致
            let sftp = SftpManager::new(sessions.known_hosts());
            app.manage(AppState {
                sessions,
                sftp,
                store,
                local: LocalManager::new(),
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
            cmd::list_sessions,
            cmd::create_session,
            cmd::update_session,
            cmd::delete_session,
            cmd::save_password,
            cmd::load_password,
            cmd::import_xsh,
            cmd::export_xsh,
            cmd::export_all_xsh,
            cmd::list_ssh_keys,
            cmd::verify_key,
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
            cmd::local_spawn,
            cmd::local_write,
            cmd::local_resize,
            cmd::local_close,
        ])
        .run(tauri::generate_context!())
        .expect("启动 Tauri 应用失败");
}
