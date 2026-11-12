//! 代码块语法高亮：syntect → 每行的 RGB 片段。
//!
//! 配色来自 [`super::theme::Theme::syntect`]（GUI 自己的设计令牌），
//! 不依赖 TUI 的多彩主题体系。

use crate::utils::{color::Rgb, syntax};
use syntect::{easy::HighlightLines, highlighting::Theme as SynTheme, util::LinesWithEndings};

/// 代码块高亮结果：每行若干 `(颜色, 文本)`；`color` 为 None 表示用默认前景色。
pub fn highlight(
    code: &str,
    lang: Option<&str>,
    theme: &SynTheme,
) -> Vec<Vec<(Option<Rgb>, String)>> {
    let set = syntax::syntax_set();
    let found = lang
        .and_then(syntax::find_syntax_by_token)
        .unwrap_or_else(|| set.find_syntax_plain_text());

    let mut highlighter = HighlightLines::new(found, theme);
    let mut out = Vec::new();
    for line in LinesWithEndings::from(code) {
        let Ok(ranges) = highlighter.highlight_line(line, set) else {
            out.push(vec![(None, line.trim_end_matches('\n').to_string())]);
            continue;
        };

        let mut spans = Vec::new();
        for (style, text) in ranges {
            let text = text.trim_end_matches('\n');
            if text.is_empty() {
                continue;
            }

            let color = style.foreground;
            spans.push((Some(Rgb::new(color.r, color.g, color.b)), text.to_string()));
        }

        if spans.is_empty() {
            spans.push((None, String::new()));
        }

        out.push(spans);
    }
    out
}
