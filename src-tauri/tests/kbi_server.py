"""本地假 SSH 服务端，只用于验证 NexusTerm 的认证链路。

它刻意复刻真实 MFA 服务器的行为：
  * `password` 认证一律失败 —— 很多 PAM / 堡垒机就是这样，密码必须走 keyboard-interactive
  * keyboard-interactive 一次下发两个问题：Password: + Verification code:
    （和 Google Authenticator 的 pam 配置一致，也是界面截图里的形态）
  * 密码或验证码错一个就整体失败
  * 额外提供 `sftp` 子系统（一个内存里的假目录），用来验证「同一条已认证连接
    可以直接再开 sftp 通道」—— 这是文件面板免二次认证的核心机制

也可以切换成**公钥认证**模式，用来验证密钥登录：
  * 传 `--pubkey <公钥文件>`（authorized_keys 风格的单行文本）后只接受该密钥
  * 其余认证方式一律拒绝，便于确认「确实是公钥通过，而不是碰巧走了别的路」

用法：
    python kbi_server.py [port] [--rounds N] [--pubkey PATH]

    --rounds N     把两个问题拆成 N 轮问（默认 1 轮问完），用来测多轮问答
    --pubkey PATH  公钥认证模式：只接受 PATH 里那把公钥
"""

import base64
import os
import socket
import stat
import sys
import threading

import paramiko

PASSWORD = "test123"
OTP = "654321"


def load_public_key(path: str):
    """读取 authorized_keys 风格的单行公钥，转成 paramiko 的 PKey。

    paramiko 没有「直接读公钥文件」的入口（`from_path` 只认私钥），
    只能自己按算法名挑构造器。
    """
    with open(path, "r", encoding="utf-8") as f:
        parts = f.read().split()
    if len(parts) < 2:
        raise SystemExit(f"公钥文件格式不对：{path}")
    algo, blob = parts[0], base64.b64decode(parts[1])
    if algo == "ssh-ed25519":
        return paramiko.Ed25519Key(data=blob)
    if algo in ("ssh-rsa", "rsa-sha2-256", "rsa-sha2-512"):
        return paramiko.RSAKey(data=blob)
    if algo.startswith("ecdsa-sha2-"):
        return paramiko.ECDSAKey(data=blob)
    raise SystemExit(f"不支持的密钥类型：{algo}")


class StubSFTPServer(paramiko.SFTPServerInterface):
    """极简只读 SFTP 文件系统：够验证「子系统开得起来、能列目录」就行。

    不碰真实磁盘 —— 测试不应该对运行环境有依赖，也不该有写坏的余地。
    """

    LISTING = [
        ("readme.txt", 21, stat.S_IFREG | 0o644),
        ("logs", 0, stat.S_IFDIR | 0o755),
    ]

    @staticmethod
    def _attr(name, size, mode):
        # paramiko 5.x 把 SFTPAttributes.__init__ 收成了无参，参数只能事后赋值；
        # 老版本也支持这种写法，所以两条路都通
        a = paramiko.SFTPAttributes()
        a.filename = name
        a.st_size = size
        a.st_mode = mode
        return a

    def list_folder(self, path):
        if path in ("", ".", "/", "/home/tester"):
            return [
                self._attr(name, size, mode) for name, size, mode in self.LISTING
            ]
        return paramiko.SFTP_NO_SUCH_FILE

    def stat(self, path):
        return self._attr(os.path.basename(path.rstrip("/")) or "/", 21, stat.S_IFREG | 0o644)

    # 同一个实现即可：假目录里没有符号链接
    def lstat(self, path):
        return self.stat(path)

    def canonicalize(self, path):
        if path.startswith("/"):
            return os.path.normpath(path)
        return os.path.normpath(os.path.join("/home/tester", path))


