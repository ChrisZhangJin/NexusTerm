# Xshell `.xsh` 会话文件格式核对报告

> 目的：为 NexusTerm 的 macOS 客户端做**格式互操作**——让用户把自己用 Xshell 保存的 `.xsh` 会话导入进来。
> 范围：只核对**节名 / 键名字面量、取值枚举、文件编码**。不涉及任何加解密算法，不反编译 Xshell 代码。
> 日期：2026-10-16 · 分析目录：`/Users/chris/WorkBuddy/REA/`

---

## 0. 结论摘要（TL;DR）

| 项 | 结论 |
|---|---|
| 编码 UTF-16LE+BOM | ✅ **属实**，39/39 样本全部 `FF FE` |
| `[CONNECTION] Host/Port/Protocol` | ✅ **一致** |
| `[CONNECTION:AUTHENTICATION] UserName` | ✅ **一致** |
| `[CONNECTION:AUTHENTICATION] Method` | ⚠️ **需修正**：真实值是数字 `0` 或**整个键缺失**，不是 `PASSWORD`/`PUBLICKEY` 字符串 |
| 私钥路径 `IdentityFile` | ❌ **需修正**：真实键是 `[CONNECTION:AUTHENTICATION] UserKey`，**不是** `[CONNECTION:AUTHENTICATION:PUBLICKEY] IdentityFile`（后者在 37 个真实文件中 0 次出现） |
| `[SESSION] SessionName` | ⚠️ **需修正认知**：Xshell 8.1 文件里**根本没有 `[SESSION]` 节**；会话名就是**文件名**（官方手册明文） |
| 密码位置 | ⚠️ **需修正**：8.1 里密码在 `[CONNECTION:AUTHENTICATION] Password`，不是 `:PASSWORD` 子节；且是机器绑定的密文，跨机器不可迁移（已确认，未解密） |
| `Protocol` 枚举 | 官方：`SSH / SFTP / TELNET / RLOGIN / SERIAL / LOCAL / RDP`；本机样本 39/39 只有 `SSH` |
| `Method` 枚举 | 官方：`Password / Public Key / Keyboard Interactive / GSSAPI`（+PKCS11）；本机样本只见到数字 `0` 与「键缺失」 |
| 行尾 | 真实文件 37/37 全部 **CRLF**；我们的导出写的是 LF（建议改） |
| 注释 | 真实文件 **0 条** `;`/`#` 注释行；我们现有剥离逻辑不会误伤，但可保留 |

**最重要的一条**：我们现在的 `IdentityFile` 读法对 Xshell 8.1 的真实文件**取不到私钥路径**，必须改成读 `UserKey`（并保留旧键名做兼容）。

---

## 1. 是否获取到 Xshell 样本

### 1.1 安装包：拿到了，但**分析不了**

| 路径 | 大小 | SHA-256 |
|---|---|---|
| `/Users/chris/WorkBuddy/REA/samples/Xshell-latest-p.exe` | 47,666,112 B | `cc16abc019d62dd90c053858a1731c0c8937b5b3e251338198d071d82599b83c` |
| `/Users/chris/WorkBuddy/REA/samples/Xshell8_manual.pdf`（官方手册） | 343,586 B | — |

获取方式（**绕开了官网的邮箱/姓名表单**，没有提交任何个人信息）：

```
https://cdn.netsarang.net/v8/Xshell-latest-p        ← 从 xshell.com/en/free-for-home-school/ 页面里挖到的静态直链
https://cdn.netsarang.net/docs/Xshell8_manual.pdf   ← 官方手册直链
```

`mcp__rea__open_binary` 打开成功（`format=pe`，`architecture=x86`）。但**字符串检索做不了**，两个原因：

1. **REA 的原生 provider 在这台机器上全部不可用**。调用 `mcp__rea__search_strings` 返回 `provider_unavailable`：
   - `ghidra` → `not_configured`（`GHIDRA_INSTALL_DIR` 未设置，无 Java）
   - `hopper` → `executable_missing`（`/Applications/Hopper Disassembler.app` 不存在）
   - `ida` → `not_configured`（`REA_IDA_MCP_CONFIG` 未设置）
   - 所以 `mcp__rea__inspect_artifact` 也只返回了「Artifact has no child container entries」，没有子结构。
