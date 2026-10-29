//! RFC 6238 TOTP：按连接配置里存的种子本地算动态口令，供二次验证自动填码。
//!
//! 没有引第三方 TOTP crate —— `hmac` / `sha1` / `sha2` 为了 PuTTY `.ppk` 已经是
//! 直接依赖，算法本身只有三十行，自己写反而能把「剩余窗口不足就等下一轮」
//! 这条真正要紧的时序逻辑攥在手里（见 [`TotpAutofill::next_code`]）。
//!
//! 只支持标准 TOTP。Duo / RSA SecurID 那类种子在厂商侧，拿不到，不在范围内。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};

/// 剩余窗口少于这么多秒就不发了 —— 算出来那一刻还有效，等 RTT 走完 + 服务端
/// 校验时可能已经跨进下一个时间步，白白失败一次，而失败一次就会让本次连接
/// 退回「只预填」。宁可多等三秒。
const MIN_REMAINING: u64 = 3;

/// `last_step` 的初始值：还没发过任何码。
const NO_STEP: u64 = u64::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    Sha1,
    Sha256,
    Sha512,
}

/// 一个种子解析出来的完整参数。
#[derive(Debug, Clone)]
pub struct TotpConfig {
    key: Vec<u8>,
    digits: u32,
    period: u64,
    algorithm: Algorithm,
}

impl TotpConfig {
    /// 按给定的 Unix 时间戳算码。单独拆出来是为了能对着 RFC 6238 的测试向量跑。
    pub fn code_at(&self, unix_secs: u64) -> String {
        hotp(
            self.algorithm,
            &self.key,
            unix_secs / self.period,
            self.digits,
        )
    }

    pub fn period(&self) -> u64 {
        self.period
    }
}

/// 解析用户填的种子。接受两种写法：
///
/// - 裸 base32（`JBSWY3DPEHPK3PXP`），允许带空格/连字符/小写/补等号 —— 手抄常见；
/// - `otpauth://totp/...?secret=...&digits=&period=&algorithm=` URI，扫码软件导出的格式。
///
/// 不接受 `otpauth-migration://`（Google Authenticator 的「导出账号」二维码）：
/// 那是 protobuf 打包的一批账号，要先拆包，不是一个种子。
pub fn parse_secret(input: &str) -> Result<TotpConfig, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("种子为空".into());
    }
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with("otpauth-migration://") {
        return Err(
            "这是 Google Authenticator 的「导出账号」二维码，里面打包了多个账号，\
             不是单个种子。请在导出前用「手动添加」查看单个账号的密钥（base32），再填到这里。"
                .into(),
        );
    }
    if lower.starts_with("otpauth://") {
        return parse_uri(raw);
    }
    Ok(TotpConfig {
        key: decode_base32(raw)?,
        digits: 6,
        period: 30,
        algorithm: Algorithm::Sha1,
    })
}

fn parse_uri(uri: &str) -> Result<TotpConfig, String> {
    if !uri.to_ascii_lowercase().starts_with("otpauth://totp/") {
        return Err("只支持 otpauth://totp/ 开头的链接（HOTP 不是按时间走的，不适用）".into());
    }
    let query = uri.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut secret = None;
    let mut digits = 6u32;
    let mut period = 30u64;
    let mut algorithm = Algorithm::Sha1;
    for pair in query.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        match k.to_ascii_lowercase().as_str() {
            "secret" => secret = Some(v.to_string()),
            "digits" => digits = v.parse().unwrap_or(6),
            "period" => period = v.parse().unwrap_or(30),
            "algorithm" => {
                algorithm = match v.to_ascii_uppercase().as_str() {
                    "SHA1" => Algorithm::Sha1,
                    "SHA256" => Algorithm::Sha256,
                    "SHA512" => Algorithm::Sha512,
                    other => return Err(format!("不支持的算法：{}", other)),
                }
            }
            _ => {}
        }
    }
    let secret = secret.ok_or("链接里没有 secret= 参数")?;
    Ok(TotpConfig {
        key: decode_base32(&secret)?,
        digits: clamp_digits(digits),
        period: clamp_period(period),
        algorithm,
    })
}

/// 6~8 位之外的都不合规；再说 `10^10` 会把 u32 撑爆。
fn clamp_digits(d: u32) -> u32 {
    d.clamp(6, 8)
}

fn clamp_period(p: u64) -> u64 {
    if p == 0 {
        30
    } else {
        p.clamp(5, 300)
    }
}

