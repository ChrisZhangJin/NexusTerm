# Roadmap / 待办方向

记录已经讨论过设计、但还没动手实现的大方向，避免讨论过的结论随对话丢失。

## Nexus 控制面：本机 Socket API + nexus-cli

**现状**：AI Agent 面板只能在 app 内部、对当前激活的单个标签工作（进程内 `invoke()` 调用）。

**目标**：让本机其它 AI agent（如 Claude Code）能通过一个独立的 `nexus-cli` 二进制 + 本机
Unix socket / Windows 命名管道，枚举/读取/控制 NexusTerm 管理的所有会话（本地 + 远程 SSH），
实现"session A 的输出原样转发给 session B 做输入"这类编排。这是 NexusTerm 相对于
`herdr`（本机进程型 agent 编排器）的差异化场景——Nexus 管理的大多是**远程**会话，对端通常没有
权限装任何探针，只能靠这一端"看字节流 + 发按键"。

**已确认的设计约束**：
- 传输：本机 Unix socket / 命名管道，不监听网络端口。
- 鉴权：进程启动时生成一次性 token，写入仅当前用户可读的文件，CLI 调用必须带 token。
- A→B 转发语义：原样转发（把 A 的某段输出原文当作 B 的输入发送），不做模型摘要/解析。
- **必须保留现有隐私契约**：`agent/mod.rs` 的"AI Agent 总开关关闭时后端完全不采集旁路，
  零开销"不能被控制面绕过——控制面需要自己独立的开关，和 AI Agent 开关各自独立、按位或后
  共同决定"是否缓冲"，但不共享"是否允许 LLM 出网"等其它语义。

**关键发现**（为后续实现省去重新调研）：
- 后端 `agent/buffer.rs` 的按 session id 环形缓冲**已经**覆盖所有打开的本地/SSH 标签，
  与前端是否渲染无关——`local/mod.rs` 的 PTY 读线程、`ssh/session.rs` 的读循环在往前端发
  事件的同时，无条件调用 `state.agent.feed(id, data, kind)`。控制面的"读"完全可以复用
  `AppState.agent.snapshot(id)` / `.context(id, n)`，不需要另起一份缓冲。
- "枚举当前打开了哪些 session"目前完全不存在：标签 id 是前端铸造的不透明字符串
  （`tab-<ts>-<seq>`），后端 `LocalManager`/`SessionManager` 只把它当 `HashMap` key 用，
  没有任何 `list_ids()` 之类的方法——这是新增能力，不是复用。
- 详细架构方案见 `.claude/plans/harmonic-splashing-wirth.md`（如果本地还留着的话），
  核心包括：`control/` 新模块（protocol/server/auth）、`AgentManager` 拆分
  `enabled` 和 `control_plane_enabled` 两个开关、`nexus-cli` 作为同一 cargo package 下的
  第二个 `[[bin]]`、设置页「行为」弹窗里加开关 + token 展示。

**优先级**：低于"完善面板上的 Nexus Agent 本身能力"——先把单会话场景做扎实，再考虑跨会话编排。

## 面板 Nexus Agent 能力增强（进行中）

见对话记录；下一步是梳理现有 tool call 清单，决定要补齐哪些。

## 连接配置关联 TOTP 种子，二次验证自动填码

**现状**：连跳板机要走两道——先密码，再 keyboard-interactive 问 MFA 验证码。
后者每次都弹框等人手输，开一个标签输一次，频繁连同一台机器时很烦。

**外面的做法**：有人用 expect 脚本绕过去——`spawn ssh ...`，匹配到 `password:`
发密码，匹配到 `[OTP Code]:` 就 `exec oathtool --totp -b <种子>` 算一个发进去。
能跑，但要在客户端外面维护一份脆弱的提示语匹配，密码和种子还常常明文写在脚本里。
这件事放进客户端做更干净：我们本来就拿得到结构化的 prompt，不必猜。