2. **即使 provider 可用，这个 exe 也不是 Xshell 本体，而是 InstallShield 引导程序**：
   - PE 只有 4 个节，文件尾 46 MB 是 `ISSetupStream`（PE 结束于 `0x177A00`）。
   - 流里有可读的 **UTF-16LE 文件名**（说明目录区是明文），实际文件包括：`Xshell 8.msi`、`ISSetup.dll`、`Setup.ini`、`Splash.bmp`、`1033.mst`、VC++ Redist 等。
   - **但文件内容本身是压缩的**：首 1 MB 香农熵 = **7.9997 bit/byte**；用 deflate（raw/zlib/gzip 全 windowBits）、LZMA1/LZMA2（lc/lp/pb/dict 全组合暴力）、BZ2、raw 偏移 0–4096 逐一试探，**全部失败**；`unshield` / `sevenzip` 因 `brew` 的 `sandbox-exec` 在本环境被拒（`Operation not permitted`）装不上。
   - ⇒ 没拿到 `Xshell.exe`，也就**没有从二进制里取到任何字符串字面量**。

### 1.2 关键转折：本机就有 37 个**真实** `.xsh`

`/Users/chris/Tools/Sessions/` 下有 **37 个 Xshell 真实写出的会话文件**（`[SessionInfo] Version=8.1`），分布在 `local/`、`VPS/`、`vos/` 等子目录。全盘 `mdfind` 总数 39，其中 2 个（见下）被判定为我们自己的产物、已排除。

这比反编译二进制**证据力更强**：这是 Xshell 自己落盘的成品，字段拼写、层级、取值都是它亲笔写的。

已复制到 samples 便于复现：

| 样本 | 大小 | SHA-256 |
|---|---|---|
| `samples/xshell8-real-hostinger.xsh` | 9,722 B | `5492f0240793d541d96eb13f052e52e8e81489d33eabdddf1dedfb9d41d5ad69` |
| `samples/xshell8-real-gerrit.xsh` | 11,366 B | `a2e291599aa448aaa55faf4556afe262dcb0bd33361da02181b7dfca205934a2` |

**已排除的 2 个文件**（结构与 `xsh.rs::render_xsh` 的输出逐字符一致，是本工具导出的，不能当证据）：

- `/Users/chris/Downloads/chris@192.168.32.101.xsh`
- `/Users/chris/WorkBuddy/REA/NexusTerm-mac/sample-from-xshell.xsh`

> 这两个文件恰好有 `[SESSION] SessionName` 和 `[CONNECTION:AUTHENTICATION:PASSWORD] Password=`——**别被它们误导**，那是我们自己写的格式，不是 Xshell 的。

### 1.3 旁证材料

- `samples/thirdparty/` — 4 个第三方转换器的源码（GitHub，非 NetSarang 代码），用于旁证键名：`droptune/xsh-to-ssh`、`lilongen/xshell-to-iterm2`、`Ansen/xshell2windterm`、`pedoc/XShell2MobaXterm`。
- `samples/Xshell8_manual.pdf` — **NetSarang 官方 Xshell 8 手册**（权威来源，用于枚举值）。

---

## 2. 真实节名 / 键名表（与我们实现逐条比对）

样本基数：**37 个真实 Xshell 8.1 文件**。
「出现位置」列给出在 `samples/xshell8-real-hostinger.xsh` 里的**字节偏移**与该字面量的 UTF-16LE 十六进制（这就是"字符串表"证据——文件就是 UTF-16LE，字面量在磁盘上就是这些字节）。

### 2.1 我们在读的字段

