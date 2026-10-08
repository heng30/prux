//! 会话选择器渲染：/resume、/session 列表绘制（header + 树形列表 + 删除/重命名面板）。
//!
//! 状态与数据处理在 `session_selector`，事件处理在 `handlers::sessions`。

use super::super::{
    app::App,
    session_selector::{DisplayNode, MAX_VISIBLE, NameFilter, Scope, SessionSelector, SortMode},
};
use crate::utils::{
    self,
    display::truncate_display,
    glyphs::{DEF_CURSOR, DEF_DONE, DEF_DOT_EMPTY, DEF_DOT_RING},
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::time::Instant;

/// 列表行最大消息宽度下限
const MIN_MSG_WIDTH: usize = 10;

/// 按主题键取样式，缺失时用 `default`（十六进制色值）兜底。
fn theme_style(app: &App, key: &str, default: &str) -> Style {
    app.theme.style(key, default)
}

/// 选择器总高度（含上下分割线与内容上下各 1 行空行）。
///
/// 与 [`render`] 同源：都来自 [`session_lines`]（`内容行数 + 4`），
/// 保证「分割线 ↔ 内容」恰好 1 行空行间隔。
pub fn selector_height(app: &App, width: usize) -> u16 {
    let s = &app.session_selector;
    if !s.active {
        return 0;
    }
    super::panel::trim_panel_blanks(session_lines(app, width)).len() as u16 + 4
}

/// 选择器内容行（可能自带首尾空行；`render_panel_lines` 会统一裁掉再补留白）。
fn session_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if app.session_selector.rename_mode {
        render_rename_panel(&mut lines, app, width);
    } else {
        render_selector(&mut lines, app, width);
    }
    lines
}

/// 渲染选择器（session_selector.active 时占据输入框区域）
pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width as usize;
    let border = theme_style(app, "border", "#5f87ff");
    let lines = session_lines(app, width);
    super::panel::render_panel_lines(frame, area, width, border, lines);
}

/// 渲染选择器主体：标题/状态行、提示行、搜索输入与树形会话列表，追加进 `lines`。
fn render_selector(lines: &mut Vec<Line<'static>>, app: &App, width: usize) {
    let s = &app.session_selector;
    let accent = theme_style(app, "accent", "#8abeb7");
    let muted = theme_style(app, "muted", "#808080");
    let dim = theme_style(app, "dim", "#666666");
    let error = theme_style(app, "error", "#cc4444");
    let warning = theme_style(app, "warning", "#ffff00");
    let success = theme_style(app, "success", "#00ff00");
    let bold = Style::default().add_modifier(Modifier::BOLD);

    lines.push(Line::from("")); // 空行

    // header 行 1：标题 + 右侧状态
    render_header_row(lines, s, width, &accent, &muted, &bold);

    // hint 行 2/3：确认删除 / 状态消息 / 默认快捷键提示
    render_hint_row(lines, s, width, &accent, &muted, &dim, &error);

    // 空行 + 搜索输入行（对齐 pi Input：`> ` 前缀）
    lines.push(Line::from(""));
    let (view, _) = s.filter.visible_window(width.saturating_sub(2));
    let (f, _) = utils::display::truncate_display(&view, width.saturating_sub(2));
    lines.push(Line::from(vec![Span::raw("> "), Span::raw(f)]));
    lines.push(Line::from(""));

    // 空列表提示
    if s.display.is_empty() {
        render_empty_hint(lines, s, width, &muted);
        return;
    }

    // 列表 + 页码
    render_list(
        lines, s, width, &accent, &muted, &dim, &error, &warning, &success,
    );

    lines.push(Line::from(""));
}

