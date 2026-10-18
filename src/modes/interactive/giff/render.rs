//! `/giff` 视图渲染：header / 文件列表 / 双栏与统一 diff / 帮助模态 / 底部键位行。
//!
//! 内容全部由 [`GiffView`] 持有；渲染同时把最近一帧的区域与翻页步长写回视图
//! （鼠标滚轮命中判定与 `PageUp`/`PageDown` 用）。

use super::{GiffStyle, GiffView, LineChange, Pane, ViewMode};
use crate::{
    modes::interactive::{render::syntax_hl, theme::Theme},
    utils::glyphs::DEF_ELLIPSIS,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState,
    },
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// 渲染整个 `/giff` 视图。`area` 是 TUI 分给视图的整块区域（底栏与输入区由调用方排除）。
pub fn render(frame: &mut Frame, area: Rect, view: &mut GiffView, theme: &Theme) {
    view.style = GiffStyle::from_theme(theme);
    ensure_syntect_theme(view, theme);

    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);

    render_header(frame, view, chunks[0]);

    // 内容区可显示行数（扣掉上下边框）：翻页步长与滚动夹取都用它
    let content_area = chunks[1];
    let visible = content_area.height.saturating_sub(2) as usize;
    view.page_rows = visible.max(1) as u16;
    clamp_scroll(view, visible);

    match view.view_mode {
        ViewMode::SideBySide => {
            let cols = Layout::horizontal([
                Constraint::Percentage(20),
                Constraint::Percentage(40),
                Constraint::Percentage(40),
            ])
            .split(content_area);
            view.file_list_area = Some(cols[0]);
            view.content_area = Some(Rect {
                x: cols[1].x,
                y: content_area.y,
                width: cols[1].width + cols[2].width,
                height: content_area.height,
            });

            render_file_list(frame, view, cols[0]);
            if view.current_file().is_some() {
                render_side_by_side(frame, view, cols[1], cols[2]);
            }
        }
        ViewMode::Unified => {
            let cols = Layout::horizontal([Constraint::Percentage(20), Constraint::Percentage(80)])
                .split(content_area);
            view.file_list_area = Some(cols[0]);
            view.content_area = Some(cols[1]);

            render_file_list(frame, view, cols[0]);
            if view.current_file().is_some() {
                render_unified_diff(frame, view, cols[1]);
            }
        }
    }

    render_help(frame, view, chunks[2]);

    if view.show_help {
        render_help_modal(frame, view, area);
    }
}

/// 顶部一行：视图标识、两侧标签、视图模式、当前文件与计数。
fn render_header(frame: &mut Frame, view: &GiffView, area: Rect) {
    let t = &view.style;
    let view_mode = match view.view_mode {
        ViewMode::SideBySide => "Side-by-Side",
        ViewMode::Unified => "Unified",
    };
    let current = if view.file_count() > 0 {
        view.current_file_idx + 1
    } else {
        0
    };
    let current_file = view.current_file().unwrap_or("");

    let separator = || Span::styled(" │ ", Style::default().fg(t.border_dim));
    let mut spans = vec![
        Span::styled(
            " giff ",
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled("│ ", Style::default().fg(t.border_dim)),
        Span::styled(
            format!("{} → {}", view.left_label, view.right_label),
            Style::default().fg(t.fg_normal),
        ),
        separator(),
        Span::styled(view_mode, Style::default().fg(t.fg_dim)),
    ];

    if !current_file.is_empty() {
        spans.push(separator());
        spans.push(Span::styled(
            current_file.to_string(),
            Style::default()
                .fg(t.fg_bright)
                .add_modifier(Modifier::BOLD),
        ));
    }

    spans.push(separator());
    spans.push(Span::styled(
        format!("{}/{}", current, view.file_count()),
        Style::default().fg(t.fg_dim),
    ));

    let header = Paragraph::new(Line::from(spans)).style(Style::default().bg(t.bg_header));
    frame.render_widget(header, area);
}

/// 左侧文件列表：选中行整行高亮，行尾带 `+n -m` 统计。
fn render_file_list(frame: &mut Frame, view: &GiffView, area: Rect) {
    let t = &view.style;
    let is_focused = view.focused_pane == Pane::FileList;
    let border_color = if is_focused {
        t.border_focused
    } else {
        t.border_dim
    };
    let title_style = if is_focused {
        Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t.fg_dim)
    };

    let block = Block::default()
        .title(Span::styled(" Files ", title_style))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));

    if view.file_names.is_empty() {
        let empty = Paragraph::new(Span::styled("  No changes", Style::default().fg(t.fg_dim)))
            .block(block);
        frame.render_widget(empty, area);
        return;
    }

    // 边框 2 列 + 选中标记 "▌ " 2 列
    const FILE_LIST_CHROME_WIDTH: u16 = 4;
    let inner_width = area.width.saturating_sub(FILE_LIST_CHROME_WIDTH) as usize;

    let items: Vec<ListItem> = view
        .file_names
        .iter()
        .enumerate()
        .map(|(i, file)| {
            let (adds, dels) = count_file_changes(view, file);
            let is_current = i == view.current_file_idx;
            let name_style = if is_current {
                Style::default()
                    .fg(t.fg_bright)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.fg_normal)
            };

            let (stat_spans, stats_width) = build_file_stats(adds, dels, t);
            let max_name_width = inner_width.saturating_sub(stats_width);
            let display_name = truncate_path(file, max_name_width);

            let mut spans = vec![Span::styled(display_name, name_style)];
            spans.extend(stat_spans);
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(t.bg_selection))
        .highlight_symbol("▌ ");

    frame.render_stateful_widget(
        list,
        area,
        &mut ListState::default().with_selected(Some(view.current_file_idx)),
    );
}

