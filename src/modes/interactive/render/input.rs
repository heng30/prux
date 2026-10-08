//! 输入框渲染：多行编辑器折行渲染 + 光标定位。
//!
//! 输入框带上下边框（DynamicBorder，`─` 横线，border 色）；
//! 高度随文本行数（硬换行 + 软折行）增长，上限 [`MAX_INPUT_LINES`] 行；
//! 超过上限时上下滚动，保证光标所在行始终可见。

use super::{
    super::app::{App, char_width, display_width, truncate_display, wrap_line},
    suggestion,
};
use crate::{
    modes::interactive::editor::Editor,
    utils::glyphs::{DEF_ARROW_DOWN, DEF_ARROW_UP},
};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

/// 输入框内容展示行数上限（不含上下边框）
pub const MAX_INPUT_LINES: usize = 10;

/// 输入视图：把多行编辑器平铺为若干可见行（每行软折行展开）
pub struct InputView {
    /// 每个可视行对应的文本段
    pub rows: Vec<String>,
    /// 光标所在可视行在 rows 中的全局下标
    pub cursor_row: usize,
    /// 光标在光标行内的字符偏移
    pub cursor_in_row: usize,
}

/// 内容列宽（保留少量右边距留白，与布局计算一致）
fn content_cols(width: usize) -> usize {
    width.saturating_sub(2).max(1)
}

/// 构建编辑器视图：逐物理行软折行展开，并计算光标落在哪一段
pub fn build_input_view(app: &App, width: usize) -> InputView {
    build_editor_view(&app.editor, width)
}

/// 同上，但作用在任意 [`Editor`] 上（覆盖层多行编辑器复用同一套折行/光标计算）。
pub fn build_editor_view(editor: &Editor, width: usize) -> InputView {
    let cols = content_cols(width);
    let mut rows: Vec<String> = Vec::new();
    let mut cursor_row = 0usize;
    let mut cursor_in_row = 0usize;

    for (li, line) in editor.lines.iter().enumerate() {
        let segs = wrap_line(line, cols);
        let segs = if segs.is_empty() {
            vec![String::new()]
        } else {
            segs
        };

        let start = rows.len();
        for s in &segs {
            rows.push(s.clone());
        }

        if li == editor.cursor_line {
            let col = editor.cursor_col;
            let mut consumed = 0usize;
            let mut found = start;
            let mut off = 0usize;
            let mut hit = false;

            for (i, s) in segs.iter().enumerate() {
                let n = s.chars().count();
                if col <= consumed + n {
                    found = start + i;
                    off = col.saturating_sub(consumed);
                    hit = true;
                    break;
                }
                consumed += n;
            }

            if !hit {
                // 光标 col 超过可见字符数（行内残留零宽/控制字符）：定位到行末，
                // 避免 fallback 到行首导致显示光标与内部光标脱节
                found = start + segs.len() - 1;
                off = segs.last().map(|s| s.chars().count()).unwrap_or(0);
            }

            cursor_row = found;
            cursor_in_row = off;
        }
    }

    InputView {
        rows,
        cursor_row,
        cursor_in_row,
    }
}

/// 输入框内容行数（clamp 到 [`MAX_INPUT_LINES`]）；供布局计算高度
pub fn content_height(app: &App, width: usize) -> usize {
    let cols = content_cols(width);
    let total = app
        .editor
        .lines
        .iter()
        .map(|l| wrap_line(l, cols).len().max(1))
        .sum::<usize>()
        .max(1);
    total.min(MAX_INPUT_LINES)
}

/// 生成边框行：顶部/底部有隐藏行时嵌入 `─── ↑ N more ───` / `─── ↓ N more ───`（对齐
/// 会话摘要 `─── ↑ 12 more ───` 的折叠横幅格式），否则整行 `─`
fn border_row(width: usize, hidden: usize, up: bool, style: Style) -> Line<'static> {
    let arrow = if up { DEF_ARROW_UP } else { DEF_ARROW_DOWN };
    let mut s = if hidden > 0 {
        format!("─── {} {} more ─", arrow, hidden)
    } else {
        "─".repeat(width)
    };

    while s.chars().count() < width {
        s.push('─');
    }

    if s.chars().count() > width {
        s = s.chars().take(width).collect();
    }

    Line::from(Span::styled(s, style))
}

/// 滚动窗口起点：总行数超可视区时保证光标行可见
fn scroll_top(view: &InputView, avail: usize) -> usize {
    let total = view.rows.len();
    if total > avail {
        view.cursor_row.saturating_sub(avail - 1).min(total - avail)
    } else {
        0
    }
}