/// header 行 1：标题 + 右侧状态
fn render_header_row(
    lines: &mut Vec<Line<'static>>,
    s: &SessionSelector,
    width: usize,
    accent: &Style,
    muted: &Style,
    bold: &Style,
) {
    let title = match s.scope {
        Scope::Current => "Resume Session (Current Folder)",
        Scope::All => "Resume Session (All)",
    };

    let mut right_spans: Vec<Span<'static>> = Vec::new();
    match s.scope {
        Scope::Current => {
            right_spans.push(Span::styled(
                format!("{DEF_DOT_RING} Current Folder"),
                *accent,
            ));
            right_spans.push(Span::styled(format!(" | {DEF_DOT_EMPTY} All"), *muted));
        }
        Scope::All => {
            right_spans.push(Span::styled(
                format!("{DEF_DOT_EMPTY} Current Folder | "),
                *muted,
            ));
            right_spans.push(Span::styled(format!("{DEF_DOT_RING} All"), *accent));
        }
    }
    right_spans.push(Span::raw("  "));
    right_spans.push(Span::styled("Name: ", *muted));
    right_spans.push(Span::styled(
        match s.name_filter {
            NameFilter::All => "All",
            NameFilter::Named => "Named",
        },
        *accent,
    ));
    right_spans.push(Span::raw("  "));
    right_spans.push(Span::styled("Sort: ", *muted));
    right_spans.push(Span::styled(
        match s.sort_mode {
            SortMode::Threaded => "Threaded",
            SortMode::Recent => "Recent",
            SortMode::Relevance => "Fuzzy",
        },
        *accent,
    ));

    // 渐进加载进度（结果按 mtime 顺序逐帧补齐）
    if s.is_loading() {
        let (done, total) = s.loading_progress();
        right_spans.push(Span::raw("  "));
        right_spans.push(Span::styled(format!("Loading {done}/{total}…"), *muted));
    }

    let right = truncate_spans(right_spans, width);
    let right_w = spans_width(&right);
    let avail_left = width.saturating_sub(right_w).saturating_sub(1);
    let mut left_spans = vec![Span::styled(title, *bold)];
    left_spans = truncate_spans(left_spans, avail_left);
    let left_w = spans_width(&left_spans);
    let spacing = width.saturating_sub(left_w).saturating_sub(right_w);
    let mut row: Vec<Span<'static>> = left_spans;
    row.push(Span::raw(" ".repeat(spacing)));
    row.extend(right);
    lines.push(Line::from(row));
}

/// hint 行 2/3：确认删除 / 状态消息 / 默认快捷键提示
fn render_hint_row(
    lines: &mut Vec<Line<'static>>,
    s: &SessionSelector,
    width: usize,
    accent: &Style,
    muted: &Style,
    dim: &Style,
    error: &Style,
) {
    if s.confirming_delete.is_some() {
        let sep = Span::styled(" · ", *muted);
        let mut hint: Vec<Span<'static>> = vec![Span::styled("Delete session? ", *error)];
        hint.push(Span::styled("enter", *dim));
        hint.push(Span::styled(" confirm", *muted));
        hint.push(sep);
        hint.push(Span::styled("escape", *dim));
        hint.push(Span::styled(" cancel", *muted));
        let hint = truncate_spans(hint, width);
        lines.push(Line::from(style_line(hint, *error)));
        lines.push(Line::from(""));
    } else if let Some((is_err, msg, until)) = &s.status {
        if Instant::now() < *until {
            let color = if *is_err { *error } else { *accent };
            let hint = truncate_spans(vec![Span::styled(msg.clone(), Style::default())], width);
            lines.push(Line::from(style_line(hint, color)));
            lines.push(Line::from(""));
        } else {
            render_default_hints(lines, s, width, dim, muted);
        }
    } else {
        render_default_hints(lines, s, width, dim, muted);
    }
}

/// 空列表提示
fn render_empty_hint(
    lines: &mut Vec<Line<'static>>,
    s: &SessionSelector,
    width: usize,
    muted: &Style,
) {
    // 渐进加载中且尚无结果：先提示加载中（而不是"没有会话"）
    if s.is_loading() && s.current.is_empty() && s.all.is_empty() {
        let (done, total) = s.loading_progress();
        let (m, _) = truncate_display(&format!("Loading sessions… ({done}/{total})"), width);
        lines.push(Line::from(Span::styled(format!("  {m}"), *muted)));
        lines.push(Line::from(""));
        return;
    }

    let msg = match s.name_filter {
        NameFilter::Named => {
            if s.scope == Scope::All {
                "No named sessions found. Press ctrl+n to show all."
            } else {
                "No named sessions in current folder. Press ctrl+n to show all, or Tab to view all."
            }
        }
        NameFilter::All => {
            if s.scope == Scope::All {
                "No sessions found"
            } else {
                "No sessions in current folder. Press Tab to view all."
            }
        }
    };
    let (m, _) = utils::display::truncate_display(msg, width);
    lines.push(Line::from(Span::styled(format!("  {}", m), *muted)));
    lines.push(Line::from(""));
}

