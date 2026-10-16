//! 网络错误 → 中文可读提示。
//!
//! SSH 的失败原因九成集中在 TCP 握手与认证两步，而 russh 只会把 `io::Error`
//! 原样冒泡成 `Operation timed out (os error 60)` 这种对终端用户毫无意义的话。
//! 这里统一做一次翻译：主文案给人类看，原始错误保留在尾部小字里便于排障。

use std::io;
use std::time::Duration;

/// TCP 连接默认超时（秒）。局域网主机通常 1 秒内响应，10 秒足够宽容。
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u32 = 10;

/// 超时上限，避免误填一个天文数字把 UI 永久卡在"连接中"。
pub const MAX_CONNECT_TIMEOUT_SECS: u32 = 600;

/// 主文案与原始错误之间的分隔符。前端据此把两者拆成大字与小字。
pub const RAW_SUFFIX: &str = "\n\n原始错误：";

pub const AUTH_FAILED: &str = "认证失败：用户名、密码或密钥不正确";

/// 把可选的用户配置收敛成一个安全时长。
pub fn resolve_timeout(secs: Option<u32>) -> Duration {
    let s = secs
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS)
        .min(MAX_CONNECT_TIMEOUT_SECS);
    Duration::from_secs(u64::from(s))
}

/// 组装「友好提示 + 原始错误」。detail 为空或与主文案重复时只返回主文案。
pub fn compose(message: &str, detail: &str) -> String {
    let message = message.trim();
    let detail = detail.trim();
    if detail.is_empty() || detail == message {
        return message.to_string();
    }
    format!("{}{}{}", message, RAW_SUFFIX, detail)
}

/// 超时专属文案。tokio 的 Elapsed 不带任何上下文，需要自己补 host/port。
pub fn timeout_message(host: &str, port: u16, timeout: Duration) -> String {
    format!(
        "连接超时：{}:{} 在 {} 秒内没有响应。请检查 IP 是否正确、主机是否开机、\
         是否在同一网络、防火墙是否放行 {} 端口",
        host,
        port,
        timeout.as_secs(),
        port
    )
}

/// russh 的连接错误 → 中文提示。
pub fn connect_error(host: &str, port: u16, err: &russh::Error) -> String {
    match err {
        russh::Error::IO(e) => io_error(host, port, e),
        other => compose(
            &format!("无法与 {}:{} 建立 SSH 连接", host, port),
            &other.to_string(),
        ),
    }
}

/// io::Error → 中文提示。
///
/// 先匹配 `raw_os_error()`（BSD/macOS 的 errno 稳定且信息量最大），
/// 再用 `ErrorKind` 兜底，保证 Linux / Windows 上也有合理文案。
pub fn io_error(host: &str, port: u16, err: &io::Error) -> String {
    let msg = err.to_string();
    let lower = msg.to_lowercase();

    // 主机名解析失败 —— 发生在 connect 之前，ErrorKind 通常是 Uncategorized
    let unresolved_host =
        err.kind() == io::ErrorKind::AddrNotAvailable && host.parse::<std::net::IpAddr>().is_err();

    if lower.contains("nodename")
        || lower.contains("name or service not known")
        || lower.contains("failed to lookup address")
        || lower.contains("no address associated")
        || unresolved_host
    {
        return compose(
            &format!("无法解析主机名 {}：请检查拼写，或改用 IP 地址", host),
            &msg,
        );
    }

    match (err.raw_os_error(), err.kind()) {
        // ETIMEDOUT
        (Some(60), _) | (_, io::ErrorKind::TimedOut) => compose(
            &format!(
                "连接超时：{}:{} 无响应。请检查 IP 是否正确、主机是否开机、\
                 是否在同一网络、防火墙是否放行 {} 端口",
                host, port, port
            ),
            &msg,
        ),
        // ECONNREFUSED
        (Some(61), _) | (_, io::ErrorKind::ConnectionRefused) => compose(
            &format!("连接被拒绝：{}:{} 端口未开放或服务未启动", host, port),
            &msg,
        ),
        // ENETUNREACH / EHOSTUNREACH
        (Some(51), _) | (Some(65), _) => compose(
            &format!("网络不可达：{} 不在可达网段内，请检查本机网络与路由", host),
            &msg,
        ),
        // EHOSTDOWN
        (Some(64), _) => compose(&format!("主机已关机：{} 无响应", host), &msg),
        // ECONNRESET
        (Some(54), _) | (_, io::ErrorKind::ConnectionReset) => compose(
            &format!("连接被重置：{}:{} 主动断开了握手", host, port),
            &msg,
        ),
        (_, io::ErrorKind::ConnectionAborted) => compose(
            &format!("连接被中止：{}:{} 未通过防火墙校验", host, port),
            &msg,
        ),
        (_, io::ErrorKind::PermissionDenied) => compose(
            &format!("本机拒绝发起连接：{}:{}，请检查系统防火墙或 VPN", host, port),
            &msg,
        ),
        _ => compose(
            &format!("无法连接到 {}:{}：{}", host, port, kind_label(err.kind())),
            &msg,
        ),
    }
}

fn kind_label(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::TimedOut => "连接超时",
        io::ErrorKind::ConnectionRefused => "端口未开放",
        io::ErrorKind::ConnectionReset => "连接被重置",
        io::ErrorKind::ConnectionAborted => "连接被中止",
        io::ErrorKind::NotConnected => "网络未连接",
        io::ErrorKind::AddrNotAvailable => "地址不可用",
        io::ErrorKind::AddrInUse => "地址被占用",
        io::ErrorKind::PermissionDenied => "权限不足",
        _ => "网络错误",
    }
}

/// 认证阶段统一文案。russh 的认证错误不区分"密码错"和"账号不存在"，
/// 出于安全考虑也不该区分，因此统一提示。
pub fn auth_error(detail: &str) -> String {
    let detail = detail.trim();
    if detail.is_empty() || detail == AUTH_FAILED {
        return AUTH_FAILED.to_string();
    }
    format!("{}{}{}", AUTH_FAILED, RAW_SUFFIX, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_drops_duplicate_detail() {
        assert_eq!(compose("网络不可达", "网络不可达"), "网络不可达");
        assert_eq!(compose("网络不可达", ""), "网络不可达");
        assert_eq!(
            compose("网络不可达", "os error 51"),
            "网络不可达\n\n原始错误：os error 51"
        );
    }

    #[test]
    fn timeout_default_is_ten_seconds() {
        assert_eq!(resolve_timeout(None).as_secs(), 10);
        assert_eq!(resolve_timeout(Some(0)).as_secs(), 10);
        assert_eq!(resolve_timeout(Some(9999)).as_secs(), 600);
    }

    #[test]
    fn errno_maps_to_expected_copy() {
        let timed_out = io::Error::from_raw_os_error(60);
        assert!(io_error("1.2.3.4", 22, &timed_out).starts_with("连接超时"));

        let refused = io::Error::from_raw_os_error(61);
        assert!(io_error("1.2.3.4", 22, &refused).starts_with("连接被拒绝"));

        let unreach = io::Error::from_raw_os_error(51);
        assert!(io_error("1.2.3.4", 22, &unreach).starts_with("网络不可达"));
    }
}