/// 渲染输入框：手动逐行渲染
/// 上边框 ─、编辑器内容行、下边框 ─、候选列表行（autocomplete 嵌入）
pub fn render_input_box(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width as usize;
    let view = build_input_view(app, width);

    // 内容可见区（扣除上下边框与候选区）
    let sugg_rows = if app.suggestion.active {
        suggestion::suggestion_height(app, width) as usize
    } else {
        0
    };

    let avail = area.height.saturating_sub(2 + sugg_rows as u16).max(1) as usize;
    let top = scroll_top(&view, avail);
    let end = (top + avail).min(view.rows.len());

    // 输入以 ! 开头时编辑器边框染 bashMode 主题色
    let is_bash_mode = app
        .editor
        .lines
        .first()
        .map(|l: &String| l.trim_start().starts_with('!'))
        .unwrap_or(false);
    let border_style = if is_bash_mode {
        app.theme.style("bashMode", "#7f9f7f")
    } else {
        app.theme.style("border", "#5f87ff")
    };

    let total_rows = view.rows.len();
    let mut all: Vec<Line> = Vec::new();

    // 上边框（对齐 DynamicBorder：整宽 ─；顶部有隐藏行时显示 ─── ↑ N more ───）
    all.push(border_row(width, top, true, border_style));

    // 编辑器内容行
    all.extend(build_content_rows(&view, top, end, avail));

    // 下边框（底部有未显示行时显示 ─── ↓ N more ───）
    all.push(border_row(
        width,
        total_rows.saturating_sub(end),
        false,
        border_style,
    ));

    // 候选列表行（在编辑器边框之后渲染）
    if app.suggestion.active {
        all.extend(suggestion::suggestion_lines(app, width));
    }

    // 截断/填充到区域高度并渲染
    render_final_lines(frame, area, width, all);
}

/// 构建编辑器内容可视行（光标高亮 + 内容不足时空白填充）
fn build_content_rows(view: &InputView, top: usize, end: usize, avail: usize) -> Vec<Line<'_>> {
    let mut content_drawn = 0usize;
    let mut rows: Vec<Line> = Vec::new();

    for (i, seg) in view.rows[top..end].iter().enumerate() {
        let global = top + i;
        let mut spans: Vec<Span> = Vec::new();

        if global == view.cursor_row {
            let ci = view.cursor_in_row.min(seg.chars().count());
            let before: String = seg.chars().take(ci).collect();
            let rest: Vec<char> = seg.chars().skip(ci).collect();
            let cursor_char = rest.first().copied().unwrap_or(' ');
            let after: String = rest.iter().skip(1).collect();
            spans.push(Span::raw(before));
            spans.push(Span::raw(cursor_char.to_string()));
            spans.push(Span::raw(after));
        } else {
            spans.push(Span::raw(seg.clone()));
        }

        rows.push(Line::from(spans));
        content_drawn += 1;
    }

    // 仅按内容行数填充空白（上边框不计入），内容填满/超出时不再追加空行
    for _ in content_drawn..avail {
        rows.push(Line::from(vec![Span::raw("")]));
    }
    rows
}

/// 截断/填充行到区域尺寸并渲染到 frame
fn render_final_lines(frame: &mut Frame, area: Rect, width: usize, all: Vec<Line>) {
    let mut final_lines: Vec<Line> = Vec::new();
    for line in all.into_iter().take(area.height as usize) {
        let mut spans = Vec::new();
        let mut used = 0usize;

        for sp in line.spans {
            let w = display_width(&sp.content);

            if used + w > width {
                let (t, _) = truncate_display(&sp.content, width.saturating_sub(used));
                if !t.is_empty() {
                    spans.push(Span::styled(t, sp.style));
                }
                used = width;
                break;
            }

            spans.push(sp);
            used += w;
        }

        if used < width {
            spans.push(Span::raw(" ".repeat(width - used)));
        }

        final_lines.push(Line::from(spans));
    }

    while final_lines.len() < area.height as usize {
        final_lines.push(Line::from(Span::raw(" ".repeat(width))));
    }

    let para = Paragraph::new(final_lines).style(Style::default());
    frame.render_widget(para, area);
}

