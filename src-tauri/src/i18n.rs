//! 界面语言（中文 / English）：后端直接显示给用户的提示跟着前端选的语言走。
//!
//! 前端启动和切换语言时调 `set_locale`。显示给用户的文案写成中英一对：
//!
//! ```ignore
//! tr!("连不上 {host}:{port}", "Cannot reach {host}:{port}")
//! tr!("连不上 {}:{}", "Cannot reach {}:{}", host, port)
//! ```
//!
//! 只翻**给人看的**：给模型看的提示词、协议里的机器码（`SIDECAR_DENIED` 等）、日志不翻。
//! 默认中文（前端还没告诉我们之前、以及单元测试里），现有用例按中文断言。

use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);

/// 当前界面语言是不是英文
pub fn is_en() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

pub fn set_locale(locale: &str) {
    ENGLISH.store(locale.eq_ignore_ascii_case("en"), Ordering::Relaxed);
}

/// 中英一对里取当前语言那条，用法同 `format!`（支持 `{name}` 就地捕获）。
#[macro_export]
macro_rules! tr {
    ($zh:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::is_en() {
            format!($en $(, $arg)*)
        } else {
            format!($zh $(, $arg)*)
        }
    };
}

/// 给 AI 的系统提示追加的语言要求：界面是英文就让它用英文回答（含提问与完成报告）。中文界面不追加。
pub fn reply_language_note() -> &'static str {
    if is_en() {
        "\n\n## Reply language\nThe user's interface language is English. Write everything the user will read \
         in English: answers, questions (nx_ask_user), notes, and the nx_complete report. Keep commands and \
         file contents as they are."
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn tr_picks_the_current_language() {
        // 测试里共用一个全局开关：这条用例自己设回中文，不影响别的用例
        let host = "web1";
        super::set_locale("en");
        assert_eq!(crate::tr!("连不上 {host}", "Cannot reach {host}"), "Cannot reach web1");
        assert!(super::reply_language_note().contains("English"));
        super::set_locale("zh");
        assert_eq!(crate::tr!("连不上 {}", "Cannot reach {}", host), "连不上 web1");
        assert_eq!(super::reply_language_note(), "");
    }
}