/// 双栏模式下的一栏：标题带百分比位置，内容按 `scroll` 滚动，超出视口时右侧画滚动条。
#[allow(clippy::too_many_arguments)]
fn render_diff_pane(
    frame: &mut Frame,
    title: &str,
    lines: &[LineChange],
    filename: &str,
    scroll: usize,
    is_focused: bool,
    area: Rect,
    view: &GiffView,
) {
    let t = &view.style;
    let border_color = if is_focused {
        t.border_focused
    } else {
        t.border_dim
    };
    let title_style = if is_focused {
        Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t.fg_dim)
    };

    let highlighted = highlight_line_changes(lines, filename, t, view.syntect_theme.as_ref());
    let total_lines = highlighted.len();
    let visible_height = area.height.saturating_sub(2) as usize;

    let title_text = if total_lines > visible_height {
        let max_scroll = total_lines.saturating_sub(visible_height);
        let pos = scroll.min(max_scroll);
        let pct = (pos * 100).checked_div(max_scroll).unwrap_or(0);
        format!(" {} ({}%) ", title, pct)
    } else {
        format!(" {} ", title)
    };

    let block = Block::default()
        .title(Span::styled(title_text, title_style))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));

    let paragraph = Paragraph::new(Text::from(highlighted))
        .block(block)
        .scroll((scroll.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(paragraph, area);

    if total_lines > visible_height {
        let scrollbar_area = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(2),
        );
        let max_scroll = total_lines.saturating_sub(visible_height);
        let mut scrollbar_state = ScrollbarState::new(max_scroll).position(scroll.min(max_scroll));
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            scrollbar_area,
            &mut scrollbar_state,
        );
    }
}

/// 双栏渲染：左右各自按改动块对齐后逐行画。
fn render_side_by_side(frame: &mut Frame, view: &GiffView, base_area: Rect, head_area: Rect) {
    let Some(current_file) = view.current_file() else {
        return;
    };
    let Some((base_lines, head_lines)) = view.file_changes.get(current_file) else {
        return;
    };

    let scroll = view
        .scroll_positions
        .get(current_file)
        .copied()
        .unwrap_or(0);
    let is_focused = view.focused_pane == Pane::DiffContent;

    let (aligned_base, aligned_head) = align_lines(base_lines, head_lines);
    render_diff_pane(
        frame,
        &view.left_label,
        &aligned_base,
        current_file,
        scroll,
        is_focused,
        base_area,
        view,
    );
    render_diff_pane(
        frame,
        &view.right_label,
        &aligned_head,
        current_file,
        scroll,
        is_focused,
        head_area,
        view,
    );
}