/// 计算输入光标在屏幕上的 (x, y)
/// x = 光标前字符宽；y = 输入区顶 + 上边框 + 光标所在可视行
pub fn input_cursor_pos(area: Rect, app: &App) -> Option<(u16, u16)> {
    let view = build_input_view(app, area.width as usize);
    let sugg_rows = if app.suggestion.active {
        suggestion::suggestion_height(app, area.width as usize) as usize
    } else {
        0
    };

    let avail = area.height.saturating_sub(2 + sugg_rows as u16).max(1) as usize;
    let top = scroll_top(&view, avail);
    let seg = view.rows.get(view.cursor_row)?;
    let mut cursor_w = 0usize;

    for c in seg.chars().take(view.cursor_in_row) {
        cursor_w += char_width(c);
    }

    let cx = area
        .x
        .saturating_add(cursor_w.min(area.width as usize) as u16);
    let cy = area
        .y
        .saturating_add(1)
        .saturating_add((view.cursor_row - top) as u16);
    Some((cx, cy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::app::App;

    #[test]
    fn cursor_is_on_same_row_as_input_text() {
        let mut app = App::new();
        app.editor.insert_text("ab");
        let area = Rect::new(0, 10, 80, 3);
        let (x, y) = input_cursor_pos(area, &app).unwrap();
        assert_eq!(y, area.y + 1, "光标在首行（边框内）");
        assert_eq!(
            x,
            area.x + 2,
            "no prompt prefix, cursor sits after 2 typed chars"
        );
    }

    #[test]
    fn cursor_after_arrow_prefix() {
        let app = App::new();
        let area = Rect::new(0, 5, 80, 3);
        let (x, y) = input_cursor_pos(area, &app).unwrap();
        assert_eq!((x, y), (area.x, area.y + 1));
    }

    #[test]
    fn pasted_multiline_cursor_lands_on_last_line_end() {
        let mut app = App::new();
        app.editor.insert_text("line1\nline2");
        // 高度 = 2 内容行 + 2 边框，光标应显示在第二行行末（x = 首列 + 5 字符）
        let area = Rect::new(0, 10, 80, 4);
        let (x, y) = input_cursor_pos(area, &app).unwrap();
        assert_eq!(y, area.y + 2, "光标在第二内容行（粘贴后不在行首）");
        assert_eq!(x, area.x + 5, "光标在行末，与内部 cursor_col 一致");
    }

    #[test]
    fn large_paste_collapses_so_input_box_stays_short() {
        let mut app = App::new();
        let big: String = (0..20)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        app.editor.paste_text(&big);
        // 20 行粘贴折叠为单行 marker → 输入框高度 = 1 内容 + 2 边框，不再占满屏幕
        assert_eq!(app.editor.lines.len(), 1);
        assert!(app.editor.lines[0].starts_with("[paste #1 +20 lines]"));
        assert_eq!(content_height(&app, 80), 1);
        assert_eq!(super::super::layout_heights(&app, 80).2, 3);
    }

    #[test]
    fn multi_line_grows_height() {
        let mut app = App::new();
        // 3 个硬换行 → 4 行内容
        for c in ['a', 'b', 'c'] {
            app.editor.newline();
            app.editor.insert_char(c);
        }
        assert_eq!(app.editor.lines.len(), 4);
        assert_eq!(content_height(&app, 80), 4);
        // 高度（含上下边框 + 无候选）= 4 + 2
        assert_eq!(super::super::layout_heights(&app, 80).2, 6);
    }

    #[test]
    fn height_caps_at_max_lines() {
        let mut app = App::new();
        for _ in 0..20 {
            app.editor.newline();
        }
        assert_eq!(app.editor.lines.len(), 21);
        assert_eq!(content_height(&app, 80), MAX_INPUT_LINES);
        assert_eq!(
            super::super::layout_heights(&app, 80).2,
            MAX_INPUT_LINES as u16 + 2
        );
    }

    #[test]
    fn suggestion_embeds_into_input_area() {
        let mut app = App::new();
        app.suggestion.active = true;
        app.suggestion.items = vec![crate::modes::interactive::app::SuggestionItem {
            name: "help".into(),
            description: "show help".into(),
            insert: "help".into(),
        }];
        // 输入区高度 = 编辑器1 + 边框2 + 候选行（1 项无页码）= 4
        assert_eq!(super::super::layout_heights(&app, 80).2, 4);
    }

    #[test]
    fn long_line_wraps_multi_rows() {
        let mut app = App::new();
        app.editor.insert_text(&"x".repeat(30));
        let view = build_input_view(&app, 10); // cols = 6
        assert!(
            view.rows.len() >= 3,
            "30 chars should wrap to 3+ rows at width 10, got {}",
            view.rows.len()
        );
        // 光标在文末 → 落在最后一段
        assert_eq!(view.cursor_row, view.rows.len() - 1);
    }

    /// 渲染输入框到 TestBackend，取第 row 行首字符（'─' = 上/下边框）
    fn rendered_row_char(app: &App, width: u16, height: u16, row: u16) -> Option<char> {
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, width, height);
                render_input_box(f, area, app);
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .cell((0, row))
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
    }

    /// 渲染输入框到 TestBackend，取第 row 行的完整文本
    fn rendered_row_text(app: &App, width: u16, height: u16, row: u16) -> String {
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, width, height);
                render_input_box(f, area, app);
            })
            .unwrap();
        (0..width)
            .map(|x| {
                terminal
                    .backend()
                    .buffer()
                    .cell((x, row))
                    .map(|c| c.symbol().to_string())
                    .unwrap_or_default()
            })
            .collect()
    }

    #[test]
    fn borders_show_hidden_line_counts_when_scrolled() {
        // 9 行内容、可视 2 行（高度 4 - 边框 2）、光标在末尾
        // → 顶部隐藏 7 行，上边框显示 ─── ↑ 7 more ───，下边框为纯横线
        let mut app = App::new();
        for _ in 0..8 {
            app.editor.newline();
        }
        app.editor.insert_text("tail");
        let top_row = rendered_row_text(&app, 30, 4, 0);
        assert!(
            top_row.contains("↑ 7 more"),
            "上边框应显示顶部隐藏行数: {:?}",
            top_row
        );
        let bottom_row = rendered_row_text(&app, 30, 4, 3);
        assert!(
            !bottom_row.contains('↓'),
            "光标在末尾时底部无隐藏行: {:?}",
            bottom_row
        );

        // 光标回到顶部 → 顶部无隐藏、底部隐藏 7 行
        app.editor.cursor_line = 0;
        app.editor.cursor_col = 0;
        let top_row = rendered_row_text(&app, 30, 4, 0);
        assert!(
            !top_row.contains('↑'),
            "光标在顶部时上方无隐藏行: {:?}",
            top_row
        );
        let bottom_row = rendered_row_text(&app, 30, 4, 3);
        assert!(
            bottom_row.contains("↓ 7 more"),
            "下边框应显示底部隐藏行数: {:?}",
            bottom_row
        );
    }

    #[test]
    fn bottom_border_stays_visible_when_content_fills_area() {
        let mut app = App::new();
        app.editor.insert_text("line1");
        app.editor.newline();
        app.editor.insert_text("line2");
        // 高度 4 = 上边框 + 2 内容行 + 下边框（内容恰好填满可用区）
        assert_eq!(content_height(&app, 20), 2);
        assert_eq!(
            rendered_row_char(&app, 20, 4, 3),
            Some('─'),
            "第二行输入后下边框应仍可见"
        );
        assert_eq!(rendered_row_char(&app, 20, 4, 0), Some('─'), "上边框");
        // 滚动态（内容超区）：下边框仍紧贴内容区底部
        let mut tall = App::new();
        for _ in 0..6 {
            tall.editor.newline();
        }
        tall.editor.insert_text("tail");
        assert_eq!(
            rendered_row_char(&tall, 20, 4, 3),
            Some('─'),
            "滚动态下边框"
        );
    }
    /// 回归：文本填满可视行后继续输入触发软折行，终端光标必须落在
    /// 最后一个字符之后（渲染与光标定位的折行宽度必须一致，否则光标会压住字符）。
    #[test]
    fn cursor_lands_after_last_char_when_row_fills_and_wraps() {
        use ratatui::backend::TestBackend;
        let area = Rect::new(0, 0, 20, 6);
        let mut app = App::new();
        // 30 字符 → cols = 18 下展开为 18+12 两行
        app.editor.insert_text(&"x".repeat(30));
        let mut term = ratatui::Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        let mut pos = None;
        term.draw(|f| {
            render_input_box(f, area, &app);
            pos = input_cursor_pos(area, &app);
            if let Some(p) = pos {
                f.set_cursor_position(p);
            }
        })
        .unwrap();
        let (cx, cy) = pos.expect("光标应在输入框内");
        assert_eq!(
            (cx, cy),
            (12, 2),
            "折行后光标应位于第二行第 12 列（最后一个字符之后）"
        );
        let cell = term.backend().buffer().cell((cx, cy)).unwrap();
        assert_eq!(
            cell.symbol(),
            " ",
            "光标所在格应为留白（在最后一个字符之后），实际是 {:?}",
            cell.symbol()
        );
    }

    /// 回归：文本恰好填满一行的最后一列时，光标在行尾留白处，不压住最后一个字符。
    #[test]
    fn cursor_stays_after_last_char_when_row_exactly_full() {
        use ratatui::backend::TestBackend;
        let area = Rect::new(0, 0, 20, 6);
        let mut app = App::new();
        // cols = width - 2 = 18，恰好填满一行
        app.editor.insert_text(&"x".repeat(18));
        let mut term = ratatui::Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        let mut pos = None;
        term.draw(|f| {
            render_input_box(f, area, &app);
            pos = input_cursor_pos(area, &app);
            if let Some(p) = pos {
                f.set_cursor_position(p);
            }
        })
        .unwrap();
        let (cx, cy) = pos.expect("光标应在输入框内");
        assert_eq!((cx, cy), (18, 1));
        let cell = term.backend().buffer().cell((cx, cy)).unwrap();
        assert_eq!(cell.symbol(), " ", "光标应停在紧邻最后一个字符的留白处");
    }
}
