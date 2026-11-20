//! 「托管需要我时」提醒的落地：系统通知、任务栏闪烁、可选的 webhook（手机推送桥）。
//!
//! 说什么由纯函数 `host::attention::attention_for` 决定；这里只管发，且**绝不阻塞托管循环**：
//! 系统通知是同步的一次调用，webhook 丢到后台任务里、5 秒超时、失败只记日志。
//! 配置单独存在 settings 表的 `host_notify` 键里（不进 AgentConfig：它不涉及保险库，
//! 改它也不该要求保险库解锁）。

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State, UserAttentionType};
use tauri_plugin_notification::NotificationExt;

use crate::cmd::AppState;
use crate::store::Store;

use super::host::attention::Attention;
use super::steward::now_ms;

const SETTING_KEY: &str = "host_notify";
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// 托管提醒设置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HostNotifyConfig {
    /// 系统通知（窗口在前台时不弹）
    pub system: bool,
    /// 窗口不在前台时让任务栏 / Dock 图标闪一下
    pub taskbar: bool,
    /// 空 = 不推；否则 POST JSON `{title, body, host, state, ts}`
    pub webhook: String,
}

impl Default for HostNotifyConfig {
    fn default() -> Self {
        Self { system: true, taskbar: true, webhook: String::new() }
    }
}

pub fn load(store: &Store) -> HostNotifyConfig {
    store
        .get_setting(SETTING_KEY)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 只接受 http(s) 地址；空串 = 关掉 webhook。
pub fn normalize(mut c: HostNotifyConfig) -> Result<HostNotifyConfig, String> {
    c.webhook = c.webhook.trim().to_string();
    if !c.webhook.is_empty() && !(c.webhook.starts_with("https://") || c.webhook.starts_with("http://")) {
        return Err("Webhook 地址要以 http:// 或 https:// 开头".to_string());
    }
    Ok(c)
}

#[tauri::command]
pub fn host_notify_get(state: State<'_, AppState>) -> HostNotifyConfig {
    load(&state.store)
}

#[tauri::command]
pub fn host_notify_set(config: HostNotifyConfig, state: State<'_, AppState>) -> Result<(), String> {
    let c = normalize(config)?;
    let json = serde_json::to_string(&c).map_err(|e| e.to_string())?;
    state.store.set_setting(SETTING_KEY, &json)
}

/// webhook 的载荷。只有一行原因，不带终端内容。
pub(crate) fn webhook_payload(a: &Attention, host: &str, ts: i64) -> serde_json::Value {
    serde_json::json!({
        "title": a.title,
        "body": a.body,
        "host": host,
        "state": a.kind,
        "ts": ts,
    })
}

/// 有窗口在前台（主窗口或浮动的 Agent 窗口）＝ 人就在电脑前看着应用。
fn app_focused(app: &AppHandle) -> bool {
    app.webview_windows().values().any(|w| w.is_focused().unwrap_or(false))
}

/// 按设置发出一条提醒。窗口在前台时不弹系统通知、也不闪任务栏（标签上的徽标照常显示）；
/// webhook 是给不在电脑前的人用的，照发。
pub fn dispatch(app: &AppHandle, a: &Attention, host: &str) {
    let Some(st) = app.try_state::<AppState>() else { return };
    let cfg = load(&st.store);
    let focused = app_focused(app);
    if !focused {
        if cfg.system {
            if let Err(e) = app.notification().builder().title(&a.title).body(&a.body).show() {
                tracing::warn!("托管提醒：系统通知发送失败：{e}");
            }
        }
        if cfg.taskbar {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.request_user_attention(Some(UserAttentionType::Informational));
            }
        }
    }
    if !cfg.webhook.is_empty() {
        let url = cfg.webhook.clone();
        let payload = webhook_payload(a, host, now_ms());
        tauri::async_runtime::spawn(async move {
            let client = match super::llm::http_client() {
                Ok(c) => c,
                Err(e) => return tracing::warn!("托管提醒：webhook 客户端不可用：{e}"),
            };
            match client.post(&url).timeout(WEBHOOK_TIMEOUT).json(&payload).send().await {
                Ok(r) if !r.status().is_success() => tracing::warn!("托管提醒：webhook 返回 {}", r.status()),
                Ok(_) => {}
                // 错误里可能带完整 URL（webhook 的 key 常在 query 里），不原样记
                Err(e) => tracing::warn!("托管提醒：webhook 发送失败（超时：{}）", e.is_timeout()),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_on_and_tolerates_missing_fields() {
        let d = HostNotifyConfig::default();
        assert!(d.system && d.taskbar && d.webhook.is_empty());
        let c: HostNotifyConfig = serde_json::from_str(r#"{"system":false}"#).unwrap();
        assert_eq!(c, HostNotifyConfig { system: false, ..Default::default() });
    }

    #[test]
    fn webhook_url_must_be_http() {
        let c = |w: &str| HostNotifyConfig { webhook: w.into(), ..Default::default() };
        assert_eq!(normalize(c("  https://x/hook ")).unwrap().webhook, "https://x/hook");
        assert!(normalize(c("")).is_ok());
        assert!(normalize(c("file:///etc/passwd")).is_err());
    }

    #[test]
    fn payload_has_only_the_one_line() {
        let a = Attention { title: "NexusTerm 托管 · web".into(), body: "等你回答：q".into(), kind: "waiting" };
        let v = webhook_payload(&a, "web", 7);
        assert_eq!(v["title"], "NexusTerm 托管 · web");
        assert_eq!(v["body"], "等你回答：q");
        assert_eq!(v["host"], "web");
        assert_eq!(v["state"], "waiting");
        assert_eq!(v["ts"], 7);
        assert_eq!(v.as_object().unwrap().len(), 5);
    }
}