/// 列表（居中滚动 maxVisible=10）+ 页码
#[allow(clippy::too_many_arguments)]
fn render_list(
    lines: &mut Vec<Line<'static>>,
    s: &SessionSelector,
    width: usize,
    accent: &Style,
    muted: &Style,
    dim: &Style,
    error: &Style,
    warning: &Style,
    success: &Style,
) {
    let total = s.display.len();
    let start = s
        .selected
        .saturating_sub(MAX_VISIBLE / 2)
        .min(total.saturating_sub(MAX_VISIBLE));
    let end = (start + MAX_VISIBLE).min(total);
    let show_cwd = s.scope == Scope::All;

    for i in start..end {
        render_list_row(
            lines, s, i, show_cwd, width, accent, dim, error, warning, success,
        );
    }

    // 页码（仅滚动时）
    if start > 0 || end < total {
        lines.push(Line::from(Span::styled(
            format!("  ({}/{})", s.selected + 1, total),
            *muted,
        )));
    }
}

/// 渲染单行会话列表项：左侧光标+树前缀+消息文本，右侧 msgCount+age（含 cwd/path 可选列）
#[allow(clippy::too_many_arguments)]
fn render_list_row(
    lines: &mut Vec<Line<'static>>,
    s: &SessionSelector,
    i: usize,
    show_cwd: bool,
    width: usize,
    accent: &Style,
    dim: &Style,
    error: &Style,
    warning: &Style,
    success: &Style,
) {
    let node = &s.display[i];
    let row = &s.rows[node.idx];
    let selected = i == s.selected;
    let confirming = s
        .confirming_delete
        .as_deref()
        .map(|p| p == row.path.to_string_lossy().as_ref())
        .unwrap_or(false);

    // 左侧：光标 + 树前缀 + 消息文本
    // 选中行：→ 箭头 + accent 文本高亮（对齐 /model 选中样式，无背景）；
    // 当前打开的会话：✓ 标记（已打开不可选中，因此不会与选中符重合）
    let cursor = if selected && !row.is_current {
        Span::styled(DEF_CURSOR, *accent)
    } else if row.is_current {
        Span::styled(format!("{DEF_DONE} "), *success)
    } else {
        Span::raw("  ")
    };

    let prefix = build_tree_prefix(node);
    let display_text = row.name.as_deref().unwrap_or(row.text.as_str());
    let normalized: String = display_text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string();

    // 右侧：msgCount + age（all scope 加 cwd；path 显示加路径）
    let mut right_parts: Vec<String> = Vec::new();
    if show_cwd && !row.cwd.is_empty() {
        right_parts.push(shorten_path(&row.cwd));
    }
    if s.show_path {
        right_parts.push(shorten_path(&row.path.to_string_lossy()));
    }
    right_parts.push(format!(
        "{}{} {}",
        row.msg_count,
        if row.msg_count_capped { "+" } else { "" },
        row.age
    ));
    let right_str = right_parts.join(" ");

    let prefix_w = utils::display::display_width(&prefix);
    let right_w = utils::display::display_width(&right_str) + 2;
    let avail = width
        .saturating_sub(2)
        .saturating_sub(prefix_w)
        .saturating_sub(right_w);
    let (msg, _) = utils::display::truncate_display(&normalized, avail.max(MIN_MSG_WIDTH));

    // 消息色：确认删除 → error；选中 → accent（切换高亮）；
    // 命名会话 → warning
    let msg_style = if confirming {
        *error
    } else if selected {
        *accent
    } else if row.name.is_some() {
        *warning
    } else {
        Style::default()
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(cursor);
    if !prefix.is_empty() {
        spans.push(Span::styled(prefix, *dim));
    }
    spans.push(Span::styled(msg, msg_style));
    let left_w = spans_width(&spans);
    let spacing = width
        .saturating_sub(left_w)
        .saturating_sub(utils::display::display_width(&right_str))
        .max(1);
    spans.push(Span::raw(" ".repeat(spacing)));
    spans.push(Span::styled(
        right_str,
        if confirming { *error } else { *dim },
    ));
    lines.push(Line::from(truncate_spans(spans, width)));
}

/// 整行套一种前景色，但保留已显式设置前景的 span（内层 dim/muted 颜色优先级高于外层 error）
fn style_line(spans: Vec<Span<'static>>, fg: Style) -> Vec<Span<'static>> {
    spans
        .into_iter()
        .map(|sp| {
            if sp.style.fg.is_some() {
                sp
            } else {
                Span::styled(sp.content, sp.style.patch(fg))
            }
        })
        .collect()
}

/// 追加默认快捷键提示两行（tab scope / 正则搜索，以及 ctrl+s 排序等操作），超宽截断。
fn render_default_hints(
    lines: &mut Vec<Line<'static>>,
    s: &SessionSelector,
    width: usize,
    dim: &Style,
    muted: &Style,
) {
    // hint1：tab scope · re:<pattern> regex · "phrase" exact
    let sep = " · ";
    let mut hint1 = vec![Span::styled("tab", *dim)];
    hint1.push(Span::styled(" scope", *muted));
    hint1.push(Span::styled(sep, *muted));
    hint1.push(Span::styled(
        "re:<pattern> regex · \"phrase\" exact",
        *muted,
    ));
    let hint1 = truncate_spans(hint1, width);
    lines.push(Line::from(hint1));

    // hint2：ctrl+s sort · ctrl+n named · ctrl+d delete · ctrl+p path (on/off) · ctrl+r rename
    let mut hint2: Vec<Span<'static>> = Vec::new();
    let push_hint = |key: String, desc: String, hint2: &mut Vec<Span<'static>>| {
        if !hint2.is_empty() {
            hint2.push(Span::styled(sep, *muted));
        }
        hint2.push(Span::styled(key, *dim));
        hint2.push(Span::styled(format!(" {}", desc), *muted));
    };
    push_hint("ctrl+s".to_string(), "sort".to_string(), &mut hint2);
    push_hint("ctrl+n".to_string(), "named".to_string(), &mut hint2);
    push_hint("ctrl+d".to_string(), "delete".to_string(), &mut hint2);
    push_hint(
        "ctrl+p".to_string(),
        format!("path {}", if s.show_path { "(on)" } else { "(off)" }),
        &mut hint2,
    );
    push_hint("ctrl+r".to_string(), "rename".to_string(), &mut hint2);
    push_hint("ctrl+g".to_string(), "next".to_string(), &mut hint2);
    push_hint("home/end".to_string(), "first/last".to_string(), &mut hint2);
    let hint2 = truncate_spans(hint2, width);
    lines.push(Line::from(hint2));
}

/// 渲染重命名面板：标题、`> ` 输入行（已按宽度截断）与保存/取消提示。
fn render_rename_panel(lines: &mut Vec<Line<'static>>, app: &App, width: usize) {
    let s = &app.session_selector;
    let muted = theme_style(app, "muted", "#808080");
    let bold = Style::default().add_modifier(Modifier::BOLD);

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Rename Session", bold)));
    lines.push(Line::from(""));
    let (view, _) = s.rename_input.visible_window(width.saturating_sub(2));
    let (f, _) = utils::display::truncate_display(&view, width.saturating_sub(2));
    lines.push(Line::from(vec![Span::raw("> "), Span::raw(f)]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "enter to save · escape to cancel",
        muted,
    )));
    lines.push(Line::from(""));
}