**目标**：连接配置里可选地存一个 TOTP 种子（base32）；遇到二次验证提示时按 RFC 6238
本地算码并预填，用户确认或直接自动提交。只做标准 TOTP——Duo / RSA SecurID 这类
种子在厂商侧的拿不到，不在范围内。

**关键发现**（省去重新调研）：
- **接入点已经现成**：`ssh/session.rs` 的 KBI 循环（`Resp::InfoRequest` 分支，约
  1010-1045 行）已经有一套预填机制——`AuthPromptField { prompt, echo, prefill }`，
  当前只在 `asks_for_password()` 命中时用 `reusable_password()` 填密码。加 TOTP 就是
  在同一处多一条判断，填 `prefill`，前端那边一行不用改。
- **提示语判定可以照抄**：`asks_for_password()`（session.rs:821）就是
  `echo == false` 加几个关键词匹配。验证码正相反——服务端通常 `echo == true`
  （截图里 `257357` 是回显出来的），关键词 `otp` / `mfa` / `verification` /
  `one-time` / `验证码` / `动态码`。`echo` 这个信号比关键词可靠，先看它。
- **`saw_challenge` 的语义要跟着改**：现在的逻辑是「`prefill` 为空 ⇒ 这是挑战题」，
  一旦给验证码也填上 `prefill`，这个推断就废了，失败信息会从「验证码错误或已过期」
  退化成「密码不正确」。得把判定从「有没有预填」换成「这道题是不是密码题」。
- **种子存哪里是个要单独决定的事**：`store/mod.rs` 的 `migrate_legacy_secrets()`
  说明密码已经从系统凭据服务**迁出**、改存 db 明文列了。TOTP 种子跟着走最省事，
  但密码和种子躺在同一个明文文件里，二次验证就名存实亡——至少值得考虑种子单独
  回到 keyring（`keyring` crate 还在 Cargo.toml 里，三端都有）。
- 算码本身不用造轮子，Rust 侧有现成的 TOTP 库；注意剩余窗口不足两三秒时要等下一个
  窗口再发，否则刚算出来就过期。

**待定**：预填后要不要自动提交（省一次回车 vs 失手连错触发账号锁定）；多台机器共用
一个种子的情况要不要支持种子独立于连接存一份。

**优先级**：看实际使用频率。要连跳板机的人每天烦好几次，不连的人完全用不上。

## 广播输入：同一串按键同时发给多个标签

**现状**：键盘输入只进当前激活的那个标签。批量运维（同一条命令要在十台机器上跑一遍）
现在只能一个标签一个标签地粘贴，或者给每台机器各跑一次宏——宏是单目标的，
`App.tsx:549` 的 `runMacro(tabId, …)` 一次只认一个 `tabId`。

**目标**：开一个「广播」开关，选定一组已打开的标签，此后敲进去的字节同时发给这组里的
每一个。竞品（Voltius）把它和分屏绑在一起宣传，我们没有分屏，但广播本身不依赖分屏——
标签已经全部常驻，缺的只是扇出。

**关键发现**（省去重新调研）：
- **所有标签的终端实例都常驻，不是只有激活的那个**。`App.tsx:1506-1532` 用
  `tabs.map` 给每个标签各渲染一个 `<TerminalView>`，`active` 只是个 prop；
  `lib/termRegistry.ts` 的 `registerTerm(id, term)` 因此覆盖全部打开的标签，
  `getTerm(id)` 对后台标签同样拿得到。广播不需要先切过去再发。
