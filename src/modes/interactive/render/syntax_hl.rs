//! syntect 语法高亮：嵌入语法集 + 内置精简 Zig 语法。
//!
//! 嵌入式默认语法集（约 75 种，覆盖 Rust/Python/JS/Go/C/C++/Java/Bash/Shell/
//! JSON/YAML/HTML/CSS/Markdown/Diff/SQL 等）不含 Zig/TypeScript；Zig 用内置
//! 精简语法（`assets/syntaxes/Zig.sublime-syntax`）补齐，其余未覆盖语言回退纯文本。
//! token 颜色映射到主题 `syntax*` 变量（对齐 VSCode Dark+ 配色语义）。

use crate::{embedded, modes::interactive::theme::Theme as AppTheme};
use ratatui::style::{Color as RtColor, Modifier, Style as RtStyle};
use std::{str::FromStr, sync::OnceLock};
use syntect::{
    easy::HighlightLines,
    highlighting::{Color, FontStyle, ScopeSelectors, Style, StyleModifier, Theme, ThemeItem},
    parsing::{SyntaxDefinition, SyntaxReference, SyntaxSet},
};

/// 常驻语法集（首次使用时懒加载；加载并编译约 75+ 个语法，一次性开销）
struct Highlighter {
    /// 已编译的语法集，含内置默认语法与精简 Zig 语法
    set: SyntaxSet,
}

/// 进程内常驻的语法高亮器单例，首次调用时懒加载并编译语法集。
fn highlighter() -> &'static Highlighter {
    /// 常驻语法高亮器单例，首次使用时懒加载并编译语法集。
    static HL: OnceLock<Highlighter> = OnceLock::new();
    HL.get_or_init(|| Highlighter {
        set: build_syntax_set(),
    })
}

/// 构建语法集：默认 nonewlines 语法集 + 内置精简 Zig 语法（Zig 未包含在默认集中）。
fn build_syntax_set() -> SyntaxSet {
    // 渲染按行喂入（无尾部 \n），必须用 nonewlines 变体：newlines 变体里以「行尾\n」
    // 为退栈条件的上下文（如 C++ 的 meta.preprocessor.include）永远不弹栈，
    // 导致 include 上下文泄漏、整块代码高亮塌陷。
    let mut builder = SyntaxSet::load_defaults_nonewlines().into_builder();
    if let Ok(syn) =
        SyntaxDefinition::load_from_str(embedded!("syntaxes/Zig.sublime-syntax"), false, None)
    {
        builder.add(syn);
    }
    builder.build()
}

/// 取语言的语法；`lang` 可能是 GitHub 风格 fence token（如 "rust,ignore"、
/// "{rust,linenos}"、"python hl_lines=1"），取首个空白分隔 token 并截断到
/// 首个分隔符（`,` `.` `;` `:` `{` `}`），避免 `,ignore` 这类后缀使查找失败。
fn find_syntax<'a>(set: &'a SyntaxSet, lang: &str) -> Option<&'a SyntaxReference> {
    let tok = lang
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_start_matches('{')
        .split([',', '.', ';', ':', '{', '}'])
        .next()
        .unwrap_or("");
    if tok.is_empty() {
        return None;
    }
    set.find_syntax_by_token(tok)
}

/// 单块代码高亮会话：跨行维持语法解析状态（多行注释/字符串正确着色）。
pub struct HighlightSession<'a> {
    lines: HighlightLines<'a>,
    set: &'a SyntaxSet,
}

/// 为 `lang` 创建高亮会话；未知语言返回 None（调用方回退纯文本）。
pub fn session<'a>(lang: &str, theme: &'a Theme) -> Option<HighlightSession<'a>> {
    let hl = highlighter();
    let syntax = find_syntax(&hl.set, lang)?;
    Some(HighlightSession {
        lines: HighlightLines::new(syntax, theme),
        set: &hl.set,
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
