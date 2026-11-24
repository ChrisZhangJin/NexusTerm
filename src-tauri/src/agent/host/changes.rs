//! 「实际改动」：从一次托管的决策记录（真正执行了的命令 / 文件写入）里提取事实性的改动摘要。
//!
//! 只看动作本身（`chosen.kind` + `chosen.text`），**不看模型怎么说**。纯函数，可单测：
//! - 文件：nx_write / nx_edit 的路径、`tee` / `>` / `>>` 重定向的目标、`sed -i` 的文件；
//! - 软件包：apt / apt-get / yum / dnf / apk / brew / snap / pip / npm / pnpm / yarn / cargo 的装 / 卸（名字过一遍脱敏，
//!   `git+https://user:TOKEN@…` 这类安装串不原样留）；
//! - 服务：systemctl / service 的 start / stop / restart / reload / enable / disable / mask；
//! - 端口：ufw allow / deny / delete、firewall-cmd --add-port / --remove-port / --add-service、
//!   iptables -A / -I / -D … --dport、-P 默认策略、docker -p；
//! - 容器：docker / podman run / stop / start / restart / pull / build、compose up / down；
//! - 删除：rm / rmdir 的目标（去重、最多 [`MAX_REMOVALS`] 条）、docker rm / rmi / prune、iptables -F / -X、userdel
//!   （apt / yum 的 remove / purge 仍归软件包）；
//! - 下载：curl -o / -O / 重定向，wget（默认存成远端文件名，-O 指定）。
//!
//! 一行里的 `&&` `||` `;` `|` `&`、换行（nx_run 的多行脚本）都会拆开；heredoc 正文不当命令；
//! `sudo` / `env` / 变量赋值等前缀剥掉；`sh -c '…'` 递归解析；`$[ ]` `(( ))` `[[ ]]` 里的 `<<` / `<` 不当重定向。

use serde::{Deserialize, Serialize};

use super::super::decision::DecisionRecord;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeSummary {
    /// 写过 / 改过的文件（nx_write / nx_edit 是模型给的写法，可能是相对路径或 `~` 开头）
    pub files_written: Vec<String>,
    /// 执行过的命令条数（nx_exec / nx_run / 打开会话时附带的命令，各算一条）
    pub commands: u32,
    /// 「安装 nginx（apt）」「卸载 left-pad（npm）」
    pub packages: Vec<String>,
    /// 「重启 nginx」「开机自启 docker」
    pub services: Vec<String>,
    /// 「放行 80/tcp（ufw）」「8080:80（docker 映射）」
    pub ports: Vec<String>,
    /// 「运行 nginx:latest（名为 web）」「删除 web」「compose up」
    pub containers: Vec<String>,
    /// 「https://x/y.tar.gz → /tmp/y.tar.gz」
    pub downloads: Vec<String>,
    /// 「删除 /var/www/old（递归）」「删除容器 web（docker）」「清空 iptables 规则（全部链）」「删除用户 bob」
    #[serde(default)]
    pub removals: Vec<String>,
    /// 合并到了哪条决策记录（`steward_decisions.seq`）。托管进行中每次落盘把新记录增量并进来，
    /// 决策表裁剪掉的早期改动因此不丢。旧版本在结束时一次定格的摘要没有它（None = 已定格，不再并）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_seq: Option<i64>,
}

impl ChangeSummary {
    pub fn is_empty(&self) -> bool {
        self.files_written.is_empty()
            && self.commands == 0
            && self.packages.is_empty()
            && self.services.is_empty()
            && self.ports.is_empty()
            && self.containers.is_empty()
            && self.downloads.is_empty()
            && self.removals.is_empty()
    }
}

impl ChangeSummary {
    /// 并进另一份摘要：各列表取并集（保持先后、去重），命令条数相加。
    pub fn merge(&mut self, o: ChangeSummary) {
        for (dst, src) in [
            (&mut self.files_written, o.files_written),
            (&mut self.packages, o.packages),
            (&mut self.services, o.services),
            (&mut self.ports, o.ports),
            (&mut self.containers, o.containers),
            (&mut self.downloads, o.downloads),
        ] {
            for x in src {
                push(dst, x);
            }
        }
        for x in o.removals {
            push_removal(&mut self.removals, x);
        }
        self.commands = self.commands.saturating_add(o.commands);
    }

    /// 把 seq 大于 `merged_seq` 的决策记录 `(seq, JSON)` 增量并进来（同一条不会算两次）。
    /// 返回有没有新记录。`merged_seq` 为 None（旧版本定格的）时不动。
    pub fn absorb_rows<'a>(&mut self, contract_id: &str, rows: impl IntoIterator<Item = (i64, &'a str)>) -> bool {
        let Some(mut upto) = self.merged_seq else { return false };
        let start = upto;
        let mut recs = Vec::new();
        for (seq, j) in rows {
            if seq <= start {
                continue;
            }
            upto = upto.max(seq);
            if let Ok(r) = serde_json::from_str::<DecisionRecord>(j) {
                if r.contract_id == contract_id {
                    recs.push(r);
                }
            }
        }
        self.merge(summarize(&recs));
        self.merged_seq = Some(upto);
        upto != start
    }
}

fn push(v: &mut Vec<String>, s: String) {
    if !s.is_empty() && !v.contains(&s) {
        v.push(s);
    }
}

/// 「删除」最多列多少条（`rm` 一大串路径时只列前面的）
pub const MAX_REMOVALS: usize = 30;
const MORE_REMOVALS: &str = "……还有更多删除，未全部列出";
const MORE_REMOVALS_EN: &str = "…more removals not listed";

/// 「还有更多」那一行（两种语言都认：合并的摘要可能是切换语言前写的）
fn is_more(s: &str) -> bool {
    s == MORE_REMOVALS || s == MORE_REMOVALS_EN
}

fn en() -> bool {
    crate::i18n::is_en()
}

fn push_removal(v: &mut Vec<String>, s: String) {
    if is_more(&s) || v.contains(&s) {
        if is_more(&s) && !v.iter().any(|x| is_more(x)) {
            v.push(s);
        }
        return;
    }
    let n = v.iter().filter(|x| !is_more(x)).count();
    if n < MAX_REMOVALS {
        // 「还有更多」始终在最后
        let at = v.iter().position(|x| is_more(x)).unwrap_or(v.len());
        v.insert(at, s);
    } else if !v.iter().any(|x| is_more(x)) {
        v.push(if en() { MORE_REMOVALS_EN } else { MORE_REMOVALS }.to_string());
    }
}

/// 一份契约的决策记录 JSON（`steward_decisions.record_json`）→ 改动摘要。解析不了的行跳过。
pub fn from_json_rows<'a>(contract_id: &str, rows: impl IntoIterator<Item = &'a str>) -> ChangeSummary {
    let recs: Vec<DecisionRecord> = rows
        .into_iter()
        .filter_map(|j| serde_json::from_str::<DecisionRecord>(j).ok())
        .filter(|r| r.contract_id == contract_id)
        .collect();
    summarize(&recs)
}

/// 决策记录 → 改动摘要（调用方已按契约筛过；被拒的 / 只是提议的不算）。
pub fn summarize(records: &[DecisionRecord]) -> ChangeSummary {
    let mut s = ChangeSummary::default();
    for r in records {
        if matches!(r.outcome.as_str(), "rejected" | "proposed" | "escalated") {
            continue;
        }
        let text = r.chosen.text.as_str();
        match r.chosen.kind.as_str() {
            "exec" => {
                s.commands += 1;
                // 决策记录按当时的界面语言写：两种前缀都认
                let cmd = text.strip_prefix("（旁路）").or_else(|| text.strip_prefix("(side channel) ")).unwrap_or(text);
                analyze_script(cmd, &mut s);
            }
            "open" => {
                if let Some((_, cmd)) = text.split_once("并执行：").or_else(|| text.split_once(" and run: ")) {
                    s.commands += 1;
                    analyze_script(cmd, &mut s);
                }
            }
            "file_write" => {
                if let Some(p) = file_write_path(text) {
                    push(&mut s.files_written, p);
                }
            }
            _ => {}
        }
    }
    s
}

/// `FileCall::summary()` 的写法：`写入 <path>（N 字节）` / `修改 <path>` / `修改 <path>（全部替换）`。
/// 英文界面下是 `Write <path> (N bytes)` / `Edit <path>` / `Edit <path> (replace all)`。
fn file_write_path(text: &str) -> Option<String> {
    if let Some(rest) = text.strip_prefix("Write ").or_else(|| text.strip_prefix("Edit ")) {
        let path = rest.strip_suffix(" (replace all)").unwrap_or(rest);
        let path = match path.rfind(" (") {
            Some(i) if path.ends_with(" bytes)") => &path[..i],
            _ => path,
        };
        let path = path.trim();
        return (!path.is_empty()).then(|| path.to_string());
    }
    let rest = text.strip_prefix("写入 ").or_else(|| text.strip_prefix("修改 "))?;
    let path = match rest.rfind('（') {
        Some(i) if rest.ends_with('）') => &rest[..i],
        _ => rest,
    };
    let path = path.trim();
    (!path.is_empty()).then(|| path.to_string())
}

