"""把 OpenSSH 私钥转成 PuTTY .ppk，用于生成测试样本。

只服务于测试：NexusTerm 的 PPK **解析器**在 Rust 里，这个脚本负责反向
（编码），这样才有真实样本可以喂给它。

字段布局严格照 PuTTY 的 `sshpubk.c`：

    SSH-RSA        mpint d; mpint p; mpint q; mpint iqmp
    ssh-ed25519    32 字节小端定长（`put_mp_le_fixedlen`）
    ECDSA          mpint d

MAC 覆盖 `算法 / 加密方式 / 注释 / 公钥块 / 私钥块(明文)` 五个 SSH string，
v2 用 HMAC-SHA1（密钥 = SHA1("putty-private-key-file-mac-key" + passphrase)），
v3 用 HMAC-SHA256。

用法：
    python make_ppk.py <openssh私钥> <输出.ppk> [--version 2|3] [--passphrase xxx]
"""

import base64
import hashlib
import hmac
import struct
import sys

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import ec, ed25519, rsa
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

MAC_LABEL = b"putty-private-key-file-mac-key"
AES_BLOCK = 16


def ssh_string(data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + data


def mpint(value: int) -> bytes:
    """SSH mpint：正整数大端，最高位为 1 时补一个前导零。"""
    if value == 0:
        return ssh_string(b"")
    body = value.to_bytes((value.bit_length() + 7) // 8, "big")
    if body[0] & 0x80:
        body = b"\x00" + body
    return ssh_string(body)


def b64_block(data: bytes) -> tuple[int, str]:
    """PPK 的 base64 段：每行 64 字符，行数要写在 `*-Lines` 里。"""
    text = base64.b64encode(data).decode()
    lines = [text[i : i + 64] for i in range(0, len(text), 64)]
    return len(lines), "\n".join(lines)


def public_blob(key) -> tuple[str, bytes]:
    if isinstance(key, rsa.RSAPrivateKey):
        nums = key.private_numbers().public_numbers
        return "ssh-rsa", ssh_string(b"ssh-rsa") + mpint(nums.e) + mpint(nums.n)

    if isinstance(key, ed25519.Ed25519PrivateKey):
        raw = key.public_key().public_bytes(
            serialization.Encoding.Raw, serialization.PublicFormat.Raw
        )
        return "ssh-ed25519", ssh_string(b"ssh-ed25519") + ssh_string(raw)

    if isinstance(key, ec.EllipticCurvePrivateKey):
        curve = {
            "secp256r1": ("ecdsa-sha2-nistp256", "nistp256"),
            "secp384r1": ("ecdsa-sha2-nistp384", "nistp384"),
            "secp521r1": ("ecdsa-sha2-nistp521", "nistp521"),
        }[key.curve.name]
        point = key.public_key().public_bytes(
            serialization.Encoding.X962,
            serialization.PublicFormat.UncompressedPoint,
        )
        alg, curve_name = curve
        return alg, (
            ssh_string(alg.encode())
            + ssh_string(curve_name.encode())
            + ssh_string(point)
        )

    raise SystemExit(f"不支持的密钥类型：{type(key).__name__}")


def private_blob(key) -> bytes:
    if isinstance(key, rsa.RSAPrivateKey):
        nums = key.private_numbers()
        # 顺序是 PuTTY 的：d, p, q, iqmp —— 和 OpenSSH 的 n,e,d,iqmp,p,q 不同
        return mpint(nums.d) + mpint(nums.p) + mpint(nums.q) + mpint(nums.iqmp)

    if isinstance(key, ed25519.Ed25519PrivateKey):
        seed = key.private_bytes(
            serialization.Encoding.Raw,
            serialization.PrivateFormat.Raw,
            serialization.NoEncryption(),
        )
        # PuTTY 当小端整数存，写出时又按小端，两者互逆 → 原样 32 字节
        return ssh_string(seed)

    if isinstance(key, ec.EllipticCurvePrivateKey):
        return mpint(key.private_numbers().private_value)

    raise SystemExit(f"不支持的密钥类型：{type(key).__name__}")


def derive_legacy(passphrase: bytes, key_len: int) -> bytes:
    """v1/v2 的 SHA-1 计数派生（每轮独立，只换计数器）。"""
    out = b""
    counter = 0
    while len(out) < key_len:
        out += hashlib.sha1(struct.pack(">I", counter) + passphrase).digest()
        counter += 1
    return out[:key_len]


def build(openssh_path: str, version: int, passphrase: str | None) -> str:
    with open(openssh_path, "rb") as f:
        key = serialization.load_ssh_private_key(f.read(), password=None)

    alg, pub = public_blob(key)
    priv = private_blob(key)
    comment = "nexus-ppk-test"

    encrypted = bool(passphrase)
    phrase = (passphrase or "").encode()

    if encrypted:
        # 补零到分组倍数；PuTTY 用零填充而不是 PKCS#7
        pad = (-len(priv)) % AES_BLOCK
        plain = priv + b"\x00" * pad
        cipher_key = derive_legacy(phrase, 32)
        encryptor = Cipher(
            algorithms.AES(cipher_key), modes.CBC(b"\x00" * AES_BLOCK)
        ).encryptor()
        stored = encryptor.update(plain) + encryptor.finalize()
        encryption = "aes256-cbc"
    else:
        plain = priv
        stored = priv
        encryption = "none"

    # MAC 覆盖的是**明文**私钥块（含尾部零填充），不是密文
    mac_data = (
        ssh_string(alg.encode())
        + ssh_string(encryption.encode())
        + ssh_string(comment.encode())
        + ssh_string(pub)
        + ssh_string(plain)
    )

    if version >= 3:
        # v3 的 MAC 密钥是 Argon2 输出的尾段；这里只做 v2 样本，
        # 但保留分支以免调用方误用时静默写出错误文件。
        raise SystemExit("本脚本只生成 v2 样本（v3 的 KDF 已用 PuTTY 官方向量验证）")

    mac_key = hashlib.sha1(MAC_LABEL + phrase).digest()
    mac = hmac.new(mac_key, mac_data, hashlib.sha1).hexdigest()

    pub_lines, pub_text = b64_block(pub)
    priv_lines, priv_text = b64_block(stored)
    return "\n".join(
        [
            f"PuTTY-User-Key-File-{version}: {alg}",
            f"Encryption: {encryption}",
            f"Comment: {comment}",
            f"Public-Lines: {pub_lines}",
            pub_text,
            f"Private-Lines: {priv_lines}",
            priv_text,
            f"Private-MAC: {mac}",
        ]
    ) + "\n"


def main() -> None:
    args = sys.argv[1:]
    if len(args) < 2:
        raise SystemExit(__doc__)
    src, dst = args[0], args[1]
    version = 2
    passphrase = None
    i = 2
    while i < len(args):
        if args[i] == "--version":
            version = int(args[i + 1])
            i += 2
        elif args[i] == "--passphrase":
            passphrase = args[i + 1]
            i += 2
        else:
            raise SystemExit(f"未知参数：{args[i]}")

    text = build(src, version, passphrase)
    with open(dst, "w", newline="\n") as f:
        f.write(text)
    print(f"wrote {dst} ({len(text)} bytes)")


if __name__ == "__main__":
    main()