- **前端扇出是现成的，后端扇出要新造**。写入口有两条且签名对称：
  `api.sshWrite(id, number[])`（api.ts:36 → `cmd.rs:169`）和
  `api.localWrite(id, number[])`（api.ts:140 → `cmd.rs:109`）；标签是哪一种在
  前端 state 里直接有（`TerminalTab.local`，types.ts:168）。所以前端循环目标 id
  各调一次 write 就能跑通，**后端一行不用改**。反过来如果要后端扇出，
  `agent/act.rs:1138` 的 `sink_of()` + `:1159` 的 `write_to()` 已经是一份
  「按 id 自动判 Local/Ssh 再写」的 kind 无关派发，照抄它新增一个收 `Vec<String>`
  的 command 即可——但那样前端仍要先把目标集合传下去，后端自己并不知道开了哪些标签
  （见上文控制面那条：`LocalManager`/`SessionManager` 只有 `contains()`，没有枚举）。
  **结论：先做前端扇出**，N 次 IPC 对人工键入的量级完全不是问题。
- **扇出点应该在 `TerminalView.tsx:722` 的 `handleInput()` 里，不要在 `App` 层另起一条路**。
  这个函数是「用户敲进来的字节」的唯一出口（`term.onData` 和 mac 接管层
  `lib/macInput` 两条路都汇到这里，注释写明了），在它里面扇出可以白捡三件事：
  `zmodem.isActive()` 的闸（:723，ZMODEM 传输中出站被协议帧占用，这时广播会直接
  毁掉传输）、`emitInput()` 旁路（宏录制靠它）、以及 `feedKeys`/命令历史的记账。
  绕开它从 App 层直接调 `api.sshWrite` 这三样全丢。
- **副本标签的本地回显/补全状态要想清楚**：`handleInput` 里 `lineRef` / `lineEchoedRef` /
  `recordCommand` 都是**源标签**的状态。广播只应把字节原样送进目标会话，
  不要去动目标 `TerminalView` 的行缓冲——目标终端自己会收到服务端回显并照常渲染。
- **UI 落点**：开关适合放 `components/TabBar.tsx`（已有标签右键菜单，`ctx` state +
  `onDuplicate`/`onDisconnect` 等条目，加「加入/移出广播组」最顺），全局默认行为放
  `components/SettingsDialogs.tsx:179` 的 `BehaviorDialog`「终端行为」段（:192）；
  持久化照 `lib/prefs.ts` 的 `Prefs`（:12，localStorage `nx.prefs`）加字段。
  快捷键走 `ShortcutAction`（prefs.ts:39）+ `SHORTCUT_LABELS`/`SHORTCUT_ORDER` 三处同步加一项。
- 标签 id 是前端铸造的 `tab-<ts>-<seq>`（`App.tsx:70`），广播组存一组 id 即可；
  标签关闭时要把 id 从组里摘掉，否则会往已消失的会话写（后端只会报
  「会话不存在」，不致命，但会刷一串 toast）。

**待定**：广播组是跟着标签走还是跟着连接走（重连后标签 id 不变，所以跟标签走更简单）；
要不要在广播生效时给目标标签加个显眼的视觉标记（不加很容易误操作到生产机）；
`Ctrl+C`、`Ctrl+D` 这类危险键要不要单独确认一次。

**优先级**：低于 TOTP 自动填码——TOTP 是每天都撞的摩擦，广播是偶发的批量场景；
但高于 Nexus 控制面：广播纯前端改动，一两天的量，控制面是新模块。

## 导入 SecureCRT / Termius 的连接配置

**现状**：只支持 Xshell `.xsh` 的导入导出（`src-tauri/src/xsh.rs`，498 行）和
PuTTY `.ppk` 私钥解析（`src-tauri/src/keys/ppk.rs`，882 行）。从 SecureCRT / Termius
迁过来的人只能一条条手敲。

**目标**：再支持两种来源的**连接配置**（主机/端口/用户名/分组/认证方式），
让从别家搬家这件事是一次文件选择而不是一下午。

