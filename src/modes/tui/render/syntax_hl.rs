//! syntect 语法高亮：语法集来自 [`crate::utils::syntax`]（默认集 + 内嵌补充语法）。
//!
//! token 颜色映射到主题 `syntax*` 变量（对齐 VSCode Dark+ 配色语义）。

use crate::{modes::tui::theme::Theme as AppTheme, utils::syntax};
use ratatui::style::{Color as RtColor, Modifier, Style as RtStyle};
use std::str::FromStr;
use syntect::{
    easy::HighlightLines,
    highlighting::{Color, FontStyle, ScopeSelectors, Style, StyleModifier, Theme, ThemeItem},
    parsing::SyntaxSet,
};

/// 单块代码高亮会话：跨行维持语法解析状态（多行注释/字符串正确着色）。
pub struct HighlightSession<'a> {
    lines: HighlightLines<'a>,
    set: &'a SyntaxSet,
}

/// 为 `lang` 创建高亮会话；未知语言返回 None（调用方回退纯文本）。
pub fn session<'a>(lang: &str, theme: &'a Theme) -> Option<HighlightSession<'a>> {
    let set = syntax::syntax_set();
    let found = syntax::find_syntax_by_token(lang)?;
    Some(HighlightSession {
        lines: HighlightLines::new(found, theme),
        set,
    })
}

/// 按文件名（扩展名 / 首行 shebang 由语法集自行判定）创建高亮会话；
/// 识别不出语言时返回 None（调用方回退纯文本）。
pub fn session_for_filename<'a>(path: &str, theme: &'a Theme) -> Option<HighlightSession<'a>> {
    let set = syntax::syntax_set();
    let found = syntax::find_syntax_for_filename(path)?;
    Some(HighlightSession {
        lines: HighlightLines::new(found, theme),
        set,
    })
}

impl HighlightSession<'_> {
    /// 高亮一行，产出 (样式, 文本) 序列；解析失败回退 `base` 色整行。
    pub fn line(&mut self, text: &str, base: RtStyle) -> Vec<(RtStyle, String)> {
        match self.lines.highlight_line(text, self.set) {
            Ok(ranges) => ranges
                .iter()
                .map(|(st, t)| (to_ratatui(*st), t.to_string()))
                .collect(),
            Err(_) => vec![(base, text.to_string())],
        }
    }
}

/// 主题的 `syntax*` 颜色变量构建 syntect 主题。
/// 默认前景 = mdCodeBlock 色：未匹配 scope 的 token（含行首缩进）落到代码块正文色。
pub fn build_theme(t: &AppTheme) -> Theme {
    let mut th = Theme::default();
    th.settings.foreground = Some(hex_color(
        &t.get("mdCodeBlock", "#b5bd68"),
        (0xb5, 0xbd, 0x68),
    ));

    let item = |sel: &str, var: &str, fallback: (u8, u8, u8)| ThemeItem {
        scope: ScopeSelectors::from_str(sel).unwrap_or_default(),
        style: StyleModifier {
            foreground: Some(hex_color(&t.get(var, ""), fallback)),
            font_style: None,
            ..Default::default()
        },
    };

    th.scopes.push(item(
        "keyword.control, keyword.other",
        "syntaxKeyword",
        (0x56, 0x9c, 0xd6),
    ));
    th.scopes
        .push(item("keyword", "syntaxKeyword", (0x56, 0x9c, 0xd6)));
    th.scopes.push(item(
        "storage, storage.modifier",
        "syntaxKeyword",
        (0x56, 0x9c, 0xd6),
    ));
    th.scopes
        .push(item("storage.type", "syntaxType", (0x4e, 0xc9, 0xb0)));
    th.scopes.push(item(
        "keyword.operator",
        "syntaxOperator",
        (0xd4, 0xd4, 0xd4),
    ));
    th.scopes
        .push(item("comment", "syntaxComment", (0x6a, 0x99, 0x55)));
    th.scopes
        .push(item("string", "syntaxString", (0xce, 0x91, 0x78)));
    th.scopes
        .push(item("constant.numeric", "syntaxNumber", (0xb5, 0xce, 0xa8)));
    th.scopes.push(item(
        "entity.name.function, support.function",
        "syntaxFunction",
        (0xdc, 0xdc, 0xaa),
    ));
    th.scopes.push(item(
        "entity.name.type, entity.name.class, entity.name.namespace, support.type",
        "syntaxType",
        (0x4e, 0xc9, 0xb0),
    ));
    th.scopes.push(item(
        "variable, variable.parameter, entity.name.variable",
        "syntaxVariable",
        (0x9c, 0xdc, 0xfe),
    ));
    th.scopes
        .push(item("punctuation", "syntaxPunctuation", (0xd4, 0xd4, 0xd4)));
    th
}

/// 解析 `#RRGGBB` 形式的主题色为 syntect `Color`；格式非法或解析失败时回退到 `fallback`。
fn hex_color(hex: &str, fallback: (u8, u8, u8)) -> Color {
    let h = hex.trim_start_matches('#');
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    if h.len() == 6 && h.chars().all(|c| c.is_ascii_hexdigit()) {
        Color {
            r: byte(0).unwrap_or(fallback.0),
            g: byte(2).unwrap_or(fallback.1),
            b: byte(4).unwrap_or(fallback.2),
            a: 255,
        }
    } else {
        Color {
            r: fallback.0,
            g: fallback.1,
            b: fallback.2,
            a: 255,
        }
    }
}

/// syntect Style → ratatui Style：只取前景与字体样式，背景保持透明（不覆盖色块背景）。
fn to_ratatui(st: Style) -> RtStyle {
    let mut s = RtStyle::default().fg(RtColor::Rgb(
        st.foreground.r,
        st.foreground.g,
        st.foreground.b,
    ));

    if st.font_style.contains(FontStyle::BOLD) {
        s = s.add_modifier(Modifier::BOLD);
    }
    if st.font_style.contains(FontStyle::ITALIC) {
        s = s.add_modifier(Modifier::ITALIC);
    }
    if st.font_style.contains(FontStyle::UNDERLINE) {
        s = s.add_modifier(Modifier::UNDERLINED);
    }
    s
}
