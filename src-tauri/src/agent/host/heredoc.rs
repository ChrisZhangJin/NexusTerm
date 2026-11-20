//! 「在可见终端里敲 heredoc」的识别与改道。
//!
//! 模型爱用 `cat > /tmp/gen.py <<'EOF'` 往文件里写多行内容：在交互 shell 里这要一行一行敲进续行提示
//! （`> `），读屏时代码行（如 `def gen_password():`）还会被认成密码提示，后面的动作全被拦住。
//! 可靠的通道本来就有：写文件用 `nx_write`（本地 / SFTP），跑多行命令用 `nx_run`（旁路执行）。
//!
//! 这里只做纯判断：一行命令里有没有 heredoc（`<<` / `<<-` 后跟分隔符，`<<<` 不算），
//! 以及它是不是「把正文写进文件」（`cat > f <<EOF` / `cat <<EOF > f` / `tee [-a] f <<EOF` /
//! `cat <<EOF | sudo tee f`）。写文件 → 建议 nx_write；其他（`ssh h <<EOF`、`psql <<SQL`、
//! `python3 - <<'PY'`）→ 建议 nx_run。改道话术由 [`redirect`] 拼好，直接作为 Deny 反馈给模型。

use super::super::act::AgentAction;
use super::super::interact;
use super::super::situation::{Program, Situation};
use super::super::steward_mode::typed_lines;

/// 一行命令里的 heredoc 是干什么用的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heredoc {
    /// 把正文写进文件（路径原样，可能是相对路径或 `~` 开头）；`append` = `>>` / `tee -a`
    File { path: String, append: bool },
    /// 喂给某个程序的标准输入（`ssh` / `psql` / `python3 -` …），`head` 是那个程序名
    Other { head: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word(String),
    /// 命令分隔：`;` `&&` `||` `&`
    Sep,
    /// 管道 `|`
    Pipe,
    /// 输出重定向（`>` `>>` `>|` `&>`，以及带 fd 的 `1>` `2>>`…）：(fd, 追加)
    Out(Option<u32>, bool),
    /// `<<` / `<<-`
    HereDoc,
    /// 其余不关心的重定向（`<` `<<<` `>&`），后面那个词一并吃掉
    Other,
}