/// 统一视图渲染：删除行在前、新增行在后、上下文行只出现一次。
fn render_unified_diff(frame: &mut Frame, view: &GiffView, area: Rect) {
    let Some(current_file) = view.current_file() else {
        return;
    };
    let Some((base_lines, head_lines)) = view.file_changes.get(current_file) else {
        return;
    };

    let scroll = view
        .scroll_positions
        .get(current_file)
        .copied()
        .unwrap_or(0);
    let is_focused = view.focused_pane == Pane::DiffContent;
    let unified = build_unified_lines(base_lines, head_lines);
    let title = format!("{} vs {}", view.left_label, view.right_label);

    render_diff_pane(
        frame,
        &title,
        &unified,
        current_file,
        scroll,
        is_focused,
        area,
        view,
    );
}

/// 生成左右对齐的行序列：改动块内逐行配对，短的一侧补 `(0, "")` 占位行。
pub(super) fn align_lines(
    base_lines: &[LineChange],
    head_lines: &[LineChange],
) -> (Vec<LineChange>, Vec<LineChange>) {
    let mut aligned_base = Vec::new();
    let mut aligned_head = Vec::new();
    let mut bi = 0;
    let mut hi = 0;

    while bi < base_lines.len() || hi < head_lines.len() {
        let b_is_change = bi < base_lines.len() && base_lines[bi].1.starts_with('-');
        let h_is_change = hi < head_lines.len() && head_lines[hi].1.starts_with('+');

        if b_is_change || h_is_change {
            let mut b_chunk = Vec::new();
            let mut h_chunk = Vec::new();

            while bi < base_lines.len() && base_lines[bi].1.starts_with('-') {
                b_chunk.push(base_lines[bi].clone());
                bi += 1;
            }
            while hi < head_lines.len() && head_lines[hi].1.starts_with('+') {
                h_chunk.push(head_lines[hi].clone());
                hi += 1;
            }

            let max_len = b_chunk.len().max(h_chunk.len());
            for i in 0..max_len {
                aligned_base.push(b_chunk.get(i).cloned().unwrap_or((0, String::new())));
                aligned_head.push(h_chunk.get(i).cloned().unwrap_or((0, String::new())));
            }
        } else if bi < base_lines.len() && hi < head_lines.len() {
            // 上下文行：两侧各推一行
            aligned_base.push(base_lines[bi].clone());
            aligned_head.push(head_lines[hi].clone());
            bi += 1;
            hi += 1;
        } else if bi < base_lines.len() {
            aligned_base.push(base_lines[bi].clone());
            aligned_head.push((0, String::new()));
            bi += 1;
        } else {
            aligned_base.push((0, String::new()));
            aligned_head.push(head_lines[hi].clone());
            hi += 1;
        }
    }

    (aligned_base, aligned_head)
}

/// 双栏模式下对齐后的总行数（不分配行序列，供滚动夹取使用）。
fn aligned_line_count(base_lines: &[LineChange], head_lines: &[LineChange]) -> usize {
    let mut count = 0;
    let mut bi = 0;
    let mut hi = 0;

    while bi < base_lines.len() || hi < head_lines.len() {
        let b_is_change = bi < base_lines.len() && base_lines[bi].1.starts_with('-');
        let h_is_change = hi < head_lines.len() && head_lines[hi].1.starts_with('+');

        if b_is_change || h_is_change {
            let mut b_count = 0;
            let mut h_count = 0;
            while bi < base_lines.len() && base_lines[bi].1.starts_with('-') {
                b_count += 1;
                bi += 1;
            }
            while hi < head_lines.len() && head_lines[hi].1.starts_with('+') {
                h_count += 1;
                hi += 1;
            }
            count += b_count.max(h_count);
        } else {
            if bi < base_lines.len() {
                bi += 1;
            }
            if hi < head_lines.len() {
                hi += 1;
            }
            count += 1;
        }
    }

    count
}

/// 统一视图下的总行数（不分配行序列，供滚动夹取使用）。
pub(super) fn unified_line_count(base_lines: &[LineChange], head_lines: &[LineChange]) -> usize {
    let mut count = 0;
    let mut bi = 0;
    let mut hi = 0;

    while bi < base_lines.len() || hi < head_lines.len() {
        let b_is_change = bi < base_lines.len() && base_lines[bi].1.starts_with('-');
        let h_is_change = hi < head_lines.len() && head_lines[hi].1.starts_with('+');

        if b_is_change || h_is_change {
            while bi < base_lines.len() && base_lines[bi].1.starts_with('-') {
                count += 1;
                bi += 1;
            }
            while hi < head_lines.len() && head_lines[hi].1.starts_with('+') {
                count += 1;
                hi += 1;
            }
        } else {
            if bi < base_lines.len() {
                bi += 1;
            }
            if hi < head_lines.len() {
                hi += 1;
            }
            count += 1;
        }
    }

    count
}