// ───────────────────────────── 词法 ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word(String),
    /// 输出重定向的目标（`>` `>>` `&>` `2>` …；`>&2` 这种复制 fd 的不算）
    Out(String),
    /// 命令分隔：`;` `&&` `||` `|` `&` 换行 `(` `)`
    Sep,
}

/// 粗粒度的 shell 词法：引号、反斜杠、`$(…)` / 反引号、注释、续行、heredoc 正文都照顾到，够提取用。
fn lex(src: &str) -> Vec<Tok> {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    // 待跳过正文的 heredoc：(分隔符, 是否 `<<-`)
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut i = 0;

    macro_rules! flush {
        () => {
            if in_word {
                out.push(Tok::Word(std::mem::take(&mut word)));
                in_word = false;
            }
        };
    }

    // 读一个词（重定向目标 / heredoc 分隔符），返回去掉引号后的文本与新位置
    fn read_word(chars: &[char], mut i: usize) -> (String, usize) {
        let n = chars.len();
        while i < n && (chars[i] == ' ' || chars[i] == '\t') {
            i += 1;
        }
        let mut w = String::new();
        while i < n {
            let c = chars[i];
            match c {
                ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' => break,
                '\'' | '"' => {
                    i += 1;
                    while i < n && chars[i] != c {
                        w.push(chars[i]);
                        i += 1;
                    }
                    i += 1;
                }
                '\\' if i + 1 < n => {
                    w.push(chars[i + 1]);
                    i += 2;
                }
                _ => {
                    w.push(c);
                    i += 1;
                }
            }
        }
        (w, i)
    }

    while i < n {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\r' => {
                flush!();
                i += 1;
            }
            '\n' => {
                flush!();
                out.push(Tok::Sep);
                i += 1;
                // heredoc 正文：一直跳到分隔符行
                for (delim, dash) in std::mem::take(&mut heredocs) {
                    while i < n {
                        let end = chars[i..].iter().position(|&ch| ch == '\n').map_or(n, |p| i + p);
                        let line: String = chars[i..end].iter().collect();
                        i = (end + 1).min(n);
                        let l = if dash { line.trim_start_matches('\t') } else { line.as_str() };
                        if l.trim_end() == delim {
                            break;
                        }
                    }
                }
            }
            '#' if !in_word => {
                while i < n && chars[i] != '\n' {
                    i += 1;
                }
            }
            '(' if !in_word && i + 1 < n && chars[i + 1] == '(' => {
                // `(( … ))` 算术命令：整段跳过（里面的 `<<` / `>` 是运算符）
                let mut depth = 0;
                while i < n {
                    match chars[i] {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        '\n' => break,
                        _ => {}
                    }
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                }
                out.push(Tok::Sep);
            }
            '[' if !in_word && i + 2 < n && chars[i + 1] == '[' && matches!(chars[i + 2], ' ' | '\t') => {
                // `[[ … ]]` 条件测试：没有副作用，整段跳过（里面的 `<` `>` 是比较）
                i += 2;
                while i < n && !(chars[i] == ']' && i + 1 < n && chars[i + 1] == ']') && chars[i] != '\n' {
                    i += 1;
                }
                if i < n && chars[i] == ']' {
                    i += 2;
                }
                out.push(Tok::Sep);
            }
            ';' | '(' | ')' => {
                flush!();
                out.push(Tok::Sep);
                i += 1;
            }
            '|' => {
                flush!();
                out.push(Tok::Sep);
                i += if i + 1 < n && chars[i + 1] == '|' { 2 } else { 1 };
            }
            '&' if i + 1 < n && chars[i + 1] == '>' => {
                // `&>file` / `&>>file`
                flush!();
                i += 2;
                if i < n && chars[i] == '>' {
                    i += 1;
                }
                let (w, j) = read_word(&chars, i);
                i = j;
                if !w.is_empty() {
                    out.push(Tok::Out(w));
                }
            }
            '&' => {
                flush!();
                out.push(Tok::Sep);
                i += if i + 1 < n && chars[i + 1] == '&' { 2 } else { 1 };
            }
            '>' => {
                // `2>` 的 fd 号已经进了 word：数字就丢掉，否则是正常的词
                if in_word && word.chars().all(|ch| ch.is_ascii_digit()) {
                    word.clear();
                    in_word = false;
                }
                flush!();
                i += 1;
                if i < n && (chars[i] == '>' || chars[i] == '|') {
                    i += 1;
                }
                if i < n && chars[i] == '&' {
                    // `>&2` 复制 fd
                    i += 1;
                    while i < n && (chars[i].is_ascii_digit() || chars[i] == '-') {
                        i += 1;
                    }
                    continue;
                }
                let (w, j) = read_word(&chars, i);
                i = j;
                if !w.is_empty() {
                    out.push(Tok::Out(w));
                }
            }
            '<' => {
                if in_word && word.chars().all(|ch| ch.is_ascii_digit()) {
                    word.clear();
                    in_word = false;
                }
                flush!();
                if i + 2 < n && chars[i + 1] == '<' && chars[i + 2] == '<' {
                    // here-string：跳过一个词
                    let (_, j) = read_word(&chars, i + 3);
                    i = j;
                } else if i + 1 < n && chars[i + 1] == '<' {
                    let mut j = i + 2;
                    let dash = j < n && chars[j] == '-';
                    if dash {
                        j += 1;
                    }
                    let (w, j) = read_word(&chars, j);
                    i = j;
                    if !w.is_empty() {
                        heredocs.push((w, dash));
                    }
                } else {
                    let (_, j) = read_word(&chars, i + 1);
                    i = j;
                }
            }
            '\'' => {
                in_word = true;
                i += 1;
                while i < n && chars[i] != '\'' {
                    word.push(chars[i]);
                    i += 1;
                }
                i += 1;
            }
            '"' => {
                in_word = true;
                i += 1;
                while i < n && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < n && matches!(chars[i + 1], '"' | '\\' | '$' | '`') {
                        i += 1;
                    }
                    word.push(chars[i]);
                    i += 1;
                }
                i += 1;
            }
            '\\' => {
                if i + 1 < n && chars[i + 1] == '\n' {
                    // 续行
                    i += 2;
                } else if i + 1 < n {
                    in_word = true;
                    word.push(chars[i + 1]);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            '$' if i + 1 < n && chars[i + 1] == '[' => {
                // `$[ … ]` 旧式算术：整个当成词的一部分（里面的 `<<` 是移位，不是 heredoc）
                in_word = true;
                let mut depth = 0;
                while i < n {
                    let ch = chars[i];
                    word.push(ch);
                    i += 1;
                    if ch == '[' {
                        depth += 1;
                    } else if ch == ']' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
            }
            '$' if i + 1 < n && chars[i + 1] == '(' => {
                // `$(…)` 整个当成词的一部分（不深入）
                in_word = true;
                let mut depth = 0;
                while i < n {
                    let ch = chars[i];
                    word.push(ch);
                    i += 1;
                    if ch == '(' {
                        depth += 1;
                    } else if ch == ')' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
            }
            '`' => {
                in_word = true;
                word.push(c);
                i += 1;
                while i < n && chars[i] != '`' {
                    word.push(chars[i]);
                    i += 1;
                }
                if i < n {
                    word.push('`');
                    i += 1;
                }
            }
            _ => {
                in_word = true;
                word.push(c);
                i += 1;
            }
        }
    }
    if in_word {
        out.push(Tok::Word(word));
    }
    out
}

/// 一段简单命令：词 + 输出重定向目标
struct Simple {
    words: Vec<String>,
    outs: Vec<String>,
}

fn split_simple(toks: Vec<Tok>) -> Vec<Simple> {
    let mut v = Vec::new();
    let mut cur = Simple { words: Vec::new(), outs: Vec::new() };
    for t in toks {
        match t {
            Tok::Word(w) => cur.words.push(w),
            Tok::Out(o) => cur.outs.push(o),
            Tok::Sep => {
                if !cur.words.is_empty() || !cur.outs.is_empty() {
                    v.push(std::mem::replace(&mut cur, Simple { words: Vec::new(), outs: Vec::new() }));
                }
            }
        }
    }
    if !cur.words.is_empty() || !cur.outs.is_empty() {
        v.push(cur);
    }
    v
}

// ───────────────────────────── 分析 ─────────────────────────────

/// 一段 shell 命令（可多行）会写哪些文件：重定向目标（`>` `>>` `&>` …，heredoc 的 `cat > f <<EOF` 也在内）、
/// `tee`、`sed -i`、`cp` / `mv` / `install` 的目标、`dd of=`；`/dev/*`（`/dev/null`、`/dev/stdout` …）不算。
/// 托管门禁据此给 shell 写文件推出 `file_write` 标签（契约里 deny `file_write` = 不许改文件，不分工具）。
pub fn files_written(cmd: &str) -> Vec<String> {
    let mut s = ChangeSummary::default();
    analyze_script(cmd, &mut s);
    s.files_written
}

fn analyze_script(src: &str, s: &mut ChangeSummary) {
    analyze_depth(src, s, 0);
}

fn analyze_depth(src: &str, s: &mut ChangeSummary, depth: u8) {
    for cmd in split_simple(lex(src)) {
        let url = cmd.words.iter().find(|w| is_url(w)).cloned();
        for o in &cmd.outs {
            if is_real_file(o) {
                push(&mut s.files_written, o.clone());
            }
        }
        let words = strip_prefix(&cmd.words);
        let Some(prog) = words.first() else { continue };
        let prog = basename(prog);
        let args = &words[1..];
        // `curl url > file`：下载落在重定向目标上
        if prog == "curl" {
            if let (Some(u), Some(o)) = (&url, cmd.outs.iter().find(|o| is_real_file(o))) {
                push(&mut s.downloads, format!("{} → {o}", shown_url(u)));
            }
        }
        analyze_cmd(prog, args, s, depth);
    }
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn is_url(w: &str) -> bool {
    let l = w.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://") || l.starts_with("ftp://")
}

/// URL 里可能带凭据 / token：给人看的那份过一遍脱敏，查询串去掉
fn shown_url(u: &str) -> String {
    let base = u.split(['?', '#']).next().unwrap_or(u);
    super::super::redact::redact(base)
}

fn is_real_file(p: &str) -> bool {
    !p.is_empty() && !p.starts_with("/dev/") && p != "-" && !p.starts_with('&')
}

fn is_assignment(w: &str) -> bool {
    match w.split_once('=') {
        Some((k, _)) => !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !k.starts_with(|c: char| c.is_ascii_digit()),
        None => false,
    }
}

/// 剥掉不改变「真正执行的是谁」的前缀：变量赋值、sudo / doas（含选项）、env、nohup、time、exec、
/// command、nice、stdbuf、timeout N，以及 if / then / do / ! 这类关键字。
fn strip_prefix(words: &[String]) -> &[String] {
    let mut i = 0;
    while i < words.len() {
        let w = words[i].as_str();
        if is_assignment(w) {
            i += 1;
            continue;
        }
        match basename(w) {
            "sudo" | "doas" => {
                i += 1;
                while i < words.len() && words[i].starts_with('-') {
                    i += sudo_opt_width(&words[i]);
                }
            }
            "env" => {
                i += 1;
                while i < words.len() && (words[i].starts_with('-') || is_assignment(&words[i])) {
                    i += if words[i] == "-u" { 2 } else { 1 };
                }
            }
            "nohup" | "time" | "exec" | "command" | "builtin" | "then" | "do" | "else" | "if" | "while" | "until"
            | "!" | "{" | "}" => i += 1,
            "nice" | "ionice" | "stdbuf" => {
                i += 1;
                while i < words.len() && words[i].starts_with('-') {
                    i += if matches!(words[i].as_str(), "-n" | "-c") { 2 } else { 1 };
                }
            }
            "timeout" => {
                i += 1;
                while i < words.len() && words[i].starts_with('-') {
                    i += 1;
                }
                i += 1; // 时长
            }
            _ => break,
        }
    }
    &words[i.min(words.len())..]
}

/// sudo / doas 的一个选项词占几个词：带值的（`-u root`、`--user root`）占 2，值贴着写的
/// （`-uroot`、`--user=root`）和不带值的占 1；短选项可以成串（`-iu root`、`-Eu root`：值给串里最后那个）。
fn sudo_opt_width(w: &str) -> usize {
    const SHORT: &[char] = &['u', 'g', 'C', 'h', 'p', 'U', 'r', 't', 'D', 'R', 'T'];
    if w == "--" {
        return 1;
    }
    if let Some(long) = w.strip_prefix("--") {
        let takes = matches!(
            long,
            "user" | "group" | "close-from" | "host" | "prompt" | "other-user" | "role" | "type" | "chdir" | "chroot" | "command-timeout"
        );
        return if takes { 2 } else { 1 };
    }
    let flags: Vec<char> = w.chars().skip(1).collect();
    match flags.iter().position(|c| SHORT.contains(c)) {
        // 带值的那个后面没东西了：值是下一个词
        Some(p) if p + 1 == flags.len() => 2,
        _ => 1,
    }
}

/// 第一个不以 `-` 开头的参数及其位置（跳过 `takes_value` 里列出的带值选项）
fn positionals<'a>(args: &'a [String], takes_value: &[&str]) -> Vec<&'a str> {
    let mut v = Vec::new();
    let mut i = 0;
    let mut opts_done = false;
    while i < args.len() {
        let a = args[i].as_str();
        if !opts_done && a == "--" {
            opts_done = true;
        } else if !opts_done && a.starts_with('-') && a.len() > 1 {
            if !a.contains('=') && takes_value.contains(&a) {
                i += 1;
            }
        } else {
            v.push(a);
        }
        i += 1;
    }
    v
}

