//! PuTTY 私钥（`.ppk`）解析。
//!
//! PPK 是 PuTTY 自成一体的容器格式，和 OpenSSH 私钥没有任何共通之处：
//! 不是 PEM、字段布局不同、KDF 不同、连完整性校验的算法都换过两次。
//! `ssh-key` 只认 OpenSSH，所以这里按 PuTTY 的 `sshpubk.c` 独立实现一遍。
//!
//! 三个版本的差别：
//!
//! | | v1 | v2 | v3（PuTTY 0.75+） |
//! |---|---|---|---|
//! | KDF | SHA-1 计数派生 | SHA-1 计数派生 | Argon2id |
//! | CBC IV | 全零 | 全零 | Argon2 输出的中段 |
//! | 完整性 | HMAC-SHA1，仅覆盖私钥块 | HMAC-SHA1 | HMAC-SHA256 |
//!
//! 内层私钥布局同样是 PuTTY 私有约定，最典型的是 Ed25519 用小端定长存储
//! （OpenSSH 直接用 seed 原字节），而 RSA 的 `p`/`q` 顺序也和 OpenSSH 相反。
//!
//! 测试用的是 PuTTY 官方测试套件（`test/cryptsuite.py`）里的真实样本，
//! 含已知口令与 MAC，不是自己编码再自己解码的自证。

use aes::Aes256;
use argon2::{Algorithm as KdfFlavour, Argon2, Params as KdfParams, Version as KdfVersion};
use base64::Engine as _;
use cbc::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use ssh_encoding::Decode;
use ssh_key::private::{
    EcdsaKeypair, EcdsaPrivateKey, Ed25519Keypair, KeypairData, RsaKeypair, RsaPrivateKey,
};
use ssh_key::public::{EcdsaPublicKey, KeyData};
use ssh_key::{Mpint, PrivateKey, PublicKey};

/// MAC 密钥的派生标签，字面量来自 PuTTY 源码，不能改（改了所有加密私钥都验不过）
const MAC_KEY_LABEL: &[u8] = b"putty-private-key-file-mac-key";

/// 单个 blob 的行数上限。防止畸形文件里的 `Public-Lines: 999999999` 把内存吃光。
const MAX_BLOB_LINES: u32 = 4096;

/// 文件是不是 PPK —— 只看首行，不解密。
pub fn looks_like_ppk(text: &str) -> bool {
    // Windows 记事本保存的文件可能带 BOM，不剥掉首行就匹配不上
    text.trim_start_matches('\u{feff}')
        .trim_start()
        .starts_with("PuTTY-User-Key-File-")
}

/// 是否加密。只解析头部，不碰私钥内容。
///
/// 返回 `None` 表示连 `Encryption:` 行都找不到，不是合法 PPK。
pub fn is_encrypted(text: &str) -> Option<bool> {
    for line in text.lines() {
        if let Some(v) = line.trim_end().strip_prefix("Encryption:") {
            return Some(v.trim() != "none");
        }
    }
    None
}

/// 只取公钥。加密私钥也能拿到 —— PPK 的公钥段本来就是明文。
pub fn public_key_only(text: &str) -> Option<PublicKey> {
    let h = Headers::parse(text).ok()?;
    PublicKey::from_bytes(&h.public_blob).ok()
}

/// 取文件里记录的注释。
///
/// PPK 的注释在头部独立一行，不在公钥 blob 里 —— 而 OpenSSH 是把注释
/// 附在公钥末尾的，所以这两种格式的注释要分开读。
pub fn comment(text: &str) -> Option<String> {
    Headers::parse(text).ok().map(|h| h.comment)
}

/// 完整解析，返回可直接用于认证的私钥。
///
/// `passphrase` 对未加密的 PPK 会被忽略。
pub fn parse(text: &str, passphrase: Option<&str>) -> Result<PrivateKey, String> {
    let h = Headers::parse(text)?;
    let phrase = passphrase.unwrap_or("");
    let encrypted = h.encryption != "none";

    if encrypted && phrase.is_empty() {
        return Err("该私钥已加密，需要提供密码".into());
    }

    let (key, iv, mac_key) = derive_keys(&h, phrase.as_bytes())?;

    // 顺序很重要：PuTTY 是先解密、再拿明文算 MAC。
    // 所以「口令错」的判定依据是 MAC 不匹配，而不是解密本身失败
    // （CBC 对错误密钥不会报错，只会吐出垃圾字节）。
    let plain = if encrypted {
        decrypt(&key, &iv, &h.private_blob)?
    } else {
        h.private_blob.clone()
    };

    verify_mac(&h, &mac_key, &plain)?;
    build_private_key(&h, &plain)
}

