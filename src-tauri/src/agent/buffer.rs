//! 终端原始流的旁路缓冲与清洗。
//!
//! 人在屏幕上看到的画面和喂给模型的文本是两回事：pty 出来的字节流里混着
//! 颜色、光标定位、清屏这些转义序列，`\r` 刷新的进度条一行能刷出几百次。
//! 直接喂给 LLM，几 MB 上下文里九成是垃圾，还白白烧 token。
//!
//! 又因为这些序列在终端有着确定的语义（"移动到 12 行 3 列"而不是文本），
//! 能不能安全剥离不取决于"看起来像不像"，取决于它对屏幕上最终画面的作用：
//! 剥离它们丢掉的只是「颜色 / 位置」，留下的是字符本身 —— 这条边界可以放心自动化。

use std::collections::VecDeque;

/// 单会话默认的缓冲上限（字节）。
///
/// 32KB 够不够要看场景：一次 `ls -la` 几百行大约 8KB，但 `npm install`、
/// `docker build` 轻松突破几百 KB。取 256KB 是「能装下几分钟真实操作」与
/// 「不会因为一个失控的 `yes | dd` 把内存吃光」之间的折中。
pub const DEFAULT_CAPACITY: usize = 256 * 1024;

/// 单段上限。超过就另起一段，避免长时间不间断输出让单段无限膨胀 ——
/// 那样每次从头部 trim 都要搬动整块内存。
const MAX_CHUNK_BYTES: usize = 64 * 1024;

/// 段数上限，防止 In/Out 高频交替时段数线性增长（每个按键都可能自成一段）。
const MAX_CHUNKS: usize = 128;

/// 待抵消回显队列的上限（字节）。
///
/// 人敲进去的字节在被 pty 回显之前先堆在 `pending_echo` 里，正常情况下
/// 几毫秒就被随后的输出抵消掉；只有「程序关掉了回显」或「输入没被回显」
/// 才会滞留。设上限是为了防止一大段粘贴 / 恶意输入把队列堆死。
/// 超限即整体作废 —— 代价是快照里可能出现一次重复，但缓冲不可能因此膨胀。
const MAX_PENDING_ECHO: usize = 4096;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// 旁路流的来源：区分「程序输出」和「人敲进去的输入」。
/// Agent 需要知道人执行过什么命令，而不只是看到一堆输出。
///
/// 两个变体的构造点：`StreamKind::In` 在 cmd.rs 的四个写入命令里，
/// `StreamKind::Out` 在 ssh/session.rs 的 read_loop 与 local/mod.rs 的读线程里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// 程序（远端 / PTY）写到终端上的东西
    Out,
    /// 人从键盘打进去的东西
    In,
}

/// 缓冲状态，直接序列化给前端做「已丢弃 N 字节」提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BufferStats {
    /// 当前保留的字节数
    pub bytes: usize,
    /// 累计因超出容量被丢掉的字节数
    pub dropped: u64,
    /// 曾经超限丢弃过内容 → 首尾都不完整，模型看不到 session 的全貌
    pub truncated: bool,
}

/// 一段同源（同 StreamKind）的数据。
///
/// `VecDeque` 而不是 `Vec`：环形缓冲的超限要从头部丢弃，
/// `Vec::remove(0)` 是 O(n)，每个 8KB 读包摊一次就够疼的了。
struct Chunk {
    kind: StreamKind,
    data: VecDeque<u8>,
}

impl Chunk {
    /// lossy 解码 + 剥离 ANSI，得到可以当模型输入的纯文本。
    fn text(&self) -> String {
        let (head, tail) = self.data.as_slices();
        // VecDeque 是环形数组，两段切片都不保证覆盖完整内容，
        // 且转义序列可能正好横跨接缝 —— 必须先拼成连续内存再解码。
        let mut bytes = Vec::with_capacity(head.len() + tail.len());
        bytes.extend_from_slice(head);
        bytes.extend_from_slice(tail);
        strip_ansi(&String::from_utf8_lossy(&bytes))
    }
}