fn pkg_line(s: &mut ChangeSummary, install: bool, tool: &str, names: &[&str]) {
    let verb = match (install, en()) {
        (true, false) => "安装",
        (false, false) => "卸载",
        (true, true) => "Install",
        (false, true) => "Uninstall",
    };
    for n in names {
        // 安装串里可能带凭据（`git+https://user:TOKEN@host/…`）
        let n = super::super::redact::redact(n);
        push(&mut s.packages, crate::tr!("{verb} {n}（{tool}）", "{verb} {n} ({tool})"));
    }
}

fn analyze_cmd(prog: &str, args: &[String], s: &mut ChangeSummary, depth: u8) {
    match prog {
        "sh" | "bash" | "zsh" | "dash" | "ash" => {
            if depth < 3 {
                if let Some(i) = args.iter().position(|a| a == "-c" || (a.starts_with('-') && !a.starts_with("--") && a.ends_with('c'))) {
                    if let Some(script) = args.get(i + 1) {
                        analyze_depth(script, s, depth + 1);
                    }
                }
            }
        }
        "apt" | "apt-get" | "aptitude" | "yum" | "dnf" | "microdnf" | "zypper" => {
            let p = positionals(args, &["-o", "-t", "-c", "--option", "--target-release", "--setopt", "--enablerepo", "--disablerepo"]);
            let Some((sub, names)) = p.split_first() else { return };
            match *sub {
                "install" | "reinstall" | "in" => pkg_line(s, true, prog, names),
                "remove" | "purge" | "erase" | "autoremove" | "uninstall" | "rm" => pkg_line(s, false, prog, names),
                _ => {}
            }
        }
        "apk" => {
            let p = positionals(args, &["-X", "--repository", "-t", "--virtual"]);
            let Some((sub, names)) = p.split_first() else { return };
            match *sub {
                "add" => pkg_line(s, true, "apk", names),
                "del" => pkg_line(s, false, "apk", names),
                _ => {}
            }
        }
        "brew" | "snap" | "gem" => {
            let p = positionals(args, &["--channel"]);
            let Some((sub, names)) = p.split_first() else { return };
            match *sub {
                "install" | "reinstall" => pkg_line(s, true, prog, names),
                "uninstall" | "remove" | "rm" => pkg_line(s, false, prog, names),
                _ => {}
            }
        }
        "pip" | "pip3" | "pipx" | "uv" => {
            let (tool, args) = if prog == "uv" && args.first().is_some_and(|a| a == "pip") { ("pip", &args[1..]) } else { (prog, args) };
            pip(tool, args, s);
        }
        "python" | "python3" => {
            if let Some(i) = args.iter().position(|a| a == "-m") {
                if args.get(i + 1).is_some_and(|m| m == "pip") {
                    pip("pip", &args[i + 2..], s);
                }
            }
        }
        "npm" | "pnpm" | "yarn" | "bun" => {
            let p = positionals(args, &["--prefix", "-C", "--registry", "--cwd"]);
            let sub = p.first().copied();
            let names: Vec<&str> = p.iter().skip(1).copied().collect();
            match sub {
                Some("install" | "i" | "add" | "ci") => {
                    if names.is_empty() {
                        push(&mut s.packages, crate::tr!("安装 package.json 里的依赖（{prog}）", "Install dependencies from package.json ({prog})"));
                    } else {
                        pkg_line(s, true, prog, &names);
                    }
                }
                None if prog == "yarn" && args.is_empty() => {
                    push(&mut s.packages, crate::tr!("安装 package.json 里的依赖（yarn）", "Install dependencies from package.json (yarn)"))
                }
                Some("uninstall" | "remove" | "rm" | "un" | "r") => pkg_line(s, false, prog, &names),
                _ => {}
            }
        }
        "cargo" => {
            let p = positionals(args, &["--version", "--git", "--branch", "--tag", "--rev", "--path", "--root", "--registry", "--features", "-F", "--target", "-p", "--package"]);
            let Some((sub, names)) = p.split_first() else { return };
            match *sub {
                "install" | "add" => pkg_line(s, true, "cargo", names),
                "uninstall" | "remove" | "rm" => pkg_line(s, false, "cargo", names),
                _ => {}
            }
        }
        "systemctl" => {
            let p = positionals(args, &["-H", "--host", "-M", "--machine", "-p", "--property", "-n", "--lines", "-o", "--output", "-s", "--signal"]);
            let Some((sub, units)) = p.split_first() else { return };
            if let Some(verb) = service_verb(sub) {
                for u in units {
                    push(&mut s.services, format!("{verb} {u}"));
                }
                if matches!(*sub, "enable" | "disable") && args.iter().any(|a| a == "--now") {
                    let now = match (*sub == "enable", en()) {
                        (true, false) => "启动",
                        (false, false) => "停止",
                        (true, true) => "Start",
                        (false, true) => "Stop",
                    };
                    for u in units {
                        push(&mut s.services, format!("{now} {u}"));
                    }
                }
            }
        }
        "service" => {
            let p = positionals(args, &[]);
            if let [unit, sub, ..] = p.as_slice() {
                if let Some(verb) = service_verb(sub) {
                    push(&mut s.services, format!("{verb} {unit}"));
                }
            }
        }
        "ufw" => ufw(args, s),
        "firewall-cmd" => firewall_cmd(args, s),
        "iptables" | "ip6tables" => iptables(prog, args, s),
        "docker" | "podman" | "nerdctl" => docker(prog, args, s),
        "docker-compose" | "podman-compose" => compose(prog, args, s),
        "curl" => curl(args, s),
        "wget" => wget(args, s),
        "cp" | "mv" | "install" | "ln" | "rsync" => {
            // 目标：`-t DIR` / `--target-directory=DIR`，否则最后一个位置参数（至少两个时）；`install -d` 的每个参数都是要建的目录
            let dirs_only = prog == "install" && args.iter().any(|a| a == "-d" || a == "--directory");
            let takes = ["-t", "--target-directory", "-S", "--suffix", "-m", "--mode", "-o", "--owner", "-g", "--group", "-e"];
            let p = positionals(args, &takes);
            let target = args.iter().enumerate().find_map(|(i, a)| {
                if a == "-t" || a == "--target-directory" {
                    args.get(i + 1).cloned()
                } else {
                    a.strip_prefix("--target-directory=").map(str::to_string)
                }
            });
            let dests: Vec<String> = if dirs_only {
                p.iter().map(|x| x.to_string()).collect()
            } else if let Some(t) = target {
                vec![t]
            } else if p.len() >= 2 {
                vec![p[p.len() - 1].to_string()]
            } else {
                Vec::new()
            };
            for f in dests.into_iter().filter(|f| is_real_file(f)) {
                push(&mut s.files_written, f);
            }
        }
        "dd" => {
            for a in args {
                if let Some(f) = a.strip_prefix("of=").filter(|f| is_real_file(f)) {
                    push(&mut s.files_written, f.to_string());
                }
            }
        }
        "tee" => {
            for f in positionals(args, &[]) {
                if is_real_file(f) {
                    push(&mut s.files_written, f.to_string());
                }
            }
        }
        "sed" => {
            for f in sed_in_place_files(args) {
                push(&mut s.files_written, f.to_string());
            }
        }
        "rm" | "rmdir" | "unlink" => {
            let recursive = prog == "rm"
                && args.iter().any(|a| {
                    a == "--recursive" || (a.starts_with('-') && !a.starts_with("--") && a.contains(['r', 'R']))
                });
            for t in positionals(args, &[]) {
                let line = if recursive {
                    crate::tr!("删除 {t}（递归）", "Delete {t} (recursive)")
                } else {
                    crate::tr!("删除 {t}", "Delete {t}")
                };
                push_removal(&mut s.removals, line);
            }
        }
        "userdel" | "deluser" => {
            let p = positionals(args, &["-R", "--root", "-P", "--prefix", "--conf"]);
            if let Some(u) = p.first() {
                let home = args.iter().any(|a| matches!(a.as_str(), "-r" | "--remove" | "--remove-home"));
                let line = match (home, en()) {
                    (true, false) => format!("删除用户 {u}（连同主目录）"),
                    (false, false) => format!("删除用户 {u}"),
                    (true, true) => format!("Delete user {u} (with home directory)"),
                    (false, true) => format!("Delete user {u}"),
                };
                push_removal(&mut s.removals, line);
            }
        }
        _ => {}
    }
}