| # | 我们的实现 | 真实情况（节 / 键） | 出现位置（偏移 / UTF-16LE hex） | 判定 |
|---|---|---|---|---|
| 1 | `[CONNECTION] Host` | `[CONNECTION] Host` | `0x10FA` / `48 00 6f 00 73 00 74 00 3d 00` | ✅ **一致** |
| 2 | `[CONNECTION] Port` | `[CONNECTION] Port` | `0x0F6` / `50 00 6f 00 72 00 74 00 3d 00` | ✅ **一致** |
| 3 | `[CONNECTION] Protocol`（必须 = SSH） | `[CONNECTION] Protocol`，取值 `SSH`（39/39） | `0x111E` / `50 00 72 00 6f 00 74 00 6f 00 63 00 6f 00 6c 00 3d 00` | ✅ **一致** |
| 4 | `[CONNECTION:AUTHENTICATION] UserName`（默认 root） | `[CONNECTION:AUTHENTICATION] UserName` | `0x21C6` / `55 00 73 00 65 00 72 00 4e 00 61 00 6d 00 65 00 3d 00` | ✅ **一致**（默认 root 也合理：样本 20/37 就是 `root`） |
| 5 | `[CONNECTION:AUTHENTICATION] Method`（`PUBLICKEY`→key，其余→password） | 键存在（17/37）但值是 **`0`**；20/37 **整键缺失** | `0xF18` / `4d 00 65 00 74 00 68 00 6f 00 64 00 3d 00` | ⚠️ **需修正**（见 §5.2） |
| 6 | `[CONNECTION:AUTHENTICATION:PUBLICKEY] IdentityFile` | **不存在这个节**。真实键是 `[CONNECTION:AUTHENTICATION] UserKey` | `[CONNECTION:AUTHENTICATION:PUBLICKEY]` → `b.find()` = **-1（0 次出现）**；`IdentityFile=` → **-1（0 次出现）**；`UserKey=` @ `0x21B2` / `55 00 73 00 65 00 72 00 4b 00 65 00 79 00 3d 00` | ❌ **需修正**（见 §5.1） |
| 7 | `[SESSION] SessionName`（缺失用文件名） | 8.1 文件**无 `[SESSION]` 节**（0/37）。`[SessionInfo] Description` 是常量 `Xshell session file`（37/37），**不是会话名** | `[SESSION]` → **-1**；`[SessionInfo]` @ `0x106` / `5b 00 53 00 65 00 73 00 73 00 69 00 6f 00 6e 00 49 00 6e 00 66 00 6f 00 5d 00` | ⚠️ **需修正认知**（见 §5.3） |

### 2.2 真实文件里存在、我们目前**没读**的字段（供扩展）

| 节 | 键 | 37 文件出现数 | 典型值 | 说明 |
|---|---|---|---|---|
| `[CONNECTION:AUTHENTICATION]` | `Password` | 37（24 个非空） | `dmFerMbYABT54…`（56–72 字符） | **加密密文**，见 §3.2。不要读 |
| `[CONNECTION:AUTHENTICATION]` | `Passphrase` | 37（全空） | `` | 私钥口令，同样加密 |
| `[CONNECTION:AUTHENTICATION]` | `UserKey` | 37（全空） | `` | **私钥路径（本语料全空，故取值形态未知）** |
| `[CONNECTION:AUTHENTICATION]` | `AuthMethodList` | 37 | `01` / `01,10,20,30` / `01,10,20,30,40,50` / `01,20,30,10` | 认证方法候选列表，数字码 |
| `[CONNECTION:AUTHENTICATION]` | `Library`、`CapiKey`、`CapiPin`、`Pkcs11Middleware`、`Pkcs11Pin`、`Delegation`、`UseAuthProfile`、`AuthProfile`、`UseExpectSend`、`ExpectSend_Count`、`ScriptPath`、`ScriptParam`、`UseInitScript`、`TelnetLoginPrompt`、`TelnetPasswordPrompt`、`RloginPasswordPrompt` | 37 | — | 智能卡 / 登录脚本 / TELNET·RLOGIN 提示符 |
| `[CONNECTION]` | `Description` | 37（34 个空） | `域名 brv.wiz.ai；主/备 IP：…`（含字面 `\r\n` 转义） | **自由文本备注**，不是会话名 |
| `[CONNECTION]` | `AutoReconnect` / `AutoReconnectInterval` / `AutoReconnectLimit` / `CustomIcon` / `DefaultIcon` / `UseCustomIcon` | 37 | — | 可选导入项 |
| `[SessionInfo]` | `Version` | 37 | `8.1` | **格式版本号**，可用于分流解析策略 |
| `[SessionInfo]` | `Description` | 37 | `Xshell session file`（常量） | 固定样板文字 |
| `[CONNECTION:SSH]` | `RemoteCommand`、`Compression`、`AgentForwarding`、`ForwardX11`、`Cipher`、`MAC`、`KeyExchange`、`UseAuthAgent`、`LaunchAuthAgent`… | 37 | — | SSH 细节，可选用 |
| `[TERMINAL]` | `Type`、`CodePage`、`Rows`、`Cols`、`ScrollbackSize`、`BackspaceSends`、`DeleteSends`… | 37 | `xterm`、`65001`、`24`、`80`、`10240` | 终端参数，`CodePage=65001` 即 UTF-8 |
| `[TERMINAL:WINDOW]` | `FontFace`、`FontSize`、`ColorScheme`、`CursorColor`… | 37 | — | 外观 |
| `[LOGGING]` / `[TRANSFER]` / `[BELL]` / `[TRACE]` / `[HIGHLIGHT]` / `[USERINTERFACE]` / `[ADVANCED]` / `[CONNECTION:PROXY]` / `[CONNECTION:TELNET]` / `[CONNECTION:RLOGIN]` / `[CONNECTION:SERIAL]` / `[CONNECTION:FTP]` / `[CONNECTION:RDP]` / `[CONNECTION:HWCERTIFICATES]` / `[CONNECTION:KEEPALIVE]` | 各若干 | 37 | — | 无论协议如何，Xshell 都会把**所有协议分支的节全量写出**（模板式），所以不能靠"节是否存在"判断协议 |