// ——— 头部 ———

#[derive(Debug)]
struct Headers {
    version: u32,
    algorithm: String,
    encryption: String,
    comment: String,
    public_blob: Vec<u8>,
    /// 仍是密文（未加密时就是明文）
    private_blob: Vec<u8>,
    mac_hex: String,
    /// v1 的 `Private-Hash:` 是纯 SHA1，不是 HMAC
    plain_hash: bool,
    argon: Option<Argon>,
}

#[derive(Debug)]
struct Argon {
    flavour: String,
    memory: u32,
    passes: u32,
    parallelism: u32,
    salt: Vec<u8>,
}

impl Headers {
    fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim_start_matches('\u{feff}');
        let lines: Vec<&str> = text.lines().map(|l| l.trim_end_matches('\r')).collect();
        let first = lines.first().ok_or("PPK 文件为空")?;
        let (head, algo) = first
            .split_once(':')
            .ok_or("不是 PPK 文件：缺少 PuTTY-User-Key-File 头")?;
        let version: u32 = head
            .trim()
            .strip_prefix("PuTTY-User-Key-File-")
            .ok_or("不是 PPK 文件：缺少 PuTTY-User-Key-File 头")?
            .parse()
            .map_err(|_| format!("无法识别的 PPK 版本：{first}"))?;
        if !(1..=3).contains(&version) {
            return Err(format!("不支持的 PPK 版本：{version}"));
        }
        // 算法名要参与 MAC 计算，必须和文件里写的完全一致，不能归一化
        let algorithm = header_value(algo).to_string();
        if algorithm.is_empty() {
            return Err("PPK 头缺少算法名".into());
        }

        let mut encryption = String::new();
        let mut comment = String::new();
        let mut public_blob = Vec::new();
        let mut private_blob = Vec::new();
        let mut mac_hex = String::new();
        let mut plain_hash = false;

        // Argon2 参数：只在「v3 且加密」时出现，先逐字段攒着
        let mut kdf_flavour = String::new();
        let mut kdf_memory = 0u32;
        let mut kdf_passes = 0u32;
        let mut kdf_parallelism = 0u32;
        let mut kdf_salt = String::new();

        let mut i = 1;
        while i < lines.len() {
            let line = lines[i];
            if line.trim().is_empty() {
                i += 1;
                continue;
            }
            let (key, value) = line
                .split_once(':')
                .ok_or_else(|| format!("PPK 第 {} 行格式不对：{line}", i + 1))?;
            // 只吃掉冒号后的那一个空格，其余原样保留（见 header_value）
            let value = header_value(value);

            match key {
                "Encryption" => encryption = value.to_string(),
                "Comment" => comment = value.to_string(),
                "Public-Lines" => {
                    public_blob = read_blob(&lines, &mut i, parse_line_count(value)?)?;
                    continue;
                }
                "Private-Lines" => {
                    private_blob = read_blob(&lines, &mut i, parse_line_count(value)?)?;
                    continue;
                }
                "Private-MAC" => mac_hex = value.to_string(),
                "Private-Hash" => {
                    mac_hex = value.to_string();
                    plain_hash = true;
                }
                "Key-Derivation" => kdf_flavour = value.to_string(),
                "Argon2-Memory" => kdf_memory = parse_u32(value, "Argon2-Memory")?,
                "Argon2-Passes" => kdf_passes = parse_u32(value, "Argon2-Passes")?,
                "Argon2-Parallelism" => {
                    kdf_parallelism = parse_u32(value, "Argon2-Parallelism")?
                }
                "Argon2-Salt" => kdf_salt = value.to_string(),
                // 未知字段按规范应当忽略（PuTTY 自己也留了扩展余地）
                _ => {}
            }
            i += 1;
        }

        if public_blob.is_empty() {
            return Err("PPK 缺少公钥段".into());
        }
        if mac_hex.is_empty() {
            return Err("PPK 缺少完整性校验字段（Private-MAC）".into());
        }

