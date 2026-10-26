//! Markdown → 可渲染的行/块模型。
//!
//! 只做 GUI 需要的那部分（段落、标题、列表、引用、代码块、分隔线、行内强调/代码/链接），
//! 解析用现成的 `pulldown-cmark`，不做自己的语法分析。

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// 行内片段的样式开关。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpanStyle {
    /// 加粗
    pub bold: bool,
    /// 斜体
    pub italic: bool,
    /// 行内代码
    pub code: bool,
    /// 链接文字
    pub link: bool,
}

/// 一段带样式的行内文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// 文本内容
    pub text: String,
    /// 该片段的样式
    pub style: SpanStyle,
}

/// 一行的语义类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// 普通段落
    Text,
    /// 标题（1–6）
    Heading(u8),
    /// 无序列表项（层级从 0 起）
    Bullet(usize),
    /// 有序列表项（序号从 1 起）
    Numbered(usize),
    /// 引用块
    Quote,
}

/// 一行：语义类型 + 行内片段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// 语义类型
    pub kind: LineKind,
    /// 行内片段（按顺序渲染）
    pub spans: Vec<Span>,
}

/// 一个渲染单元。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// 一行文本（段落/标题/列表项/引用）
    Line(Line),
    /// 围栏代码块：整块交给语法高亮，`lang` 为 None 表示无语言
    Code {
        /// 围栏标注的语言
        lang: Option<String>,
        /// 代码正文（末行可能带换行）
        code: String,
    },
    /// 分隔线
    Rule,
}

/// Markdown 源码 → 渲染单元序列。
pub fn parse(text: &str) -> Vec<Block> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);

    let mut out: Vec<Block> = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    let mut style = SpanStyle::default();
    let mut kind = LineKind::Text;
    let mut quote_depth = 0usize;
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut code: Option<(Option<String>, String)> = None;

    /// 收尾当前行：把累积的片段推成一行。
    fn flush(out: &mut Vec<Block>, spans: &mut Vec<Span>, kind: LineKind) {
        if spans.is_empty() {
            return;
        }
        let line = Line {
            kind,
            spans: std::mem::take(spans),
        };
        out.push(Block::Line(line));
    }

    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                flush(&mut out, &mut spans, line_kind(kind, quote_depth));
            }
            Event::Start(Tag::Heading { level, .. }) => {
                kind = LineKind::Heading(level as u8);
            }
            Event::End(TagEnd::Heading(_)) => {
                flush(&mut out, &mut spans, kind);
                kind = LineKind::Text;
            }
            Event::Start(Tag::BlockQuote(_)) => {
                quote_depth += 1;
                kind = LineKind::Quote;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                flush(&mut out, &mut spans, LineKind::Quote);
                quote_depth = quote_depth.saturating_sub(1);
                kind = LineKind::Text;
            }
            Event::Start(Tag::List(start)) => list_stack.push(start),
            Event::End(TagEnd::List(_)) => {
                list_stack.pop();
            }
            Event::Start(Tag::Item) => {
                kind = match list_stack.last().copied().flatten() {
                    Some(start) => LineKind::Numbered(start as usize),
                    None => LineKind::Bullet(list_stack.len().saturating_sub(1)),
                };
            }
            Event::End(TagEnd::Item) => {
                flush(&mut out, &mut spans, kind);
                if let Some(Some(start)) = list_stack.last_mut() {
                    *start += 1;
                }
                kind = LineKind::Text;
            }
            Event::Start(Tag::CodeBlock(fenced)) => {
                let lang = match &fenced {
                    pulldown_cmark::CodeBlockKind::Fenced(info) => {
                        let lang = info.split_whitespace().next().unwrap_or("").trim();
                        (!lang.is_empty()).then(|| lang.to_string())
                    }
                    pulldown_cmark::CodeBlockKind::Indented => None,
                };
                code = Some((lang, String::new()));
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((lang, code)) = code.take() {
                    out.push(Block::Code { lang, code });
                }
            }
            Event::Text(text) => {
                if let Some((_, body)) = code.as_mut() {
                    body.push_str(&text);
                } else {
                    spans.push(Span {
                        text: text.to_string(),
                        style,
                    });
                }
            }
            Event::Code(text) => {
                let mut code_style = style;
                code_style.code = true;
                spans.push(Span {
                    text: text.to_string(),
                    style: code_style,
                });
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                spans.push(Span {
                    text: html.to_string(),
                    style,
                });
            }
            Event::Start(Tag::Strong) => style.bold = true,
            Event::End(TagEnd::Strong) => style.bold = false,
            Event::Start(Tag::Emphasis) => style.italic = true,
            Event::End(TagEnd::Emphasis) => style.italic = false,
            Event::Start(Tag::Strikethrough) => style.italic = true,
            Event::End(TagEnd::Strikethrough) => style.italic = false,
            Event::Start(Tag::Link { .. }) => style.link = true,
            Event::End(TagEnd::Link) => style.link = false,
            Event::SoftBreak | Event::HardBreak => {
                flush(&mut out, &mut spans, kind);
            }
            Event::Rule => {
                flush(&mut out, &mut spans, kind);
                out.push(Block::Rule);
            }
            _ => {}
        }
    }
    flush(&mut out, &mut spans, kind);
    out
}

/// 引用块内的行按引用渲染，其余保持原语义。
fn line_kind(kind: LineKind, quote_depth: usize) -> LineKind {
    if quote_depth > 0 && kind == LineKind::Text {
        LineKind::Quote
    } else {
        kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 标题、强调与行内代码各自成行/成片段。
    #[test]
    fn heading_and_inline_styles() {
        let blocks = parse("# 标题\n\n正文 **粗** `码` 尾");
        assert_eq!(blocks.len(), 2);
        let Block::Line(heading) = &blocks[0] else {
            panic!("首块应为行");
        };
        assert_eq!(heading.kind, LineKind::Heading(1));
        assert_eq!(heading.spans[0].text, "标题");

        let Block::Line(body) = &blocks[1] else {
            panic!("次块应为行");
        };
        assert!(body.spans.iter().any(|s| s.style.bold));
        assert!(body.spans.iter().any(|s| s.style.code));
    }

    /// 围栏代码块整块保留原文与语言标注。
    #[test]
    fn fenced_code_keeps_body() {
        let blocks = parse("```rust\nfn main() {}\n```");
        let Block::Code { lang, code } = &blocks[0] else {
            panic!("应为代码块");
        };
        assert_eq!(lang.as_deref(), Some("rust"));
        assert!(code.contains("fn main()"));
    }

    /// 列表项带上序号/层级，引用块标记为 Quote。
    #[test]
    fn lists_and_quotes() {
        let blocks = parse("- a\n- b\n\n1. x\n\n> q");
        let kinds: Vec<LineKind> = blocks
            .iter()
            .filter_map(|b| match b {
                Block::Line(l) => Some(l.kind),
                _ => None,
            })
            .collect();
        assert!(kinds.contains(&LineKind::Bullet(0)));
        assert!(kinds.contains(&LineKind::Numbered(1)));
        assert!(kinds.contains(&LineKind::Quote));
    }
}
