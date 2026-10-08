//! 把 Markdown 渲染成覆盖层行（[`DockLine`]）。
//!
//! 供扩展的覆盖层/停靠面板使用：扩展只提供 Markdown 文本，渲染规则由核心提供，
//! 于是扩展不必各自实现一套 Markdown 解析，也能拿到与主题一致的颜色键。
//!
//! 注意：subagent 的会话查看器**已不再**用它——查看器改为把 transcript 作为
//! `AgentMessage` 交给 TUI，用聊天区同一条 `render/markdown.rs` 管线渲染（见
//! `OverlayView::messages`），以做到与主页逐像素一致。本模块保留给其它扩展面。
//!
//! 与 `modes/interactive/render/markdown.rs`（聊天区渲染器）的分工：那一份产出
//! `Line<Span>` 且需要 `Theme`，只能在 TUI 内用；这一份产出 `DockLine`（主题键 +
//! 缺省色，渲染期由 dock 层解析），因此扩展也能产出它。颜色键沿用聊天区同一套命名
//! （`mdHeading`/`mdCode`…），主题改动两边一起生效。
//!
//! 有意不做的事：`DockSpan` 没有 modifiers，所以加粗/斜体只**去标记**不换字重；
//! 数学/mermaid 回退为普通文本行。表格按 `┌─┬─┐` 边框渲染，规则与聊天区
//! （`modes/interactive/render/markdown.rs`）一致，只是表头不加粗（同样没有 modifiers）。