/// 一行 shell 的粗切分：认引号、反斜杠、注释和上面那几种操作符；`$(( … ))` 整段跳过（里面的 `<<` 是位移）。
fn tokenize(line: &str) -> Vec<Tok> {
    let cs: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    // 当前词是否有内容（`''` 也算一个词）
    let mut has = false;
    let mut i = 0;
    macro_rules! flush {
        () => {
            if has {
                out.push(Tok::Word(std::mem::take(&mut cur)));
                has = false;
            }
        };
    }
    while i < cs.len() {
        let c = cs[i];
        let next = cs.get(i + 1).copied();
        match c {
            '\'' => {
                has = true;
                i += 1;
                while i < cs.len() && cs[i] != '\'' {
                    cur.push(cs[i]);
                    i += 1;
                }
                i += 1;
            }
            '"' => {
                has = true;
                i += 1;
                while i < cs.len() && cs[i] != '"' {
                    if cs[i] == '\\' && i + 1 < cs.len() {
                        i += 1;
                    }
                    cur.push(cs[i]);
                    i += 1;
                }
                i += 1;
            }
            '\\' => {
                has = true;
                if let Some(n) = next {
                    cur.push(n);
                }
                i += 2;
            }
            '$' if next == Some('(') && cs.get(i + 2) == Some(&'(') => {
                // 算术展开：跳到配对的 `))`
                has = true;
                let mut depth = 0i32;
                while i < cs.len() {
                    match cs[i] {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    cur.push(cs[i]);
                    i += 1;
                }
            }
            '#' if !has => break,
            ' ' | '\t' | '\r' | '\n' => {
                flush!();
                i += 1;
            }
            ';' => {
                flush!();
                out.push(Tok::Sep);
                i += 1;
            }
            '|' => {
                flush!();
                if next == Some('|') {
                    out.push(Tok::Sep);
                    i += 2;
                } else {
                    out.push(Tok::Pipe);
                    i += 1;
                }
            }
            '&' => {
                flush!();
                if next == Some('&') {
                    out.push(Tok::Sep);
                    i += 2;
                } else if next == Some('>') {
                    let append = cs.get(i + 2) == Some(&'>');
                    out.push(Tok::Out(None, append));
                    i += if append { 3 } else { 2 };
                } else {
                    out.push(Tok::Sep);
                    i += 1;
                }
            }
            '>' => {
                // 紧贴在前面的纯数字是 fd（`2>`）
                let fd = if has && !cur.is_empty() && cur.chars().all(|d| d.is_ascii_digit()) {
                    let fd = cur.parse().ok();
                    cur.clear();
                    has = false;
                    fd
                } else {
                    flush!();
                    None
                };
                if next == Some('&') {
                    // `>&2` / `2>&1`：复制 fd，不是文件
                    out.push(Tok::Other);
                    i += 2;
                } else if next == Some('>') {
                    out.push(Tok::Out(fd, true));
                    i += 2;
                } else {
                    out.push(Tok::Out(fd, false));
                    i += if next == Some('|') { 2 } else { 1 };
                }
            }
            '<' => {
                flush!();
                if next == Some('<') && cs.get(i + 2) == Some(&'<') {
                    out.push(Tok::Other); // here-string
                    i += 3;
                } else if next == Some('<') {
                    out.push(Tok::HereDoc);
                    i += if cs.get(i + 2) == Some(&'-') { 3 } else { 2 };
                } else {
                    out.push(Tok::Other);
                    i += 1;
                }
            }
            _ => {
                has = true;
                cur.push(c);
                i += 1;
            }
        }
    }
    if has {
        out.push(Tok::Word(cur));
    }
    out
}

/// 一条简单命令：参数词、输出重定向目标、有没有 heredoc。
#[derive(Default)]
struct Simple {
    words: Vec<String>,
    outs: Vec<(String, bool)>,
    heredoc: bool,
}

/// 按 `;` `&&` `||` 切成若干管道，每条管道按 `|` 切成简单命令。
fn pipelines(toks: Vec<Tok>) -> Vec<Vec<Simple>> {
    let mut all = vec![vec![Simple::default()]];
    let mut it = toks.into_iter().peekable();
    while let Some(t) = it.next() {
        let cmd = all.last_mut().unwrap().last_mut().unwrap();
        match t {
            Tok::Word(w) => cmd.words.push(w),
            Tok::Sep => all.push(vec![Simple::default()]),
            Tok::Pipe => all.last_mut().unwrap().push(Simple::default()),
            Tok::Out(fd, append) => {
                if let Some(Tok::Word(w)) = it.peek().cloned() {
                    it.next();
                    if fd.is_none() || fd == Some(1) {
                        cmd.outs.push((w, append));
                    }
                }
            }
            Tok::HereDoc => {
                // 分隔符要像个词（字母 / 下划线开头）；`1 << 3` 这类不是 heredoc
                if let Some(Tok::Word(w)) = it.peek().cloned() {
                    if w.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
                        cmd.heredoc = true;
                    }
                    it.next();
                }
            }
            Tok::Other => {
                if matches!(it.peek(), Some(Tok::Word(_))) {
                    it.next();
                }
            }
        }
    }
    all
}

/// 去掉 `sudo` / `env` / `VAR=x` 这类前缀后的程序名（取 basename）与其参数。
fn head_and_args(words: &[String]) -> Option<(String, &[String])> {
    let mut i = 0;
    while i < words.len() {
        let w = words[i].as_str();
        let base = w.rsplit('/').next().unwrap_or(w);
        if w.contains('=') && !w.starts_with('-') && !w.starts_with('=') {
            i += 1; // 环境变量赋值
        } else if matches!(base, "sudo" | "doas") {
            i += 1;
            while i < words.len() && words[i].starts_with('-') {
                // 带参数的选项：-u user / -g group / -C n / -h host / -p prompt
                let takes = matches!(words[i].as_str(), "-u" | "-g" | "-C" | "-h" | "-p" | "-U" | "-r" | "-t");
                i += if takes { 2 } else { 1 };
            }
        } else if matches!(base, "env" | "command" | "nohup" | "exec" | "time" | "stdbuf") {
            i += 1;
            while i < words.len() && words[i].starts_with('-') {
                i += 1;
            }
        } else {
            return Some((base.to_string(), &words[i + 1..]));
        }
    }
    None
}