**关键发现**（省去重新调研）：
- **现有导入的骨架很薄，照着加一种格式只动五处**。以 `.xsh` 为例：
  1. `xsh.rs` 里一个 `parse_files(paths: &[String]) -> Vec<ParsedFile>`（:322），
     `ParsedFile { label: String, result: Result<NewSession, String>, password: Option<String> }`（:314）——
     **解析层只产出 `NewSession` 和一个可选明文密码，不碰数据库**；
  2. `cmd.rs:446` 的 `import_xsh(paths, state) -> Result<Vec<ImportedFile>, String>`
     负责入库（`state.store.create()`）和存密码（`state.store.set_password()`），
     返回 `ImportedFile { label, ok, error, password_imported }`（cmd.rs:434，`serde(rename_all="camelCase")`）；
  3. `lib.rs:208-211` 的 `invoke_handler` 注册表加一行；
  4. `src/api.ts:84-88` 加一个 `invoke` 包装；
  5. 前端触发在 `App.tsx:1361` 的 `importXsh()`（`@tauri-apps/plugin-dialog` 的 `open({multiple:true})`
     选文件 → 调 api → `refresh()` → toast 汇总成功/失败条数），菜单项在
     `components/IOMenu.tsx:84`（`IOMenu` 挂在 `SessionList.tsx:416`）。
  新格式完全可以复用 2-5，只需要新写一个 `parse_files` 等价物。
- **`NewSession` 的字段就是导入能表达的全部**（`store/mod.rs:32-43`）：
  `name / folder / host / port / username / auth_type("password"|"key"|"agent") /
  key_path / startup_cmd / theme / connect_timeout`。没有「密码」字段——密码是
  入库拿到 id 之后单独 `set_password` 的。分组是扁平字符串路径（`.xsh` 导入统一塞
  `Some("/导入")`，`lib/folders.ts` 按 `/` 切层级建树），所以 SecureCRT 的
  Sessions 子目录、Termius 的 Group 都能直接映射成 `folder`。
- **重复导入会产生重复连接**：`store::create()`（store/mod.rs:242）是裸 `INSERT`，
  id 现铸 UUID，没有任何按 host+user 去重。`.xsh` 当初没管，新格式如果一次导入
  几十台，用户重跑一次就会翻倍——值得顺手加一个「已存在同 host+username 则跳过」。
- **导出不带凭据是既有决策，别破坏**：`encode_xsh`（xsh.rs:369）的签名只收
  `name / host / port / username / auth_type / key_path` 六个标量，**结构上就拿不到密码**，
  `render_xsh` 里 `Password=` 永远写空（:295）。三个导出 command（`export_xsh` /
  `export_sessions_xsh` / `export_all_xsh`，cmd.rs:475/490/524）都走这一个函数。
  本条只做导入，不要给导出开口子。
- **导入侧的凭据要按各自的加密方案单独判断，不能照搬 Xshell 的结论**。
  `xsh.rs:145` 的 `try_decrypt_password` 确实能解 Xshell 的密码，方案是
  `key = sha256(reverse(SID) + windows_username)`、`RC4(key, 明文) + sha256(明文)` 再 base64，
  但 `derive_xshell_key()` 只有 `#[cfg(windows)]` 实现（:173），非 Windows 直接返回 `None`（:192）——
  **机器绑定 + 账户绑定，只在原机原账户下有效**。这是 Xshell 方案的性质，不是通例。
