//! 停靠面板渲染（状态栏上方的可滚动面板）。
//!
//! 内容由已启用扩展经 [`Extension::dock_lines`] 提供（含扩展自带的标题行）；
//! **所有**提供非空内容的扩展的段（含段头）按注册顺序平铺成一个列表，段间空行分割，
//! 整块使用一个滚动视口——段头就是普通一行，跟随滚动滚出屏幕。
//! 面板高度 = `min(总行数, MAX_DOCK_TOTAL)`，内容超出高度由鼠标滚轮整体滚动浏览。
//! 可见性：`app.dock_visible`（/dock 切换或 ShowDock 请求置位）且存在内容；内容为空自动隐藏。

use crate::{
    core::extensions::{self, DockLine, DockSection, DockSpan},
    modes::interactive::app::App,
    utils::{
        display::{display_width, truncate_display},
        glyphs::DEF_SCROLLBAR_THUMB,
    },
};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};

/// 整块停靠面板高度上限（行；含各段标题行与段间空行）
pub const MAX_DOCK_TOTAL: usize = 14;

/// 空行占位（段间分割；借用进平铺列表不克隆）
static EMPTY_LINE: DockLine = Vec::new();

/// 待渲染的停靠内容（布局高度与渲染共用，每帧只查询一次扩展）
pub struct DockRender {
    /// 各提供者的内容段（每段首行为扩展自带的标题行）
    pub sections: Vec<DockSection>,
    /// 平铺后总行数（含段间空行）
    pub total: usize,
}

/// 查询当前停靠内容：`dock_visible` 且存在提供非空行的扩展时返回；
/// 隐藏或无内容时返回 None（调用方负责自动隐藏）。无锚点：打开从顶部开始。
pub fn dock_render(app: &App) -> Option<DockRender> {
    if !app.dock_visible {
        return None;
    }
    let sections = extensions::dock_sections();
    if sections.is_empty() {
        return None;
    }
    let total = sections.iter().map(|s| s.lines.len()).sum::<usize>() + sections.len() - 1;
    Some(DockRender { sections, total })
}

/// 停靠面板高度：`min(平铺总行数, MAX_DOCK_TOTAL)`；无内容时返回 0（调用方先判空）。
pub fn dock_height(dock: &DockRender) -> u16 {
    dock.total.min(MAX_DOCK_TOTAL) as u16
}

/// 渲染停靠面板并记录 `dock_area`（滚轮整体滚动命中测试用）。
/// 内容为空时自动隐藏：置 `dock_visible = false`、清偏移与区域。
pub fn render_dock(frame: &mut Frame, area: Rect, app: &mut App, dock: &DockRender) {
    if dock.sections.is_empty() || dock.total == 0 {
        app.dock_visible = false;
        app.dock_offset = 0;
        app.dock_area = None;
        return;
    }

    let width = area.width as usize;
    let view = area.height as usize;
    let max_offset = dock.total.saturating_sub(view);
    // 内容变短 / 终端变矮时夹回合法范围（增长不动偏移，无自动跟随）
    app.dock_offset = app.dock_offset.min(max_offset);
    let offset = app.dock_offset;

    // 平铺：段行 + 段间空行组成一个列表（借用，不克隆）
    let mut flat: Vec<&DockLine> = Vec::with_capacity(dock.total);
    for sec in &dock.sections {
        for l in &sec.lines {
            flat.push(l);
        }
        flat.push(&EMPTY_LINE);
    }

    let needs_scrollbar = max_offset > 0 && width >= 2;
    let avail = width.saturating_sub(usize::from(needs_scrollbar));

    let mut lines = Vec::with_capacity(view);
    for i in 0..view {
        let idx = offset + i;
        if idx >= flat.len() {
            lines.push(Line::from(""));
            continue;
        }
        lines.push(Line::from(dock_line_spans(flat[idx], avail, app)));
    }

    let para = Paragraph::new(lines).style(Style::default());
    frame.render_widget(para, area);

    if needs_scrollbar {
        let thumb_style = app.theme.style("scrollbarThumb", "#6a6a78");
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None)
            .thumb_symbol(DEF_SCROLLBAR_THUMB)
            .thumb_style(thumb_style);
        let mut state = ScrollbarState::new(dock.total)
            .position(offset)
            .viewport_content_length(view);
        frame.render_stateful_widget(
            scrollbar,
            Rect {
                x: area.x + area.width.saturating_sub(1),
                width: 1,
                ..area
            },
            &mut state,
        );
    }

    app.dock_area = Some(area);
}