/// 写到这些地方不算「写文件」
fn is_sink(p: &str) -> bool {
    matches!(p, "/dev/null" | "/dev/stdout" | "/dev/stderr" | "/dev/tty" | "-")
}

/// 一行命令里的 heredoc：没有返回 `None`。只看这一行（heredoc 的起始行）。
pub fn detect(line: &str) -> Option<Heredoc> {
    for pipe in pipelines(tokenize(line)) {
        let Some(at) = pipe.iter().position(|c| c.heredoc) else { continue };
        // heredoc 所在的命令自己就是 `cat > f` / `tee f`，或者下游管道里有 `tee f`
        for (k, cmd) in pipe.iter().enumerate().skip(at) {
            let Some((head, args)) = head_and_args(&cmd.words) else { continue };
            if head == "cat" && (k == at || args.is_empty()) {
                if let Some((p, append)) = cmd.outs.iter().find(|(p, _)| !is_sink(p)) {
                    return Some(Heredoc::File { path: p.clone(), append: *append });
                }
            }
            if head == "tee" {
                let append = args.iter().any(|a| a == "-a" || a == "--append" || (a.starts_with('-') && !a.starts_with("--") && a.contains('a')));
                if let Some(p) = args.iter().find(|a| !a.starts_with('-') && !is_sink(a)) {
                    return Some(Heredoc::File { path: p.clone(), append });
                }
            }
        }
        let head = head_and_args(&pipe[at].words).map(|(h, _)| h).unwrap_or_else(|| "sh".to_string());
        return Some(Heredoc::Other { head });
    }
    None
}

/// 只要「写文件」那种：返回目标路径。
pub fn heredoc_write(cmd: &str) -> Option<String> {
    cmd.lines().find_map(|l| match detect(l) {
        Some(Heredoc::File { path, .. }) => Some(path),
        _ => None,
    })
}

/// 改道话术：告诉模型这次没执行、该换哪个工具。`file_write_denied` = 契约禁止写 / 改文件：
/// 这时不指路到 nx_write / shell 写文件（那些同样会被拒），直接说这一步不能做。
pub fn feedback(h: &Heredoc, file_write_denied: bool) -> String {
    match h {
        Heredoc::File { path, .. } if file_write_denied => format!(
            "没有执行：这是用 heredoc 往文件「{path}」里写内容，而契约禁止写 / 改文件（file_write）—— \
             不论用 nx_write / nx_edit 还是 shell（重定向、tee、cp …）都一样不许。换一个不改文件的做法；\
             这一步确实是目标必需的，就用 nx_ask_user 请用户放开（不要给 recommendation，契约只有用户能改）"
        ),
        Heredoc::File { path, append } => {
            let how = if *append {
                format!(
                    "要追加：先 nx_read「{path}」，再用 nx_edit 在末尾补上，或把新内容 nx_write 到临时文件后用 nx_run 执行 `cat 临时文件 >> {path}`"
                )
            } else {
                format!("调用 nx_write，path 填「{path}」，content 填 heredoc 里的正文（不含结尾的分隔符那一行）")
            };
            format!(
                "没有执行：这是用 heredoc 往文件「{path}」里写内容。在可见终端里逐行敲 heredoc 很容易被误判\
                 （续行提示、代码行被当成密码 / 交互提示），会卡住后面的每一步。请改用文件工具：{how}。\
                 nx_write 报权限不足（系统目录要 sudo）时，先 nx_write 到 /tmp 下，再用 nx_run 执行 \
                 `sudo install -m 644 /tmp/<文件> {path}`（nx_run 不可用就用 nx_exec 跑这一行）。\
                 heredoc 前后真正要执行的命令部分用 nx_run。"
            )
        }
        Heredoc::Other { head } => format!(
            "没有执行：这条命令带 heredoc（`{head} <<…`），在可见终端里逐行敲多行输入很容易被误判、卡住后面的步骤。\
             请改用 nx_run 一次跑完（它接受多行命令，heredoc 原样写在 command 里）；\
             nx_run 不可用时，先用 nx_write 把这段输入写成文件（如 /tmp/nx-input.txt），\
             再 nx_exec 执行 `{head} … < /tmp/nx-input.txt`。"
        ),
    }
}