/// `sed` 原地改写（`-i` / `-i.bak` / `-Ei` / `-ri` / `-ni` / `--in-place[=SUF]`）时改到的文件；不是原地改写为空。
/// 短选项成串时 `i` 之后的都是备份后缀；`e` / `f` / `l` 带值（贴着写或下一个词）。
fn sed_in_place_files(args: &[String]) -> Vec<&str> {
    let mut in_place = false;
    let mut script_opt = false;
    let mut pos = Vec::new();
    let mut opts_done = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if opts_done || a == "-" || !a.starts_with('-') {
            pos.push(a);
        } else if a == "--" {
            opts_done = true;
        } else if let Some(long) = a.strip_prefix("--") {
            let key = long.split_once('=').map_or(long, |(k, _)| k);
            match key {
                "in-place" => in_place = true,
                "expression" | "file" => {
                    script_opt = true;
                    if !long.contains('=') {
                        i += 1;
                    }
                }
                "line-length" if !long.contains('=') => i += 1,
                _ => {}
            }
        } else {
            let flags: Vec<char> = a.chars().skip(1).collect();
            for (k, c) in flags.iter().enumerate() {
                match c {
                    'i' => {
                        in_place = true;
                        break;
                    }
                    'e' | 'f' | 'l' => {
                        script_opt |= *c != 'l';
                        if k + 1 == flags.len() {
                            i += 1;
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
        i += 1;
    }
    if !in_place {
        return Vec::new();
    }
    // 没有 -e / -f 时第一个位置参数是脚本
    pos.into_iter().skip(if script_opt { 0 } else { 1 }).collect()
}

fn service_verb(sub: &str) -> Option<&'static str> {
    if en() {
        return Some(match sub {
            "start" => "Start",
            "stop" => "Stop",
            "restart" | "try-restart" | "condrestart" => "Restart",
            "reload" | "reload-or-restart" => "Reload",
            "enable" => "Enable at boot",
            "disable" => "Disable at boot",
            "mask" => "Mask",
            "unmask" => "Unmask",
            "kill" => "Kill",
            _ => return None,
        });
    }
    Some(match sub {
        "start" => "启动",
        "stop" => "停止",
        "restart" | "try-restart" | "condrestart" => "重启",
        "reload" | "reload-or-restart" => "重载",
        "enable" => "开机自启",
        "disable" => "取消自启",
        "mask" => "屏蔽",
        "unmask" => "取消屏蔽",
        "kill" => "杀掉",
        _ => return None,
    })
}

fn pip(tool: &str, args: &[String], s: &mut ChangeSummary) {
    let Some((sub, rest)) = args.split_first() else { return };
    let install = match sub.as_str() {
        "install" => true,
        "uninstall" => false,
        _ => return,
    };
    let mut names = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        match a {
            "-r" | "--requirement" => {
                if let Some(f) = rest.get(i + 1) {
                    let f = super::super::redact::redact(f);
                    let line = match (install, en()) {
                        (true, false) => format!("安装 {f} 里的依赖（{tool}）"),
                        (false, false) => format!("卸载 {f} 里的依赖（{tool}）"),
                        (true, true) => format!("Install dependencies from {f} ({tool})"),
                        (false, true) => format!("Uninstall dependencies from {f} ({tool})"),
                    };
                    push(&mut s.packages, line);
                }
                i += 1;
            }
            "-e" | "--editable" => {
                if let Some(p) = rest.get(i + 1) {
                    names.push(p.as_str());
                }
                i += 1;
            }
            "-i" | "--index-url" | "--extra-index-url" | "-c" | "--constraint" | "-t" | "--target" | "--prefix" | "--root"
            | "-f" | "--find-links" | "--trusted-host" | "--python" | "-p" => i += 1,
            _ if a.starts_with('-') => {}
            _ => names.push(a),
        }
        i += 1;
    }
    pkg_line(s, install, tool, &names);
}

fn ufw(args: &[String], s: &mut ChangeSummary) {
    let p = positionals(args, &[]);
    let Some((sub, rest)) = p.split_first() else { return };
    let (verb, rest, deleted) = match *sub {
        "delete" => match rest.split_first() {
            Some((n, _)) if n.chars().all(|c| c.is_ascii_digit()) => {
                return push(&mut s.ports, crate::tr!("删除 ufw 规则 {n}", "Delete ufw rule {n}"))
            }
            Some((v, r)) => (*v, r, true),
            None => return,
        },
        "enable" => return push(&mut s.ports, crate::tr!("启用防火墙（ufw）", "Enable firewall (ufw)")),
        "disable" => return push(&mut s.ports, crate::tr!("关闭防火墙（ufw）", "Disable firewall (ufw)")),
        "reset" => return push(&mut s.ports, crate::tr!("重置防火墙规则（ufw）", "Reset firewall rules (ufw)")),
        "insert" | "prepend" => match rest.get(1) {
            Some(v) => (*v, &rest[2..], false),
            None => return,
        },
        v => (v, rest, false),
    };
    let word = match (verb, en()) {
        ("allow", false) => "放行",
        ("deny", false) => "拒绝",
        ("reject", false) => "拒绝",
        ("limit", false) => "限速",
        ("allow", true) => "Allow",
        ("deny", true) => "Deny",
        ("reject", true) => "Reject",
        ("limit", true) => "Rate-limit",
        _ => return,
    };
    // `ufw allow from x to any port 22 proto tcp` / `ufw allow 80/tcp` / `ufw allow OpenSSH` / `ufw allow from 10.0.0.0/8`
    let after = |key: &str| rest.iter().position(|w| *w == key).and_then(|i| rest.get(i + 1)).copied();
    let from = after("from").filter(|v| *v != "any");
    let to = after("to").filter(|v| *v != "any");
    let full = rest.iter().any(|w| matches!(*w, "from" | "to" | "port"));
    let port = match after("port") {
        Some(p) => Some(match after("proto") {
            Some(pr) => format!("{p}/{pr}"),
            None => p.to_string(),
        }),
        // 简单写法：`ufw allow 80/tcp`、`ufw allow in on eth0 80`；`on` 后面是网卡名
        None if !full => {
            let mut it = rest.iter().copied().filter(|w| !matches!(*w, "in" | "out" | "log" | "log-all"));
            let mut spec = None;
            while let Some(w) = it.next() {
                if w == "on" {
                    it.next();
                    continue;
                }
                spec = Some(w.to_string());
                break;
            }
            spec
        }
        None => None,
    };
    let all_ports = port.is_none();
    let mut spec = port.unwrap_or_else(|| crate::tr!("全部端口", "all ports"));
    let peers: Vec<String> = [
        from.map(|f| crate::tr!("来源 {f}", "from {f}")),
        to.map(|t| crate::tr!("目标 {t}", "to {t}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !peers.is_empty() {
        let sep = if en() { ", " } else { "，" };
        spec = format!("{spec}{sep}{}", peers.join(sep));
    } else if all_ports {
        return;
    }
    let line = if deleted {
        crate::tr!("删除规则：{word} {spec}（ufw）", "Delete rule: {word} {spec} (ufw)")
    } else {
        crate::tr!("{word} {spec}（ufw）", "{word} {spec} (ufw)")
    };
    push(&mut s.ports, line);
}

fn firewall_cmd(args: &[String], s: &mut ChangeSummary) {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let (key, val) = match a.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (a, args.get(i + 1).filter(|n| !n.starts_with('-')).cloned()),
        };
        let word = match (key, en()) {
            ("--add-port", false) => Some("放行"),
            ("--remove-port", false) => Some("关闭"),
            ("--add-service", false) => Some("放行服务"),
            ("--remove-service", false) => Some("关闭服务"),
            ("--add-port", true) => Some("Allow"),
            ("--remove-port", true) => Some("Close"),
            ("--add-service", true) => Some("Allow service"),
            ("--remove-service", true) => Some("Close service"),
            _ => None,
        };
        if let Some(w) = word {
            if let Some(v) = val {
                if !a.contains('=') {
                    i += 1;
                }
                push(&mut s.ports, crate::tr!("{w} {v}（firewalld）", "{w} {v} (firewalld)"));
            }
        }
        i += 1;
    }
}

fn iptables(prog: &str, args: &[String], s: &mut ChangeSummary) {
    let mut op = None;
    let mut chain = "";
    let mut port = None;
    let mut proto = None;
    let mut target = None;
    let mut table = None;
    let mut i = 0;
    // 选项后面的链名（可省；下一个词是选项就没给）
    let chain_arg = |next: Option<&str>| next.filter(|n| !n.starts_with('-')).map(str::to_string);
    while i < args.len() {
        let a = args[i].as_str();
        let next = args.get(i + 1).map(String::as_str);
        match a {
            "-t" | "--table" => {
                table = next.filter(|t| *t != "filter");
                i += 1;
            }
            "-F" | "--flush" | "-X" | "--delete-chain" => {
                let flush = matches!(a, "-F" | "--flush");
                let what = match (flush, en()) {
                    (true, false) => "清空规则",
                    (false, false) => "删除自定义链",
                    (true, true) => "Flush rules",
                    (false, true) => "Delete custom chains",
                };
                let chain = chain_arg(next);
                if chain.is_some() {
                    i += 1;
                }
                let scope = match (&chain, table) {
                    (Some(c), Some(t)) => crate::tr!("{t} 表 {c} 链", "{t} table, {c} chain"),
                    (Some(c), None) => crate::tr!("{c} 链", "{c} chain"),
                    (None, Some(t)) => crate::tr!("{t} 表全部", "all of {t} table"),
                    (None, None) => crate::tr!("全部", "all"),
                };
                push_removal(&mut s.removals, crate::tr!("{what}（{scope}，{prog}）", "{what} ({scope}, {prog})"));
            }
            "-P" | "--policy" => {
                if let (Some(c), Some(p)) = (next, args.get(i + 2)) {
                    push(&mut s.ports, crate::tr!("默认策略 {c} → {p}（{prog}）", "Default policy {c} → {p} ({prog})"));
                    i += 1;
                }
                i += 1;
            }
            "-A" | "--append" | "-I" | "--insert" | "-D" | "--delete" => {
                op = Some(matches!(a, "-D" | "--delete"));
                chain = next.unwrap_or("");
                i += 1;
            }
            "--dport" | "--destination-port" | "--dports" => {
                port = next;
                i += 1;
            }
            "-p" | "--protocol" => {
                proto = next;
                i += 1;
            }
            "-j" | "--jump" => {
                target = next;
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    if let (Some(op), Some(port)) = (op, port) {
        let spec = match proto {
            Some(p) => format!("{port}/{p}"),
            None => port.to_string(),
        };
        let tgt = target.map(|t| format!(" → {t}")).unwrap_or_default();
        let line = match (op, en()) {
            (true, false) => format!("删除规则 {chain} {spec}{tgt}（{prog}）"),
            (false, false) => format!("添加规则 {chain} {spec}{tgt}（{prog}）"),
            (true, true) => format!("Delete rule {chain} {spec}{tgt} ({prog})"),
            (false, true) => format!("Add rule {chain} {spec}{tgt} ({prog})"),
        };
        push(&mut s.ports, line);
    }
}

/// `docker run` 等带值的选项（`--x=y` 写法自带值，不在此列）
const DOCKER_VALUE_OPTS: &[&str] = &[
    "-p", "--publish", "-v", "--volume", "-e", "--env", "--env-file", "--name", "-w", "--workdir", "-u", "--user",
    "--network", "--net", "--restart", "--entrypoint", "-l", "--label", "--mount", "-m", "--memory", "--cpus", "-h",
    "--hostname", "--add-host", "--device", "--log-driver", "--log-opt", "--ulimit", "--pull", "--platform",
    "--cap-add", "--cap-drop", "--dns", "--ip", "--shm-size", "--expose", "--gpus", "--security-opt", "--tmpfs",
    "--health-cmd", "--stop-signal", "--runtime", "--pid", "--ipc", "--group-add", "--tag", "--file",
    "--build-arg", "--target", "--time", "-c", "--cpu-shares", "--memory-swap", "--network-alias", "--volumes-from",
    "--link", "--label-file", "--cidfile", "--userns", "--uts", "--isolation", "--attach", "-a",
];

fn docker(prog: &str, args: &[String], s: &mut ChangeSummary) {
    // 全局选项：-H / --context / --host 带值
    let mut i = 0;
    while i < args.len() && args[i].starts_with('-') {
        i += if matches!(args[i].as_str(), "-H" | "--host" | "-c" | "--context" | "--config" | "-l" | "--log-level") { 2 } else { 1 };
    }
    let Some(sub) = args.get(i) else { return };
    let mut rest = &args[(i + 1).min(args.len())..];
    // `docker container run` / `docker image pull` 等管理命令写法
    let mut sub = sub.as_str();
    let group = sub;
    if matches!(sub, "container" | "image" | "system" | "volume" | "network") {
        if let Some((inner, r)) = rest.split_first() {
            sub = inner.as_str();
            rest = r;
        }
    }
    let expanded = expand_short_clusters(rest, DOCKER_VALUE_OPTS);
    let rest = expanded.as_slice();
    match (group, sub) {
        ("image", "rm" | "remove") => sub = "rmi",
        ("system" | "volume" | "network" | "image" | "container", "prune") | ("volume" | "network", "rm" | "remove") => {
            let what = match (group, en()) {
                ("system", false) => "未用的容器 / 镜像 / 网络",
                ("image", false) => "未用的镜像",
                ("volume", false) => "数据卷",
                ("network", false) => "网络",
                (_, false) => "已停止的容器",
                ("system", true) => "unused containers / images / networks",
                ("image", true) => "unused images",
                ("volume", true) => "volume",
                ("network", true) => "network",
                (_, true) => "stopped containers",
            };
            if sub == "prune" {
                return push_removal(
                    &mut s.removals,
                    crate::tr!("清理{what}（{prog} {group} prune）", "Prune {what} ({prog} {group} prune)"),
                );
            }
            for n in positionals(rest, &[]) {
                push_removal(&mut s.removals, crate::tr!("删除{what} {n}（{prog}）", "Delete {what} {n} ({prog})"));
            }
            return;
        }
        ("volume" | "network" | "system", _) => return,
        _ => {}
    }
    match sub {
        "compose" => compose(&format!("{prog} compose"), rest, s),
        "run" | "create" => {
            let p = positionals(rest, DOCKER_VALUE_OPTS);
            let Some(image) = p.first() else { return };
            let name = opt_value(rest, &["--name"]);
            let verb = match (sub == "run", en()) {
                (true, false) => "运行",
                (false, false) => "创建",
                (true, true) => "Run",
                (false, true) => "Create",
            };
            let line = match name {
                Some(n) => crate::tr!("{verb} {image}（名为 {n}）", "{verb} {image} (named {n})"),
                None => format!("{verb} {image}"),
            };
            push(&mut s.containers, line);
            for p in opt_values(rest, &["-p", "--publish"]) {
                push(&mut s.ports, crate::tr!("{p}（{prog} 映射）", "{p} ({prog} mapping)"));
            }
        }
        "rm" => {
            for n in positionals(rest, &["-t", "--time", "-s", "--signal"]) {
                push_removal(&mut s.removals, crate::tr!("删除容器 {n}（{prog}）", "Delete container {n} ({prog})"));
            }
        }
        "stop" | "start" | "restart" | "kill" | "pause" | "unpause" => {
            let verb = match (sub, en()) {
                ("stop", false) => "停止",
                ("start", false) => "启动",
                ("restart", false) => "重启",
                ("kill", false) => "杀掉",
                ("pause", false) => "暂停",
                (_, false) => "恢复",
                ("stop", true) => "Stop",
                ("start", true) => "Start",
                ("restart", true) => "Restart",
                ("kill", true) => "Kill",
                ("pause", true) => "Pause",
                (_, true) => "Unpause",
            };
            for n in positionals(rest, &["-t", "--time", "-s", "--signal"]) {
                push(&mut s.containers, format!("{verb} {n}"));
            }
        }
        "rmi" => {
            for n in positionals(rest, &[]) {
                push_removal(&mut s.removals, crate::tr!("删除镜像 {n}（{prog}）", "Delete image {n} ({prog})"));
            }
        }
        "pull" => {
            for n in positionals(rest, &["--platform"]) {
                push(&mut s.containers, crate::tr!("拉取镜像 {n}", "Pull image {n}"));
            }
        }
        "build" => {
            let tag = opt_value(rest, &["-t", "--tag"]);
            push(&mut s.containers, match tag {
                Some(t) => crate::tr!("构建镜像 {t}", "Build image {t}"),
                None => crate::tr!("构建镜像", "Build image"),
            });
        }
        _ => {}
    }
}

/// 把成串的短选项拆开：`-dp 80:80` → `-d -p 80:80`，`-p80:80` → `-p 80:80`（`takes_value` 里的短选项吃掉串里剩下的当值）。
fn expand_short_clusters(args: &[String], takes_value: &[&str]) -> Vec<String> {
    let mut v = Vec::with_capacity(args.len());
    for (k, a) in args.iter().enumerate() {
        let cluster = a.len() > 2 && a.starts_with('-') && !a.starts_with("--") && a[1..].starts_with(|c: char| c.is_ascii_alphabetic());
        // 前一个词是带值选项：这个词是它的值，不拆
        let is_value = k > 0 && takes_value.contains(&args[k - 1].as_str());
        if !cluster || is_value {
            v.push(a.clone());
            continue;
        }
        for (j, c) in a[1..].char_indices() {
            let flag = format!("-{c}");
            let takes = takes_value.contains(&flag.as_str());
            v.push(flag);
            let tail = &a[1 + j + c.len_utf8()..];
            if takes {
                if !tail.is_empty() {
                    v.push(tail.to_string());
                }
                break;
            }
        }
    }
    v
}

fn compose(prog: &str, args: &[String], s: &mut ChangeSummary) {
    let file = opt_value(args, &["-f", "--file"]);
    let p = positionals(args, &["-f", "--file", "-p", "--project-name", "--env-file", "--profile", "--project-directory", "-t", "--timeout"]);
    let Some(sub) = p.first() else { return };
    let verb = match *sub {
        "up" => "up",
        "down" => "down",
        "restart" => "restart",
        "stop" => "stop",
        "start" => "start",
        "rm" => "rm",
        "pull" => "pull",
        _ => return,
    };
    let services: Vec<&str> = p.iter().skip(1).copied().collect();
    let mut line = format!("{prog} {verb}");
    if !services.is_empty() {
        line.push(' ');
        line.push_str(&services.join(" "));
    }
    if let Some(f) = file {
        line.push_str(&crate::tr!("（{f}）", " ({f})"));
    }
    push(&mut s.containers, line);
}

/// 选项的值（`--name x` / `--name=x`；短选项 `-tfoo` 也认）
fn opt_value(args: &[String], keys: &[&str]) -> Option<String> {
    opt_values(args, keys).into_iter().next()
}

fn opt_values(args: &[String], keys: &[&str]) -> Vec<String> {
    let mut v = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if keys.contains(&a) {
            if let Some(n) = args.get(i + 1) {
                v.push(n.clone());
            }
            i += 2;
            continue;
        }
        if let Some((k, val)) = a.split_once('=') {
            if keys.contains(&k) {
                v.push(val.to_string());
            }
        }
        i += 1;
    }
    v
}

fn remote_name(url: &str) -> String {
    let base = url.split(['?', '#']).next().unwrap_or(url);
    let name = base.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    if name.is_empty() || name.contains(':') {
        "index.html".to_string()
    } else {
        name.to_string()
    }
}

fn curl(args: &[String], s: &mut ChangeSummary) {
    let mut out: Option<String> = None;
    let mut remote = false;
    let mut urls = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "-o" || a == "--output" {
            out = args.get(i + 1).cloned();
            i += 2;
            continue;
        }
        if let Some(v) = a.strip_prefix("--output=") {
            out = Some(v.to_string());
        } else if a == "-O" || a == "--remote-name" || a == "--remote-name-all" {
            remote = true;
        } else if a.starts_with('-') && !a.starts_with("--") && a.len() > 1 {
            // 组合短选项：`-sSLo file` / `-fsSLO` / `-ofile`
            let flags = &a[1..];
            if let Some(pos) = flags.find('o') {
                let after = &flags[pos + 1..];
                if after.is_empty() {
                    out = args.get(i + 1).cloned();
                    i += 2;
                    continue;
                }
                out = Some(after.to_string());
            } else if flags.contains('O') {
                remote = true;
            }
            // 其它带值短选项（-H / -d / -u / -X …）吞掉下一个词
            if flags.ends_with(['H', 'd', 'u', 'X', 'A', 'e', 'b', 'c', 'T', 'F', 'x', 'w', 'm', 'r', 'E', 'K', 'C', 'z', 'Y', 'y']) {
                i += 2;
                continue;
            }
        } else if matches!(a, "--header" | "--data" | "--data-raw" | "--data-binary" | "--user" | "--request" | "--user-agent" | "--proxy" | "--max-time" | "--connect-timeout" | "--retry" | "--cacert" | "--cert" | "--key" | "--form" | "--upload-file" | "--write-out" | "--cookie" | "--referer") {
            i += 2;
            continue;
        } else if is_url(a) {
            urls.push(a.to_string());
        }
        i += 1;
    }
    let Some(url) = urls.first() else { return };
    let dest = match out {
        Some(o) if o != "-" && is_real_file(&o) => o,
        Some(_) => return,
        None if remote => remote_name(url),
        None => return,
    };
    push(&mut s.downloads, format!("{} → {dest}", shown_url(url)));
}

fn wget(args: &[String], s: &mut ChangeSummary) {
    let mut out: Option<String> = None;
    let mut dir: Option<String> = None;
    let mut urls = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "-O" || a == "--output-document" {
            out = args.get(i + 1).cloned();
            i += 2;
            continue;
        }
        if a == "-P" || a == "--directory-prefix" {
            dir = args.get(i + 1).cloned();
            i += 2;
            continue;
        }
        if let Some(v) = a.strip_prefix("--output-document=") {
            out = Some(v.to_string());
        } else if let Some(v) = a.strip_prefix("--directory-prefix=") {
            dir = Some(v.to_string());
        } else if a.starts_with('-') && !a.starts_with("--") && a.len() > 1 {
            // 组合短选项：`-qO-` / `-qO file` / `-O/tmp/x`
            let flags = &a[1..];
            if let Some(pos) = flags.find('O') {
                let after = &flags[pos + 1..];
                if after.is_empty() {
                    out = args.get(i + 1).cloned();
                    i += 2;
                    continue;
                }
                out = Some(after.to_string());
            } else if flags.ends_with(['o', 'a', 'e', 't', 'T', 'U', 'i']) {
                i += 2;
                continue;
            }
        } else if matches!(a, "--header" | "--user" | "--password" | "--user-agent" | "--tries" | "--timeout") {
            i += 2;
            continue;
        } else if is_url(a) {
            urls.push(a.to_string());
        }
        i += 1;
    }
    let Some(url) = urls.first() else { return };
    let dest = match out {
        Some(o) if o == "-" || !is_real_file(&o) => return,
        Some(o) => o,
        None => {
            let name = remote_name(url);
            match dir {
                Some(d) => format!("{}/{name}", d.trim_end_matches('/')),
                None => name,
            }
        }
    };
    push(&mut s.downloads, format!("{} → {dest}", shown_url(url)));
}

#[cfg(test)]
mod tests {
    use super::super::super::decision::DecisionAction;
    use super::*;

    fn sum(cmd: &str) -> ChangeSummary {
        let mut s = ChangeSummary::default();
        analyze_script(cmd, &mut s);
        s
    }

    fn rec(contract: &str, kind: &str, text: &str) -> DecisionRecord {
        DecisionRecord {
            seq: 0,
            ts_ms: 0,
            contract_id: contract.into(),
            console_id: "t".into(),
            situation_signature: String::new(),
            situation_hash: String::new(),
            ctx_refs: vec![],
            candidates: vec![],
            chosen: DecisionAction { kind: kind.into(), text: text.into(), option_index: None, injectable: true, submit: Default::default() },
            rationale: String::new(),
            confidence: 1.0,
            tier: "auto".into(),
            llm_used: true,
            outcome: "applied".into(),
            escalation: None,
            receipt: None,
        }
    }

    #[test]
    fn packages_install_and_remove() {
        let s = sum("sudo apt-get install -y nginx curl && sudo apt remove --purge apache2");
        assert_eq!(s.packages, vec!["安装 nginx（apt-get）", "安装 curl（apt-get）", "卸载 apache2（apt）"]);
        let s = sum("DEBIAN_FRONTEND=noninteractive apt-get -o Dpkg::Options::=--force-confold install -y redis-server");
        assert_eq!(s.packages, vec!["安装 redis-server（apt-get）"]);
        let s = sum("yum install -y git; dnf erase vim; apk add --no-cache bash; brew install jq");
        assert_eq!(s.packages, vec!["安装 git（yum）", "卸载 vim（dnf）", "安装 bash（apk）", "安装 jq（brew）"]);
        assert!(sum("apt update && apt list --installed").packages.is_empty());
    }

    #[test]
    fn pip_npm_cargo() {
        let s = sum("pip install -U requests==2.31 -i https://mirror/simple flask");
        assert_eq!(s.packages, vec!["安装 requests==2.31（pip）", "安装 flask（pip）"]);
        let s = sum("python3 -m pip install -r requirements.txt");
        assert_eq!(s.packages, vec!["安装 requirements.txt 里的依赖（pip）"]);
        let s = sum("pip3 uninstall -y numpy");
        assert_eq!(s.packages, vec!["卸载 numpy（pip3）"]);
        let s = sum("npm i -g pm2 && npm uninstall left-pad && npm install");
        assert_eq!(s.packages, vec!["安装 pm2（npm）", "卸载 left-pad（npm）", "安装 package.json 里的依赖（npm）"]);
        let s = sum("pnpm add -D vite; yarn add react");
        assert_eq!(s.packages, vec!["安装 vite（pnpm）", "安装 react（yarn）"]);
        let s = sum("cargo install ripgrep --version 14.0.0 && cargo uninstall bat");
        assert_eq!(s.packages, vec!["安装 ripgrep（cargo）", "卸载 bat（cargo）"]);
        assert!(sum("npm run build && npm test && cargo build --release").packages.is_empty());
    }

    #[test]
    fn services() {
        let s = sum("sudo systemctl daemon-reload && sudo systemctl restart nginx.service && systemctl enable --now redis");
        assert_eq!(s.services, vec!["重启 nginx.service", "开机自启 redis", "启动 redis"]);
        let s = sum("systemctl status nginx; service mysql stop; systemctl stop a b");
        assert_eq!(s.services, vec!["停止 mysql", "停止 a", "停止 b"]);
    }

    #[test]
    fn ports_and_firewalls() {
        let s = sum("sudo ufw allow 80/tcp && ufw allow from 10.0.0.0/8 to any port 22 proto tcp && ufw deny 3306 && ufw delete allow 8080 && ufw enable");
        assert_eq!(
            s.ports,
            vec!["放行 80/tcp（ufw）", "放行 22/tcp，来源 10.0.0.0/8（ufw）", "拒绝 3306（ufw）", "删除规则：放行 8080（ufw）", "启用防火墙（ufw）"]
        );
        let s = sum("ufw allow from 10.0.0.0/8; ufw delete 3; ufw allow in on eth0 to any port 80; ufw allow in on eth0 443; ufw deny from 1.2.3.4 to 10.0.0.5");
        assert_eq!(
            s.ports,
            vec!["放行 全部端口，来源 10.0.0.0/8（ufw）", "删除 ufw 规则 3", "放行 80（ufw）", "放行 443（ufw）", "拒绝 全部端口，来源 1.2.3.4，目标 10.0.0.5（ufw）"]
        );
        let s = sum("iptables -P INPUT DROP; iptables -F; iptables -t nat -F POSTROUTING; iptables -X MYCHAIN");
        assert_eq!(s.ports, vec!["默认策略 INPUT → DROP（iptables）"]);
        assert_eq!(s.removals, vec!["清空规则（全部，iptables）", "清空规则（nat 表 POSTROUTING 链，iptables）", "删除自定义链（MYCHAIN 链，iptables）"]);
        let s = sum("firewall-cmd --permanent --add-port=8080/tcp && firewall-cmd --remove-port 9000/udp --permanent && firewall-cmd --reload");
        assert_eq!(s.ports, vec!["放行 8080/tcp（firewalld）", "关闭 9000/udp（firewalld）"]);
        let s = sum("iptables -A INPUT -p tcp --dport 443 -j ACCEPT; iptables -D INPUT -p tcp --dport 23 -j DROP; iptables -L");
        assert_eq!(s.ports, vec!["添加规则 INPUT 443/tcp → ACCEPT（iptables）", "删除规则 INPUT 23/tcp → DROP（iptables）"]);
        assert!(sum("ufw status verbose").ports.is_empty());
    }

    #[test]
    fn containers() {
        let s = sum("docker run -d --name web -p 8080:80 -v /srv:/usr/share/nginx/html:ro --restart=always nginx:1.25 nginx -g 'daemon off;'");
        assert_eq!(s.containers, vec!["运行 nginx:1.25（名为 web）"]);
        assert_eq!(s.ports, vec!["8080:80（docker 映射）"]);
        let s = sum("docker rm -f web old && docker compose -f deploy/compose.yml up -d && docker-compose down && docker pull redis:7");
        assert_eq!(s.containers, vec!["docker compose up（deploy/compose.yml）", "docker-compose down", "拉取镜像 redis:7"]);
        assert_eq!(s.removals, vec!["删除容器 web（docker）", "删除容器 old（docker）"]);
        // 成串的短选项带值：`-dp 80:80`、`-itp8080:80`
        let s = sum("docker run -dp 80:80 nginx; docker run -itp8080:80 --rm httpd:2 sh");
        assert_eq!(s.containers, vec!["运行 nginx", "运行 httpd:2"]);
        assert_eq!(s.ports, vec!["80:80（docker 映射）", "8080:80（docker 映射）"]);
        // 镜像删除不算容器
        let s = sum("docker image rm nginx:old && docker rmi a && docker container rm -f c1 && docker system prune -af && docker image prune");
        assert!(s.containers.is_empty());
        assert_eq!(
            s.removals,
            vec!["删除镜像 nginx:old（docker）", "删除镜像 a（docker）", "删除容器 c1（docker）", "清理未用的容器 / 镜像 / 网络（docker system prune）", "清理未用的镜像（docker image prune）"]
        );
        let s = sum("docker system df; docker image ls");
        assert!(s.containers.is_empty() && s.removals.is_empty());
        let s = sum("docker ps -a; docker logs web; docker build -t app:dev .");
        assert_eq!(s.containers, vec!["构建镜像 app:dev"]);
    }

    #[test]
    fn downloads() {
        let s = sum("curl -fsSL https://get.example.com/install.sh -o /tmp/install.sh && wget https://x.org/a/b.tar.gz?sig=SECRET");
        assert_eq!(s.downloads, vec!["https://get.example.com/install.sh → /tmp/install.sh", "https://x.org/a/b.tar.gz → b.tar.gz"]);
        let s = sum("curl -LO https://h/f.zip; wget -qO- https://h/s.sh | bash; wget -O /opt/x.bin https://h/x; wget -P /opt https://h/y.deb");
        assert_eq!(s.downloads, vec!["https://h/f.zip → f.zip", "https://h/x → /opt/x.bin", "https://h/y.deb → /opt/y.deb"]);
        let s = sum("curl -s https://api/x > out.json");
        assert_eq!(s.downloads, vec!["https://api/x → out.json"]);
        assert_eq!(s.files_written, vec!["out.json"]);
        assert!(sum("curl -s https://api/health").downloads.is_empty());
        assert!(sum("curl -H 'Authorization: x' -o - https://a/b").downloads.is_empty());
    }

    #[test]
    fn redirections_tee_and_sed() {
        let s = sum("echo 'x > y' > /etc/motd && cat a >> ~/.bashrc 2>/dev/null && make 2>&1 | tee build.log");
        assert_eq!(s.files_written, vec!["/etc/motd", "~/.bashrc", "build.log"]);
        let s = sum("echo 1 | sudo tee -a /etc/sysctl.conf > /dev/null; ls &> /tmp/ls.txt; cmd 2> err.log");
        assert_eq!(s.files_written, vec!["/etc/sysctl.conf", "/tmp/ls.txt", "err.log"]);
        let s = sum("sed -i 's/a/b/' /etc/nginx/nginx.conf && sed -i.bak -e 's/x/y/' f1 f2 && sed 's/a/b/' g");
        assert_eq!(s.files_written, vec!["/etc/nginx/nginx.conf", "f1", "f2"]);
        // 成串的短选项里带 i（含贴着的备份后缀）、-e 在串尾带值、--in-place=SUF
        let s = sum("sed -Ei 's/a/b/' h1; sed -ri.orig 's/a/b/' h2; sed -ni -e '1p' h3; sed -Ee 's/x/y/' -i h4; sed --in-place=.b --expression=s/a/b/ h5; sed -n 's/a/b/p' nope");
        assert_eq!(s.files_written, vec!["h1", "h2", "h3", "h4", "h5"]);
        // 比较运算 / 引号里的 > 不是重定向
        assert!(sum("test 1 -gt 0 && echo \"a>b\" && awk '$1 > 3'").files_written.is_empty());
    }

    #[test]
    fn files_written_covers_copies_and_skips_dev() {
        assert_eq!(files_written("cp a.txt /etc/app.conf && mv -f x y/"), vec!["/etc/app.conf", "y/"]);
        assert_eq!(files_written("sudo install -m 644 /tmp/f /etc/f"), vec!["/etc/f"]);
        assert_eq!(files_written("cp -t /srv a b"), vec!["/srv"]);
        assert_eq!(files_written("dd if=/dev/zero of=disk.img bs=1M count=1"), vec!["disk.img"]);
        assert!(files_written("ls > /dev/null 2>/dev/stderr; echo hi >&2; cp a").is_empty());
        assert_eq!(files_written("cat > /tmp/x <<EOF\nhi\nEOF"), vec!["/tmp/x"]);
    }

    #[test]
    fn multi_line_scripts_heredocs_and_sh_c() {
        let script = "set -e\ncat > /etc/app.conf <<'EOF'\napt install evil\nport=80\nEOF\nsystemctl restart app\n# apt install commented\napt-get install -y \\\n  htop";
        let s = sum(script);
        assert_eq!(s.files_written, vec!["/etc/app.conf"]);
        assert_eq!(s.services, vec!["重启 app"]);
        assert_eq!(s.packages, vec!["安装 htop（apt-get）"], "heredoc 正文和注释不当命令，续行接上");
        let s = sum("sudo bash -c 'apt-get install -y nginx && systemctl start nginx'");
        assert_eq!(s.packages, vec!["安装 nginx（apt-get）"]);
        assert_eq!(s.services, vec!["启动 nginx"]);
        let s = sum("cd /srv && (docker compose up -d || true) & sleep 1; if true; then systemctl stop x; fi");
        assert_eq!(s.containers, vec!["docker compose up"]);
        assert_eq!(s.services, vec!["停止 x"]);
        let s = sum("sudo -u deploy env A=1 nohup npm install express");
        assert_eq!(s.packages, vec!["安装 express（npm）"]);
        // sudo 成串的短选项带值 / 值贴着写 / 长选项
        for cmd in ["sudo -iu root apt install x", "sudo -u root apt install x", "sudo -g grp apt install x", "sudo -uroot apt install x", "sudo -E -H --user=root apt install x", "sudo --user root -- apt install x"] {
            assert_eq!(sum(cmd).packages, vec!["安装 x（apt）"], "{cmd}");
        }
    }

    #[test]
    fn arithmetic_shift_is_not_a_heredoc() {
        for script in ["echo $[1<<2]\napt install nginx", "(( x = 1 << 2 ))\napt install nginx", "if [[ a < b ]]; then echo; fi\napt install nginx", "echo $((1<<2))\napt install nginx"] {
            let s = sum(script);
            assert_eq!(s.packages, vec!["安装 nginx（apt）"], "{script}");
            assert!(s.files_written.is_empty(), "{script}");
        }
        // 写文件检测（files_written）同样受益
        assert_eq!(files_written("echo $[1<<2]\necho x > /etc/motd"), vec!["/etc/motd"]);
        // 真 heredoc 照旧
        assert!(sum("cat <<EOF\napt install evil\nEOF").packages.is_empty());
        assert_eq!(sum("((n++)) && apt install a").packages, vec!["安装 a（apt）"]);
    }

    #[test]
    fn summarize_records_filters_and_counts() {
        let rows = vec![
            rec("c1", "exec", "apt-get install -y nginx"),
            rec("c1", "exec", "（旁路）systemctl restart nginx\nufw allow 80"),
            rec("c1", "file_write", "写入 /etc/nginx/sites-enabled/app（120 字节）"),
            rec("c1", "file_write", "修改 ~/app/.env（全部替换）"),
            rec("c1", "file_write", "修改 src/main.rs"),
            rec("c1", "keys", "发送按键：<C-c>"),
            rec("c1", "open", "打开本地会话「x」并执行：docker run redis"),
            rec("c2", "exec", "apt install vim"),
            {
                let mut r = rec("c1", "exec", "apt install rejected-pkg");
                r.outcome = "rejected".into();
                r
            },
        ];
        let js: Vec<String> = rows.iter().map(|r| serde_json::to_string(r).unwrap()).collect();
        let s = from_json_rows("c1", js.iter().map(String::as_str).chain(["not json"]));
        assert_eq!(s.commands, 3);
        assert_eq!(s.files_written, vec!["/etc/nginx/sites-enabled/app", "~/app/.env", "src/main.rs"]);
        assert_eq!(s.packages, vec!["安装 nginx（apt-get）"]);
        assert_eq!(s.services, vec!["重启 nginx"]);
        assert_eq!(s.ports, vec!["放行 80（ufw）"]);
        assert_eq!(s.containers, vec!["运行 redis"]);
        assert!(!s.is_empty());
        assert!(from_json_rows("nobody", js.iter().map(String::as_str)).is_empty());
        // 序列化成 camelCase
        let v = serde_json::to_value(&s).unwrap();
        assert!(v.get("filesWritten").is_some());
    }

    #[test]
    fn package_specs_are_redacted() {
        let s = sum("pip install git+https://bob:ghp_SECRETTOKEN123@github.com/org/repo.git && pip install -r https://u:pw123@h/req.txt");
        assert!(s.packages.iter().all(|p| !p.contains("ghp_SECRETTOKEN123") && !p.contains("pw123")), "{:?}", s.packages);
        assert_eq!(s.packages.len(), 2);
        assert_eq!(sum("npm i lodash").packages, vec!["安装 lodash（npm）"]);
    }

    #[test]
    fn removals_rm_userdel_and_cap() {
        let s = sum("rm -rf /var/www/old && rm /tmp/a.sh /tmp/a.sh && rmdir /srv/empty && sudo userdel -r bob && apt purge nginx");
        assert_eq!(s.removals, vec!["删除 /var/www/old（递归）", "删除 /tmp/a.sh", "删除 /srv/empty", "删除用户 bob（连同主目录）"]);
        assert_eq!(s.packages, vec!["卸载 nginx（apt）"], "apt purge 仍归软件包");
        let many: Vec<String> = (0..40).map(|i| format!("/tmp/f{i}")).collect();
        let s = sum(&format!("rm -f {}", many.join(" ")));
        assert_eq!(s.removals.len(), MAX_REMOVALS + 1);
        assert_eq!(s.removals.last().unwrap(), MORE_REMOVALS);
        // 合并时上限照旧、「还有更多」留在最后
        let mut a = s.clone();
        a.merge(sum("rm /x"));
        assert_eq!(a.removals.len(), MAX_REMOVALS + 1);
        assert_eq!(a.removals.last().unwrap(), MORE_REMOVALS);
    }

    #[test]
    fn merge_unions_in_order_and_absorbs_only_new_rows() {
        let mut a = sum("apt install a; systemctl restart x; echo > f1");
        a.commands = 2;
        let mut b = sum("apt install b; apt install a; echo > f2; echo > f1");
        b.commands = 3;
        a.merge(b);
        assert_eq!(a.packages, vec!["安装 a（apt）", "安装 b（apt）"]);
        assert_eq!(a.files_written, vec!["f1", "f2"]);
        assert_eq!(a.services, vec!["重启 x"]);
        assert_eq!(a.commands, 5);
        // 按 seq 增量：已并过的不再算
        let r1 = serde_json::to_string(&rec("c1", "exec", "apt install a")).unwrap();
        let r2 = serde_json::to_string(&rec("c1", "exec", "apt install b")).unwrap();
        let other = serde_json::to_string(&rec("c2", "exec", "apt install z")).unwrap();
        let mut s = ChangeSummary { merged_seq: Some(0), ..Default::default() };
        assert!(s.absorb_rows("c1", [(1, r1.as_str()), (2, other.as_str())]));
        assert!(!s.absorb_rows("c1", [(1, r1.as_str())]));
        assert!(s.absorb_rows("c1", [(1, r1.as_str()), (5, r2.as_str())]));
        assert_eq!((s.commands, s.packages.len(), s.merged_seq), (2, 2, Some(5)));
        // 旧版本定格的（没有 merged_seq）不动
        let mut legacy = ChangeSummary::default();
        assert!(!legacy.absorb_rows("c1", [(9, r2.as_str())]));
        assert!(legacy.is_empty());
        // 旧的存档（没有 removals / mergedSeq 字段）能读
        let old: ChangeSummary = serde_json::from_str(r#"{"filesWritten":[],"commands":1,"packages":[],"services":[],"ports":[],"containers":[],"downloads":[]}"#).unwrap();
        assert_eq!((old.commands, old.merged_seq), (1, None));
    }

    #[test]
    fn duplicates_collapse() {
        let s = sum("systemctl restart a; systemctl restart a; echo > f; echo >> f");
        assert_eq!(s.services, vec!["重启 a"]);
        assert_eq!(s.files_written, vec!["f"]);
    }
}