/// 宽松的 base32 解码：手抄的种子常带空格、连字符、小写，也常被补上或去掉等号。
fn decode_base32(s: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-' && *c != '=')
        .flat_map(|c| c.to_uppercase())
        .collect();
    if cleaned.is_empty() {
        return Err("种子为空".into());
    }
    data_encoding::BASE32_NOPAD
        .decode(cleaned.as_bytes())
        .map_err(|_| {
            "种子不是合法的 base32。base32 只含 A~Z 和 2~7，注意别把数字 0/1/8 \
             和字母 O/I/B 看混了"
                .to_string()
        })
        .and_then(|k| {
            if k.is_empty() {
                Err("种子为空".into())
            } else {
                Ok(k)
            }
        })
}

/// RFC 4226 的 HOTP：HMAC → 动态截断 → 取模。
fn hotp(algo: Algorithm, key: &[u8], counter: u64, digits: u32) -> String {
    macro_rules! digest {
        ($d:ty) => {{
            // HMAC 接受任意长度的 key，这里不可能失败。
            let mut m = Hmac::<$d>::new_from_slice(key).expect("HMAC 接受任意长度 key");
            m.update(&counter.to_be_bytes());
            m.finalize().into_bytes().to_vec()
        }};
    }
    let digest = match algo {
        Algorithm::Sha1 => digest!(sha1::Sha1),
        Algorithm::Sha256 => digest!(sha2::Sha256),
        Algorithm::Sha512 => digest!(sha2::Sha512),
    };

    // 动态截断：用最后一个字节的低 4 位当偏移，从那里取 4 字节、抹掉符号位。
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    let bin = u32::from_be_bytes([
        digest[offset] & 0x7f,
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]);
    let value = bin % 10u32.pow(digits);
    format!("{:0>width$}", value, width = digits as usize)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 一次连接期间的自动填码器。
///
/// 持有两份跨轮次的状态，都是**单次连接内**有效：
///
/// - `auto`：自动提交开关。只要自动提交过一次却被服务端拒了，就就地降级为
///   「只预填、交给用户按回车」——连错几次会锁账号，而真出问题时（种子抄错、
///   手机和服务器时间不同步）自动重试只会把次数烧得更快。下次新开标签又是
///   一条新连接，重新从自动开始。
/// - `last_step`：上一次发出去的时间步。TOTP 码一个时间步只能用一次，而
///   `open_authenticated` 在「同连接 KBI」失败后还会换一条新连接再走一遍 KBI
///   （见 `KBI_MUST_BE_FIRST`），两次很可能落在同一个 30 秒窗口里 —— 不挡住的话
///   第二次发的是个已经被服务端消费掉的码，必然失败。
pub struct TotpAutofill {
    cfg: TotpConfig,
    auto: AtomicBool,
    last_step: AtomicU64,
}

impl TotpAutofill {
    pub fn new(cfg: TotpConfig) -> Self {
        Self {
            cfg,
            auto: AtomicBool::new(true),
            last_step: AtomicU64::new(NO_STEP),
        }
    }

    /// 本次连接还允不允许自动提交。
    pub fn auto_submit(&self) -> bool {
        self.auto.load(Ordering::Relaxed)
    }

    /// 自动提交失败过一次 —— 本次连接剩下的问答一律只预填。
    pub fn disable_auto(&self) {
        self.auto.store(false, Ordering::Relaxed);
    }

    /// 在 `now` 这一刻算码前要不要先等，等几秒。`None` = 可以直接算。
    ///
    /// 和 [`next_code`](Self::next_code) 分开，纯粹是为了能拿合成的时间戳测
    /// 这段时序判断 —— 它依赖的是 `SystemTime`（真实挂钟），tokio 的虚拟时钟
    /// 推不动它，混在一起就只能靠 sleep 真等 30 秒来测。
    fn wait_secs(&self, now: u64) -> Option<u64> {
        let period = self.cfg.period;
        let remaining = period - (now % period);
        let used = self.last_step.load(Ordering::Relaxed) == now / period;
        if used || remaining < MIN_REMAINING {
            // +1 是为了稳稳落进下一个时间步，而不是卡在边界上。
            Some(remaining + 1)
        } else {
            None
        }
    }

    /// 算一个此刻可用的码。
    ///
    /// 必要时会**等到下一个时间步**再算：剩余窗口不足 [`MIN_REMAINING`] 秒，
    /// 或者这个时间步的码已经发过一次。最坏情况多等一个周期（默认 30 秒），
    /// 比发一个注定失败的码划算 —— 后者会让本次连接直接退回手输。
    pub async fn next_code(&self) -> String {
        let mut now = now_secs();
        if let Some(wait) = self.wait_secs(now) {
            tracing::debug!(wait, "等待下一个 TOTP 时间窗口再算码");
            tokio::time::sleep(Duration::from_secs(wait)).await;
            now = now_secs();
        }
        self.last_step.store(now / self.cfg.period, Ordering::Relaxed);
        self.cfg.code_at(now)
    }
}

/// 按连接配置取出种子，装成一个本次连接用的填码器。
///
/// 种子**只在后端流转**：这里从 db 读出解密后直接用，不经 IPC 发给前端。
/// 任何一环缺失（没绑配置、没存种子、换了机器解不开、种子存进去之后格式
/// 被手改坏了）都返回 `None`，走原来的「弹框让人手输」。
pub fn autofill_for(app: &tauri::AppHandle, config_id: Option<&str>) -> Option<TotpAutofill> {
    use tauri::Manager as _;

    let id = config_id?;
    let state = app.try_state::<crate::cmd::AppState>()?;
    let seed = state.store.get_totp_seed(id)?;
    match parse_secret(&seed) {
        Ok(cfg) => {
            tracing::debug!(config = id, "已加载 TOTP 种子，二次验证将自动填码");
            Some(TotpAutofill::new(cfg))
        }
        Err(e) => {
            // 存的时候解析过一遍，走到这里说明 db 被手工改过。
            tracing::warn!(config = id, error = %e, "TOTP 种子无法解析，本次按未配置处理");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 附录 B 的官方测试向量。三种算法各自的种子长度不同
    /// （SHA-1 用 20 字节，SHA-256 用 32，SHA-512 用 64），都是 ASCII 数字循环。
    fn vector_key(algo: Algorithm) -> Vec<u8> {
        let seed = b"12345678901234567890";
        let len = match algo {
            Algorithm::Sha1 => 20,
            Algorithm::Sha256 => 32,
            Algorithm::Sha512 => 64,
        };
        seed.iter().cycle().take(len).copied().collect()
    }

    fn cfg(algo: Algorithm) -> TotpConfig {
        TotpConfig {
            key: vector_key(algo),
            digits: 8,
            period: 30,
            algorithm: algo,
        }
    }

    #[test]
    fn rfc6238_sha1_vectors() {
        let c = cfg(Algorithm::Sha1);
        assert_eq!(c.code_at(59), "94287082");
        assert_eq!(c.code_at(1111111109), "07081804");
        assert_eq!(c.code_at(1111111111), "14050471");
        assert_eq!(c.code_at(1234567890), "89005924");
        assert_eq!(c.code_at(2000000000), "69279037");
        assert_eq!(c.code_at(20000000000), "65353130");
    }

    #[test]
    fn rfc6238_sha256_vectors() {
        let c = cfg(Algorithm::Sha256);
        assert_eq!(c.code_at(59), "46119246");
        assert_eq!(c.code_at(1111111109), "68084774");
        assert_eq!(c.code_at(2000000000), "90698825");
    }

    #[test]
    fn rfc6238_sha512_vectors() {
        let c = cfg(Algorithm::Sha512);
        assert_eq!(c.code_at(59), "90693936");
        assert_eq!(c.code_at(1111111109), "25091201");
        assert_eq!(c.code_at(2000000000), "38618901");
    }

    /// 默认 6 位：同一个种子同一时刻，8 位码的后 6 位就是 6 位码。
    #[test]
    fn six_digits_is_suffix_of_eight() {
        let mut c = cfg(Algorithm::Sha1);
        let eight = c.code_at(59);
        c.digits = 6;
        assert_eq!(c.code_at(59), eight[2..]);
    }

    #[test]
    fn parses_bare_base32_leniently() {
        // 小写、带空格、带连字符、补等号 —— 手抄进来的各种写法都该落到同一个 key。
        let canonical = parse_secret("JBSWY3DPEHPK3PXP").unwrap().key;
        for variant in [
            "jbswy3dpehpk3pxp",
            "JBSW Y3DP EHPK 3PXP",
            "JBSW-Y3DP-EHPK-3PXP",
            "  JBSWY3DPEHPK3PXP  ",
        ] {
            assert_eq!(parse_secret(variant).unwrap().key, canonical, "{variant}");
        }
    }

    #[test]
    fn rejects_non_base32() {
        // 0/1/8 不在 base32 字母表里，正是手抄最容易和 O/I/B 弄混的几个。
        assert!(parse_secret("JBSW01PX").is_err());
        assert!(parse_secret("").is_err());
        assert!(parse_secret("   ").is_err());
    }

    #[test]
    fn parses_otpauth_uri_with_params() {
        let c = parse_secret(
            "otpauth://totp/ACME:alice@example.com\
             ?secret=JBSWY3DPEHPK3PXP&issuer=ACME&algorithm=SHA256&digits=8&period=60",
        )
        .unwrap();
        assert_eq!(c.digits, 8);
        assert_eq!(c.period, 60);
        assert_eq!(c.algorithm, Algorithm::Sha256);
        assert_eq!(c.key, parse_secret("JBSWY3DPEHPK3PXP").unwrap().key);
    }

    #[test]
    fn otpauth_uri_defaults_match_bare_secret() {
        let uri = parse_secret("otpauth://totp/x?secret=JBSWY3DPEHPK3PXP").unwrap();
        assert_eq!(uri.digits, 6);
        assert_eq!(uri.period, 30);
        assert_eq!(uri.algorithm, Algorithm::Sha1);
    }

    /// Google Authenticator 的「导出账号」二维码是一批账号的 protobuf 包，
    /// 不是种子。必须给出能照着做的提示，而不是「base32 解析失败」。
    #[test]
    fn rejects_migration_uri_with_actionable_message() {
        let err = parse_secret("otpauth-migration://offline?data=Ch0KCkhlbGxv").unwrap_err();
        assert!(err.contains("导出账号"), "{err}");
        assert!(err.contains("手动添加"), "{err}");
    }

    #[test]
    fn rejects_hotp_uri() {
        assert!(parse_secret("otpauth://hotp/x?secret=JBSWY3DPEHPK3PXP&counter=1").is_err());
    }

    #[test]
    fn clamps_out_of_range_params() {
        let c = parse_secret("otpauth://totp/x?secret=JBSWY3DPEHPK3PXP&digits=10&period=0").unwrap();
        assert_eq!(c.digits, 8);
        assert_eq!(c.period, 30);
    }

    #[test]
    fn autofill_starts_in_auto_and_latches_off() {
        let a = TotpAutofill::new(parse_secret("JBSWY3DPEHPK3PXP").unwrap());
        assert!(a.auto_submit());
        a.disable_auto();
        assert!(!a.auto_submit());
        // 不可逆：本次连接内不会自己恢复
        a.disable_auto();
        assert!(!a.auto_submit());
    }

    /// 窗口还剩得够多、这一步也没用过 —— 直接算，不等。
    #[test]
    fn does_not_wait_mid_window() {
        let a = TotpAutofill::new(parse_secret("JBSWY3DPEHPK3PXP").unwrap());
        // 1000 % 30 == 10，还剩 20 秒
        assert_eq!(a.wait_secs(1000), None);
    }

    /// 剩余窗口不足 3 秒就等下一个 —— 否则算出来那一刻有效，走完 RTT 已经过期。
    #[test]
    fn waits_out_a_nearly_expired_window() {
        // 时间步 33 是 [990, 1020)
        let a = TotpAutofill::new(parse_secret("JBSWY3DPEHPK3PXP").unwrap());
        assert_eq!(a.wait_secs(1017), None); // 还剩 3 秒，正好够
        assert_eq!(a.wait_secs(1018), Some(3)); // 还剩 2 秒 → 等 3 秒跨进步 34
        assert_eq!(a.wait_secs(1019), Some(2)); // 还剩 1 秒
    }

    /// 同一个时间步绝不发第二次 —— 换连接重试 KBI 时会撞上这条：
    /// 两次 KBI 很可能落在同一个 30 秒窗口里，而码已经被服务端消费掉了。
    #[test]
    fn never_reuses_a_time_step() {
        let a = TotpAutofill::new(parse_secret("JBSWY3DPEHPK3PXP").unwrap());
        assert_eq!(a.wait_secs(1000), None);
        a.last_step.store(1000 / 30, Ordering::Relaxed);
        // 同一步：哪怕还剩 20 秒也必须等下一个窗口
        assert_eq!(a.wait_secs(1000), Some(21));
        // 下一步：恢复正常
        assert_eq!(a.wait_secs(1030), None);
    }
}
