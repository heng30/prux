//! 扩展覆盖层渲染（输入区下方的 Inline 面板 / 占满聊天区的 Fullscreen 面板）。
//!
//! 内容来自 [`crate::core::extensions::overlay_view`]（扩展持有），本层只做：
//! 标题行（含 `起-止/总` 位置）、固定区（`header`）、内容视窗（`lines` + 已渲染的
//! `messages`）、滚动条、键位底栏、内联输入行、选择高亮，并把渲染区域/内容几何/
//! 滚动条几何回写到覆盖层状态（鼠标命中与拖动用）。

use crate::{
    core::extensions::{OverlayEditor, OverlaySize, OverlayView},
    modes::interactive::{
        app::App,
        editor::Editor,
        overlay::{self, EDITOR_CHROME_ROWS},
        render::{dock::dock_line_spans, input::build_editor_view},
        theme::color,
    },
    utils::{display::display_width, glyphs::DEF_SCROLLBAR_THUMB},
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};

/// 无内容行的纯编辑器覆盖层（如 `/agents types → edit <type>`）：文件路径并进标题行，省下的标签行留给正文。
fn label_joins_title(view: &OverlayView) -> bool {
    view.editor.is_some() && view.lines.is_empty() && view.messages.is_empty()
}

/// 面板样式的水平分割线（`─` 铺满宽度，`border` 色）。
fn border_line(width: usize, style: Style) -> Line<'static> {
    Line::from(Span::styled("─".repeat(width), style))
}

/// 左截断到 `max` 列：超宽时保留尾部（文件名比目录更有信息量），前面补 `…`。
fn truncate_left(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }

    if max == 0 {
        return String::new();
    }

    let mut tail: Vec<char> = Vec::new();
    let mut w = 0;

    for ch in s.chars().rev() {
        let cw = display_width(&ch.to_string());
        if w + cw > max - 1 {
            break;
        }
        w += cw;
        tail.push(ch);
    }
    tail.reverse();
    format!("…{}", tail.into_iter().collect::<String>())
}