        let argon = if kdf_flavour.is_empty() {
            None
        } else {
            Some(Argon {
                flavour: kdf_flavour,
                memory: kdf_memory,
                passes: kdf_passes,
                parallelism: kdf_parallelism,
                salt: hex_decode(&kdf_salt)?,
            })
        };

        Ok(Self {
            version,
            algorithm,
            encryption,
            comment,
            public_blob,
            private_blob,
            mac_hex,
            plain_hash,
            argon,
        })
    }
}

/// 读 `*-Lines` 后面紧跟的 n 行 base64。
///
/// `i` 进来时指向 `Public-Lines:` 那一行，出去时指向 blob **之后**的第一行
/// —— 调用方要接着 `continue`，否则会从半路重新解析。
fn read_blob(lines: &[&str], i: &mut usize, count: usize) -> Result<Vec<u8>, String> {
    let start = *i + 1;
    let end = start
        .checked_add(count)
        .filter(|e| *e <= lines.len())
        .ok_or("PPK 内容行数不足，文件可能已损坏或被截断")?;
    let joined: String = lines[start..end].concat();
    *i = end;
    base64::engine::general_purpose::STANDARD
        .decode(joined.trim())
        .map_err(|e| format!("PPK base64 解码失败：{e}"))
}

/// 取头部字段的值。
///
/// PuTTY 的 `read_header` 要求冒号后紧跟**一个**空格，并且只吃掉那一个；
/// 值自己的前导/尾随空格属于内容，会参与 MAC 计算 —— 所以这里不能 `trim()`，
/// 否则注释带尾随空格的文件会被误判成「密码不正确」。
fn header_value(v: &str) -> &str {
    v.strip_prefix(' ').unwrap_or(v)
}

fn parse_line_count(v: &str) -> Result<usize, String> {
    let n: u32 = v
        .trim()
        .parse()
        .map_err(|_| format!("PPK 行数字段不是整数：{v}"))?;
    if n == 0 || n > MAX_BLOB_LINES {
        return Err(format!("PPK 行数字段超出合理范围：{n}"));
    }
    Ok(n as usize)
}

fn parse_u32(v: &str, field: &str) -> Result<u32, String> {
    v.trim()
        .parse()
        .map_err(|_| format!("PPK 的 {field} 不是整数：{v}"))
}

// ——— 密钥派生 ———

fn derive_keys(h: &Headers, phrase: &[u8]) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), String> {
    let (key_len, iv_len) = match h.encryption.as_str() {
        "none" => (0, 0),
        "aes256-cbc" => (32, 16),
        other => return Err(format!("不支持的 PPK 加密算法：{other}")),
    };

    match h.version {
        1 | 2 => Ok(derive_legacy(phrase, key_len, iv_len)),
        3 => derive_v3(h, phrase, key_len, iv_len),
        v => Err(format!("不支持的 PPK 版本：{v}")),
    }
}

/// v1/v2：`SHA1(be32(ctr) || passphrase)` 拼够密钥长度。
///
/// 每轮都是独立的哈希（只有计数器变），不是链式的 —— 这点和
/// OpenSSL 的 EVP_BytesToKey 容易搞混。IV 恒为全零是本格式的定义。
fn derive_legacy(phrase: &[u8], key_len: usize, iv_len: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let mut storage = Vec::with_capacity(key_len.max(20) * 2);
    let mut ctr: u32 = 0;
    while storage.len() < key_len {
        let mut h = Sha1::new();
        h.update(ctr.to_be_bytes());
        h.update(phrase);
        storage.extend_from_slice(&h.finalize());
        ctr += 1;
    }
    storage.truncate(key_len);

    let iv = vec![0u8; iv_len];

    // MAC 密钥完全独立于上面的计数派生
    let mut h = Sha1::new();
    h.update(MAC_KEY_LABEL);
    h.update(phrase);
    let mac_key = h.finalize().to_vec();

    (storage, iv, mac_key)
}