- **SecureCRT（以下为网上查到的，需拿真实文件验证）**：会话在
  `%APPDATA%\VanDyke\Config\Sessions\*.ini`（一个会话一个文件，文件名即会话名），
  macOS/Linux 约为 `~/.vandyke/SecureCRT/Config/Sessions/`（路径未独立确认）。
  **它不是真正的 `[section]` INI**，而是扁平的带类型前缀的 `key=value`：
  `S:"Hostname"=1.2.3.4`、`D:"Port"=00000016`（Dword，**十六进制零填充**，0x16=22）、
  `S:"Username"=root`、`S:"Protocol Name"=SSH2`；`Z:` 是多值列表、`B:` 是二进制 blob
  （VanDyke 论坛的支持人员说 B 行可以整行省略，SecureCRT 会补默认值）。
  编码**疑似** UTF-16LE+BOM（旁证：社区解密脚本按 UTF-16 解密码字段）——
  这一条置信度不高，**拿到真实样本第一件事就是看头两个字节**，正好可以复用
  `xsh.rs:48` 的 `read_text()`（已按 BOM 处理 UTF-16LE/BE/UTF-8）。
  密码键名是 `S:"Password"`（旧）或 `S:"Password V2"`（7.3.3 起）：旧方案是两层
  Blowfish-CBC + 公开的硬编码密钥，新方案是 AES-256-CBC、全零 IV、
  `key = sha256(配置口令)`；**用户没设配置口令时 key 就是 `sha256("")`，可直接解**。
  有成熟的开源实现可对照（HyperSine/how-does-SecureCRT-encrypt-password 及其若干 fork）。
  和 Xshell 不同，这个方案**不绑机器**，所以 SecureCRT 的密码理论上是能跨机导入的——
  但「理论上能」不等于「应该默认做」，见下。
  另外 SecureCRT 自己有官方的 `Tools → Export Settings`，产出单个 XML
  （含全部会话与目录结构，有「是否包含个人配置数据」的勾选项），没有公开 schema。
  **让用户走这条官方导出、我们只解一个 XML，可能比去扒 Sessions 目录更省事**，
  值得先拿真实文件比一比再定。
- **Termius（信息互相矛盾，需拿真实导出验证）**：唯一**有官方文档**的导出是
  已归档的 `termius-cli` 的 `termius export-ssh-config`，产出标准 **SSH config 文本**
  （`./termius/sshconfig`）——按定义只有 host/port/user/跳板信息，**不含密码和私钥**。
  桌面版 GUI 是否能导出 JSON/CSV，一堆二手博客说能，但多个第三方工具
  （termius-exporter 等）的作者明确说原生导出拿不到数据、他们才去直接读本地
  Electron/IndexedDB 加密库；评测站也反复出现「没有导入导出」「备份同步要付费」的抱怨。
  **结论：先只支持 SSH config 这一条路**（它同时还能白捡 OpenSSH `~/.ssh/config` 的导入），
  JSON/CSV 等拿到真实导出文件再说。Termius 的概念模型是
  Vaults → Groups → Hosts + 可复用的 Identities（用户名/密码/密钥打包），
  Group 映射到我们的 `folder`，Identity 要摊平到每条连接上。
- **私钥这块已经有半套**：`keys/ppk.rs` 的 `looks_like_ppk(text)`（:42）是现成的
  格式嗅探先例，`parse(text, passphrase)`（:78）返回 `PrivateKey`；
  两家导出里如果带 OpenSSH 格式私钥，走 `keys/mod.rs` 的 `import_key` 即可，不用新写解析。

**待定**：SecureCRT 的密码既然可解又不绑机器，要不要默认解——倾向于**默认不导入密码**，
在导入对话框里给一个显式勾选（「尝试导入已保存的密码」）并说明风险，
理由是现在密码落在 db 明文列里（见 TOTP 那条的 `migrate_legacy_secrets()`），
悄悄把别家的密码批量搬进一个明文文件不是好默认值。还有：优先做 Sessions 目录还是
官方 XML 导出；要不要顺带支持 OpenSSH `~/.ssh/config`（Termius 那条路做完基本就免费送了）。

**优先级**：低于广播输入。导入是**一次性**动作——搬完家就再也用不到，而广播是天天用的；
但它的获客价值高（竞品把「一键从 Termius/PuTTY/SecureCRT 导入」当卖点打），
且骨架已经铺好、增量小。真正的瓶颈不是写代码，是**拿到两家的真实样本文件**——
在拿到之前上面标了「需验证」的结论一条都不要当真。