/// 生成统一视图的行序列：同一改动块内先列删除、再列新增，上下文行取左侧一份。
pub(super) fn build_unified_lines(
    base_lines: &[LineChange],
    head_lines: &[LineChange],
) -> Vec<LineChange> {
    let mut unified = Vec::new();
    let mut bi = 0;
    let mut hi = 0;

    while bi < base_lines.len() || hi < head_lines.len() {
        let b_is_change = bi < base_lines.len() && base_lines[bi].1.starts_with('-');
        let h_is_change = hi < head_lines.len() && head_lines[hi].1.starts_with('+');

        if b_is_change || h_is_change {
            while bi < base_lines.len() && base_lines[bi].1.starts_with('-') {
                unified.push(base_lines[bi].clone());
                bi += 1;
            }
            while hi < head_lines.len() && head_lines[hi].1.starts_with('+') {
                unified.push(head_lines[hi].clone());
                hi += 1;
            }
        } else if bi < base_lines.len() {
            unified.push(base_lines[bi].clone());
            bi += 1;
            if hi < head_lines.len() {
                hi += 1;
            }
        } else if hi < head_lines.len() {
            unified.push(head_lines[hi].clone());
            hi += 1;
        }
    }

    unified
}

/// 把行序列渲染成带行号、增删标记与语法高亮的行；`(0, "")` 渲染为空行（双栏对齐占位）。
fn highlight_line_changes(
    lines: &[LineChange],
    filename: &str,
    style: &GiffStyle,
    syntheme: Option<&syntect::highlighting::Theme>,
) -> Vec<Line<'static>> {
    let mut session = syntheme.and_then(|theme| syntax_hl::session_for_filename(filename, theme));

    lines
        .iter()
        .map(|(line_num, line)| {
            if *line_num == 0 {
                return Line::from("");
            }

            if let Some(rest) = line.strip_prefix('-') {
                let mut spans = vec![
                    Span::styled(
                        format!("{line_num:4} "),
                        Style::default().fg(style.fg_line_num).bg(style.bg_removed),
                    ),
                    Span::styled(
                        "- ",
                        Style::default()
                            .fg(style.fg_removed_marker)
                            .bg(style.bg_removed)
                            .add_modifier(Modifier::BOLD),
                    ),
                ];
                spans.extend(highlight_with_bg(session.as_mut(), rest, style.bg_removed));
                Line::from(spans)
            } else if let Some(rest) = line.strip_prefix('+') {
                let mut spans = vec![
                    Span::styled(
                        format!("{line_num:4} "),
                        Style::default().fg(style.fg_line_num).bg(style.bg_added),
                    ),
                    Span::styled(
                        "+ ",
                        Style::default()
                            .fg(style.fg_added_marker)
                            .bg(style.bg_added)
                            .add_modifier(Modifier::BOLD),
                    ),
                ];
                spans.extend(highlight_with_bg(session.as_mut(), rest, style.bg_added));
                Line::from(spans)
            } else {
                // 上下文行带一个前导空格：剥掉后与增删行正文对齐，缩进敏感的
                // 语言（Python / YAML）也才能正确着色。
                let content = line.strip_prefix(' ').unwrap_or(line);
                let mut spans = vec![
                    Span::styled(
                        format!("{line_num:4} "),
                        Style::default().fg(style.fg_line_num),
                    ),
                    Span::styled("  ", Style::default()),
                ];
                spans.extend(highlight_with_bg(session.as_mut(), content, Color::Reset));
                Line::from(spans)
            }
        })
        .collect()
}

/// 用高亮会话渲染一段文本并铺上整行背景色；无会话（未识别语言 / 关闭高亮）时按默认前景。
fn highlight_with_bg(
    session: Option<&mut syntax_hl::HighlightSession<'_>>,
    text: &str,
    bg: Color,
) -> Vec<Span<'static>> {
    match session {
        Some(session) => session
            .line(text, Style::default())
            .into_iter()
            .map(|(style, text)| Span::styled(text, style.bg(bg)))
            .collect(),
        None => vec![Span::styled(text.to_string(), Style::default().bg(bg))],
    }
}