/// v3：一次 Argon2 吐出 `key || iv || mac_key`。
///
/// 注意未加密时 MAC 密钥长度是 **0**（PuTTY 源码里 `mac_keylen = 0`），
/// 此时用的是空密钥的 HMAC，而不是跳过校验。
fn derive_v3(
    h: &Headers,
    phrase: &[u8],
    key_len: usize,
    iv_len: usize,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), String> {
    if key_len == 0 {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }

    let a = h
        .argon
        .as_ref()
        .ok_or("PPK v3 缺少 Argon2 派生参数（Key-Derivation）")?;
    let flavour = match a.flavour.as_str() {
        "Argon2id" => KdfFlavour::Argon2id,
        "Argon2i" => KdfFlavour::Argon2i,
        "Argon2d" => KdfFlavour::Argon2d,
        other => return Err(format!("不支持的 PPK 密钥派生算法：{other}")),
    };

    let mac_key_len = 32usize;
    let out_len = key_len + iv_len + mac_key_len;

    // PuTTY 的 Argon2-Memory 单位是 KiB，和 argon2 crate 的 m_cost 一致
    let params = KdfParams::new(a.memory, a.passes, a.parallelism, Some(out_len))
        .map_err(|e| format!("PPK 的 Argon2 参数不合法：{e}"))?;
    let alg = Argon2::new(flavour, KdfVersion::V0x13, params);

    let mut out = vec![0u8; out_len];
    alg.hash_password_into(phrase, &a.salt, &mut out)
        .map_err(|e| format!("PPK 密钥派生失败：{e}"))?;

    let key = out[..key_len].to_vec();
    let iv = out[key_len..key_len + iv_len].to_vec();
    let mac_key = out[key_len + iv_len..].to_vec();
    Ok((key, iv, mac_key))
}

// ——— 解密与校验 ———

fn decrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() || data.len() % 16 != 0 {
        return Err("PPK 密文长度不是 AES 分组长度的整数倍".into());
    }
    let mut buf = data.to_vec();
    let plain = cbc::Decryptor::<Aes256>::new_from_slices(key, iv)
        .map_err(|_| "PPK 解密参数错误".to_string())?
        // 私钥块尾部自带零填充，不按 PKCS#7 剥离
        .decrypt_padded_mut::<NoPadding>(&mut buf)
        .map_err(|_| "PPK 解密失败".to_string())?;
    Ok(plain.to_vec())
}

fn verify_mac(h: &Headers, mac_key: &[u8], plain: &[u8]) -> Result<(), String> {
    let mut data = Vec::new();
    if h.version == 1 {
        // v1 的校验只覆盖私钥块，连长度前缀都没有
        data.extend_from_slice(plain);
    } else {
        push_string(&mut data, h.algorithm.as_bytes());
        push_string(&mut data, h.encryption.as_bytes());
        push_string(&mut data, h.comment.as_bytes());
        push_string(&mut data, &h.public_blob);
        push_string(&mut data, plain);
    }

    let actual = if h.plain_hash {
        let mut s = Sha1::new();
        s.update(&data);
        s.finalize().to_vec()
    } else {
        match h.version {
            1 | 2 => hmac_sha1(mac_key, &data)?,
            _ => hmac_sha256(mac_key, &data)?,
        }
    };

    // 校验值本身对空白不敏感，宽容一点；但 comment 的空白已参与上面的 MAC 计算
    let expected = hex_decode(h.mac_hex.trim())?;
    if actual != expected {
        // CBC 用错密钥不会报错，只是解出垃圾；所以口令错只能在这里被发现
        return Err(if h.encryption != "none" {
            "私钥密码不正确".into()
        } else {
            "PPK 文件校验失败，文件可能已损坏".into()
        });
    }
    Ok(())
}

fn hmac_sha1(key: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    let mut m = <Hmac<Sha1> as Mac>::new_from_slice(key)
        .map_err(|_| "HMAC 初始化失败".to_string())?;
    m.update(data);
    Ok(m.finalize().into_bytes().to_vec())
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key)
        .map_err(|_| "HMAC 初始化失败".to_string())?;
    m.update(data);
    Ok(m.finalize().into_bytes().to_vec())
}

// ——— 内层私钥 → ssh-key 类型 ———