/// 根据节点深度与祖先连接状态拼出树形前缀（缩进 + `├─`/`└─`），顶层为空串。
fn build_tree_prefix(node: &DisplayNode) -> String {
    if node.depth == 0 {
        return String::new();
    }
    let mut out = String::new();
    for c in &node.ancestor_continues {
        out.push_str(if *c { "│  " } else { "   " });
    }
    out.push_str(if node.is_last { "└─ " } else { "├─ " });
    out
}

/// 缩短路径：home 前缀 → ~
fn shorten_path(p: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && p.starts_with(&home) {
        format!("~{}", &p[home.len()..])
    } else {
        p.to_string()
    }
}

/// 一组 span 的显示宽度之和（按字符显示宽度，不是字节长度）。
fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|sp| utils::display::display_width(&sp.content))
        .sum()
}

/// 按可见宽度截断 spans（尾截断）
fn truncate_spans(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for sp in spans {
        let w = utils::display::display_width(&sp.content);
        if used + w > max_width {
            let room = max_width.saturating_sub(used);
            if room > 0 {
                let (t, _) = utils::display::truncate_display(&sp.content, room);
                if !t.is_empty() {
                    out.push(Span::styled(t, sp.style));
                }
            }
            break;
        }
        out.push(sp);
        used += w;
    }
    out
}