/// 底部一行键位提示：输入态与实际导航键位不同，两套都给。
fn render_help(frame: &mut Frame, view: &GiffView, area: Rect) {
    let t = &view.style;
    let pairs: &[(&str, &str)] = if view.input_active {
        &[("Enter", "Send"), ("Esc", "Cancel input")]
    } else {
        &[
            ("q", "Quit"),
            ("j/k", "Navigate"),
            ("Tab", "Focus"),
            ("h/l", "Panes"),
            ("u", "View"),
            ("i", "Input"),
            ("PgUp/Dn", "Page"),
            ("?", "Help"),
        ]
    };

    let mut spans: Vec<Span> = vec![Span::styled(" ", Style::default())];
    for (i, (key, desc)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", Style::default().fg(t.border_dim)));
        }
        spans.push(Span::styled(
            (*key).to_owned(),
            Style::default().fg(t.fg_key),
        ));
        spans.push(Span::styled(
            format!(" {}", desc),
            Style::default().fg(t.fg_dim),
        ));
    }

    let help = Paragraph::new(Line::from(spans)).style(Style::default().bg(t.bg_header));
    frame.render_widget(help, area);
}

/// 帮助模态：居中弹窗列出全部键位；打开期间 `?` / `q` / `Esc` 关闭。
fn render_help_modal(frame: &mut Frame, view: &GiffView, area: Rect) {
    let t = &view.style;
    let modal_area = centered_rect(52, 26, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.border_modal))
        .style(Style::default().bg(t.bg_modal))
        .title(Span::styled(
            " Keybindings ",
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled(" ? ", Style::default().fg(t.fg_badge).bg(t.bg_key_badge)),
            Span::styled(" ", Style::default()),
            Span::styled(" q ", Style::default().fg(t.fg_badge).bg(t.bg_key_badge)),
            Span::styled(" to close ", Style::default().fg(t.fg_dim)),
        ]));

    frame.render_widget(Clear, modal_area);
    frame.render_widget(&block, modal_area);

    let inner = block.inner(modal_area);
    let inner_width = inner.width as usize;

    let section = |title: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled("  ● ", Style::default().fg(t.accent)),
            Span::styled(
                title.to_owned(),
                Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
            ),
        ])
    };
    let separator = |width: usize| -> Line<'static> {
        Line::from(Span::styled(
            "─".repeat(width),
            Style::default().fg(t.fg_separator),
        ))
    };
    let row = |key: &str, desc: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(
                format!(" {:^8} ", key),
                Style::default().fg(t.fg_badge).bg(t.bg_key_badge),
            ),
            Span::styled("  ", Style::default()),
            Span::styled(desc.to_owned(), Style::default().fg(t.fg_normal)),
        ])
    };

    let mut lines: Vec<Line<'static>> = vec![
        Line::from(""),
        section("Navigation"),
        Line::from(""),
        row("j / ↓", "Move down / next item"),
        row("k / ↑", "Move up / previous item"),
        row("PgDn", "Page down"),
        row("PgUp", "Page up"),
        row("Home", "Go to first"),
        row("End", "Go to last"),
        separator(inner_width),
        Line::from(""),
        section("View"),
        Line::from(""),
        row("Tab", "Toggle focus (files / diff)"),
        row("h / ←", "Focus file list"),
        row("l / →", "Focus diff content"),
        row("u", "Toggle unified / side-by-side"),
        separator(inner_width),
        Line::from(""),
        section("Input"),
        Line::from(""),
        row("i", "Focus the message input box"),
        row("Esc", "Leave input mode"),
        separator(inner_width),
        Line::from(""),
        section("General"),
        Line::from(""),
        row("q", "Quit the review view"),
        row("?", "Toggle this help"),
    ];

    // 模态框比内容矮时截断，避免越出边框
    lines.truncate(inner.height as usize);
    let paragraph = Paragraph::new(Text::from(lines)).style(Style::default().bg(t.bg_modal));
    frame.render_widget(paragraph, inner);
}