fn build_private_key(h: &Headers, plain: &[u8]) -> Result<PrivateKey, String> {
    let public = PublicKey::from_bytes(&h.public_blob)
        .map_err(|e| format!("PPK 里的公钥无法解析：{e}"))?;
    let mut r = Reader::new(plain);

    let keypair = match public.key_data() {
        // PPK: d, p, q, iqmp —— 注意和 OpenSSH 的 n,e,d,iqmp,p,q 顺序不同
        KeyData::Rsa(k) => {
            let d = r.mpint()?;
            let p = r.mpint()?;
            let q = r.mpint()?;
            let iqmp = r.mpint()?;
            KeypairData::Rsa(RsaKeypair {
                public: k.clone(),
                private: RsaPrivateKey {
                    d: to_mpint(&d)?,
                    iqmp: to_mpint(&iqmp)?,
                    p: to_mpint(&p)?,
                    q: to_mpint(&q)?,
                },
            })
        }
        KeyData::Ed25519(k) => {
            // PuTTY 的 `put_mp_le_fixedlen` 把 seed 当小端整数再写回小端 32 字节，
            // 两个方向互逆，所以落盘字节序和 OpenSSH 的 seed 完全一致，不能再反转。
            let seed_bytes = r.le_fixed(32)?;
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&seed_bytes);
            let kp = Ed25519Keypair::from_seed(&seed);
            // 字节序一旦搞错公钥必然对不上，这里拦住比事后连不上服务器好排查
            if &kp.public != k {
                return Err("PPK 里的 Ed25519 私钥与公钥不匹配".into());
            }
            KeypairData::Ed25519(kp)
        }
        KeyData::Ecdsa(k) => {
            let raw = r.mpint_raw()?;
            let mut src: &[u8] = &raw;
            let pair = match k {
                EcdsaPublicKey::NistP256(q) => EcdsaKeypair::NistP256 {
                    public: *q,
                    private: EcdsaPrivateKey::<32>::decode(&mut src).map_err(ecdsa_err)?,
                },
                EcdsaPublicKey::NistP384(q) => EcdsaKeypair::NistP384 {
                    public: *q,
                    private: EcdsaPrivateKey::<48>::decode(&mut src).map_err(ecdsa_err)?,
                },
                EcdsaPublicKey::NistP521(q) => EcdsaKeypair::NistP521 {
                    public: *q,
                    private: EcdsaPrivateKey::<66>::decode(&mut src).map_err(ecdsa_err)?,
                },
            };
            KeypairData::Ecdsa(pair)
        }
        other => {
            return Err(format!(
                "暂不支持 {} 类型的 PPK 私钥（DSA 已被 OpenSSH 移除，FIDO 密钥无法从文件复制）",
                other.algorithm().as_str()
            ))
        }
    };

    PrivateKey::new(keypair, h.comment.clone()).map_err(|e| format!("构造私钥失败：{e}"))
}

fn ecdsa_err(e: ssh_key::Error) -> String {
    format!("PPK 里的 ECDSA 私钥无法解析：{e}")
}

fn to_mpint(bytes: &[u8]) -> Result<Mpint, String> {
    // 空字节序列在 SSH 里表示 0，但作为 RSA 参数没有意义
    if bytes.is_empty() {
        return Err("PPK 里的 RSA 参数为空".into());
    }
    // 必须用 `from_positive_bytes`：`from_bytes` 会把最高位为 1 的输入当负数解读，
    // 而 RSA 的 d/p/q 最高位几乎必然是 1，用错会让参数变负值，签名一路失败。
    Mpint::from_positive_bytes(bytes).map_err(|e| format!("PPK 里的整数不合法：{e}"))
}

// ——— 字节读取 ———

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or("PPK 私钥块长度溢出")?;
        if end > self.buf.len() {
            return Err("PPK 私钥块不完整，可能已损坏".into());
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// 读 SSH mpint，返回去掉前导零的正整数（大端）字节。
    fn mpint(&mut self) -> Result<Vec<u8>, String> {
        let len = self.u32()? as usize;
        if len > self.buf.len() {
            return Err("PPK 私钥块里的整数长度异常".into());
        }
        let raw = self.take(len)?;
        let start = raw.iter().position(|&b| b != 0).unwrap_or(raw.len());
        Ok(raw[start..].to_vec())
    }

    /// 读完整 mpint 编码（含长度前缀），交给 ssh-key 自己的 Decode 用。
    fn mpint_raw(&mut self) -> Result<Vec<u8>, String> {
        let start = self.pos;
        let len = self.u32()? as usize;
        if len > self.buf.len() {
            return Err("PPK 私钥块里的整数长度异常".into());
        }
        self.take(len)?;
        Ok(self.buf[start..self.pos].to_vec())
    }

    /// PuTTY 的 `put_mp_le_fixedlen`：小端、定长，前面带 uint32 长度。
    fn le_fixed(&mut self, want: usize) -> Result<Vec<u8>, String> {
        let len = self.u32()? as usize;
        if len != want {
            return Err(format!(
                "PPK 私钥字段长度异常：期望 {want} 字节，实际 {len} 字节"
            ));
        }
        Ok(self.take(len)?.to_vec())
    }
}