/// 门禁卡上给人看的一句话（给模型的完整话术见 [`feedback`]）。
pub fn card_reason(h: &Heredoc) -> String {
    match h {
        Heredoc::File { path, .. } => format!(
            "在终端里逐行敲 heredoc 写文件容易被读屏误判、卡住后面的步骤，没有执行；已请 AI 改用 nx_write 直接写「{path}」"
        ),
        Heredoc::Other { head } => format!(
            "在终端里逐行敲带 heredoc 的命令（{head} <<…）容易被读屏误判，没有执行；已请 AI 改用 nx_run 旁路执行"
        ),
    }
}

/// 动作级判断：`nx_exec` 的命令、`nx_send_keys` 真正提交（带回车）的那几行里，有没有 heredoc。
/// 有就返回改道话术（调用方不执行，作为拒绝原因反馈给模型）。
///
/// 按键只在终端前台是 shell（或认不出）时才管：往 vim / Claude Code 输入框里打一段含 `<<` 的文字不是在敲 heredoc。
pub fn redirect(a: &AgentAction, sit: Option<&Situation>) -> Option<String> {
    find(a, sit).map(|h| feedback(&h, false))
}

/// 同 [`redirect`]，返回识别结果本身（拼卡片 / 话术由调用方决定）。
pub fn find(a: &AgentAction, sit: Option<&Situation>) -> Option<Heredoc> {
    let lines: Vec<String> = match a {
        AgentAction::Exec { command, .. } => command.lines().map(str::to_string).collect(),
        AgentAction::Keys { keys, .. } => {
            if sit.is_some_and(|s| !matches!(s.program, Program::Shell | Program::Ssh | Program::Unknown)) {
                return None;
            }
            // 只看已经提交（后面跟了回车 / 换行）的行；最后一段没回车 = 还只是敲在输入行上
            let bytes = interact::parse_keys(keys).ok()?;
            let end = bytes.iter().rposition(|b| matches!(b, b'\r' | b'\n'))?;
            typed_lines(&bytes[..=end])
        }
        _ => return None,
    };
    lines.iter().find_map(|l| detect(l))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(p: &str) -> Option<Heredoc> {
        Some(Heredoc::File { path: p.into(), append: false })
    }
    fn other(h: &str) -> Option<Heredoc> {
        Some(Heredoc::Other { head: h.into() })
    }

    #[test]
    fn file_writing_heredocs_are_recognised() {
        assert_eq!(detect("cat > /etc/x.json <<'EOF'"), file("/etc/x.json"));
        assert_eq!(detect("cat <<EOF > f"), file("f"));
        assert_eq!(detect("cat >/tmp/gen.py <<-\"PY\""), file("/tmp/gen.py"));
        assert_eq!(detect("cat << 'EOF' >> ~/.bashrc"), Some(Heredoc::File { path: "~/.bashrc".into(), append: true }));
        assert_eq!(detect("tee -a f <<EOF"), Some(Heredoc::File { path: "f".into(), append: true }));
        assert_eq!(detect("sudo tee f >/dev/null <<EOF"), file("f"));
        assert_eq!(detect("sudo -u www tee /srv/a.conf > /dev/null <<'EOF'"), file("/srv/a.conf"));
        assert_eq!(detect("cat <<'EOF' | sudo tee /etc/nginx/conf.d/a.conf"), file("/etc/nginx/conf.d/a.conf"));
        assert_eq!(detect("mkdir -p d && cat > d/x.yml << \\EOF"), file("d/x.yml"));
        assert_eq!(heredoc_write("cat > /tmp/gen.py <<'EOF'\ndef gen_password():\nEOF"), Some("/tmp/gen.py".into()));
    }

    #[test]
    fn other_heredocs_suggest_nx_run() {
        assert_eq!(detect("ssh h <<EOF"), other("ssh"));
        assert_eq!(detect("python3 - <<'PY'"), other("python3"));
        assert_eq!(detect("psql -U app <<SQL"), other("psql"));
        // 程序输出落文件不是「heredoc 写文件」：正文是给 python 的代码
        assert_eq!(detect("python3 - <<'PY' > out.json"), other("python3"));
        assert_eq!(detect("cat <<EOF"), other("cat"));
        assert_eq!(detect("cat <<EOF 2>/tmp/err"), other("cat"));
        assert_eq!(heredoc_write("ssh h <<EOF"), None);
    }

    #[test]
    fn non_heredocs_are_left_alone() {
        for c in [
            "cat f",
            "cat f > g",
            "echo $((1 << 3))",
            "echo $((x<<y))",
            "grep '<<EOF' notes.md",
            "echo \"a << b\"",
            "tr a b <<< hello",
            "sort < in.txt > out.txt",
            "ls # cat > f <<EOF",
            "echo 1 <<2",
        ] {
            assert_eq!(detect(c), None, "{c}");
        }
    }

    #[test]
    fn redirect_covers_exec_and_submitted_keys_only() {
        let exec = |c: &str| AgentAction::Exec { target: None, command: c.into() };
        let keys = |k: &str| AgentAction::Keys { target: None, keys: k.into() };
        let fb = redirect(&exec("cat > /etc/x.json <<'EOF'"), None).unwrap();
        assert!(fb.contains("nx_write") && fb.contains("/etc/x.json"), "{fb}");
        let fb = redirect(&exec("ssh h <<EOF"), None).unwrap();
        assert!(fb.contains("nx_run") && !fb.contains("path 填"), "{fb}");
        assert!(redirect(&exec("cat f"), None).is_none());
        // 按键：多行粘贴一整段 heredoc
        let fb = redirect(&keys("cat > /tmp/gen.py <<'EOF'\\rdef gen_password():\\r    pass\\rEOF\\r"), None).unwrap();
        assert!(fb.contains("/tmp/gen.py"), "{fb}");
        // 没回车：只是敲字，没提交
        assert!(redirect(&keys("cat > f <<EOF"), None).is_none());
        // 前台不是 shell（Claude Code 输入框）：不管
        let mut s = super::super::super::situation::frames::classify_lines(super::super::super::situation::frames::CC_TRUST_V2);
        s.program = Program::ClaudeCode;
        assert!(redirect(&keys("cat > f <<EOF<Enter>"), Some(&s)).is_none());
        s.program = Program::Shell;
        assert!(redirect(&keys("cat > f <<EOF<Enter>"), Some(&s)).is_some());
        // 其他动作不管
        assert!(redirect(&AgentAction::Screen { target: None }, None).is_none());
        // 门禁卡上的一句话：说清没执行、改用哪个工具
        let h = find(&exec("cat > /etc/x.json <<'EOF'"), None).unwrap();
        assert!(card_reason(&h).contains("nx_write") && card_reason(&h).contains("/etc/x.json"));
        assert!(card_reason(&find(&exec("psql <<SQL"), None).unwrap()).contains("nx_run"));
        // 契约禁止写文件：不指路到 nx_write / shell 写文件
        let fb = feedback(&h, true);
        assert!(fb.contains("file_write") && !fb.contains("调用 nx_write") && !fb.contains("sudo install") && !fb.contains(">>"), "{fb}");
    }
}
