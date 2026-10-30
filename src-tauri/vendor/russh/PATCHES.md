# 本目录为什么存在

`vendor/russh` 是 **russh 0.49.2 的原样拷贝** + 四处修补（两处针对
keyboard-interactive 二次验证，一处是探活留痕，一处是丢弃连接时的告别）。
通过 `Cargo.toml` 的 `[patch.crates-io]` 生效。

升级 russh 时务必先确认上游是否已修，再决定要不要保留本目录。
对照文件：`src/client/encrypted.rs`。

## 补丁 1：解析 USERAUTH_FAILURE 的 partial-success 位

RFC 4252 §5.1 规定：`SSH_MSG_USERAUTH_FAILURE` 里有一个布尔位表示
「这一步凭据是对的，但还要继续下一个认证方法」。**密码 + 动态口令**的服务器
（`AuthenticationMethods password,keyboard-interactive`，以及绝大多数 Go 写的
堡垒机）全靠它通知客户端「密码已过，去走 KBI」。

上游直接把该字段丢掉，只留一个 `Reply::AuthFailure`，于是程序里
「密码对了还要验证码」和「密码错」完全无法区分 —— 用户看到的就是
「认证失败：密码不正确」，而实际密码是对的。

补丁只增加了一行 `debug!` 输出（`partial_success={}`）。业务层暂时没依赖它，
但排障时这条日志是判断「第一因子到底过没过」最快的手段。

## 补丁 2：允许 keyboard-interactive 出现在非首个认证请求上

上游用 `auth_request.current` 判断「这条 INFO_REQUEST 是给谁的」，
而 `current` **只在 SERVICE_ACCEPT 那一刻**、且当时挂起的认证方法就是 KBI
时才会被置成 `KeyboardInteractive`。因此：

1. 客户端先发 `password`；
2. 服务端回 failure 并给出 `keyboard-interactive`；
3. 客户端接着发 KBI；
4. 服务端下发 `SSH_MSG_USERAUTH_INFO_REQUEST`；
5. `current` 仍是 `None` → 两个分支都不匹配 → **包被静默丢弃**；
6. 客户端在 `wait_recv_keyboard_interactive_reply()` 里永久挂起。

现象：不弹验证码框、不报错、也不超时（除非外面另加超时）。

补丁把判据补成「`current` 是 KBI **或** 当前挂起的 auth_method 是 KBI」，
第 5 步即可正常进入 KBI 分支。

## 补丁 3：探活（keepalive）留痕

`src/client/mod.rs` 的 keepalive 分支原本一声不响地发
`keepalive@openssh.com`，只有失败时才有一条 `Timeout, server not responding to keepalives`。
排「连上几十秒就断」这类问题时，完全无法判断断开前的探活到底发没发、发了几个。

补丁加一行 `debug!`（`keepalive_no` + `interval_secs`）。**唯一的时间锚点**，
配合 `NEXUS_DEBUG=1` 使用。

注意：光有这行没用 —— 本 crate 用的是 `log`，宿主要装 `tracing-log` 桥才会被看到
（见 `src/lib.rs::init_logging`）。

## 补丁 4：丢弃 `Handle` 时主动发 `SSH_MSG_DISCONNECT`

上游的 `impl Drop for Handle` 只有一行 `debug!("drop handle")`。也就是说
「释放句柄」在协议层等于**一言不发地消失** —— 我们会关掉发送端，会话循环
随即 shutdown TCP；服务端要靠自己的超时才能发现，这期间它上面的 shell 进程、
会话记录、以及堡垒机按账号记的「在线会话数」都还在。

对二次验证（OTP）尤其致命：验证码是最容易被放弃的一步（输慢了、超时了、
直接关标签页）。攒几条，就是「明明能连，突然一个都连不上」，
要等服务端自然过期才恢复。

补丁在 `Drop` 里先 `try_send` 一条 `Disconnect::ByApplication`，再让发送端关闭。
接收端在收到 `None` 之前仍会处理队列里的消息，所以对端能立刻销毁会话。
队列满或连接已关时 `try_send` 失败，忽略即可。

业务侧另有显式收尾（`ssh/mod.rs::abort_session`）：认证未完成时主动
`disconnect` 并等 `is_closed()`，这条补丁是「万一哪条路径漏了」的兜底。

## 附带证据（2026-10-21 实测）

`wiz-jump.wiz.ai:2222`（banner `SSH-2.0-Go`，密码 + MFA）：

```
userauth_failure  remaining methods "keyboard-interactive"  partial_success=true
userauth_info_request  name="chris.zhang@wiz.ai" instructions="Please Enter MFA Code."
                       prompt="[OTP Code]: " echo=true
```