/// 单个会话的环形缓冲：吞进 pty 原始字节，吐出可读的模型上下文。
pub struct SessionBuffer {
    chunks: VecDeque<Chunk>,
    capacity: usize,
    bytes: usize,
    dropped: u64,
    /// 已喂进输入段、但还没在输出流里等到对应回显的字节。
    ///
    /// pty 处于 canonical 模式时会把人敲的每个字符回显一遍，于是同一段文本
    /// 会同时出现在 In（我们喂进去的）和 Out（pty 读回来的）里；两边都收的话
    /// 快照里每条命令都会重复两遍。用它把输出流开头的那截回显减掉。
    pending_echo: VecDeque<u8>,
    /// 本会话的旁路开关。放在这里而不是 manager 里再开一张表，
    /// 是为了让「缓冲存在」和「是否启用」永远同步，不会出现半初始化状态。
    enabled: bool,
}

impl SessionBuffer {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            capacity,
            bytes: 0,
            dropped: 0,
            pending_echo: VecDeque::new(),
            enabled: true,
        }
    }

    /// 按全局 autoAttach 建立一个新缓冲（未显式登记的会话走这条路径）。
    pub fn with_attach(enabled: bool) -> Self {
        let mut b = Self::new();
        b.enabled = enabled;
        b
    }

    /// 旁路开关。关闭时 [`AgentManager::feed`] 会在此之前就短路掉。
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// 追加一段数据。超出容量时从最老的字节开始丢，并累计丢弃量。
    ///
    /// 永不失败、永不分配失败传播：调用方（pty 读线程）会在 hot path 上调用它，
    /// 任何 panic 都会直接炸掉整个终端。
    pub fn feed(&mut self, data: &[u8], kind: StreamKind) {
        if data.is_empty() {
            return;
        }
        match kind {
            StreamKind::In => self.feed_in(data),
            StreamKind::Out => self.feed_out(data),
        }
    }

    /// 人的输入：无条件记进输入段（这是「人执行过什么命令」的唯一来源，
    /// 与回显是否真的出现无关），同时挂进待抵消队列等回显。
    fn feed_in(&mut self, data: &[u8]) {
        self.push(data, StreamKind::In);
        self.trim();

        self.pending_echo.extend(data.iter().copied());
        if self.pending_echo.len() > MAX_PENDING_ECHO {
            // 超限：整体作废。残留只是让下一条命令在快照里重复一次，
            // 而让它继续堆下去会吃光内存。真正的安全阀是 `cancel_echo` 里
            // 「一个字节都对不上就清空」那条规则 —— 作废后不会再误吃输出。
            self.pending_echo.clear();
        }
    }

    /// 程序输出：先拿待抵消队列削掉开头的回显，剩下的才进输出段。
    fn feed_out(&mut self, data: &[u8]) {
        let echoed = self.cancel_echo(data);
        let rest = &data[echoed..];
        if rest.is_empty() {
            // 整段都是回显：一个字节都不该进输出段
            return;
        }
        self.push(rest, StreamKind::Out);
        self.trim();
    }

    /// 用待抵消队列吃掉 `data` 开头的回显，返回被吃掉的字节数。
    fn cancel_echo(&mut self, data: &[u8]) -> usize {
        if self.pending_echo.is_empty() {
            return 0;
        }
        let (data_len, echo_len) = echo_prefix_len(&self.pending_echo, data);
        if echo_len == 0 {
            // 一个字节都对不上 → 这个程序关掉了回显（vim / top / 密码提示 /
            // 远端切了 raw 模式）。必须清空：过期回显如果留到下一次 feed_out，
            // 极可能正好和无关输出的前几个字节撞上，把正常输出啃掉一块。
            // 宁可让这一条命令在快照里重复，也不能让输出凭空少一段。
            self.pending_echo.clear();
            return 0;
        }
        drop(self.pending_echo.drain(..echo_len));
        data_len
    }

    fn push(&mut self, data: &[u8], kind: StreamKind) {
        let mut rest = data;
        while !rest.is_empty() {
            // 末尾同类型且还有余位就续写，减少段数。
            // 一次 feed 通常只有几 KB，绝大多数情况一轮就结束。
            let room = match self.chunks.back() {
                Some(c) if c.kind == kind => MAX_CHUNK_BYTES.saturating_sub(c.data.len()),
                _ => 0,
            };
            if room == 0 {
                let cap = rest.len().min(MAX_CHUNK_BYTES);
                self.chunks.push_back(Chunk {
                    kind,
                    data: VecDeque::with_capacity(cap),
                });
                continue;
            }
            let take = room.min(rest.len());
            let Some(back) = self.chunks.back_mut() else {
                // 不可能：room > 0 意味着 back 存在。宁可丢数据也不 panic。
                self.dropped += rest.len() as u64;
                return;
            };
            back.data.extend(rest[..take].iter().copied());
            self.bytes += take;
            rest = &rest[take..];
        }
        self.cap_chunks();
    }

    fn trim(&mut self) {
        while self.bytes > self.capacity {
            let Some(front) = self.chunks.front_mut() else {
                break;
            };
            let need = self.bytes - self.capacity;
            let take = need.min(front.data.len());
            front.data.drain(..take);
            self.bytes -= take;
            self.dropped += take as u64;
            if front.data.is_empty() {
                self.chunks.pop_front();
            }
        }
    }

    /// 段数封顶：合并最旧的两段。
    ///
    /// 取舍：合并会让较新的一段继承较老那段的 kind —— 在 In/Out 极限交替时，
    /// 边界标志位可能偏移一段。之所以接受这点误差，是因为被合并的两段位于
    /// 缓冲最前端，很快就会被 `trim` 彻底丢掉；相比之下任由段数增长，
    /// 每个按键都留下一个 VecDeque  allocations，长会话会攒出成千上万个。
    fn cap_chunks(&mut self) {
        while self.chunks.len() > MAX_CHUNKS {
            let Some(oldest) = self.chunks.pop_front() else {
                break;
            };
            let Some(second) = self.chunks.front_mut() else {
                // 只剩一段，放回去即可
                self.chunks.push_front(oldest);
                break;
            };
            let mut merged = Vec::with_capacity(oldest.data.len() + second.data.len());
            merged.extend(oldest.data.iter().copied());
            merged.extend(second.data.iter().copied());
            second.data = merged.into_iter().collect();
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// 完整的可读会话记录：人的输入行带 `$ ` 前缀。
    ///
    /// 只给 UI 展示 / 调试用，实际喂模型走 [`Self::take_tail`]。
    pub fn snapshot(&self) -> String {
        let mut out = String::with_capacity(self.bytes);
        let mut at_line_start = true;
        for chunk in &self.chunks {
            push_transcript(&mut out, &chunk.text(), chunk.kind, &mut at_line_start);
        }
        out
    }

    /// 只取最近 `max_bytes`（按清洗后的字节计）的内容 —— **这是实际发给模型的东西**。
    ///
    /// 为什么不清全量再截：长会话的缓冲清一遍要扫满 256KB，
    /// 而这个方法在持锁的情况下执行，多余的开销会拖慢 pty 读线程。
    /// 从尾部倒着走，够 `max_bytes` 就停，实际只读到最后一两段。
    pub fn take_tail(&self, max_bytes: usize) -> String {
        if max_bytes == 0 {
            return String::new();
        }
        // 倒序选取，够用即停
        let mut picked: Vec<(StreamKind, String)> = Vec::new();
        let mut total = 0usize;
        for chunk in self.chunks.iter().rev() {
            let text = chunk.text();
            total += text.len();
            picked.push((chunk.kind, text));
            if total >= max_bytes {
                break;
            }
        }

        let mut out = String::with_capacity(total.min(max_bytes * 2).max(64));
        let mut at_line_start = true;
        for (kind, text) in picked.into_iter().rev() {
            push_transcript(&mut out, &text, kind, &mut at_line_start);
        }
        trim_front(&out, max_bytes)
    }

    /// 清空并归零统计（用户手动「遗忘上下文」时调用）。
    pub fn clear(&mut self) {
        self.chunks.clear();
        self.bytes = 0;
        self.dropped = 0;
        // 一并作废：清空的语义是「忘掉刚才的一切」，留着回显队列会让
        // 清空之后的第一段输出被无端削掉一截
        self.pending_echo.clear();
    }

    pub fn stats(&self) -> BufferStats {
        BufferStats {
            bytes: self.bytes,
            dropped: self.dropped,
            // truncated 表达的不是「这次取尾被截了」（那由 max_bytes 天然决定），
            // 而是「这张记录本身已经不完整」——UI 据此提示用户上下文有缺口。
            truncated: self.dropped > 0,
        }
    }
}

impl Default for SessionBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// 待抵消回显与输出流的最长公共前缀。
///
/// 返回 `(消耗的 data 字节数, 消耗的回显字节数)` —— 两个数可能不等，
/// 因为 pty 回显的行尾是 `\r\n`，而人敲下去的是 `\n`：比对时把 data 里的
/// `\r\n` 视作一个 `\n`，否则每条命令后面都会多出一个空行。
/// 只在这一步归一，真正进缓冲的仍是原始字节。
fn echo_prefix_len(pending: &VecDeque<u8>, data: &[u8]) -> (usize, usize) {
    let mut di = 0usize; // data 下标
    let mut ei = 0usize; // 回显队列下标
    while di < data.len() && ei < pending.len() {
        let want = pending[ei];
        let got = data[di];
        // 回显把一个 `\n` 画成了 `\r\n`：算作同一个字节，但吃掉两个
        if want == b'\n' && got == b'\r' && data.get(di + 1) == Some(&b'\n') {
            ei += 1;
            di += 2;
            continue;
        }
        if want != got {
            break;
        }
        ei += 1;
        di += 1;
    }
    (di, ei)
}

/// 把一段清洗后的文本按 kind 拼进会话记录，`at_line_start` 跨段维持。
fn push_transcript(out: &mut String, text: &str, kind: StreamKind, at_line_start: &mut bool) {
    if text.is_empty() {
        return;
    }
    match kind {
        StreamKind::Out => {
            out.push_str(text);
            *at_line_start = text.ends_with('\n');
        }
        StreamKind::In => {
            // 人的输入逐字符回放：每到一个行首就补 `$ `。
            // 用字符循环而不是 split('\n')，是因为 split 会在尾部产生空串，
            // 从而给还没写出来的下一行提前打上 `$ `。
            for ch in text.chars() {
                if *at_line_start {
                    out.push_str("$ ");
                    *at_line_start = false;
                }
                out.push(ch);
                if ch == '\n' {
                    *at_line_start = true;
                }
            }
        }
    }
}

/// 从头部裁到只剩 `max_bytes`，并尽量对齐到整行开头。
fn trim_front(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = text.len() - max_bytes;
    // 挪到 UTF-8 字符边界，否则切片会 panic（release profile 是 panic = abort）
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    let boundary = start;
    // 半行残文对模型没有意义，还会把句子从中间劈开，故再往下一个换行处推进
    if let Some(rel) = text[start..].find('\n') {
        start += rel + 1;
        // 极端情况：换行恰好在末尾，整段就被吃掉了 —— 退回字符边界，宁可半行也不要空
        if start >= text.len() {
            start = boundary;
        }
    }
    text[start..].to_string()
}

/// 把终端字节流洗成给 LLM 看的纯文本。
///
/// **关于 `\r` 的策略**：`\r\n` 归一成 `\n`；孤立的 `\r` 一律丢弃。
///
/// 选丢弃而不是翻译成换行的原因：`\r` 在终端里是「回到行首重画」，
/// `docker pull` / `apt-get` 这类进度条一次会话能刷几百上千次。
/// 翻译成换行会得到几百行近乎重复的文本，token 翻十倍而信息量不变；
/// 丢弃则把最终态压缩在同一行（`Progress: 10%Progress: 20%...100%`），
/// 模型依然读得出结论，代价只是可读性稍差。对「喂上下文」这个用途来说，
/// 这笔账是划算的。若将来要给人看的版式，可以另开一个开关。
///
/// 纯函数：输入是 `&str`（保证合法 UTF-8），输出只会少字符不会多，无 panic。
pub fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            ESC => i = skip_escape(bytes, i),
            b'\r' => {
                if bytes.get(i + 1) == Some(&b'\n') {
                    out.push(b'\n');
                    i += 2;
                } else {
                    // 孤立 \r：行内重画，丢弃
                    i += 1;
                }
            }
            b'\n' | b'\t' => {
                out.push(bytes[i]);
                i += 1;
            }
            c if c < 0x20 || c == 0x7f => {
                // 其余控制字符（响铃、退格、垂直制表…）一律丢弃
                i += 1;
            }
            _ => {
                // 普通字符一次性搬运（含 UTF-8 多字节序列），比逐字节 push 快得多
                let start = i;
                while i < bytes.len() {
                    let c = bytes[i];
                    if c == ESC || c == b'\r' || c == b'\n' || c == b'\t' || c < 0x20 || c == 0x7f {
                        break;
                    }
                    i += 1;
                }
                out.extend_from_slice(&bytes[start..i]);
            }
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

/// 跳过从 `i` 开始的一条转义序列，返回下一个待处理字节的下标。
fn skip_escape(bytes: &[u8], i: usize) -> usize {
    let Some(&next) = bytes.get(i + 1) else {
        // 流在此截断：把孤立的 ESC 自己丢掉
        return i + 1;
    };
    match next {
        b'[' => skip_csi(bytes, i + 2),
        // OSC / DCS / PM / APC：字符串型序列，以 BEL 或 ST 收尾
        b']' | b'P' | b'^' | b'_' => skip_string(bytes, i + 2),
        // 字符集选择序列（如 ESC ( B）固定三字节
        b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' => (i + 3).min(bytes.len()),
        // 后面跟着控制字符或非 ASCII：不是合法序列，只丢 ESC，让外层重新处理
        c if c.is_ascii_control() || !c.is_ascii() => i + 1,
        // 标准两字符 ESC 序列（如 ESC 7 存光标、ESC c 复位）
        _ => i + 2,
    }
}

/// CSI：`ESC [` + 参数/中间字节(0x20–0x3F) + 终字节(0x40–0x7E)
fn skip_csi(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        match bytes[i] {
            0x20..=0x3f => i += 1,
            0x40..=0x7e => return i + 1,
            // 非法字节：多半是流被截断了。就此打住而不是吞到行尾，
            // 否则一处截断会让后面几十行正文凭空消失
            _ => return i,
        }
    }
    bytes.len()
}