class Server(paramiko.ServerInterface):
    def __init__(self, rounds: int, authorized=None):
        self.rounds = max(1, rounds)
        self.authorized = authorized
        self.asked = 0
        self.event = threading.Event()

    # —— 认证 ——
    def check_auth_none(self, username):
        # Agent 登录本机时不接受，逼客户端走 auth 主流程
        return paramiko.AUTH_FAILED

    def check_auth_password(self, username, password):
        # 关键：故意拒绝，模拟「只认 keyboard-interactive」的服务器
        return paramiko.AUTH_FAILED

    def check_auth_publickey(self, username, key):
        if self.authorized is not None and key == self.authorized:
            return paramiko.AUTH_SUCCESSFUL
        return paramiko.AUTH_FAILED

    def get_allowed_auths(self, username):
        return "publickey" if self.authorized is not None else "keyboard-interactive"

    def check_auth_interactive(self, username, submethods):
        self.asked = 0
        return self._next_query()

    def check_auth_interactive_response(self, responses):
        self.asked += 1
        # 每轮只问一个问题时，逐轮校验
        if self.rounds > 1:
            expect = PASSWORD if self.asked == 1 else OTP
            if not responses or responses[0] != expect:
                return paramiko.AUTH_FAILED
            if self.asked >= self.rounds:
                return paramiko.AUTH_SUCCESSFUL
            return self._next_query()

        answers = list(responses or [])
        if len(answers) < 2 or answers[0] != PASSWORD or answers[1] != OTP:
            return paramiko.AUTH_FAILED
        return paramiko.AUTH_SUCCESSFUL

    def _next_query(self):
        if self.rounds > 1:
            prompt = [("Password: ", False)] if self.asked == 0 else [("Verification code: ", False)]
        else:
            prompt = [("Password: ", False), ("Verification code: ", False)]
        # 注意是 *prompt：InteractiveQuery 的第 3 个参数是可变参数，
        # 直接传列表会变成「一层套一层的 prompts」，paramiko 序列化时报 TypeError
        return paramiko.InteractiveQuery("", "", *prompt)

    # —— 通道 ——
    def check_channel_request(self, kind, chanid):
        return paramiko.OPEN_SUCCEEDED if kind == "session" else paramiko.OPEN_FAILED_ADMINISTRATIVELY_PROHIBITED


def serve(port: int, rounds: int, pubkey: str | None = None):
    # 排障用：把 paramiko 的协议交互写成日志（默认关闭，设 KBI_DEBUG=1 打开）。
    # 日志落在脚本同目录，避免污染 src-tauri/。
    if os.environ.get("KBI_DEBUG"):
        log = os.path.join(os.path.dirname(os.path.abspath(__file__)), f"kbi_{port}.log")
        paramiko.util.log_to_file(log, level="DEBUG")

    host_key = paramiko.RSAKey.generate(2048)
    authorized = load_public_key(pubkey) if pubkey else None

    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("127.0.0.1", port))
    sock.listen(8)
    mode = f"pubkey={pubkey}" if pubkey else f"rounds={rounds}"
    print(f"kbi server listening on 127.0.0.1:{port} ({mode})", flush=True)

    def handle(client: socket.socket):
        try:
            t = paramiko.Transport(client)
            t.add_server_key(host_key)
            # 必须在 start_server 之前注册：客户端发 "subsystem sftp" 请求时
            # 由这个 handler 接管，而不是当成未知请求拒绝
            t.set_subsystem_handler("sftp", paramiko.SFTPServer, StubSFTPServer)
            t.start_server(server=Server(rounds, authorized))
            # 认证成功后保持连接一段时间，让客户端有机会开通道
            t.join(30)
        except Exception as e:  # noqa: BLE001
            print("connection error:", e, flush=True)
        finally:
            try:
                client.close()
            except Exception:  # noqa: BLE001
                pass

    while True:
        client, _ = sock.accept()
        threading.Thread(target=handle, args=(client,), daemon=True).start()


if __name__ == "__main__":
    args = [a for a in sys.argv[1:]]
    rounds = 1
    if "--rounds" in args:
        i = args.index("--rounds")
        rounds = int(args[i + 1])
        del args[i : i + 2]

    pubkey = None
    if "--pubkey" in args:
        i = args.index("--pubkey")
        pubkey = args[i + 1]
        del args[i : i + 2]

    serve(int(args[0]) if args else 2222, rounds, pubkey)