use super::dock::{DockLine, DockSpan};
use crate::{
    core::markdown_table,
    utils::display::{display_width, grapheme_width},
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::collections::VecDeque;
use unicode_segmentation::UnicodeSegmentation;

/// 主题颜色键与缺省色的二元组，dock 渲染期用键解析主题、键缺失时回退该色。
type Palette = (&'static str, &'static str);

/// 正文文本的颜色键与缺省色，用于段落及普通行内文字。
const MD_TEXT: Palette = ("text", "");
/// 标题（各级 heading）的颜色键与缺省色。
const MD_HEADING: Palette = ("mdHeading", "#f0c674");
/// 链接文字的颜色键与缺省色，用于 Markdown 链接的锚文本。
const MD_LINK: Palette = ("mdLink", "#81a2be");
/// 链接目标 URL 的颜色键与缺省色。
const MD_LINK_URL: Palette = ("mdLinkUrl", "#666666");
/// 行内代码的颜色键与缺省色，用于反引号包裹的片段。
const MD_CODE: Palette = ("mdCode", "#8abeb7");
/// 围栏代码块正文的颜色键与缺省色，用于代码块内的文字。
const MD_CODE_BLOCK: Palette = ("mdCodeBlock", "#b5bd68");
/// 代码块边框的颜色键与缺省色，用于围栏边界线。
const MD_CODE_BLOCK_BORDER: Palette = ("mdCodeBlockBorder", "#808080");
/// 引用块正文的颜色键与缺省色，用于 `>` 引用的文字。
const MD_QUOTE: Palette = ("mdQuote", "#808080");
/// 引用块左侧竖线边框的颜色键与缺省色。
const MD_QUOTE_BORDER: Palette = ("mdQuoteBorder", "#808080");
/// 水平分隔线（---）的颜色键与缺省色。
const MD_HR: Palette = ("mdHr", "#808080");
/// 无序列表项目符号的颜色键与缺省色。
const MD_LIST_BULLET: Palette = ("mdListBullet", "#8abeb7");

/// 渲染 Markdown 为**已折行**的覆盖层行（每行不超过 `width` 个显示列）。
///
/// `width` 为 0 时按 1 处理（覆盖层宽度极小也只保证不 panic）。
pub fn render(text: &str, width: usize) -> Vec<DockLine> {
    let width = width.max(1);
    let mut out: Vec<DockLine> = Vec::new();
    let mut runs: Vec<Run> = Vec::new();
    // 列表/引用前缀（代码块自己处理缩进）
    let mut prefix = String::new();
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut quote_depth: usize = 0;
    let mut in_code = false;
    let mut heading: Option<HeadingLevel> = None;
    let mut link_url: Option<String> = None;

    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_TABLES);

    macro_rules! flush {
        ($blank:expr) => {{
            if !runs.is_empty() {
                out.extend(wrap_runs(std::mem::take(&mut runs), &prefix, width));
            }
            if $blank && !out.is_empty() && out.last().is_some_and(|l| !l.is_empty()) {
                out.push(Vec::new());
            }
        }};
    }

    let mut events: VecDeque<Event<'_>> = Parser::new_ext(text, options).collect();
    while let Some(event) = events.pop_front() {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => heading = Some(level),
                Tag::Paragraph => {
                    // 引用段落：每行加 `│ ` 边框（列表项自己管前缀）
                    if list_stack.is_empty() && quote_depth > 0 {
                        prefix = "│ ".repeat(quote_depth);
                    }
                }
                Tag::CodeBlock(kind) => {
                    flush!(false);
                    in_code = true;
                    if let CodeBlockKind::Fenced(lang) = kind {
                        let lang = lang.trim();
                        if !lang.is_empty() {
                            out.push(vec![DockSpan::new(
                                MD_CODE_BLOCK_BORDER.0,
                                MD_CODE_BLOCK_BORDER.1,
                                format!("```{lang}"),
                            )]);
                        }
                    }
                }
                Tag::List(start) => {
                    flush!(false);
                    list_stack.push(start);
                }
                Tag::Item => {
                    flush!(false);
                    let depth = list_stack.len().saturating_sub(1);
                    let indent = "  ".repeat(depth + quote_depth);
                    let bullet = match list_stack.last_mut() {
                        Some(Some(n)) => {
                            let cur = format!("{n}. ");
                            *n += 1;
                            cur
                        }
                        _ => "- ".to_string(),
                    };
                    prefix = indent;
                    runs.push(Run {
                        palette: MD_LIST_BULLET,
                        text: bullet,
                    });
                }
                Tag::BlockQuote(_) => {
                    flush!(false);
                    quote_depth += 1;
                }
                Tag::Link { dest_url, .. } => {
                    link_url = Some(dest_url.to_string());
                }
                Tag::Table(_) => {
                    flush!(false);

                    let (header, rows) = collect_table(&mut events);
                    out.extend(render_table(&header, &rows, width));

                    if !out.is_empty() {
                        out.push(Vec::new());
                    }
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    flush!(true);
                    heading = None;
                }
                TagEnd::Paragraph => flush!(true),
                TagEnd::CodeBlock => {
                    flush!(true);
                    in_code = false;
                }
                TagEnd::List(_) => {
                    flush!(true);
                    list_stack.pop();
                }
                TagEnd::Item => flush!(false),
                TagEnd::BlockQuote(_) => {
                    flush!(true);
                    quote_depth = quote_depth.saturating_sub(1);
                }
                TagEnd::Link => {
                    if let Some(url) = link_url.take()
                        && !url.is_empty()
                        && runs.last().map(|r| r.text.as_str()) != Some(url.as_str())
                    {
                        runs.push(Run {
                            palette: MD_LINK_URL,
                            text: format!(" ({url})"),
                        });
                    }
                }
                _ => {}
            },
            Event::Text(text) => {
                let palette = if in_code {
                    MD_CODE_BLOCK
                } else if let Some(level) = heading {
                    let _ = level;
                    MD_HEADING
                } else if link_url.is_some() {
                    MD_LINK
                } else if quote_depth > 0 {
                    MD_QUOTE
                } else {
                    MD_TEXT
                };
                push_text(&mut runs, palette, text.into_string());
            }
            Event::Code(code) => push_text(&mut runs, MD_CODE, code.into_string()),
            Event::SoftBreak | Event::HardBreak => push_text(&mut runs, MD_TEXT, " ".to_string()),
            Event::Rule => {
                flush!(false);
                out.push(vec![DockSpan::new(
                    MD_HR.0,
                    MD_HR.1,
                    "─".repeat(width.min(80)),
                )]);
                out.push(Vec::new());
            }
            Event::TaskListMarker(done) => {
                push_text(
                    &mut runs,
                    MD_LIST_BULLET,
                    if done { "[x] " } else { "[ ] " }.to_string(),
                );
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                push_text(&mut runs, MD_TEXT, html.into_string());
            }
            Event::FootnoteReference(name) => {
                push_text(&mut runs, MD_LINK, format!("[{name}]"));
            }
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                push_text(&mut runs, MD_CODE, math.into_string());
            }
        }
    }
    flush!(false);

    // 折叠末尾空行
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    if out.is_empty() {
        out.push(vec![DockSpan::plain("")]);
    }
    out
}