/// 截断/填充行并渲染（边框上下）
#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::session_selector::SessionRow;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn test_row(text: &str) -> SessionRow {
        SessionRow {
            path: PathBuf::from(format!("/tmp/{}.jsonl", text.replace(' ', "_"))),
            id: format!("id-{}", text.replace(' ', "_")),
            name: None,
            text: text.to_string(),
            msg_count: 1,
            msg_count_capped: false,
            modified: SystemTime::now(),
            age: "now".to_string(),
            cwd: String::new(),
            parent_session_id: None,
            is_current: false,
            search_text: text.to_string(),
        }
    }

    fn row_with_name(text: &str, name: &str) -> SessionRow {
        let mut r = test_row(text);
        r.name = Some(name.to_string());
        r
    }

    #[test]
    fn selector_height_and_render_text() {
        let mut app = App::new();
        app.session_selector
            .open("/tmp/cwd", std::path::Path::new("/tmp/agent"), None);
        app.session_selector.current = vec![test_row("first session"), test_row("second")];
        app.session_selector.recompute();
        let h = selector_height(&app, 120);
        // 内容 8 行（header/hint1/hint2/空/搜索/空/列表2）+ 上下分割线2 + 上下留白2
        assert_eq!(h, 12);

        // 超过 maxVisible 后加页码行
        app.session_selector.current = (0..15).map(|i| test_row(&format!("msg {}", i))).collect();
        app.session_selector.recompute();
        let h = selector_height(&app, 120);
        assert_eq!(h, 21);
    }

    /// 把渲染行转纯文本（测试布局断言）
    fn render_lines_text(app: &App, width: usize) -> Vec<String> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        render_selector(&mut lines, app, width);
        lines
            .into_iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn selector_has_exactly_one_blank_line_around_content() {
        // 统一留白：/resume、/session 选择器与其它 `/` 面板一致，内容上下各恰好 1 行空行。
        let mut app = App::new();
        app.session_selector
            .open("/tmp/cwd", std::path::Path::new("/tmp/agent"), None);
        app.session_selector.current = vec![test_row("first session"), test_row("second")];
        app.session_selector.recompute();
        let w = 120u16;
        let h = selector_height(&app, w as usize);
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, Rect::new(0, 0, w, h), &app))
            .unwrap();
        let buf = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (0..w)
                .map(|x| {
                    buf.cell((x, y))
                        .map(|c| c.symbol().to_string())
                        .unwrap_or_default()
                })
                .collect()
        };
        assert!(row(0).starts_with('─'), "上分割线: {:?}", row(0));
        assert!(row(h - 1).starts_with('─'), "下分割线: {:?}", row(h - 1));
        let inner: Vec<String> = (1..h - 1).map(row).collect();
        let lead = inner.iter().take_while(|r| r.trim().is_empty()).count();
        let trail = inner
            .iter()
            .rev()
            .take_while(|r| r.trim().is_empty())
            .count();
        assert_eq!(lead, 1, "顶部恰好 1 行空行");
        assert_eq!(trail, 1, "底部恰好 1 行空行");
    }

    #[test]
    fn render_header_and_hints_match_pi_layout() {
        let mut app = App::new();
        app.session_selector
            .open("/tmp/cwd", std::path::Path::new("/tmp/agent"), None);
        app.session_selector.current = vec![
            row_with_name("named session message", "my-name"),
            test_row("another session here"),
        ];
        app.session_selector.recompute();
        let out = render_lines_text(&app, 120);
        // 空行
        assert_eq!(out[0].trim(), "");
        // header 1：标题 + 右侧 scope/name/sort 状态（对齐 pi）
        assert!(out[1].contains("Resume Session (Current Folder)"));
        assert!(out[1].contains("◉ Current Folder | ○ All"));
        assert!(out[1].contains("Name: All"));
        assert!(out[1].contains("Sort: Threaded"));
        // hint 行（对齐 pi SessionSelectorHeader）
        assert!(out[2].contains("tab scope"));
        assert!(out[2].contains("re:<pattern> regex · \"phrase\" exact"));
        assert!(out[3].contains("ctrl+s sort"));
        assert!(out[3].contains("ctrl+n named"));
        assert!(out[3].contains("ctrl+d delete"));
        assert!(out[3].contains("ctrl+p path (off)"));
        assert!(out[3].contains("ctrl+r rename"));
        assert!(out[3].contains("ctrl+g next"));
        assert!(out[3].contains("home/end first/last"));
        // 搜索行
        let search = out[5].as_str();
        assert!(search.starts_with("> "), "search row: {:?}", search);
        // 列表行：选中 `→ ` 前缀（对齐 /model）+ 消息 + 右侧 msgCount age；命名会话显示名称
        // 两行均渲染：选中行 `→ ` 前缀 + 右侧 msgCount age；命名会话显示名称
        let rows: Vec<&String> = out
            .iter()
            .filter(|l| l.starts_with("→ ") || l.starts_with("  "))
            .collect();
        assert_eq!(rows.len(), 2, "list rows: {:#?}", out);
        assert!(
            rows.iter().any(|l| l.contains("my-name")),
            "named row: {:#?}",
            rows
        );
        assert!(
            rows.iter().any(|l| l.contains("another session here")),
            "msg row: {:#?}",
            rows
        );
        assert!(
            rows.iter().all(|l| l.trim_end().ends_with("1 now")),
            "right side: {:#?}",
            rows
        );
        assert!(rows[0].starts_with("→ "), "selected row first: {:#?}", rows);
        // 结尾空行
        assert_eq!(out.last().map(|l| l.trim()), Some(""));
    }

    #[test]
    fn render_marks_current_session_with_check() {
        let mut app = App::new();
        app.session_selector
            .open("/tmp/cwd", std::path::Path::new("/tmp/agent"), None);
        let mut cur = test_row("current session");
        cur.is_current = true;
        app.session_selector.current = vec![cur, test_row("other session")];
        app.session_selector.recompute();

        let idx_of = |app: &App, text: &str| {
            app.session_selector
                .display
                .iter()
                .position(|n| app.session_selector.rows[n.idx].text == text)
                .unwrap()
        };
        let cur_idx = idx_of(&app, "current session");
        let other_idx = idx_of(&app, "other session");

        // 选中其它行：当前会话行左侧为 ✓ 标记
        app.session_selector.selected = other_idx;
        let out = render_lines_text(&app, 120);
        let cur_line = out.iter().find(|l| l.contains("current session")).unwrap();
        assert!(cur_line.starts_with("✓ "), "current row: {cur_line:?}");

        // 即便选中项指向当前会话（已打开不可选中）：仍只显示 ✓，不显示选中符
        app.session_selector.selected = cur_idx;
        let out = render_lines_text(&app, 120);
        let cur_line = out.iter().find(|l| l.contains("current session")).unwrap();
        assert!(
            cur_line.starts_with("✓ "),
            "current row must never show selection: {cur_line:?}"
        );
    }

    #[test]
    fn render_scope_switch_and_path_show() {
        let mut app = App::new();
        app.session_selector
            .open("/tmp/cwd", std::path::Path::new("/tmp/agent"), None);
        let mut row = test_row("hello world");
        let home = std::env::var("HOME").unwrap_or_default();
        row.cwd = format!("{}/proj", home);
        row.path = std::path::PathBuf::from(format!("{}/proj/session.jsonl", home));
        app.session_selector.all = vec![row];
        app.session_selector.scope = Scope::All;
        app.session_selector.recompute();
        // all scope：标题 Resume Session (All) + ◉ All；右侧显示 cwd
        let out = render_lines_text(&app, 120);
        assert!(out[1].contains("Resume Session (All)"));
        assert!(out[1].contains("◉ All"));
        assert!(
            out[7].contains("~/proj"),
            "cwd shown in all scope: {:?}",
            out[7]
        );
        // path 显示
        app.session_selector.toggle_path();
        let out = render_lines_text(&app, 120);
        assert!(out[3].contains("ctrl+p path (on)"));
        assert!(out[7].contains("~/proj/session.jsonl"));
    }
}