/// 居中一个 `width × height` 的矩形（`r` 里放不下时缩到 `r` 的尺寸）。
fn centered_rect(width: u16, height: u16, r: Rect) -> Rect {
    if r.width == 0 || r.height == 0 {
        return r;
    }

    let height = height.min(r.height);
    let width = width.min(r.width);

    let vert_margin = 100u16.saturating_sub(height * 100 / r.height) / 2;
    let horiz_margin = 100u16.saturating_sub(width * 100 / r.width) / 2;

    let popup_layout = Layout::vertical([
        Constraint::Percentage(vert_margin),
        Constraint::Length(height),
        Constraint::Percentage(vert_margin),
    ])
    .split(r);

    Layout::horizontal([
        Constraint::Percentage(horiz_margin),
        Constraint::Length(width),
        Constraint::Percentage(horiz_margin),
    ])
    .split(popup_layout[1])[1]
}

/// 把当前文件的滚动偏移夹到合法范围（内容变短 / 窗口变高时回退）。
fn clamp_scroll(view: &mut GiffView, visible: usize) {
    let Some(file) = view.current_file().map(str::to_string) else {
        return;
    };
    let content_len = match view.file_changes.get(&file) {
        Some((base, head)) => match view.view_mode {
            ViewMode::SideBySide => aligned_line_count(base, head),
            ViewMode::Unified => unified_line_count(base, head),
        },
        None => return,
    };

    if content_len <= visible {
        view.scroll_positions.insert(file, 0);
        return;
    }

    let max_scroll = content_len - visible;
    let scroll = view.scroll_positions.get(&file).copied().unwrap_or(0);
    if scroll > max_scroll {
        view.scroll_positions.insert(file, max_scroll);
    }
}

/// 主题变化（切换主题 / 重载主题文件）时重建语法高亮用的 syntect 主题。
fn ensure_syntect_theme(view: &mut GiffView, theme: &Theme) {
    let fingerprint = format!("{}#{}", theme.render_fingerprint(), theme.syntax_highlight);
    if view.syntect_fp == fingerprint {
        return;
    }

    view.syntect_theme = theme
        .syntax_highlight
        .then(|| syntax_hl::build_theme(theme));
    view.syntect_fp = fingerprint;
}

/// 文件列表行尾的 `+n -m` 统计（返回 Spans 与它们占用的显示宽度）。
fn build_file_stats(adds: usize, dels: usize, theme: &GiffStyle) -> (Vec<Span<'static>>, usize) {
    if adds == 0 && dels == 0 {
        return (Vec::new(), 0);
    }

    let mut spans = Vec::new();
    let mut width = 1; // 前导空格
    spans.push(Span::styled(" ", Style::default()));

    if adds > 0 {
        let text = format!("+{adds}");
        width += UnicodeWidthStr::width(text.as_str());
        spans.push(Span::styled(text, Style::default().fg(theme.fg_added)));
    }
    if adds > 0 && dels > 0 {
        width += 1;
        spans.push(Span::styled(" ", Style::default()));
    }
    if dels > 0 {
        let text = format!("-{dels}");
        width += UnicodeWidthStr::width(text.as_str());
        spans.push(Span::styled(text, Style::default().fg(theme.fg_removed)));
    }

    (spans, width)
}

/// 某个文件的增删行数：(新增, 删除)。
fn count_file_changes(view: &GiffView, file: &str) -> (usize, usize) {
    match view.file_changes.get(file) {
        Some((base, head)) => {
            let dels = base.iter().filter(|(_, l)| l.starts_with('-')).count();
            let adds = head.iter().filter(|(_, l)| l.starts_with('+')).count();
            (adds, dels)
        }
        None => (0, 0),
    }
}

/// 从左侧截断路径到 `max_width` 显示列，保留文件名（尾部）；放不下时前缀 `…`。
pub(super) fn truncate_path(path: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(path) <= max_width {
        return path.to_string();
    }

    if max_width <= 1 {
        return DEF_ELLIPSIS.to_string();
    }

    // 预留 1 列给 "…"，从尾部尽量多留
    let target = max_width - 1;
    let mut width = 0;
    let mut start_byte = path.len();
    for (idx, ch) in path.char_indices().rev() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > target {
            break;
        }
        width += ch_width;
        start_byte = idx;
    }

    format!("{}{}", DEF_ELLIPSIS, &path[start_byte..])
}