/// 字符串型序列：一路吃到 BEL(0x07) 或 ST(`ESC \`)。
fn skip_string(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        match bytes[i] {
            BEL => return i + 1,
            ESC if bytes.get(i + 1) == Some(&b'\\') => return i + 2,
            // 序列内又冒出一个没有 ST 的 ESC：视为本序列已终止，
            // 把位置停在 ESC 上让外层按新序列解析（不递归，栈深度恒定）
            ESC => return i,
            _ => i += 1,
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CSI: &str = "\u{1b}[";

    #[test]
    fn strips_sgr_colors() {
        assert_eq!(strip_ansi(&format!("{CSI}31mhello{CSI}0m")), "hello");
        assert_eq!(
            strip_ansi(&format!("{CSI}1;38;5;208mwarn{CSI}0m\n")),
            "warn\n"
        );
    }

    #[test]
    fn strips_cursor_and_erase() {
        assert_eq!(strip_ansi("\u{1b}[2J\u{1b}[H\u{1b}[Kprompt"), "prompt");
        assert_eq!(strip_ansi(&format!("a{CSI}10Db")), "ab");
    }

    #[test]
    fn strips_osc_title_bel_and_st() {
        assert_eq!(strip_ansi("\u{1b}]0;my shell\u{07}ls\n"), "ls\n");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{1b}\\ls\n"), "ls\n");
        // 未终止的 OSC：吞到串尾，但不吃掉正文里后来的换行以外的东西
        assert_eq!(strip_ansi("\u{1b}]0;unterminated"), "");
    }

    #[test]
    fn strips_two_char_escape_sequences() {
        assert_eq!(strip_ansi("\u{1b}M\u{1b}7\u{1b}cx"), "x");
        // 字符集选择序列 ESC ( B
        assert_eq!(strip_ansi("\u{1b}(Bok"), "ok");
    }

    #[test]
    fn lone_escape_at_end_is_dropped() {
        assert_eq!(strip_ansi("abc\u{1b}"), "abc");
        assert_eq!(strip_ansi("abc\u{1b}["), "abc");
    }

    #[test]
    fn crlf_normalizes_to_lf() {
        assert_eq!(strip_ansi("a\r\nb\r\n"), "a\nb\n");
    }

    /// `\r` 刷新：丢弃而不是换行，否则进度条会炸成几百行重复文本
    #[test]
    fn lone_cr_is_dropped() {
        assert_eq!(strip_ansi("a\rb"), "ab");
        assert_eq!(
            strip_ansi("Progress: 10%\rProgress: 20%\rProgress: 100%"),
            "Progress: 10%Progress: 20%Progress: 100%"
        );
    }

    #[test]
    fn keeps_tab_and_newline_but_drops_other_controls() {
        assert_eq!(strip_ansi("a\tb\nc"), "a\tb\nc");
        assert_eq!(strip_ansi("\u{07}\u{00}\u{08}\u{0b}clean\u{7f}"), "clean");
    }

    #[test]
    fn multibyte_utf8_survives() {
        assert_eq!(strip_ansi("中文 ñ 🎉"), "中文 ñ 🎉");
        assert_eq!(strip_ansi(&format!("{CSI}32m中文{CSI}0m")), "中文");
    }

    /// release profile 是 panic = "abort"，任何 panic 直接等于进程死掉
    #[test]
    fn never_panics_on_adversarial_input() {
        for len in 0..40usize {
            let raw: Vec<u8> = (0..len)
                .map(|i| [ESC, b'[', b']', b'0' + (i % 10) as u8, b'\r', b'\n', 0xff][i % 7])
                .collect();
            let text = String::from_utf8_lossy(&raw);
            let out = strip_ansi(&text);
            assert!(out.len() <= text.len(), "输出不该比输入长");
        }
    }

    fn buf(capacity: usize) -> SessionBuffer {
        SessionBuffer::with_capacity(capacity)
    }

    #[test]
    fn input_lines_get_dollar_prefix() {
        let mut b = buf(1024);
        b.feed(b"hello from shell\n", StreamKind::Out);
        b.feed(b"ls -la\n", StreamKind::In);
        b.feed(b"total 8\n", StreamKind::Out);
        assert_eq!(b.snapshot(), "hello from shell\n$ ls -la\ntotal 8\n");
    }

    #[test]
    fn input_without_newline_no_trailing_prefix() {
        let mut b = buf(1024);
        b.feed(b"abc", StreamKind::In);
        b.feed(b"def", StreamKind::In);
        assert_eq!(b.snapshot(), "$ abcdef");
    }

    #[test]
    fn input_after_echoed_prompt_is_not_prefixed() {
        // pty 会把人敲的东西回显成 Out，此时不能再补 `$ `
        let mut b = buf(1024);
        b.feed(b"user@host:~$ ", StreamKind::Out);
        b.feed(b"pwd\n", StreamKind::In);
        assert_eq!(b.snapshot(), "user@host:~$ pwd\n");
    }

    #[test]
    fn consecutive_same_kind_coalesces() {
        let mut b = buf(1024);
        for _ in 0..100 {
            b.feed(b"x", StreamKind::In);
        }
        assert_eq!(b.chunks.len(), 1);
        assert_eq!(b.snapshot(), format!("$ {}", "x".repeat(100)));
    }

    #[test]
    fn overflow_drops_from_head_and_counts() {
        let mut b = buf(8);
        b.feed(b"12345678", StreamKind::Out);
        assert_eq!(b.stats().bytes, 8);
        assert_eq!(b.stats().dropped, 0);

        b.feed(b"90", StreamKind::Out);
        let s = b.stats();
        assert_eq!(s.bytes, 8);
        assert_eq!(s.dropped, 2);
        assert!(s.truncated);
        assert_eq!(b.snapshot(), "34567890");
    }

    #[test]
    fn huge_single_feed_is_trimmed() {
        let mut b = buf(16);
        b.feed(&vec![b'z'; 1000], StreamKind::Out);
        let s = b.stats();
        assert_eq!(s.bytes, 16);
        assert_eq!(s.dropped, 984);
    }

    #[test]
    fn chunk_count_is_bounded_under_alternation() {
        let mut b = buf(usize::MAX);
        for i in 0..MAX_CHUNKS * 4 {
            b.feed(b"x", if i % 2 == 0 { StreamKind::In } else { StreamKind::Out });
        }
        assert!(b.chunks.len() <= MAX_CHUNKS);
        // 合并后仍保持时序：老数据在前
        let text = b.snapshot();
        assert!(text.contains('x'));
    }

    #[test]
    fn take_tail_keeps_only_recent_bytes() {
        let mut b = buf(64 * 1024);
        b.feed(b"0000000000\n", StreamKind::Out);
        b.feed(b"1111111111\n", StreamKind::Out);
        b.feed(b"tail-marker\n", StreamKind::Out);
        let tail = b.take_tail(30);
        assert!(tail.len() <= 30, "实际 {} 字节", tail.len());
        assert!(tail.contains("tail-marker"), "尾部内容丢失：{tail:?}");
        assert!(!tail.contains("0000000000"), "不该残留最老的内容：{tail:?}");
    }

    #[test]
    fn take_tail_snaps_to_line_boundary() {
        let mut b = buf(64 * 1024);
        b.feed(b"HEADER0123456789\n", StreamKind::Out);
        b.feed(b"secondline\n", StreamKind::Out);
        // 11 字节刚好落在新行开始，cut 点落在 "secondline" 中间时应跳到下行首
        let tail = b.take_tail(11);
        assert_eq!(tail, "secondline\n");
    }

    #[test]
    fn take_tail_zero_is_empty() {
        let mut b = buf(1024);
        b.feed(b"data", StreamKind::Out);
        assert_eq!(b.take_tail(0), "");
    }

    #[test]
    fn take_tail_respects_input_prefix_on_tail() {
        let mut b = buf(1024);
        b.feed(b"out-a\n", StreamKind::Out);
        b.feed(b"cat flag\n", StreamKind::In);
        let tail = b.take_tail(64);
        assert_eq!(tail, "out-a\n$ cat flag\n");
    }

    #[test]
    fn multibyte_boundary_is_never_split() {
        let mut b = buf(64 * 1024);
        // 中文每个字 3 字节，裁切点几乎必然落在多字节序列中间
        b.feed("一二三四五六七八九十".as_bytes(), StreamKind::Out);
        let max = 7; // 恰好切在第二个字中间
        let tail = b.take_tail(max);
        assert!(tail.chars().all(|c| !c.is_ascii() || c == '\n'));
        assert!(tail.len() <= 7 + 3);
    }

    #[test]
    fn clear_resets_everything() {
        let mut b = buf(4);
        b.feed(b"abcdefgh", StreamKind::Out);
        assert!(b.stats().dropped > 0);
        b.clear();
        let s = b.stats();
        assert_eq!(s.bytes, 0);
        assert_eq!(s.dropped, 0);
        assert!(!s.truncated);
        assert_eq!(b.snapshot(), "");
    }

    #[test]
    fn ansi_is_stripped_before_context_size() {
        // 含大量转义的输入，统计 bytes 用原始字节，tail 用清洗后字节
        let mut b = buf(64 * 1024);
        b.feed(
            format!("{CSI}32mgreen-text{CSI}0m\n").as_bytes(),
            StreamKind::Out,
        );
        // 20 = ESC[32m(5) + green-text(10) + ESC[0m(4) + \n(1)
        assert_eq!(b.stats().bytes, 20);
        assert_eq!(b.take_tail(64), "green-text\n");
    }

    // ——— 回显抑制 ———
    // pty 在 canonical 模式下会把人敲的每个字符回显一遍，于是同一段文本
    // 同时出现在 In 和 Out 里。下面这组测试保证回显被抵消，
    // 且「过期回显」绝不会去吃后面无关的输出。

    #[test]
    fn full_echo_is_cancelled() {
        let mut b = buf(1024);
        b.feed(b"ls\n", StreamKind::In);
        // pty 的回显行尾是 \r\n，后面才是真正的输出
        b.feed(b"ls\r\nfile1  file2\r\n", StreamKind::Out);
        assert_eq!(b.snapshot(), "$ ls\nfile1  file2\n");
    }

    #[test]
    fn echo_is_cancelled_across_chunks() {
        // xterm.js 是逐字符发送按键的：In 与回显都切成碎片到达
        let mut b = buf(1024);
        for c in b"ls\n" {
            b.feed(&[*c], StreamKind::In);
        }
        b.feed(b"l", StreamKind::Out);
        b.feed(b"s", StreamKind::Out);
        b.feed(b"\r\n", StreamKind::Out);
        b.feed(b"file1\r\n", StreamKind::Out);
        assert_eq!(b.snapshot(), "$ ls\nfile1\n");
    }

    #[test]
    fn partial_echo_leaves_the_remainder() {
        // 回显被切成两截到达：先到的 "ma" 整段都是回显，一个字节都不该进输出段
        let mut b = buf(1024);
        b.feed(b"make\n", StreamKind::In);
        b.feed(b"ma", StreamKind::Out);
        assert_eq!(b.snapshot(), "$ make\n");
        // 剩下的 ke\n 还在队列里，下一段输出继续抵消
        b.feed(b"ke\r\n", StreamKind::Out);
        b.feed(b"gcc: ok\n", StreamKind::Out);
        assert_eq!(b.snapshot(), "$ make\ngcc: ok\n");
    }

    /// 关掉回显的程序（vim / top / 密码提示）：一个字节都对不上 → 队列必须清空，
    /// 否则过期回显会去啃下一段无关输出的开头。
    #[test]
    fn no_echo_clears_pending_instead_of_eating_output() {
        let mut b = buf(1024);
        b.feed(b"sudo x\n", StreamKind::In);
        // 首个输出字节与待抵消的 's' 不符 → 判定该程序关闭了回显
        b.feed(b"[sudo] password: ", StreamKind::Out);
        // 关键：随后的输出即使正好以同样的字节开头，也不能被啃掉
        b.feed(b"sudo x\n", StreamKind::Out);
        assert_eq!(b.snapshot(), "$ sudo x\n[sudo] password: sudo x\n");
    }

    #[test]
    fn stale_echo_never_eats_later_output() {
        let mut b = buf(1024);
        b.feed(b"top\n", StreamKind::In);
        // top 切进 alternate screen，第一个字节就是 ESC
        b.feed(b"\x1b[?1049h", StreamKind::Out);
        // 队列已清空，之后任何输出都原样保留
        b.feed(b"top - 12:00:00\n", StreamKind::Out);
        assert_eq!(b.snapshot(), "$ top\ntop - 12:00:00\n");
    }

    /// 粘贴一大段文本时队列不能无限堆积
    #[test]
    fn pending_echo_is_capped() {
        let mut b = buf(1024 * 1024);
        let pasted = vec![b'a'; MAX_PENDING_ECHO + 100];
        b.feed(&pasted, StreamKind::In);
        assert!(
            b.pending_echo.is_empty(),
            "超限后应整体作废，实际残留 {} 字节",
            b.pending_echo.len()
        );
        // 作废后输出不再被抵消：这段 "aaa" 会原样留在快照尾部
        b.feed(b"aaa", StreamKind::Out);
        assert_eq!(
            b.snapshot(),
            format!("$ {}aaa", "a".repeat(MAX_PENDING_ECHO + 100))
        );
    }

    #[test]
    fn echo_cancel_is_independent_of_capacity_trim() {
        // 输入段被容量裁掉不影响回显抵消：两条路径各自记账，互不依赖
        let mut b = buf(4);
        b.feed(b"abcd\nefgh\n", StreamKind::In);
        assert!(b.stats().truncated, "容量 4 必然发生丢弃");
        // 整段都是回显 → 输出段一个字节都不该进
        b.feed(b"abcd\nefgh\n", StreamKind::Out);
        assert_eq!(b.stats().bytes, 4, "回显不该残留成输出");
        assert_eq!(b.snapshot(), "$ fgh\n");
    }

    #[test]
    fn clear_drops_pending_echo() {
        let mut b = buf(1024);
        b.feed(b"ls\n", StreamKind::In);
        b.clear();
        // 清空后不再有回显可抵消：这段输出必须原样进来
        b.feed(b"ls\r\n", StreamKind::Out);
        assert_eq!(b.snapshot(), "ls\n");
    }

    #[test]
    fn enabled_flag_round_trips() {
        let mut b = SessionBuffer::with_attach(true);
        assert!(b.is_enabled());
        b.set_enabled(false);
        assert!(!b.is_enabled());
    }
}