/// 把一行 DockSpan 渲染为截断到 `avail` 宽度的 ratatui Spans（不折行）。
/// 超宽时在断点补 "…"（有剩余 1 列时）。
pub(crate) fn dock_line_spans(line: &[DockSpan], avail: usize, app: &App) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for span in line {
        if used >= avail {
            break;
        }
        let remaining = avail - used;
        let (text, truncated) = truncate_display(&span.text, remaining);
        used += display_width(&text);

        let mut text = text;
        if truncated && used < avail {
            text.push('…');
            used += 1;
        }

        let style = if span.key.is_empty() && span.fallback.is_empty() {
            Style::default()
        } else {
            app.theme.style(span.key, &span.fallback)
        };
        out.push(Span::styled(text, style));

        if truncated {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::DockLine;

    fn line(text: &str) -> DockLine {
        vec![DockSpan::plain(text)]
    }

    fn section(provider: &str, lines: Vec<DockLine>) -> DockSection {
        DockSection {
            provider: provider.to_string(),
            lines,
        }
    }

    fn flat_len(sections: &[DockSection]) -> usize {
        sections.iter().map(|s| s.lines.len()).sum::<usize>() + sections.len() - 1
    }

    #[test]
    fn height_caps_flat_total_at_max() {
        // 2 行 + 空行 + 3 行 = 6 行平铺 → 高度 6；超出 14 则截到 14
        let a = section("a", vec![line("title"), line("t1")]);
        let b = section("b", vec![line("T"), line("x"), line("y")]);
        let total = a.lines.len() + b.lines.len() + 1;
        let dock = DockRender {
            sections: vec![a, b],
            total,
        };
        assert_eq!(dock_height(&dock), 6);

        let big: Vec<DockLine> = (0..=50).map(|i| line(&format!("t{i}"))).collect();
        let dock = DockRender {
            sections: vec![section("a", big)],
            total: 51,
        };
        assert_eq!(dock_height(&dock), MAX_DOCK_TOTAL as u16, "超出上限截断");
    }

    #[test]
    fn hex_fallback_colors_the_span() {
        use crate::modes::interactive::app::App;
        use ratatui::style::Color;
        let app = App::new();
        // agent 类型 badge：key 为空 + fallback 十六进制 → 按该色着色
        let spans = dock_line_spans(&[DockSpan::hex("#DC2626", "badge")], 10, &app);
        assert_eq!(spans[0].style.fg, Some(Color::Rgb(0xDC, 0x26, 0x26)));
        assert_eq!(spans[0].content, "badge");
        // 完全空白段仍是终端默认前景
        let spans = dock_line_spans(&[DockSpan::plain("plain")], 10, &app);
        assert_eq!(spans[0].style.fg, None);
    }

    #[test]
    fn truncation_respects_display_width() {
        use crate::modes::interactive::app::App;
        let app = App::new();
        // 中文两列宽：5 列预算下 "一二三四" 占 8 列被截断
        let spans = dock_line_spans(&line("一二三四"), 5, &app);
        let text = spans.iter().map(|s| s.content.clone()).collect::<String>();
        assert_eq!(text, "一二…");

        // 多段拼接：总宽超出预算时第二段不再出现
        let mut l = line("ab");
        l.push(DockSpan::plain("cd"));
        let spans = dock_line_spans(&l, 3, &app);
        let text = spans.iter().map(|s| s.content.clone()).collect::<String>();
        assert_eq!(text, "abc");
    }

    #[test]
    fn offset_clamps_when_content_shrinks() {
        use crate::modes::interactive::app::App;
        let mut app = App::new();
        let lines: Vec<DockLine> = (0..=15).map(|i| line(&format!("t{i}"))).collect();
        let dock = DockRender {
            sections: vec![section("plan-mode", lines.clone())],
            total: lines.len(),
        };
        app.dock_visible = true;
        app.dock_offset = 8;
        let area = Rect::new(0, 0, 40, 11);
        let backend = ratatui::backend::TestBackend::new(40, 11);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_dock(f, area, &mut app, &dock))
            .unwrap();
        assert_eq!(app.dock_offset, 5, "total16 - view11 -> max_offset 5");
        assert!(app.dock_area.is_some(), "可见时记录 dock_area");
    }

    #[test]
    fn flat_multi_section_renders_with_separator_and_whole_scroll() {
        use crate::modes::interactive::app::App;
        let mut app = App::new();
        // a 段 4 行 + 空行 + b 段 3 行 = 8 行平铺；窗口 14 → 全部可见，无滚动条
        let a: Vec<DockLine> = (0..=3).map(|i| line(&format!("a{i}"))).collect();
        let b: Vec<DockLine> = (0..=2).map(|i| line(&format!("b{i}"))).collect();
        let sections = vec![section("alpha", a), section("beta", b)];
        let doc = DockRender {
            sections: sections.clone(),
            total: flat_len(&sections),
        };
        app.dock_visible = true;
        let area = Rect::new(0, 0, 40, 14);
        let backend = ratatui::backend::TestBackend::new(40, 14);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_dock(f, area, &mut app, &doc))
            .unwrap();
        let row = |y: u16| -> String {
            let buf = terminal.backend().buffer();
            (0..40)
                .map(|c| buf.cell((c, y)).unwrap().symbol().to_string())
                .collect()
        };
        assert!(row(0).contains("a0"), "第一段首行: {:?}", row(0));
        assert!(row(3).contains("a3"), "第一段末行: {:?}", row(3));
        assert!(row(4).trim().is_empty(), "段间空行: {:?}", row(4));
        assert!(row(5).contains("b0"), "第二段首行: {:?}", row(5));
        assert!(row(7).contains("b2"), "第二段末行: {:?}", row(7));
        assert!(row(8).trim().is_empty(), "窗口余下为空");
        assert!(app.dock_area.is_some());
        assert_eq!(app.dock_offset, 0, "内容未超窗口不滚动");
    }

    #[test]
    fn flat_scrolls_whole_list_past_section_boundaries() {
        use crate::modes::interactive::app::App;
        let mut app = App::new();
        // a 段 10 行 + 空行 + b 段 10 行 = 21 行；窗口 14 → 超限可滚，偏移穿过段边界
        let a: Vec<DockLine> = (0..=9).map(|i| line(&format!("a{i}"))).collect();
        let b: Vec<DockLine> = (0..=9).map(|i| line(&format!("b{i}"))).collect();
        let sections = vec![section("alpha", a), section("beta", b)];
        let doc = DockRender {
            sections: sections.clone(),
            total: flat_len(&sections),
        };
        app.dock_visible = true;
        app.dock_offset = 7;
        let area = Rect::new(0, 0, 40, 14);
        let backend = ratatui::backend::TestBackend::new(40, 14);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_dock(f, area, &mut app, &doc))
            .unwrap();
        let row = |y: u16| -> String {
            let buf = terminal.backend().buffer();
            (0..40)
                .map(|c| buf.cell((c, y)).unwrap().symbol().to_string())
                .collect()
        };
        // 第 7 行起：a7,a8,a9,空,b0,b1..., 偏移跨过 a 段边界进入 b 段
        assert!(row(0).contains("a7"), "第 7 行: {:?}", row(0));
        // 空行行的内容区（前 39 列，最右列是滚动条 thumb）为空
        assert!(row(3)[..39].trim().is_empty(), "跨段空行: {:?}", row(3));
        assert!(row(4).contains("b0"), "跨段后显示 b 段: {:?}", row(4));
        assert!(row(13).contains("b9"), "末行: {:?}", row(13));
        // 滚动条（21 行 / 14 窗口）
        let buf = terminal.backend().buffer();
        assert_eq!(buf.cell((39, 6)).unwrap().symbol(), "█");
    }
}
