use russh::client;
use russh::keys::PublicKey;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 主机公钥指纹缓存 —— 实现 Trust-on-First-Use。
/// 首次连接时记录指纹，之后若发生变化则拒绝连接（防中间人）。
#[derive(Debug, Default)]
pub struct KnownHosts {
    inner: Mutex<HashMap<String, String>>,
}

impl KnownHosts {
    pub fn new() -> Self {
        Self::default()
    }

    fn slot(host: &str, port: u16) -> String {
        format!("{}:{}", host, port)
    }

    /// 返回 Some(true) 表示指纹匹配或首次信任；Some(false) 表示指纹冲突。
    pub fn verify(&self, host: &str, port: u16, fingerprint: &str) -> bool {
        let key = Self::slot(host, port);
        let mut map = self.inner.lock().expect("known_hosts poisoned");
        match map.get(&key) {
            None => {
                tracing::info!(host = host, port = port, "首次连接，记录主机指纹");
                map.insert(key, fingerprint.to_string());
                true
            }
            Some(known) if known == fingerprint => true,
            Some(known) => {
                tracing::error!(
                    host = host,
                    "主机公钥指纹不匹配！已知 {}，实际 {}",
                    known,
                    fingerprint
                );
                false
            }
        }
    }

    pub fn fingerprint_of(key: &PublicKey) -> String {
        key.fingerprint(russh::keys::HashAlg::Sha256).to_string()
    }
}

/// russh 的客户端回调实现。
#[derive(Clone)]
pub struct ClientHandler {
    pub known_hosts: Arc<KnownHosts>,
    pub host: String,
    pub port: u16,
    /// 主机指纹冲突标记。russh 在 check_server_key 返回 false 时会直接断开连接，
    /// 因此需要这个旁路标记，才能在事后把失败原因精确地报给前端。
    pub host_key_rejected: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKey) -> Result<bool, Self::Error> {
        let fp = KnownHosts::fingerprint_of(key);
        let ok = self.known_hosts.verify(&self.host, self.port, &fp);
        if !ok {
            self.host_key_rejected.store(true, Ordering::SeqCst);
        }
        Ok(ok)
    }
}

pub mod session;

#[allow(unused_imports)]
pub use session::{SessionError, SessionManager};
