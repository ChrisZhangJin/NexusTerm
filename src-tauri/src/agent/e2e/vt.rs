//! 端到端回放用的极简终端模拟器：把 PTY 字节流渲染成「前端 xterm 会推上来的那一屏」。
//!
//! 依赖树里没有 vt100 / alacritty_terminal，又不值得为测试拉一个进来；托管场景里出现的
//! 东西（bash readline、cat / echo 输出、clear、python / 嵌套 shell 的提示符）用下面这些就够：
//!
//! - 可打印字符（每个 char 占一格；**不处理宽字符**：中文占 1 格而不是 2 格，行尾折行会比 xterm 晚）
//! - CR / LF / BS / TAB / BEL，行尾自动折行，底部滚屏（无滚动区：`CSI r` 被忽略）
//! - CSI：`A B C D E F G H f d J K P @ X L M`、`m`（颜色丢掉）、`?1h/l`（DECCKM）、
//!   `?47 / ?1047 / ?1049 h/l`（alt screen，切回时恢复主屏）、其余 `h/l` 忽略
//! - ESC 7 / 8（保存 / 恢复光标）、ESC c（重置）；OSC（标题等）整段跳过；其余 ESC x 吞掉一个字符
//!
//! 已知局限：没有宽字符、没有滚动区、没有 tab stop 设置、没有插入模式、没有字符集切换。
//! 全屏 TUI（vim / tmux / Claude Code）画出来的东西可能错位 —— 需要它们的场景请先补这里。

pub struct Vt {
    pub cols: usize,
    pub rows: usize,
    grid: Vec<Vec<char>>,
    pub cx: usize,
    pub cy: usize,
    saved: (usize, usize),
    pub alt: bool,
    /// 进 alt screen 前的主屏（含光标）
    main_backup: Option<(Vec<Vec<char>>, usize, usize)>,
    pub app_cursor: bool,
    /// 主屏滚出去的行数（= 抓屏窗口的绝对起点）
    pub scrolled: u64,
    /// 已滚出屏幕的历史（只给调试 / 断言用）
    pub history: Vec<String>,
    state: State,
    params: String,
    utf8: Vec<u8>,
    /// 上一个字符写在了最后一列：下一个可打印字符先折行（xterm 的「延迟折行」）
    wrap_pending: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
    /// ESC ( / ESC ) 之类带一个参数字节的序列
    EscArg,
}