/// 渲染当前覆盖层。`area` 由布局分配（Inline = 输入框下方若干行；Fullscreen = 剩余全部）。
pub fn render(frame: &mut Frame, area: Rect, app: &mut App, _size: OverlaySize) {
    // 第一段：短借更新状态（视口行数 / 区域 / 滚动夹取）
    let (content_rows, editor_rows, has_footer, show_input, editor_meta, header_rows, border_rows) = {
        let Some(state) = app.overlay.as_mut() else {
            return;
        };
        let has_footer = !state.view.footer.is_empty();
        // 输入行只在**聚焦**时占位/显示：未按 `i` 时 footer 的 `i steer`/`i resume` 就是入口提示，不需要一行空框占着位置。
        let show_input = state.view.input.is_some() && state.input_active;
        let editor_meta = state.view.editor.clone();
        let header_rows = state.view.header.len();

        // 路径已并进标题行时，标签行不占位（见 `label_joins_title`）
        let editor_chrome = if label_joins_title(&state.view) {
            EDITOR_CHROME_ROWS.saturating_sub(1)
        } else {
            EDITOR_CHROME_ROWS
        };

        // 面板样式：无标题行；上下各一条 `─` 分割线 + 上/中/下 3 个空行
        let panel = matches!(state.view.size, OverlaySize::Panel { .. });
        let border_rows = if panel { overlay::PANEL_BORDER_ROWS } else { 0 };
        let title_rows = usize::from(!panel);
        let blank_rows = if panel { 3 } else { 0 };

        let avail = (area.height as usize).saturating_sub(
            title_rows
                + border_rows
                + blank_rows
                + header_rows
                + usize::from(has_footer)
                + usize::from(show_input),
        );
        let (content_rows, editor_rows) = match &editor_meta {
            // `rows == 0` = 填满剩余空间（Fullscreen 编辑器）
            Some(ed) if ed.rows == 0 => {
                // 顶部内容行最多 2 行；编辑器行数 = 可用行 - 标签行 - 空行
                let content = state.view.lines.len().min(2);
                let editor_rows = avail.saturating_sub(editor_chrome + content).max(1);
                (content, editor_rows)
            }
            Some(ed) => {
                let editor_rows = ed.rows as usize;
                let content_rows = avail.saturating_sub(editor_chrome + editor_rows).max(1);
                (content_rows, editor_rows)
            }
            None => (avail.max(1), 0),
        };
        state.content_rows = content_rows;
        state.area = Some(area);
        state.scroll = state.scroll.min(state.max_scroll());
        // 编辑器折行宽度同步（上下光标移动依赖它）
        if editor_meta.is_some() {
            state
                .editor
                .set_visual_width(area.width.saturating_sub(1) as usize);
        }
        (
            content_rows,
            editor_rows,
            has_footer,
            show_input,
            editor_meta,
            header_rows,
            border_rows,
        )
    };

    // 第二段：只读渲染（dock_line_spans 需要 &App，故此处不能再持可变借）
    let Some(state) = app.overlay.as_ref() else {
        return;
    };
    let width = area.width as usize;
    let theme = &app.theme;
    let accent = theme.style("accent", "#8abeb7");
    let dim = theme.style("dim", "#666666");
    let muted = theme.style("muted", "#999999");

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(area.height as usize);
    let border_style = theme.style("border", "#5f87ff");

    // 面板样式：顶部 `─` 分割线 + 空行（与核心 `Select` 面板一致）
    if border_rows > 0 {
        lines.push(border_line(width, border_style));
        lines.push(Line::from(""));
    }

    // 标题行（仅非面板样式）：左标题（+ 并进来的文件路径）+ 右位置（`3-8/20`）
    let total = state.len();
    let first = (state.scroll + 1).min(total.max(1));
    let last = (state.scroll + content_rows).min(total);
    let joins = label_joins_title(&state.view);

    let pos = if joins {
        String::new() // 无内容行可报位置，这一行让给路径
    } else if total == 0 {
        "empty".to_string()
    } else {
        format!("{first}-{last}/{total}")
    };

    let title = format!(" {}", state.view.title);
    let pos_w = display_width(&pos);
    let label = if joins {
        let budget = width.saturating_sub(display_width(&title) + pos_w + 3); // 2 空格间隔 + 1 右侧留白
        editor_meta
            .as_ref()
            .map(|ed| truncate_left(&ed.label, budget))
            .filter(|l| !l.is_empty())
    } else {
        None
    };

    if border_rows == 0 {
        let mut title_line = vec![Span::styled(
            title.clone(),
            accent.add_modifier(Modifier::BOLD),
        )];
        let mut used = display_width(&title) + pos_w;

        if let Some(label) = label {
            title_line.push(Span::styled("  ".to_string(), Style::default()));
            used += 2 + display_width(&label);
            title_line.push(Span::styled(label, dim));
        }

        let pad = width.saturating_sub(used + 1).max(1);
        title_line.push(Span::styled(" ".repeat(pad), Style::default()));
        title_line.push(Span::styled(pos, dim));
        lines.push(Line::from(title_line));
    }

    // 固定区（不随滚动）
    let avail = width.saturating_sub(1); // 右侧留给滚动条
    for line in &state.view.header {
        lines.push(Line::from(dock_line_spans(line, avail, app)));
    }

    // 内容视窗（`lines` 预渲染行在前，`messages` 渲染行在后）
    let content_y = area.y + 1 + border_rows as u16 + header_rows as u16;
    let dock_len = state.view.lines.len();
    let mut content: Vec<Line<'static>> = Vec::with_capacity(content_rows);
    let mut visible_text: Vec<String> = Vec::with_capacity(content_rows);
    for i in 0..content_rows {
        let idx = state.scroll + i;
        let line = if idx < dock_len {
            Some(Line::from(dock_line_spans(
                &state.view.lines[idx],
                avail,
                app,
            )))
        } else {
            state.message_lines.get(idx - dock_len).cloned()
        };
        match line {
            Some(l) => {
                visible_text.push(l.spans.iter().map(|s| s.content.as_ref()).collect());
                content.push(l);
            }
            None => {
                visible_text.push(String::new());
                content.push(Line::from(""));
            }
        }
    }

    // 选择高亮（内容全局行坐标 → 视口行）
    apply_selection(&mut content, state.scroll, overlay::selection_range(state));
    lines.extend(content);

    // 多行编辑器（标签 + 空行 + 可滚动内容 + 光标）；路径已并进标题行时不再单独占一行
    if let Some(ed) = &editor_meta {
        if !joins {
            lines.push(Line::from(Span::styled(
                format!(" {} ", ed.label),
                accent.add_modifier(Modifier::BOLD),
            )));
        }
        // 标签（或标题）与正文之间隔一空行，避免路径贴住内容
        lines.push(Line::from(""));
        push_editor_lines(&mut lines, &state.editor, avail, editor_rows, ed, app);
    }

    // 内联输入行：只在**聚焦**时出现（未按 `i` 时不显示，footer 的 `i steer`/ `i resume with message` 就是入口）。
    // 放在键位底栏**上方**： `…正文 / steer> _ / 键位提示 / 分割线 / 主页底栏`。
    if show_input && let Some(input) = state.view.input.clone() {
        let label_style = accent.add_modifier(Modifier::BOLD);
        let mut spans = vec![Span::styled(format!("{} ", input.label), label_style)];
        let budget = width.saturating_sub(display_width(&input.label) + 2);
        let (window, _col) = state.input.visible_window(budget);
        spans.push(Span::styled(window, muted));
        spans.push(Span::styled("▏", accent.add_modifier(Modifier::SLOW_BLINK)));
        lines.push(Line::from(spans));
    }

    // 面板样式：内容与键位提示之间隔一空行
    if border_rows > 0 {
        lines.push(Line::from(""));
    }

    // 键位底栏
    if has_footer {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (i, (key, desc)) in state.view.footer.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("  ", muted));
            }
            spans.push(Span::styled(
                key.clone(),
                accent.add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(format!(" {desc}"), dim));
        }
        lines.push(Line::from(spans));
    }

    // 面板样式：键位提示后一空行 + 底部 `─` 分割线（不足时先补空行，保证它在最后一行）
    if border_rows > 0 {
        lines.push(Line::from(""));
        while lines.len() + 1 < area.height as usize {
            lines.push(Line::from(""));
        }
        lines.push(border_line(width, border_style));
    }

    frame.render_widget(Paragraph::new(lines), area);

    // 滚动条（内容超视口）：只画在**内容区**内（标题/固定区/输入行/底栏不占 track），
    // 这样 thumb 几何与鼠标拖动映射（track = content_rows）一致。
    let scrollable = state.scrollable() && width >= 2;
    let scroll = state.scroll;
    let scroll_x = area.x + area.width.saturating_sub(1);
    let track = content_rows;

    // ratatui 的 `position` 是 0..=total-1 的顶部偏移；直接把 `scroll`（0..=max_scroll）
    // 当作 position 会让滚到底时 thumb 离底还差一个 thumb 高度。按主页同一手法把
    // 两端映射到 `0` 与 `total-1`，保证 thumb 能真正贴顶/贴底。
    let max_scroll = total.saturating_sub(track);
    let position = if max_scroll == 0 {
        0
    } else {
        (((total - 1) as f64) * (scroll as f64 / max_scroll as f64)).round() as usize
    };

    let thumb = if scrollable && track > 0 {
        let thumb = app.theme.style("scrollbarThumb", "#6a6a78");
        let mut sb = ScrollbarState::new(total)
            .position(position)
            .viewport_content_length(track);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_symbol(DEF_SCROLLBAR_THUMB)
                .thumb_style(thumb),
            Rect {
                x: scroll_x,
                y: content_y,
                width: 1,
                height: track as u16,
            },
            &mut sb,
        );

        // thumb 相对区间（与 ratatui part_lengths 同公式，position 用上面映射后的值）
        let denom = (total - 1 + track).max(1);
        let thumb_len =
            ((track as f64 * track as f64 / denom as f64).round() as usize).clamp(1, track.max(1));
        let thumb_start = ((position as f64 * track as f64 / denom as f64).round() as usize)
            .min(track.saturating_sub(thumb_len));
        Some((thumb_start, thumb_len))
    } else {
        None
    };

    // 回写几何（鼠标命中 / 拖动 / 选择用）
    if let Some(state) = app.overlay.as_mut() {
        overlay::record_content_geometry(state, content_y, content_rows as u16, visible_text);
        overlay::record_scrollbar(state, scrollable.then_some(scroll_x), thumb);
    }
}

