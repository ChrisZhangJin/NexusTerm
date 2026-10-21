# 本目录为什么存在

`vendor/russh` 是 **russh 0.49.2 的原样拷贝** + 两处针对
keyboard-interactive（二次验证）的修补。通过 `Cargo.toml` 的
`[patch.crates-io]` 生效。

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

## 不要做的事

- 不要用「断开重连再发 KBI」绕过补丁 2 的缺失：重连会丢掉服务端保存的
  第一因子状态，仅适用于「KBI 本身是第一因子」的服务器。
- 不要给非 RSA 私钥传 `HashAlg`（那是另一码事，见 `ssh/session.rs::authenticate_key`）。