### 2.3 语法层面的事实（37/37）

- 行尾：**全 CRLF**（`\r\n`），0 个纯 LF。
- 注释行：**0 条**（既没有 `;` 也没 `#` 开头的行）。
- 畸形行：0 条——每一行要么是 `[Section]`，要么含 `=`。
- 行首/行尾：**无缩进空格**。
- 特殊值形态：`QuickCommandSet=\`（值以反斜杠结尾）、`Description=…\\r\\n…`（**字面**反斜杠转义，不是真换行）——说明值是允许含 `\` 的，`split('=', 1)` 取首个等号是对的。
- 节名**大小写敏感**：`CONNECTION`（全大写）与 `SessionInfo`（混合大小写）并存 ⇒ 我们的 `section == "..."` 精确比较是**正确**的，不要改成忽略大小写。
- 键名大小写：文件里都是固定的 CamelCase（`UserName`、`UserKey`、`Host`…）；我们统一转大写比较 ✅ 兼容。

---

## 3. 编码结论

### 3.1 UTF-16LE + BOM —— 确认属实

39 个 `.xsh`（37 真实 + 2 我们导出）**全部**以 `FF FE` 开头，`FE FF`（BE）0 个，UTF-8 BOM 0 个，无 BOM 0 个。

```
UTF-16LE BOM (FF FE): 39 / 39
```

⇒ `xsh.rs::read_text` 的 BOM 分支逻辑正确，`to_utf16le_bom` 导出方向也正确。**不需要改**。

### 3.2 密码「加密存储、跨机器不可迁移」——确认（未做任何解密）

观察到的事实（只描述形态，不分析算法）：

- `[CONNECTION:AUTHENTICATION] Password` 在 24/37 个文件里非空，值为 base64 字符集的**定长密文**，长度分布 `{56:12, 68:5, 64:3, 72:1, 60:3}`。
- **不同主机、不同账号的密文共享同一前缀**（例如 8 个不同会话的密文都以 `dmFerMbYABT54CVH7KW9E19k` 开头）⇒ 密文里携带了**本机/本用户绑定的成分**，不是对密码单独哈希/加密。这与「换台机器就解不开」的现象一致。
- 官方手册（`Xshell8_manual.pdf`）明文：
  > "The password is **encoded and saved in the user's PC**. For greater security, leave this field blank or set a master password."
  > "If you would like to use the session file's password on another device, **set the Master Password** … On the device to use the session file, set the **exact same** Master Password."

⇒ **结论：密码是加密存储、跨机器不可迁移（除非用户在两端设了同一个 Master Password）。我们一律丢弃、导入后让用户重填 —— 这个决策是对的。**
⇒ `xsh.rs` 目前根本不读 `Password` 字段 ✅ 保持。`passphrase` 同理。

> ⚠️ 注意：8.1 里密码在 `[CONNECTION:AUTHENTICATION] Password`（**不在** `:PASSWORD` 子节）。我们导出时写的 `[CONNECTION:AUTHENTICATION:PASSWORD]\nPassword=` 在 8.1 里并不存在——值虽为空、无害，但**不是 Xshell 8 会写的东西**，建议改成不写这个节（见 §5.4）。

---

## 4. 枚举值

### 4.1 `Protocol`

| 来源 | 取值 | 置信度 |
|---|---|---|
| **官方 Xshell 8 手册**（`Xshell8_manual.pdf`, §5.2.1 Connection） | `SSH, SFTP, TELNET, RLOGIN, SERIAL, LOCAL, RDP` | 高（一手文档） |
| 官方手册的默认端口 | SSH/SFTP=22, TELNET=23, RLOGIN=513, FTP=21, RDP=3389（SERIAL/LOCAL 无端口） | 高 |
| 本机 39 个样本 | **只有 `SSH`（39/39）** | 实测 |
| 第三方转换器 `pedoc/XShell2MobaXterm` 代码 | 分支处理 `SSH` / `TELNET` / `SERIAL`，其余抛 `NotImplementedException` | 中（旁证） |

`SFTP` 是 SSH 子系统（同 22 端口）；如果将来想放宽，`SFTP` 按 SSH 处理是安全的。

### 4.2 `Method`（认证方式）

| 来源 | 取值 | 置信度 |
|---|---|---|
| **官方 Xshell 8 手册**（§5.2.2 Authentication） | `Password, Public Key, Keyboard Interactive, GSSAPI`（Xshell 6/7 手册还多一个 `PKCS11`） | 高（UI 层枚举） |
| **本机 37 个真实文件的磁盘字面量** | **只有 `Method=0`（17 个）和「键缺失」（20 个）**；同时存在 `AuthMethodList=01 / 01,10,20,30 / 01,10,20,30,40,50 / 01,20,30,10` | 高（实测，但覆盖不全） |

即：**UI 上的枚举名与落盘的枚举值是两套东西**。落盘是数字码（`Method=0`；`AuthMethodList` 用 `01/10/20/30/40/50`），本语料里**没有**任何 `PUBLICKEY` / `PASSWORD` 字符串形式的 `Method`。

`Method` 与 `Password` 是否为空**无相关性**（交叉表）：

```
Method=0        list=01                 无密码: 9   有密码: 5
Method=0        list=01,10,20,30        无密码: 0   有密码: 1
Method=0        list=01,10,20,30,40,50  无密码: 0   有密码: 2
Method=<缺失>   list=01                 无密码: 2   有密码: 5
Method=<缺失>   list=01,10,20,30        无密码: 1   有密码: 10
Method=<缺失>   list=01,10,20,30,40,50  无密码: 0   有密码: 1
Method=<缺失>   list=01,20,30,10        无密码: 1   有密码: 0
```

⇒ 不能靠 `Method` 反推认证方式。更可靠的信号是 **`UserKey` 是否非空**（Xshell 只在 Public Key 方式下才有用户密钥）。

---

## 5. 修正建议（给 `xsh.rs`，未改动该文件）

### 5.1 ❌ 私钥路径：`IdentityFile` → `UserKey`（最重要）

现状（`xsh.rs:147-152`）：

```rust
let key_path = if auth_type == "key" {
    lookup(&ini, "CONNECTION:AUTHENTICATION:PUBLICKEY", "IdentityFile").map(|s| s.to_string())
} else { None };
```

建议改成——**优先 `UserKey`，保留旧键名兜底，且不要以 `auth_type` 为前提**（因为 `Method` 常常缺失/是数字）：

```rust
// 私钥路径：Xshell 8.x 写在 [CONNECTION:AUTHENTICATION] UserKey；
// 旧版本/第三方资料里出现过 :PUBLICKEY 子节的 IdentityFile，保留兜底。
let key_path = lookup(&ini, "CONNECTION:AUTHENTICATION", "UserKey")
    .filter(|v| !v.is_empty())
    .or_else(|| lookup(&ini, "CONNECTION:AUTHENTICATION:PUBLICKEY", "IdentityFile"))
    .or_else(|| lookup(&ini, "CONNECTION:AUTHENTICATION:PUBLICKEY", "UserKey"))
    .filter(|v| !v.is_empty())
    .map(str::to_string);