/// 一段行内文本（同一配色）。
struct Run {
    /// 该段文本的配色（样式名 + 颜色值）。
    palette: Palette,
    /// 该段行内文本内容。
    text: String,
}

/// 相邻同配色合并，减少 span 数。
fn push_text(runs: &mut Vec<Run>, palette: Palette, text: String) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = runs.last_mut()
        && last.palette == palette
    {
        last.text.push_str(&text);
        return;
    }
    runs.push(Run { palette, text });
}

/// 把一段行内 run 按 `width` 折行，`prefix` 为每行的固定前缀（列表/引用）。
fn wrap_runs(runs: Vec<Run>, prefix: &str, width: usize) -> Vec<DockLine> {
    let prefix_w = display_width(prefix);
    let avail = width.saturating_sub(prefix_w).max(1);
    let mut lines: Vec<DockLine> = Vec::new();
    let mut cur: DockLine = Vec::new();
    let mut cur_w = 0usize;
    let mut first = true;

    let mut open = |cur: &mut DockLine, first: &mut bool| {
        if *first {
            *first = false;
            if !prefix.is_empty() {
                let palette = if prefix.contains('│') {
                    MD_QUOTE_BORDER
                } else {
                    ("dim", "#666666")
                };
                cur.push(DockSpan::new(palette.0, palette.1, prefix.to_string()));
            }
        }
    };

    for run in runs {
        // 按空白切成词，保留词的边界（空白在折行时被吸收/丢弃）
        let mut word = String::new();
        for ch in run.text.chars() {
            if ch.is_whitespace() {
                if !word.is_empty() {
                    place_word(
                        &mut cur,
                        &mut cur_w,
                        &mut lines,
                        &mut first,
                        &mut open,
                        run.palette,
                        &word,
                        avail,
                    );
                    word.clear();
                }
                if cur_w < avail {
                    place_word(
                        &mut cur,
                        &mut cur_w,
                        &mut lines,
                        &mut first,
                        &mut open,
                        run.palette,
                        " ",
                        avail,
                    );
                }
            } else {
                word.push(ch);
            }
        }
        if !word.is_empty() {
            place_word(
                &mut cur,
                &mut cur_w,
                &mut lines,
                &mut first,
                &mut open,
                run.palette,
                &word,
                avail,
            );
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

/// 把一个单词按可用宽度 `avail` 放入当前行 `cur`：放不下则先换行，超长单词按字符硬断。
/// 同时维护当前行宽度 `cur_w` 与行首标志 `first`，行首空白不渲染。
#[allow(clippy::too_many_arguments)]
fn place_word(
    cur: &mut DockLine,
    cur_w: &mut usize,
    lines: &mut Vec<DockLine>,
    first: &mut bool,
    open: &mut impl FnMut(&mut DockLine, &mut bool),
    palette: Palette,
    word: &str,
    avail: usize,
) {
    let w = display_width(word);
    let is_space = word.chars().all(char::is_whitespace);
    // 折行：当前行非空且放不下 → 换行（行首的空白直接丢弃）
    if *cur_w > 0 && *cur_w + w > avail {
        lines.push(std::mem::take(cur));
        *cur_w = 0;
        *first = true;
        if is_space {
            return;
        }
    }
    if is_space && *cur_w == 0 {
        return; // 行首空白不渲染
    }
    open(cur, first);
    // 超长单词硬断
    if w > avail {
        let mut chunk = String::new();
        let mut cw = 0usize;
        for ch in word.chars() {
            let c = display_width(&ch.to_string());
            if cw + c > avail && cw > 0 {
                push_span(cur, palette, std::mem::take(&mut chunk));
                lines.push(std::mem::take(cur));
                *first = true;
                cw = 0;
            }
            chunk.push(ch);
            cw += c;
        }
        if !chunk.is_empty() {
            push_span(cur, palette, chunk);
            *cur_w = cw;
        }
    } else {
        push_span(cur, palette, word.to_string());
        *cur_w += w;
    }
}

/// 把一段同色文本追加到当前行：末尾 span 颜色相同则合并进它，否则新建一个 span。
fn push_span(cur: &mut DockLine, palette: Palette, text: String) {
    if let Some(last) = cur.last_mut()
        && last.key == palette.0
        && last.fallback == palette.1
    {
        last.text.push_str(&text);
        return;
    }
    cur.push(DockSpan::new(palette.0, palette.1, text));
}

/// 消费到 `End(Table)`，收集表头（第一行）与表体单元格。
fn collect_table(events: &mut VecDeque<Event<'_>>) -> (Vec<Vec<Run>>, Vec<Vec<Vec<Run>>>) {
    let mut header: Vec<Vec<Run>> = Vec::new();
    let mut rows: Vec<Vec<Vec<Run>>> = Vec::new();
    let mut cur_row: Option<Vec<Vec<Run>>> = None;

    while let Some(ev) = events.pop_front() {
        match ev {
            Event::Start(Tag::TableHead) | Event::Start(Tag::TableRow) => {
                cur_row = Some(Vec::new());
            }
            Event::Start(Tag::TableCell) => {
                let mut cell: Vec<Run> = Vec::new();
                collect_inline_runs(events, &mut cell);
                if let Some(row) = cur_row.as_mut() {
                    row.push(cell);
                }
            }
            Event::End(TagEnd::TableHead) | Event::End(TagEnd::TableRow) => {
                if let Some(row) = cur_row.take() {
                    if header.is_empty() {
                        header = row;
                    } else {
                        rows.push(row);
                    }
                }
            }
            Event::End(TagEnd::Table) => break,
            _ => {}
        }
    }

    (header, rows)
}

/// 收集一个单元格内的行内 run（到 `End(TableCell)` 为止）。
///
/// 与主循环同一套配色：`strong`/`em`/`del` 等只去标记（`DockSpan` 无 modifiers），
/// `code`/`math`/`html`/脚注/链接 URL 的处理与主循环一致（否则单元格里的这些内容会丢）。
fn collect_inline_runs(events: &mut VecDeque<Event<'_>>, runs: &mut Vec<Run>) {
    let mut link_url: Option<String> = None;

    while let Some(ev) = events.pop_front() {
        match ev {
            Event::Text(t) => push_text(runs, MD_TEXT, t.into_string()),
            Event::Code(c) => push_text(runs, MD_CODE, c.into_string()),
            Event::InlineMath(m) | Event::DisplayMath(m) => {
                push_text(runs, MD_CODE, m.into_string())
            }
            Event::Html(h) | Event::InlineHtml(h) => push_text(runs, MD_TEXT, h.into_string()),
            Event::FootnoteReference(name) => push_text(runs, MD_LINK, format!("[{name}]")),
            Event::SoftBreak | Event::HardBreak => push_text(runs, MD_TEXT, " ".to_string()),
            Event::Start(Tag::Link { dest_url, .. }) => link_url = Some(dest_url.to_string()),
            Event::End(TagEnd::Link) => {
                if let Some(url) = link_url.take()
                    && !url.is_empty()
                    && runs.last().map(|r| r.text.as_str()) != Some(url.as_str())
                {
                    push_text(runs, MD_LINK_URL, format!(" ({url})"));
                }
            }
            Event::End(TagEnd::TableCell) => break,
            _ => {}
        }
    }
}

/// 单元格的纯文本（算列宽用）。
fn cell_text(cell: &[Run]) -> String {
    cell.iter().map(|r| r.text.as_str()).collect()
}

/// 渲染整张表：`┌─┬─┐ / ├─┼─┤ / └─┴─┘` 边框 + 单元格折行。
///
/// 宽度连一列都放不下时退化为 `a | b | c` 的纯文本行（表头也保留）。
fn render_table(header: &[Vec<Run>], rows: &[Vec<Vec<Run>>], width: usize) -> Vec<DockLine> {
    let num_cols = header.len();
    if num_cols == 0 {
        return Vec::new();
    }

    let avail = markdown_table::column_avail(width, num_cols);
    if avail < num_cols {
        let mut out: Vec<DockLine> = Vec::new();
        let mut push_row = |cells: &[Vec<Run>]| {
            let text = cells
                .iter()
                .map(|c| cell_text(c))
                .collect::<Vec<_>>()
                .join(" | ");
            out.extend(wrap_runs(
                vec![Run {
                    palette: MD_TEXT,
                    text,
                }],
                "",
                width,
            ));
        };
        push_row(header);
        for row in rows {
            push_row(row);
        }
        return out;
    }

    let cols = markdown_table::compute_column_widths(header, rows, |c| cell_text(c), avail);
    let border = MD_TEXT;
    let horiz = |left: &str, mid: &str, right: &str| -> String {
        let seg: Vec<String> = cols.iter().map(|w| "─".repeat(*w)).collect();
        format!("{left}─{}─{right}", seg.join(&format!("─{mid}─")))
    };
    let hline = |left, mid, right| vec![DockSpan::new(border.0, border.1, horiz(left, mid, right))];

    let mut out: Vec<DockLine> = Vec::new();
    out.push(hline("┌", "┬", "┐"));
    out.extend(render_table_row(header, &cols));
    out.push(hline("├", "┼", "┤"));

    for (ri, row) in rows.iter().enumerate() {
        out.extend(render_table_row(row, &cols));
        if ri + 1 < rows.len() {
            out.push(hline("├", "┼", "┤"));
        }
    }

    out.push(hline("└", "┴", "┘"));
    out
}

/// 渲染一列行：单元格按列宽折行，不足的列补空单元格，保证每行列数一致。
fn render_table_row(cells: &[Vec<Run>], cols: &[usize]) -> Vec<DockLine> {
    let empty: Vec<Run> = Vec::new();
    let cell_lines: Vec<Vec<DockLine>> = cols
        .iter()
        .enumerate()
        .map(|(i, w)| wrap_runs_fixed(cells.get(i).unwrap_or(&empty), *w))
        .collect();

    let height = cell_lines.iter().map(Vec::len).max().unwrap_or(1);
    let border = MD_TEXT;
    let mut out: Vec<DockLine> = Vec::new();

    for li in 0..height {
        let mut line: DockLine = vec![DockSpan::new(border.0, border.1, "│ ")];
        for (i, cl) in cell_lines.iter().enumerate() {
            let w = cols[i];
            let text_w: usize = cl
                .get(li)
                .map(|l| l.iter().map(|s| grapheme_width(&s.text)).sum())
                .unwrap_or(0);
            line.extend(cl.get(li).cloned().unwrap_or_default());

            let pad = w.saturating_sub(text_w);
            if pad > 0 {
                line.push(DockSpan::plain(" ".repeat(pad)));
            }

            line.push(DockSpan::new(
                border.0,
                border.1,
                if i + 1 < cols.len() { " │ " } else { " │" },
            ));
        }
        out.push(line);
    }
    out
}

/// 按固定显示宽度折行（不按词边界），保留每个 run 的配色（表格单元格用）。
fn wrap_runs_fixed(runs: &[Run], width: usize) -> Vec<DockLine> {
    let width = width.max(1);
    if runs.is_empty() {
        return vec![Vec::new()];
    }

    let mut out: Vec<DockLine> = Vec::new();
    let mut cur: DockLine = Vec::new();
    let mut cur_w = 0usize;

    for run in runs {
        let mut pending = String::new();
        for g in UnicodeSegmentation::graphemes(run.text.as_str(), true) {
            let gw = grapheme_width(g);
            if gw == 0 {
                continue;
            }

            if cur_w + gw > width && cur_w > 0 {
                if !pending.is_empty() {
                    push_span(&mut cur, run.palette, std::mem::take(&mut pending));
                }
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            pending.push_str(g);
            cur_w += gw;
        }

        if !pending.is_empty() {
            push_span(&mut cur, run.palette, pending);
        }
    }

    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[DockLine]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.iter().map(|s| s.text.clone()).collect::<String>())
            .collect()
    }

    #[test]
    fn strips_inline_markers_and_colors_code() {
        let lines = render("hello **world** and `code`", 80);
        let text = plain(&lines);
        assert_eq!(text, vec!["hello world and code"]);
        // `code` 段带 mdCode 主题键
        assert!(
            lines[0]
                .iter()
                .any(|s| s.key == "mdCode" && s.text == "code"),
            "{lines:?}"
        );
    }

    #[test]
    fn headings_and_lists_render_structurally() {
        let lines = render("# Title\n\n- one\n- two", 80);
        let text = plain(&lines);
        assert_eq!(text[0], "Title");
        assert!(lines[0].iter().any(|s| s.key == "mdHeading"), "{lines:?}");
        let bullets: Vec<&String> = text.iter().filter(|l| l.starts_with("- ")).collect();
        assert_eq!(bullets.len(), 2, "{text:?}");
    }

    #[test]
    fn wraps_long_paragraphs_to_width() {
        let lines = render("one two three four five six seven eight", 12);
        assert!(lines.len() >= 3, "{:?}", plain(&lines));
        assert!(
            lines
                .iter()
                .all(|l| { l.iter().map(|s| display_width(&s.text)).sum::<usize>() <= 12 })
        );
    }

    #[test]
    fn code_fence_is_indented_and_colored() {
        let lines = render("```rust\nfn main() {}\n```", 40);
        assert!(
            plain(&lines).iter().any(|l| l.contains("fn main() {}")),
            "{:?}",
            plain(&lines)
        );
        assert!(
            lines.iter().flatten().any(|s| s.key == "mdCodeBlock"),
            "{lines:?}"
        );
    }

    #[test]
    fn zero_width_does_not_panic() {
        let lines = render("anything at all", 0);
        assert!(!lines.is_empty());
    }

    /// 表格：`┌─┬─┐` 边框，单元格对齐（各行等宽），表头/表体都在。
    #[test]
    fn table_renders_box_borders_and_aligns() {
        let md = "| Name | Value |\n|------|-------|\n| foo | 1 |\n| bar | 22 |";
        let lines = render(md, 80);
        let text = plain(&lines);

        assert!(text.iter().any(|l| l == "│ Name │ Value │"), "{text:?}");
        assert!(
            text.iter().any(|l| l.starts_with('┌') && l.ends_with('┐')),
            "{text:?}"
        );
        assert!(
            text.iter().any(|l| l.starts_with('└') && l.ends_with('┘')),
            "{text:?}"
        );
        assert!(text.iter().any(|l| l.contains("foo")), "{text:?}");

        // 同一表格每行显示宽度一致（列对齐）
        let widths: Vec<usize> = lines
            .iter()
            .map(|l| l.iter().map(|s| display_width(&s.text)).sum())
            .collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
    }

    /// 单元格过长时折行，且整张表不超可用宽度。
    #[test]
    fn table_cell_wraps_within_width() {
        let md = "| Name | Value |\n|---|---|\n| hello world foo bar | 1 |";
        let lines = render(md, 20);
        let widths: Vec<usize> = lines
            .iter()
            .map(|l| l.iter().map(|s| display_width(&s.text)).sum())
            .collect();
        assert!(
            widths.iter().all(|w| *w <= 20),
            "widths={widths:?} lines={:?}",
            plain(&lines)
        );
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
        // 单元格确实折了行（内容跨多行渲染）
        let text = plain(&lines);
        assert!(text.len() >= 7, "{text:?}");
        let joined = text.join("\n");
        assert!(
            joined.contains("hello") && joined.contains("bar"),
            "{joined:?}"
        );
    }

    /// 过窄时回退为 `a | b` 纯文本，不丢内容。
    #[test]
    fn narrow_table_falls_back_to_plain_text() {
        let md = "| Name | Value |\n|---|---|\n| foo | 1 |";
        let lines = render(md, 8);
        let text = plain(&lines).join("\n");
        assert!(!text.contains('┌'), "{text:?}");
        assert!(text.contains("Name") && text.contains("foo"), "{text:?}");
    }

    /// 单元格里的行内标记（code / html）不应被丢掉。
    #[test]
    fn table_cell_keeps_inline_markup() {
        let md = "| A | B |\n|---|---|\n| `code` | <b>hi</b> |";
        let lines = render(md, 80);
        let joined = plain(&lines).join("\n");
        assert!(joined.contains("code"), "{joined:?}");
        assert!(joined.contains("<b>hi</b>"), "{joined:?}");
        assert!(
            lines
                .iter()
                .flatten()
                .any(|s| s.key == "mdCode" && s.text == "code"),
            "{lines:?}"
        );
    }
}