fn push_string(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err("PPK 的校验值不是合法的十六进制".into());
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = hex_val(pair[0])?;
        let lo = hex_val(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_val(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err("PPK 的校验值不是合法的十六进制".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // —— 以下样本全部来自 PuTTY 官方测试套件 test/cryptsuite.py ——
    // 口令统一是 "test-passphrase"，公钥为已知值，可用于交叉校验。

    const PUB_BLOB_HEX: &str = "0000000b7373682d65643235353139000000207242b33387688f57ff218bb639\
f6d9fd213ba54f3100d5b5cb64ca6e85247d56";

    const V3_CLEAR: &str = "\
PuTTY-User-Key-File-3: ssh-ed25519
Encryption: none
Comment: ed25519-key-20200105
Public-Lines: 2
AAAAC3NzaC1lZDI1NTE5AAAAIHJCszOHaI9X/yGLtjn22f0hO6VPMQDVtctkym6F
JH1W
Private-Lines: 1
AAAAIGvvIpl8jyqn8Xufkw6v3FnEGtXF3KWw55AP3/AGEBpY
Private-MAC: 816c84093fc4877e8411b8e5139c5ce35d8387a2630ff087214911d67417a54d
";

    const V3_ENCRYPTED: &str = "\
PuTTY-User-Key-File-3: ssh-ed25519
Encryption: aes256-cbc
Comment: ed25519-key-20200105
Public-Lines: 2
AAAAC3NzaC1lZDI1NTE5AAAAIHJCszOHaI9X/yGLtjn22f0hO6VPMQDVtctkym6F
JH1W
Key-Derivation: Argon2id
Argon2-Memory: 8192
Argon2-Passes: 13
Argon2-Parallelism: 1
Argon2-Salt: 37c3911bfefc8c1d11ec579627d2b3d9
Private-Lines: 1
amviz4sVUBN64jLO3gt4HGXJosUArghc4Soi7aVVLb2Tir5Baj0OQClorycuaPRd
Private-MAC: 6f5e588e475e55434106ec2c3569695b03f423228b44993a9e97d52ffe7be5a8
";

    const V2_CLEAR: &str = "\
PuTTY-User-Key-File-2: ssh-ed25519
Encryption: none
Comment: ed25519-key-20200105
Public-Lines: 2
AAAAC3NzaC1lZDI1NTE5AAAAIHJCszOHaI9X/yGLtjn22f0hO6VPMQDVtctkym6F
JH1W
Private-Lines: 1
AAAAIGvvIpl8jyqn8Xufkw6v3FnEGtXF3KWw55AP3/AGEBpY
Private-MAC: 2a629acfcfbe28488a1ba9b6948c36406bc28422
";

    const V2_ENCRYPTED: &str = "\
PuTTY-User-Key-File-2: ssh-ed25519
Encryption: aes256-cbc
Comment: ed25519-key-20200105
Public-Lines: 2
AAAAC3NzaC1lZDI1NTE5AAAAIHJCszOHaI9X/yGLtjn22f0hO6VPMQDVtctkym6F
JH1W
Private-Lines: 1
4/jKlTgC652oa9HLVGrMjHZw7tj0sKRuZaJPOuLhGTvb25Jzpcqpbi+Uf+y+uo+Z
Private-MAC: 5b1f6f4cc43eb0060d2c3e181bc0129343adba2b
";

    fn public_hex(pk: &PrivateKey) -> String {
        // to_bytes 是 SSH wire 格式、不含注释，正好对上样本里的 public blob
        let bytes = pk.public_key().to_bytes().expect("公钥可序列化");
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn detects_ppk_and_encryption() {
        assert!(looks_like_ppk(V3_CLEAR));
        assert!(looks_like_ppk(V2_ENCRYPTED));
        assert!(!looks_like_ppk("-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert_eq!(is_encrypted(V3_CLEAR), Some(false));
        assert_eq!(is_encrypted(V3_ENCRYPTED), Some(true));
        assert_eq!(is_encrypted(V2_ENCRYPTED), Some(true));
        assert_eq!(is_encrypted("nope"), None);
    }

    /// v3 明文：靠 HMAC-SHA256 校验，解出的公钥必须等于官方给的值
    #[test]
    fn parses_v3_plain() {
        let key = parse(V3_CLEAR, None).expect("v3 明文应当解析成功");
        assert_eq!(public_hex(&key), PUB_BLOB_HEX);
        assert_eq!(key.comment(), "ed25519-key-20200105");
    }

    /// v3 加密：Argon2id 派生 + AES-256-CBC，口令 "test-passphrase"
    #[test]
    fn parses_v3_encrypted_with_argon2() {
        let key = parse(V3_ENCRYPTED, Some("test-passphrase")).expect("v3 加密应当解开");
        assert_eq!(public_hex(&key), PUB_BLOB_HEX);
    }

    /// v2 明文：MAC 是 HMAC-SHA1，值本身就不同，能过就说明版本分支正确
    #[test]
    fn parses_v2_plain() {
        let key = parse(V2_CLEAR, None).expect("v2 明文应当解析成功");
        assert_eq!(public_hex(&key), PUB_BLOB_HEX);
    }

    /// v2 加密：SHA-1 计数派生 + 全零 IV，和 v3 完全是两条路径
    #[test]
    fn parses_v2_encrypted_with_sha1_kdf() {
        let key = parse(V2_ENCRYPTED, Some("test-passphrase")).expect("v2 加密应当解开");
        assert_eq!(public_hex(&key), PUB_BLOB_HEX);
    }

    #[test]
    fn rejects_wrong_passphrase() {
        let err = parse(V3_ENCRYPTED, Some("wrong")).unwrap_err();
        assert_eq!(err, "私钥密码不正确");
        let err = parse(V2_ENCRYPTED, Some("wrong")).unwrap_err();
        assert_eq!(err, "私钥密码不正确");
    }

    #[test]
    fn asks_for_passphrase_when_missing() {
        // 没有口令时不该报「密码不正确」，而应明确要求提供
        let err = parse(V3_ENCRYPTED, None).unwrap_err();
        assert!(err.contains("需要提供密码"), "实际：{err}");
    }

    #[test]
    fn rejects_tampered_ciphertext() {
        // 改一个 base64 字符：口令对，但密文被改 → MAC 必须报错
        let broken = V3_ENCRYPTED.replace("amviz4sVUBN64", "bmviz4sVUBN64");
        let err = parse(&broken, Some("test-passphrase")).unwrap_err();
        assert_eq!(err, "私钥密码不正确");
    }

    #[test]
    fn rejects_truncated_file() {
        let truncated: String = V3_CLEAR.lines().take(4).collect::<Vec<_>>().join("\n");
        assert!(parse(&truncated, None).is_err());
    }

    #[test]
    fn public_key_available_without_passphrase() {
        // 加密私钥也能只读公钥 —— 密钥列表要靠它显示类型和指纹
        let pk = public_key_only(V3_ENCRYPTED).expect("公钥应当可读");
        assert_eq!(
            pk.fingerprint(ssh_key::HashAlg::Sha256).to_string().len() > 10,
            true
        );
    }

    #[test]
    fn ed25519_seed_round_trips_to_same_public() {
        // 小端转大端最容易错：重新导出 OpenSSH 再解析，公钥必须一致，否则说明 seed 被反转坏了
        let key = parse(V3_CLEAR, None).unwrap();
        let openssh = key.to_openssh(ssh_key::LineEnding::LF).unwrap();
        let again = PrivateKey::from_openssh(openssh.as_bytes()).unwrap();
        assert_eq!(public_hex(&key), public_hex(&again));
    }

    // ——— 以下用 fixtures/ 里的真实样本，覆盖 RSA 与 ECDSA ———
    //
    // 官方测试套件只有 ed25519 的 PPK，所以这两种算法用
    // `tests/make_ppk.py` 按 PuTTY 的字段布局反向生成，并保留同一个密钥的
    // OpenSSH 版本作为对照。

    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("读取 {} 失败：{e}", path.display()))
    }

    /// 同一个密钥的两种格式，解析出来的密钥材料必须完全一致。
    ///
    /// 这是判断字段映射最直接的依据：RSA 在 PPK 里是 `d,p,q,iqmp`、
    /// 在 OpenSSH 里是 `n,e,d,iqmp,p,q`；Ed25519 一个是小端定长、
    /// 一个是 seed 原字节 —— 差一位这里就会红。
    ///
    /// 不用 `PrivateKey::sign` 作判据：ssh-key 0.6.7 的
    /// `TryFrom<&RsaKeypair> for rsa::RsaPrivateKey` 有 bug（把 `p` 传了
    /// 两次、`q` 完全没用），任何 RSA 私钥签名都会返回 `Crypto`。
    /// 认证路径不受影响 —— russh 自己实现了一份正确的转换。
    fn assert_same_as_openssh(ppk_name: &str, openssh_name: &str) -> PrivateKey {
        let source = PrivateKey::from_openssh(fixture(openssh_name).as_bytes())
            .unwrap_or_else(|e| panic!("{openssh_name} 解析失败：{e}"));
        let parsed = parse(&fixture(ppk_name), None)
            .unwrap_or_else(|e| panic!("{ppk_name} 解析失败：{e}"));
        assert_eq!(
            source.key_data(),
            parsed.key_data(),
            "{ppk_name} 解出的密钥材料应当与 {openssh_name} 完全一致"
        );
        parsed
    }

    #[test]
    fn rsa_matches_openssh_source() {
        let key = assert_same_as_openssh("rsa.ppk", "rsa_openssh");
        assert_eq!(key.algorithm().as_str(), "ssh-rsa");
    }

    #[test]
    fn ecdsa_matches_openssh_source() {
        assert_same_as_openssh("ecdsa.ppk", "ecdsa_openssh");
    }

    #[test]
    fn ed25519_matches_openssh_source() {
        assert_same_as_openssh("ed.ppk", "ed_openssh");
    }

    #[test]
    fn parsed_public_matches_pub_file() {
        // 公钥还能独立算出来，说明 public blob 与私钥参数自洽
        for (ppk_name, pub_name) in [
            ("rsa.ppk", "rsa.pub"),
            ("ecdsa.ppk", "ecdsa.pub"),
            ("ed.ppk", "ed.pub"),
        ] {
            let key = parse(&fixture(ppk_name), None).expect("应当解析成功");
            let expected = PublicKey::from_openssh(fixture(pub_name).trim()).expect("公钥样本");
            assert_eq!(
                key.public_key().key_data(),
                expected.key_data(),
                "{ppk_name} 的公钥应当与 {pub_name} 一致"
            );
        }
    }

    #[test]
    fn encrypted_fixtures_round_trip() {
        for (enc, openssh) in [
            ("rsa_enc.ppk", "rsa_openssh"),
            ("ecdsa_enc.ppk", "ecdsa_openssh"),
            ("ed_enc.ppk", "ed_openssh"),
        ] {
            let text = fixture(enc);
            assert_eq!(is_encrypted(&text), Some(true), "{enc} 应当被识别为加密");
            assert!(
                parse(&text, None).unwrap_err().contains("需要提供密码"),
                "{enc} 缺口令时应当明确要求输入"
            );
            assert_eq!(
                parse(&text, Some("wrong")).unwrap_err(),
                "私钥密码不正确",
                "{enc} 错口令应当被 MAC 拦住"
            );

            let key = parse(&text, Some("test-passphrase"))
                .unwrap_or_else(|e| panic!("{enc} 正确口令应当解开：{e}"));
            let source = PrivateKey::from_openssh(fixture(openssh).as_bytes()).unwrap();
            assert_eq!(
                source.key_data(),
                key.key_data(),
                "{enc} 解密结果应当与源私钥一致"
            );
        }
    }

    #[test]
    fn comment_comes_from_header() {
        // PPK 的注释在头部独立一行，不在公钥里 —— 列表要显示它
        assert_eq!(comment(&fixture("rsa.ppk")).as_deref(), Some("nexus-ppk-test"));
    }

    #[test]
    fn tolerates_bom_and_crlf() {
        // 记事本另存为会加 BOM；从 Windows 传到 Linux 再传回来的文件常是 CRLF
        let text = format!("\u{feff}{}", V2_CLEAR.replace('\n', "\r\n"));
        assert!(looks_like_ppk(&text));
        let key = parse(&text, None).expect("带 BOM 与 CRLF 应当能解析");
        assert_eq!(public_hex(&key), PUB_BLOB_HEX);
    }
}