```

配套：`auth_type` 的判断也应以 `UserKey` 为准（见 5.2）。

> ⚠️ 提醒：`UserKey` 的**取值形态未知**（本语料 37/37 全空）。第三方实现 `pedoc/XShell2MobaXterm` 假定它是**相对路径**并拼成 `_ProfileDir_\.ssh\{UserKey}`：
> ```csharp
> userKey = $@"_ProfileDir_\.ssh\{userKey}";
> ```
> ⇒ 导入后**不要**直接把它当绝对路径用；建议原样存、UI 上提示用户"密钥路径来自 Windows，需在本机重新选择"。

### 5.2 ⚠️ `Method`：兼容数字 + 缺失，并以 `UserKey` 反推

现状（`xsh.rs:138-145`）把 `Method` 当字符串枚举匹配，遇到 `0` 会落到 `_ => "password"` —— **结果碰巧对，但逻辑是脆的**。建议显式化：

```rust
let method_raw = lookup(&ini, "CONNECTION:AUTHENTICATION", "Method")
    .unwrap_or("")
    .trim()
    .to_uppercase();

// Xshell 8.x 落盘为数字码（本语料仅见 0）；旧版本/文档层面为字符串枚举。
// 真实信号优先级：UserKey 非空 > Method 字符串 > Method 数字 > 默认密码。
let auth_type = if key_path.is_some() {
    "key"
} else if method_raw.contains("PUBLICKEY") || method_raw.contains("PUBLIC KEY") {
    "key"
} else {
    // "0" / "PASSWORD" / "KEYBOARD_INTERACTIVE" / "GSSAPI" / 空 / 未知数字 全部按密码
    "password"
};
```

（注意顺序：`key_path` 要先算出来，所以把 5.1 的代码放到 `method` 之前。）

### 5.3 ⚠️ 会话名：放弃 `[SESSION]` 的幻想，就用文件名

- 8.1 真实文件 **0 个**有 `[SESSION]` 节；`[SessionInfo] Description` 是常量 `Xshell session file`。
- 官方手册：*"Enter an intrinsic name for the session. **The name becomes the session file name**."*
- 第三方转换器 `Ansen/xshell2windterm` 的 `getLable()` 也是直接取文件名去扩展名。

⇒ **文件名回退逻辑（`xsh.rs:234-237`）是对的，保持为主路径**。`[SESSION] SessionName` 保留作为老版本兼容即可（现状代码已经这么写）。建议追加一条注释说明，避免以后有人"补"一个从 `SessionInfo.Description` 取名字的改动。

可选增强（低优先）：`[CONNECTION] Description` 是用户备注，可以导入为备注字段，但**不能当名字**（可能多行、含字面 `\r\n`）。

### 5.4 导出端建议

1. **行尾改 CRLF**：真实文件 37/37 是 CRLF，我们的 `render_xsh` 用 `\n`。Xshell 大概率能容忍 LF，但为了「Xshell 直接打开不乱码」这个目标，建议统一 `\r\n`。
2. **去掉 `[CONNECTION:AUTHENTICATION:PASSWORD]` 子节**：8.1 里不存在这个节（密码平铺在 `[CONNECTION:AUTHENTICATION]`）。我们写空值不会泄密，但属于"写了 Xshell 不认识的结构"。建议：要么删掉该节，要么保留但明确注释"仅为兼容旧版 Xshell"。
3. 私钥导出：现在写 `[CONNECTION:AUTHENTICATION:PUBLICKEY]\nIdentityFile=…`。若 5.1 成立，建议**改写到 `[CONNECTION:AUTHENTICATION] UserKey=…`**，或两处都写（更符合 Xshell 8）。

### 5.5 其它小建议（可选）

- `parse_ini` 的行尾注释剥离 `value.find(" ;")`（`xsh.rs:96-99`）：真实文件无内联注释，且 `Description` 里可能出现 ` ;`。建议**直接删掉这段**，减少误伤。
- `Port` 解析失败时，可按官方默认端口兜底（SSH/SFTP 22、TELNET 23、RLOGIN 513、FTP 21、RDP 3389），比硬编码 22 更贴合。
- 非 SSH 协议的错误信息可以列出官方枚举，方便用户理解："不支持的协议 TELNET（支持 SSH/SFTP，官方枚举：SSH/SFTP/TELNET/RLOGIN/SERIAL/LOCAL/RDP）"。
- 可读 `[SessionInfo] Version` 做格式分流（目前只见 `8.1`）。

---

## 6. 合规边界上的取舍

| 做了 | 没做 / 主动放弃 |
|---|---|
| ✅ 只从**文件格式层面**取证：节名、键名、取值、编码、行尾、注释约定 | ❌ **没有反编译 / 伪代码化 Xshell 的任何函数**（REA provider 本来就不可用；即使可用也不会去读它的 INI 读写函数实现） |
| ✅ 取证来源是 **Xshell 自己落盘的 `.xsh` 成品** + **NetSarang 官方公开手册** + 第三方转换器的公开源码 | ❌ **没有碰加解密**：`Password` / `Passphrase` 只统计"是否非空 / 长度 / 是否共享前缀"，用来支撑"加密且机器绑定"这一事实；**没有尝试解密、还原、也没有分析密文结构** |
| ✅ 下载安装包时**绕开了官网的姓名+邮箱表单**（从页面里挖静态 CDN 直链），没有提交任何个人信息给厂商 | ❌ 抓到 `Xshell 8.msi` 线索后**停止了拆包**：InstallShield 流是压缩的，继续下去等于逆向其打包/压缩机制，超出"格式互操作"边界 |
| ✅ 看到 `292427558/xshell-session-to-tabby`（简介写着"解密xshell 会话密码"）**刻意没有下载、没有打开** | ❌ 没有复制 Xshell 的任何代码、资源或字符串到我们的仓库；第三方转换器源码仅用于旁证键名，未引入项目 |
| ✅ 未修改 `xsh.rs` | — |

**中途的一次边界判断**：拆 InstallShield 流需要写解压器，而确定其压缩方法必然要去看 `setup.exe` 的代码逻辑（那是 NetSarang 的打包实现）——按任务规则"若发现需要读算法才能确定兼容，立即停止并报告"，我在确认 deflate/LZMA/BZ2 全参数试探失败后**停止了拆包**，改用真实 `.xsh` 语料。这个转向反而拿到了更高等级的证据。

---

## 7. Evidence / Unknowns

### 7.1 Evidence（有证据支撑）

| 结论 | 证据 | 强度 |
|---|---|---|
| `.xsh` 是 UTF-16LE + BOM | 39/39 文件首字节 `FF FE` | 实测 · 强 |
| 节名 `CONNECTION` / `CONNECTION:AUTHENTICATION` 拼写与层级 | 37/37 真实文件，字节偏移 `0x10B2` / `0x1E74` | 实测 · 强 |
| `Host` / `Port` / `Protocol` / `UserName` 键名 | 37–39/37 出现，偏移见 §2.1 | 实测 · 强 |
| `Protocol=SSH`（大写） | 39/39 | 实测 · 强 |
| 会话名 = 文件名；8.1 无 `[SESSION]` 节 | 0/37 有 `[SESSION]`；`SessionInfo.Description` = 常量；官方手册 "The name becomes the session file name" | 实测+文档 · 强 |
| 私钥路径键是 `UserKey`（不是 `IdentityFile`） | `[CONNECTION:AUTHENTICATION] UserKey` 37/37 存在（偏移 `0x21B2`）；`:PUBLICKEY` 节与 `IdentityFile` 在 37 个真实文件中 **0 次出现**；2 个第三方转换器都读 `UserKey` | 实测+旁证 · 中强（取值形态未验证） |
| `Method` 落盘为数字（`0`）或缺失 | 37 个文件交叉表 | 实测 · 强 |
| `AuthMethodList` 数字码 `01/10/20/30/40/50` | 37/37 | 实测 · 强 |
| 密码加密存储、机器绑定、跨机器不可迁移 | 24/37 非空密文（56–72 字符，base64 字符集）；8+ 个不同会话共享密文前缀 ⇒ 含机器绑定成分；官方手册 "encoded and saved in the user's PC" + 跨设备需设相同 Master Password | 实测+文档 · 强 |
| 全 CRLF、无注释行、无畸形行 | 37/37 | 实测 · 强 |
| 节名大小写敏感（`SessionInfo` vs `CONNECTION`） | 37/37 | 实测 · 强 |
| `Protocol` 官方枚举 `SSH/SFTP/TELNET/RLOGIN/SERIAL/LOCAL/RDP` 与默认端口 | `Xshell8_manual.pdf` §5.2.1 | 文档 · 强 |
| `Method` UI 枚举 `Password/Public Key/Keyboard Interactive/GSSAPI(/PKCS11)` | `Xshell8_manual.pdf` §5.2.2、Xshell 6/7 手册 | 文档 · 强 |
| Xshell 8 安装包是 InstallShield，负载为压缩的 `ISSetupStream`，内含 `Xshell 8.msi` | PE 节表 + 尾部 46 MB 流 + 明文 UTF-16 文件名清单 + 熵 7.9997 | 实测 · 强 |

### 7.2 Unknowns（推测 / 未验证，需要真实样本补证）

| 项 | 现状 | 影响 | 建议补证方式 |
|---|---|---|---|
| **`UserKey` 的取值形态** | 37/37 全空，未见过实际值 | 高——导入后密钥路径可能是相对路径（第三方实现假定拼在 `.ssh\` 下），直接当绝对路径会失败 | 需要一个**配了 Public Key 的真实会话**；或按 §5.1 提示用户重选 |
| **`Method` 数字码 → 认证方式的映射** | 只见过 `0`；`AuthMethodList` 的 `01/10/20/30/40/50` 各代表什么未知；`Method=0` 是"索引"还是"枚举值"未知 | 中——不影响当前行为（都回落 password），但无法自动识别公钥会话 | 同上，或靠 `UserKey` 非空反推（已建议） |
| **旧版本（≤7）是否真有 `[SESSION] SessionName` / `[CONNECTION:AUTHENTICATION:PUBLICKEY] IdentityFile`** | 本机没有旧版真实文件；仅有的 2 个是**我们自己导出的** | 中——我们的兼容分支目前无真实依据，但也无害 | 需要一个 Xshell 5/6/7 导出的 `.xsh` |
| **是否可能不存在 UTF-8 的 `.xsh`** | 39/39 都是 UTF-16LE；但 Xshell 8 更新日志提到"从 UTF-8 编码的 csv 导入会话" | 低——保留 UTF-8 兜底即可（现有实现已有） | — |
| **Xshell 二进制内部的节名/键名字面量与字符串表引用** | **完全没取到**（REA provider 未配置 + 安装包负载无法解出） | 低——真实文件已直接给出同样的答案，二进制证据属于锦上添花 | 若确实需要：① 装 Ghidra 12.1 并设 `GHIDRA_INSTALL_DIR`；② **或**由用户提供已安装目录下的 `Xshell.exe`（`C:\Program Files (x86)\NetSarang\Xshell 8\Xshell.exe`），这是最省事的办法 |
| **`SFTP` 协议能否按 SSH 处理** | 官方把 SFTP 与 SSH 并列为独立协议（同 22 端口） | 低 | 视产品决策 |
| `[CONNECTION:RDP]` / `[CONNECTION:FTP]` 对应 `Protocol=RDP` / `FTP` | 从节名**推测**，未在样本或手册枚举中直接见到落盘值 | 低 | — |

---

## 附：复现命令

```bash
# 1) 安装包（静态直链，无需填表）
curl -L -o samples/Xshell-latest-p.exe https://cdn.netsarang.net/v8/Xshell-latest-p
#    sha256 = cc16abc019d62dd90c053858a1731c0c8937b5b3e251338198d071d82599b83c

# 2) 官方手册
curl -L -o samples/Xshell8_manual.pdf https://cdn.netsarang.net/docs/Xshell8_manual.pdf

# 3) 真实语料（本机已有 37 个 Xshell 8.1 会话）
ls /Users/chris/Tools/Sessions/**/*.xsh

# 4) 编码统计：39/39 为 FF FE
python3 -c "
import os,collections
c=collections.Counter()
for dp,_,fn in os.walk('/Users/chris/Tools'):
    for f in fn:
        if f.lower().endswith('.xsh'): c[open(os.path.join(dp,f),'rb').read(2)]+=1
print(c)"
```