/// 在内容行上叠加选择高亮（反色，按字符区间，支持反向选择）。
/// `range` 是内容全局行坐标；先按 `scroll` 反算到视口行。
fn apply_selection(
    content: &mut [Line<'static>],
    scroll: usize,
    range: Option<(usize, usize, usize, usize)>,
) {
    let Some((ls, cs, le, ce)) = range else {
        return;
    };
    let rows = content.len();
    if le < scroll || ls >= scroll.saturating_add(rows) {
        return;
    }

    let sel_style = Style::default().fg(Color::Black).bg(color("#a0a0a0"));
    for (i, line) in content.iter_mut().enumerate() {
        let g = scroll + i;
        if g < ls || g > le {
            continue;
        }
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let chars: Vec<char> = text.chars().collect();
        let a = if g == ls { cs.min(chars.len()) } else { 0 };
        let b = if g == le {
            ce.min(chars.len())
        } else {
            chars.len()
        };
        if a >= b {
            continue;
        }

        let mut new_spans: Vec<Span<'static>> = Vec::new();
        let mut char_idx = 0usize;
        for sp in std::mem::take(&mut line.spans) {
            let text: String = sp.content.as_ref().to_string();
            let n = text.chars().count();
            let seg_start = char_idx;
            let seg_end = char_idx + n;
            char_idx = seg_end;
            if seg_end <= a || seg_start >= b {
                new_spans.push(sp);
                continue;
            }
            let rel_a = a.saturating_sub(seg_start).min(n);
            let rel_b = b.saturating_sub(seg_start).min(n);
            let chars: Vec<char> = text.chars().collect();
            if rel_a > 0 {
                new_spans.push(Span::styled(
                    chars[..rel_a].iter().collect::<String>(),
                    sp.style,
                ));
            }
            new_spans.push(Span::styled(
                chars[rel_a..rel_b].iter().collect::<String>(),
                sp.style.patch(sel_style),
            ));
            if rel_b < n {
                new_spans.push(Span::styled(
                    chars[rel_b..].iter().collect::<String>(),
                    sp.style,
                ));
            }
        }
        line.spans = new_spans;
    }
}

/// 把多行编辑器的可见区追加到渲染行：按宽度软折行、随光标滚动、光标用反白块高亮。
fn push_editor_lines(
    lines: &mut Vec<Line<'static>>,
    editor: &Editor,
    width: usize,
    rows: usize,
    meta: &OverlayEditor,
    app: &App,
) {
    let dim = app.theme.style("dim", "#666666");
    let text_style = app.theme.style("text", "#d0d0d0");
    let rows = rows.max(1);

    if editor.is_empty() && !meta.placeholder.is_empty() {
        lines.push(Line::from(Span::styled(meta.placeholder.clone(), dim)));
        for _ in 1..rows {
            lines.push(Line::from(""));
        }
        return;
    }

    let view = build_editor_view(editor, width);
    let top = if view.rows.len() > rows {
        view.cursor_row
            .saturating_sub(rows - 1)
            .min(view.rows.len() - rows)
    } else {
        0
    };

    let end = (top + rows).min(view.rows.len());
    let mut drawn = 0usize;
    for (i, seg) in view.rows[top..end].iter().enumerate() {
        let global = top + i;
        drawn += 1;
        if global == view.cursor_row {
            let ci = view.cursor_in_row.min(seg.chars().count());
            let before: String = seg.chars().take(ci).collect();
            let rest: Vec<char> = seg.chars().skip(ci).collect();
            let cursor_char = rest.first().copied().unwrap_or(' ');
            let after: String = rest.iter().skip(1).collect();
            lines.push(Line::from(vec![
                Span::styled(before, text_style),
                Span::styled(
                    cursor_char.to_string(),
                    text_style.add_modifier(Modifier::REVERSED),
                ),
                Span::styled(after, text_style),
            ]));
        } else {
            lines.push(Line::from(Span::styled(seg.clone(), text_style)));
        }
    }

    // 固定占满 `rows` 行，否则底部键位会被提前顶上来
    for _ in drawn..rows {
        lines.push(Line::from(""));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::{DockSpan, OverlayInput, OverlaySize, OverlayView};
    use ratatui::{Terminal, backend::TestBackend};

    fn app_with(lines: usize) -> App {
        let mut app = App::new();
        let view = OverlayView {
            title: "Fleet".to_string(),
            lines: (0..lines)
                .map(|i| vec![DockSpan::plain(format!("agent-{i}"))])
                .collect(),
            footer: vec![
                ("Enter".to_string(), "open".to_string()),
                ("Esc".to_string(), "close".to_string()),
            ],
            input: None,
            editor: None,
            size: OverlaySize::Inline { max_rows: 8 },
            selected: None,
            input_focus: false,
            ..OverlayView::inline("x", Vec::new(), 8)
        };
        crate::modes::interactive::overlay::open(&mut app, 1, view);
        app
    }

    fn draw(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        let area = Rect::new(0, 0, w, h);
        term.draw(|f| render(f, area, app, OverlaySize::Inline { max_rows: 8 }))
            .unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn renders_title_window_footer_and_records_area() {
        let mut app = app_with(20);
        let rows = draw(&mut app, 30, 8);
        assert!(rows[0].contains("Fleet"), "标题: {:?}", rows[0]);
        // 8 行 - 标题 1 - 底栏 1 = 内容 6 行
        assert!(rows[0].contains("1-6/20"), "位置提示: {:?}", rows[0]);
        assert!(rows[1].contains("agent-0"), "首行内容: {:?}", rows[1]);
        assert!(rows[6].contains("agent-5"), "末行内容: {:?}", rows[6]);
        // 底栏键位
        let footer = rows[7].clone();
        assert!(footer.contains("Enter open"), "键位底栏: {footer:?}");
        assert!(footer.contains("Esc close"), "键位底栏: {footer:?}");
        // 记录渲染区域（滚轮命中用）
        assert_eq!(
            app.overlay.as_ref().unwrap().area,
            Some(Rect::new(0, 0, 30, 8))
        );
        assert_eq!(app.overlay.as_ref().unwrap().content_rows, 6);
    }

    #[test]
    fn scroll_window_and_follow_bottom() {
        let mut app = app_with(20);
        // 贴底显示末尾（视口行数由渲染回写；先用 update_scroll 应用 follow）
        app.overlay.as_mut().unwrap().content_rows = 6;
        crate::modes::interactive::overlay::update_scroll(app.overlay.as_mut().unwrap(), true);
        let rows = draw(&mut app, 30, 8);
        assert!(rows[6].contains("agent-19"), "贴底显示末尾: {:?}", rows[6]);

        // 手工滚动到顶后窗口跟随偏移
        app.overlay.as_mut().unwrap().scroll = 0;
        let rows = draw(&mut app, 30, 8);
        assert!(rows[1].contains("agent-0"), "{:?}", rows[1]);
        assert!(rows[0].contains("1-6/20"), "{:?}", rows[0]);
        assert!(rows[6].contains("agent-5"), "{:?}", rows[6]);
    }

    /// 固定区（`header`）不随滚动移动，且计入布局扣减。
    #[test]
    fn header_lines_stay_fixed_and_shrink_the_viewport() {
        let mut app = App::new();
        let view = OverlayView {
            title: "Viewer".to_string(),
            header: vec![
                vec![DockSpan::plain("status: running")],
                vec![DockSpan::plain("session: /tmp/s.jsonl")],
            ],
            lines: (0..20)
                .map(|i| vec![DockSpan::plain(format!("line-{i}"))])
                .collect(),
            footer: vec![("Esc".to_string(), "close".to_string())],
            input: None,
            editor: None,
            size: OverlaySize::Fullscreen,
            selected: None,
            input_focus: false,
            ..OverlayView::inline("x", Vec::new(), 8)
        };
        crate::modes::interactive::overlay::open(&mut app, 1, view);
        let rows = draw(&mut app, 30, 8);
        assert!(rows[0].contains("Viewer"), "{:?}", rows[0]);
        assert!(
            rows[1].contains("status: running"),
            "固定区首行: {:?}",
            rows[1]
        );
        assert!(
            rows[2].contains("session: /tmp/s.jsonl"),
            "固定区次行: {:?}",
            rows[2]
        );
        // 8 - 标题1 - header2 - 底栏1 = 内容 4 行
        assert_eq!(app.overlay.as_ref().unwrap().content_rows, 4);
        assert!(rows[3].contains("line-0"), "{:?}", rows[3]);
        assert!(rows[6].contains("line-3"), "{:?}", rows[6]);
        // 滚动后 header 不动
        app.overlay.as_mut().unwrap().scroll = 5;
        let rows = draw(&mut app, 30, 8);
        assert!(
            rows[1].contains("status: running"),
            "滚动后固定区仍在: {:?}",
            rows[1]
        );
        assert!(rows[3].contains("line-5"), "{:?}", rows[3]);
    }

    /// 多行编辑器区域：标签 + 内容 + 光标；`rows: 0` 在 Fullscreen 下填满剩余空间。
    #[test]
    fn renders_multiline_editor_and_keeps_footer_visible() {
        use crate::core::extensions::OverlayEditor;
        let mut app = App::new();
        let view = OverlayView {
            title: "Edit agent".to_string(),
            lines: vec![vec![DockSpan::plain("hint")]],
            footer: vec![("Ctrl+S".to_string(), "save".to_string())],
            input: None,
            editor: Some(OverlayEditor {
                label: "file.md".to_string(),
                value: String::new(),
                placeholder: String::new(),
                rows: 0,
                generation: 1,
            }),
            size: OverlaySize::Fullscreen,
            selected: None,
            input_focus: false,
            ..OverlayView::inline("x", Vec::new(), 8)
        };
        crate::modes::interactive::overlay::open(&mut app, 1, view);
        // 直接装载缓冲（sync 需要已注册的扩展）
        app.overlay
            .as_mut()
            .unwrap()
            .editor
            .insert_text("line1\nline2");
        app.overlay.as_mut().unwrap().editor.cursor_line = 0;
        app.overlay.as_mut().unwrap().editor.cursor_col = 0;
        let rows = draw(&mut app, 30, 10);
        assert!(rows[0].contains("Edit agent"), "{:?}", rows[0]);
        assert!(rows[2].contains("file.md"), "编辑器标签: {:?}", rows[2]);
        // 标签后跟一空行，再是正文
        assert!(rows[3].trim().is_empty(), "标签后应为空行: {:?}", rows[3]);
        assert!(rows[4].contains("line1"), "编辑器首行: {:?}", rows[4]);
        assert!(rows[5].contains("line2"), "编辑器次行: {:?}", rows[5]);
        assert!(
            rows[9].contains("Ctrl+S save"),
            "底栏不被编辑器挤出: {:?}",
            rows[9]
        );
    }

    /// 无内容行的全屏编辑器（`/agents types → edit <type>`）：路径并进标题行 + 空行 + 正文。
    #[test]
    fn fullscreen_editor_without_content_lines_merges_the_path_into_the_title() {
        use crate::core::extensions::OverlayEditor;
        let mut app = App::new();
        let view = OverlayView {
            title: "Edit agent: general-purpose".to_string(),
            lines: Vec::new(),
            footer: vec![
                ("Ctrl+S".to_string(), "save".to_string()),
                ("Esc".to_string(), "cancel".to_string()),
            ],
            input: None,
            editor: Some(OverlayEditor {
                label: "/tmp/agents/general-purpose.md".to_string(),
                value: String::new(),
                placeholder: String::new(),
                rows: 0,
                generation: 1,
            }),
            size: OverlaySize::Fullscreen,
            selected: None,
            input_focus: false,
            ..OverlayView::inline("x", Vec::new(), 8)
        };
        crate::modes::interactive::overlay::open(&mut app, 1, view);
        app.overlay
            .as_mut()
            .unwrap()
            .editor
            .insert_text("---\nname: general-purpose");
        app.overlay.as_mut().unwrap().editor.cursor_line = 0;
        app.overlay.as_mut().unwrap().editor.cursor_col = 0;
        let rows = draw(&mut app, 70, 8);
        assert!(
            rows[0].contains("Edit agent: general-purpose"),
            "{:?}",
            rows[0]
        );
        assert!(
            rows[0].contains("/tmp/agents/general-purpose.md"),
            "路径应与标题同行: {:?}",
            rows[0]
        );
        assert!(
            !rows[0].contains("empty"),
            "无内容行不应显示位置: {:?}",
            rows[0]
        );
        assert!(rows[1].trim().is_empty(), "正文前应为空行: {:?}", rows[1]);
        assert!(rows[2].contains("---"), "正文首行: {:?}", rows[2]);
        assert!(rows[3].contains("name: general-purpose"), "{:?}", rows[3]);
        // 8 行 - 标题(含路径)1 - 空行1 - 底栏1 = 5 行正文，底栏仍在最后一行
        assert!(rows[7].contains("Ctrl+S save"), "{:?}", rows[7]);
    }

    /// 路径过长：标题行不溢出，路径左截断（保留文件名）。
    #[test]
    fn long_path_is_truncated_on_the_left_to_fit_the_title_row() {
        use crate::core::extensions::OverlayEditor;
        let mut app = App::new();
        let long = format!("/very/deep/{}/agent.md", "nested/".repeat(10));
        let view = OverlayView {
            title: "Edit agent: general-purpose".to_string(),
            lines: Vec::new(),
            footer: vec![("Esc".to_string(), "cancel".to_string())],
            input: None,
            editor: Some(OverlayEditor {
                label: long,
                value: String::new(),
                placeholder: String::new(),
                rows: 0,
                generation: 1,
            }),
            size: OverlaySize::Fullscreen,
            selected: None,
            input_focus: false,
            ..OverlayView::inline("x", Vec::new(), 8)
        };
        crate::modes::interactive::overlay::open(&mut app, 1, view);
        let rows = draw(&mut app, 40, 6);
        assert!(
            rows[0].contains("Edit agent: general-purpose"),
            "{:?}",
            rows[0]
        );
        assert!(rows[0].contains('…'), "超长路径应左截断: {:?}", rows[0]);
        assert!(rows[0].contains("agent.md"), "应保留文件名: {:?}", rows[0]);
        assert!(rows[0].chars().count() <= 40, "不得溢出: {:?}", rows[0]);
    }

    /// 面板样式（FleetView）：分割行 / 空行 / 内容 / 空行 / 键位提示 / 空行 / 分割行。
    #[test]
    fn panel_size_draws_the_panel_layout() {
        let mut app = App::new();
        let view = OverlayView {
            title: "Fleet".to_string(),
            lines: (0..20)
                .map(|i| vec![DockSpan::plain(format!("agent-{i}"))])
                .collect(),
            footer: vec![("Enter".to_string(), "open".to_string())],
            size: OverlaySize::Panel { max_rows: 10 },
            ..OverlayView::inline("x", Vec::new(), 10)
        };
        crate::modes::interactive::overlay::open(&mut app, 1, view);
        let rows = draw(&mut app, 20, 12);
        // 12 行：分割线(0) / 空(1) / 内容(2..=7) / 空(8) / 键位(9) / 空(10) / 分割线(11)
        assert!(rows[0].starts_with('─'), "顶部分割线: {:?}", rows[0]);
        assert!(rows[1].trim().is_empty(), "分割线后空行: {:?}", rows[1]);
        assert!(rows[2].contains("agent-0"), "内容首行: {:?}", rows[2]);
        assert!(rows[7].contains("agent-5"), "内容末行: {:?}", rows[7]);
        assert!(rows[8].trim().is_empty(), "内容后空行: {:?}", rows[8]);
        assert!(rows[9].contains("Enter open"), "键位提示: {:?}", rows[9]);
        assert!(rows[10].trim().is_empty(), "键位后空行: {:?}", rows[10]);
        assert!(rows[11].starts_with('─'), "底部分割线: {:?}", rows[11]);
        // 面板样式不渲染标题行
        assert!(
            !rows.iter().any(|r| r.contains("Fleet")),
            "面板样式无标题行: {rows:?}"
        );
        assert_eq!(app.overlay.as_ref().unwrap().content_rows, 6);
    }

    /// 滚到底时 thumb 应贴 track 底（不再离底一个 thumb 高度）。
    #[test]
    fn scrollbar_thumb_reaches_the_bottom_at_max_scroll() {
        let mut app = App::new();
        let view = OverlayView {
            title: "Viewer".to_string(),
            lines: (0..1000)
                .map(|i| vec![DockSpan::plain(format!("line-{i}"))])
                .collect(),
            footer: Vec::new(),
            size: OverlaySize::Fullscreen,
            ..OverlayView::inline("x", Vec::new(), 10)
        };
        crate::modes::interactive::overlay::open(&mut app, 1, view);
        app.overlay.as_mut().unwrap().content_rows = 10;
        app.overlay.as_mut().unwrap().scroll = app.overlay.as_ref().unwrap().max_scroll();
        let rows = draw(&mut app, 20, 12);
        let track = app.overlay.as_ref().unwrap().content_rows;
        let content_y = app.overlay.as_ref().unwrap().sel.content_y as usize;
        // 内容区最后一行应是 thumb（贴 track 底）
        assert!(
            rows[content_y + track - 1].ends_with('█'),
            "滚到底时 thumb 应在内容区最后一行: {rows:?}"
        );
        let (start, len) = app.overlay.as_ref().unwrap().sel.scroll_thumb.unwrap();
        assert_eq!(
            start + len,
            track,
            "thumb 应贴 track 底: start={start} len={len} track={track}"
        );
    }

    /// 滚动条只画在内容区（标题/固定区/底栏不占 track），与拖动映射一致。
    #[test]
    fn scrollbar_is_confined_to_the_content_area() {
        let mut app = App::new();
        let view = OverlayView {
            title: "Viewer".to_string(),
            header: vec![vec![DockSpan::plain("status")]],
            lines: (0..50)
                .map(|i| vec![DockSpan::plain(format!("line-{i}"))])
                .collect(),
            footer: vec![("Esc".to_string(), "close".to_string())],
            size: OverlaySize::Fullscreen,
            ..OverlayView::inline("x", Vec::new(), 8)
        };
        crate::modes::interactive::overlay::open(&mut app, 1, view);
        let rows = draw(&mut app, 20, 8);
        // 布局：标题(0) / 固定区(1) / 内容(2..=6) / 底栏(7)
        assert!(!rows[0].ends_with('█'), "标题行不应有滚动条: {:?}", rows[0]);
        assert!(!rows[1].ends_with('█'), "固定区不应有滚动条: {:?}", rows[1]);
        assert!(!rows[7].ends_with('█'), "底栏不应有滚动条: {:?}", rows[7]);
        assert!(
            rows[2..=6].iter().any(|r| r.ends_with('█')),
            "内容区应有滚动条: {rows:?}"
        );
    }

    /// 输入行（`resume>`/`steer>`）只在**聚焦**时出现，且排在键位提示行**上方**。
    #[test]
    fn input_line_shows_only_when_focused_and_sits_above_the_footer() {
        fn app_with_input(active: bool) -> App {
            let mut app = App::new();
            let view = OverlayView {
                title: "Viewer".to_string(),
                lines: (0..5)
                    .map(|i| vec![DockSpan::plain(format!("line-{i}"))])
                    .collect(),
                footer: vec![
                    ("↑↓".to_string(), "scroll".to_string()),
                    ("i".to_string(), "resume".to_string()),
                ],
                input: Some(OverlayInput {
                    label: "resume>".to_string(),
                    value: String::new(),
                    placeholder: "message…".to_string(),
                }),
                editor: None,
                size: OverlaySize::Fullscreen,
                selected: None,
                input_focus: active,
                ..OverlayView::inline("x", Vec::new(), 8)
            };
            crate::modes::interactive::overlay::open(&mut app, 9, view);
            // 直接置位（单测里不走扩展的 focus 边沿）
            app.overlay.as_mut().unwrap().input_active = active;
            app
        }

        // 未聚焦：不显示输入行，内容区多吃一行
        let mut app = app_with_input(false);
        let rows = draw(&mut app, 40, 6);
        assert!(
            !rows.iter().any(|r| r.contains("resume>")),
            "未聚焦不应显示输入行: {rows:?}"
        );
        assert!(rows[5].contains("scroll"), "键位提示应在最后一行: {rows:?}");

        // 聚焦：输入行出现，且在键位提示行**上方**
        let mut app = app_with_input(true);
        let rows = draw(&mut app, 40, 6);
        let input_row = rows
            .iter()
            .position(|r| r.contains("resume>"))
            .expect("聚焦后应显示输入行");
        let footer_row = rows
            .iter()
            .position(|r| r.contains("scroll"))
            .expect("键位提示行");
        assert!(
            input_row < footer_row,
            "输入行应在键位提示上方: input={input_row} footer={footer_row}\n{rows:?}"
        );
    }
}