补丁前：客户端在 `authenticate_password` 返回 false 后换新连接发 KBI，
服务端回 `remaining methods "password,publickey"` —— 因为第一因子状态已经丢了。
补丁后：同一连接上直接拿到 OTP 提问。

## 补丁 5：服务端主动下发的请求留痕（诊断用，debug 级）

上游对服务端发来的全局请求 / 通道请求只有 `trace!`，事后完全无法判断
「断线那一刻服务端到底发了什么、我们回了什么」。这里把两类请求各加一行
`debug!`（带 `want_reply`，通道请求还带本地号与对端号）。

之所以是 `debug` 而不是 `warn`：正常运行时它们每 30 秒就出现一次，`warn`
会把日志淹掉；而排障时开 `NEXUS_DEBUG=1` 就能看到。补丁 6 就是靠这几行定位的。

## 补丁 6：**通道请求应答必须带对端通道号**（30 秒必断的真凶）

补丁 5 的探针上线后，2026-10-23 14:00 那次抓到了真实现场：服务端下发的不是全局
请求，而是**通道请求** —— `channel_request: ChannelId(2) "keepalive@openssh.com"`，
发生在认证后 **+30.11 秒**；我们回了 `CHANNEL_SUCCESS`，**114 毫秒后 TCP 被掐**，
依旧没有 SSH DISCONNECT（`early eof`）。

根因在 `src/client/encrypted.rs` 的 `CHANNEL_REQUEST` 分支：上游回复时写的是
`channel_num`（**我方**的本地通道号），而 RFC 4254 §5.4 要求应答里的
`recipient channel` 是**对端**的通道号。russh 自己的服务端实现
（`src/server/session.rs::channel_success`）用的就是 `channel.recipient_channel`，
只有客户端这条路径写错了。

后果（对应 JumpServer/koko → golang.org/x/crypto/ssh v0.56）：
`mux.loop` 用应答里的通道号 `getChan(id)` 查表，查不到就走
`handleUnknownChannelPacket` → `ssh: invalid channel N` → `mux.loop` 返回错误 →
`m.conn.Close()`。**这条路径不发 SSH DISCONNECT**，所以客户端只能看到 `early eof`。

修改：两处应答（`keepalive@openssh.com` 分支与未知请求的兜底分支）都改为
`enc.channels.get(&channel_num).recipient_channel`。

验证：2026-10-23 14:14 修复后的构建连上后，日志里出现
`local_channel=2 peer_channel=0`（**不是同一个号**，坐实了错位），
连续 4 个 30 秒 keepalive 全部正常，会话存活 76.6 秒后由用户主动断开。

## 补丁 7：把「服务端 keepalive」暴露给宿主（心跳指示）

`Handler` 新增一个默认空实现的方法：

```rust
async fn server_keepalive(&mut self) -> Result<(), Self::Error> { Ok(()) }
```

在 `src/client/encrypted.rs` 的两处调用：通道请求里的 `keepalive@openssh.com`，
以及全局请求里的 `keepalive*`（OpenSSH `ClientAliveInterval` 走这条）。
**只做通知，不改任何协议行为**，默认实现让既有 handler 零改动。

用途：NexusTerm 状态栏的小心电图。服务端愿意在安静期探活，说明它那边的会话、
NAT 映射、堡垒机授权都还在 —— 这是「链路还活着」唯一可信的正向信号，
比本地计时器真实（本地计时器只能证明我们自己还在）。

宿主侧：`ssh/mod.rs::ClientHandler.on_keepalive` 一路传到 `handler_for`，
`open_authenticated` 里构造闭包 emit `nx://term/pulse/{session_id}`；
前端 `StatusBar` 订阅当前标签，每次心跳描一遍波形。

注意：不是所有服务器都会发 keepalive（OpenSSH 默认 `ClientAliveInterval 0`），
所以 UI 必须能表现「没有心跳」——不要用本地定时器伪造脉搏。

## 补丁 8：认证请求不再整包写进 debug 日志（**会泄露明文密码**）

`src/client/encrypted.rs` 发送认证请求后原本 `debug!("enc: {:?}", &enc.write[len..])`，
密码认证时这个切片里就是明文密码（字节数组，一眼就能还原）。NexusTerm 的
`NEXUS_DEBUG=1` 会开 russh 的 debug，于是密码被写进 `nexus.log`，用户贴日志求助时就外泄了。
改成只记长度。

**上游同样有这一行**，以后升级 russh 要重新打这个补丁。

## 不要做的事

- 不要用「断开重连再发 KBI」绕过补丁 2 的缺失：重连会丢掉服务端保存的
  第一因子状态，仅适用于「KBI 本身是第一因子」的服务器。
- 不要给非 RSA 私钥传 `HashAlg`（那是另一码事，见 `ssh/session.rs::authenticate_key`）。