impl Vt {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            cols,
            rows,
            grid: vec![vec![' '; cols]; rows],
            cx: 0,
            cy: 0,
            saved: (0, 0),
            alt: false,
            main_backup: None,
            app_cursor: false,
            scrolled: 0,
            history: Vec::new(),
            state: State::Ground,
            params: String::new(),
            utf8: Vec::new(),
            wrap_pending: false,
        }
    }

    /// 当前屏幕，每行去掉行尾空格（与前端 `translateToString(true)` 一致）。
    pub fn lines(&self) -> Vec<String> {
        self.grid.iter().map(|r| r.iter().collect::<String>().trim_end().to_string()).collect()
    }

    /// 整屏文本（调试输出用）。
    pub fn text(&self) -> String {
        self.lines().join("\n")
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.byte(b);
        }
    }

    fn byte(&mut self, b: u8) {
        // UTF-8 多字节只在 Ground 态出现；拼完一个字符再交给 put
        if self.state == State::Ground && (b >= 0x80 || !self.utf8.is_empty()) {
            self.utf8.push(b);
            match std::str::from_utf8(&self.utf8) {
                Ok(s) => {
                    let c = s.chars().next().unwrap_or('?');
                    self.utf8.clear();
                    self.put(c);
                }
                Err(e) if e.error_len().is_some() || self.utf8.len() >= 4 => {
                    self.utf8.clear();
                    self.put('\u{fffd}');
                }
                Err(_) => {}
            }
            return;
        }
        match self.state {
            State::Ground => self.ground(b),
            State::Esc => self.esc(b),
            State::EscArg => self.state = State::Ground,
            State::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    let params = std::mem::take(&mut self.params);
                    self.state = State::Ground;
                    self.csi(&params, b as char);
                } else {
                    self.params.push(b as char);
                }
            }
            State::Osc => match b {
                0x07 => self.state = State::Ground,
                0x1b => self.state = State::OscEsc,
                _ => {}
            },
            State::OscEsc => self.state = if b == b'\\' { State::Ground } else { State::Osc },
        }
    }

    fn ground(&mut self, b: u8) {
        match b {
            0x1b => self.state = State::Esc,
            b'\r' => {
                self.cx = 0;
                self.wrap_pending = false;
            }
            b'\n' | 0x0b | 0x0c => {
                self.wrap_pending = false;
                self.linefeed();
            }
            0x08 => {
                self.wrap_pending = false;
                self.cx = self.cx.saturating_sub(1);
            }
            b'\t' => {
                self.cx = ((self.cx / 8) + 1) * 8;
                if self.cx >= self.cols {
                    self.cx = self.cols - 1;
                }
            }
            0x00..=0x1f | 0x7f => {}
            _ => self.put(b as char),
        }
    }

    fn esc(&mut self, b: u8) {
        self.state = State::Ground;
        match b {
            b'[' => {
                self.params.clear();
                self.state = State::Csi;
            }
            b']' => self.state = State::Osc,
            b'7' => self.saved = (self.cx, self.cy),
            b'8' => (self.cx, self.cy) = self.saved,
            b'c' => {
                let (c, r) = (self.cols, self.rows);
                *self = Vt::new(c, r);
            }
            b'D' => self.linefeed(),
            b'M' => {
                if self.cy == 0 {
                    self.grid.pop();
                    self.grid.insert(0, vec![' '; self.cols]);
                } else {
                    self.cy -= 1;
                }
            }
            b'(' | b')' | b'*' | b'+' | b'#' | b'%' => self.state = State::EscArg,
            _ => {}
        }
    }

    fn put(&mut self, c: char) {
        if self.wrap_pending {
            self.wrap_pending = false;
            self.cx = 0;
            self.linefeed();
        }
        self.grid[self.cy][self.cx] = c;
        if self.cx + 1 >= self.cols {
            self.wrap_pending = true;
        } else {
            self.cx += 1;
        }
    }

    fn linefeed(&mut self) {
        if self.cy + 1 >= self.rows {
            let gone = self.grid.remove(0);
            if !self.alt {
                self.scrolled += 1;
                self.history.push(gone.iter().collect::<String>().trim_end().to_string());
            }
            self.grid.push(vec![' '; self.cols]);
        } else {
            self.cy += 1;
        }
    }

    fn clear_rows(&mut self, from: usize, to: usize) {
        for r in from..to.min(self.rows) {
            self.grid[r] = vec![' '; self.cols];
        }
    }

    fn csi(&mut self, params: &str, f: char) {
        let private = params.starts_with('?') || params.starts_with('>') || params.starts_with('=');
        let body = params.trim_start_matches(['?', '>', '=']);
        let nums: Vec<usize> = body.split(';').map(|p| p.parse().unwrap_or(0)).collect();
        let n = |i: usize, d: usize| nums.get(i).copied().filter(|v| *v > 0).unwrap_or(d);
        self.wrap_pending = false;
        match f {
            'A' => self.cy = self.cy.saturating_sub(n(0, 1)),
            'B' => self.cy = (self.cy + n(0, 1)).min(self.rows - 1),
            'C' => self.cx = (self.cx + n(0, 1)).min(self.cols - 1),
            'D' => self.cx = self.cx.saturating_sub(n(0, 1)),
            'E' => {
                self.cy = (self.cy + n(0, 1)).min(self.rows - 1);
                self.cx = 0;
            }
            'F' => {
                self.cy = self.cy.saturating_sub(n(0, 1));
                self.cx = 0;
            }
            'G' | '`' => self.cx = (n(0, 1) - 1).min(self.cols - 1),
            'd' => self.cy = (n(0, 1) - 1).min(self.rows - 1),
            'H' | 'f' => {
                self.cy = (n(0, 1) - 1).min(self.rows - 1);
                self.cx = (n(1, 1) - 1).min(self.cols - 1);
            }
            'J' => match nums.first().copied().unwrap_or(0) {
                0 => {
                    for x in self.cx..self.cols {
                        self.grid[self.cy][x] = ' ';
                    }
                    self.clear_rows(self.cy + 1, self.rows);
                }
                1 => {
                    self.clear_rows(0, self.cy);
                    for x in 0..=self.cx.min(self.cols - 1) {
                        self.grid[self.cy][x] = ' ';
                    }
                }
                _ => self.clear_rows(0, self.rows),
            },
            'K' => {
                let row = &mut self.grid[self.cy];
                let (a, b) = match nums.first().copied().unwrap_or(0) {
                    0 => (self.cx, self.cols),
                    1 => (0, self.cx + 1),
                    _ => (0, self.cols),
                };
                for x in a..b.min(self.cols) {
                    row[x] = ' ';
                }
            }
            'P' => {
                let row = &mut self.grid[self.cy];
                for _ in 0..n(0, 1).min(self.cols - self.cx) {
                    row.remove(self.cx);
                    row.push(' ');
                }
            }
            '@' => {
                let row = &mut self.grid[self.cy];
                for _ in 0..n(0, 1).min(self.cols - self.cx) {
                    row.pop();
                    row.insert(self.cx, ' ');
                }
            }
            'X' => {
                for x in self.cx..(self.cx + n(0, 1)).min(self.cols) {
                    self.grid[self.cy][x] = ' ';
                }
            }
            'L' => {
                for _ in 0..n(0, 1).min(self.rows - self.cy) {
                    self.grid.pop();
                    self.grid.insert(self.cy, vec![' '; self.cols]);
                }
            }
            'M' => {
                for _ in 0..n(0, 1).min(self.rows - self.cy) {
                    self.grid.remove(self.cy);
                    self.grid.push(vec![' '; self.cols]);
                }
            }
            's' if !private => self.saved = (self.cx, self.cy),
            'u' if !private => (self.cx, self.cy) = self.saved,
            'h' | 'l' if private => {
                let on = f == 'h';
                for m in &nums {
                    match m {
                        1 => self.app_cursor = on,
                        47 | 1047 | 1049 => self.set_alt(on),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn set_alt(&mut self, on: bool) {
        if on == self.alt {
            return;
        }
        if on {
            self.main_backup = Some((std::mem::replace(&mut self.grid, vec![vec![' '; self.cols]; self.rows]), self.cx, self.cy));
            self.alt = true;
        } else {
            if let Some((g, x, y)) = self.main_backup.take() {
                self.grid = g;
                self.cx = x;
                self.cy = y;
            }
            self.alt = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Vt;

    #[test]
    fn lines_cr_lf_backspace_and_clear() {
        let mut v = Vt::new(20, 4);
        v.feed(b"abc\r\nxy\x08Z\r\n");
        assert_eq!(v.lines()[..2], ["abc".to_string(), "xZ".to_string()]);
        v.feed(b"\x1b[H\x1b[2J$ ");
        assert_eq!(v.lines()[0], "$");
        assert_eq!((v.cx, v.cy), (2, 0));
    }

    #[test]
    fn scrolls_wraps_and_tracks_window_start() {
        let mut v = Vt::new(5, 2);
        v.feed(b"1\r\n2\r\n3\r\n");
        assert_eq!(v.scrolled, 2);
        assert_eq!(v.history, ["1", "2"]);
        v.feed(b"abcdefg");
        assert_eq!(v.lines(), ["abcde", "fg"]);
    }

    #[test]
    fn alt_screen_restores_main_and_utf8_and_osc() {
        let mut v = Vt::new(20, 3);
        v.feed("\x1b]0;title\x07主屏".as_bytes());
        v.feed(b"\x1b[?1049h\x1b[?1h\x1b[HTUI");
        assert!(v.alt && v.app_cursor);
        assert_eq!(v.lines()[0], "TUI");
        v.feed(b"\x1b[?1049l\x1b[?1l");
        assert!(!v.alt && !v.app_cursor);
        assert_eq!(v.lines()[0], "主屏");
    }

    #[test]
    fn readline_redraw_sequences() {
        // bash 回显退格：BS + CSI K
        let mut v = Vt::new(20, 2);
        v.feed(b"$ lsx\x08\x1b[K -l");
        assert_eq!(v.lines()[0], "$ ls -l");
        v.feed(b"\r\x1b[2C\x1b[1P");
        assert_eq!(v.lines()[0], "$ s -l");
    }
}
