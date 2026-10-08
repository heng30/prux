//! /settings 选择器渲染：行结构（label 对齐 / value 靠右 / 选中高亮）、子菜单、描述、底部 hint。
//!
//! 状态与动作在 `settings_selector`，事件处理在 `settings_selector::handle_settings_selector_key`。

use super::panel::wrap_text;
use crate::{
    modes::interactive::{
        app::App,
        settings_selector::{MAX_VISIBLE, SubmenuKind, SubmenuState},
    },
    utils,
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};

/// 选择器总高度（含上下分割线与内容上下各 1 行空行）。
///
/// 与 [`render`] 同源：都来自 [`settings_lines`]（`内容行数 + 4`），
/// 保证「分割线 ↔ 内容」恰好 1 行空行间隔。
pub fn selector_height(app: &App, width: usize) -> u16 {
    let s = &app.settings_selector;
    if !s.active {
        return 0;
    }
    super::panel::trim_panel_blanks(settings_lines(app, width)).len() as u16 + 4
}

/// 选择器内容行（可能自带首尾空行；`render_panel_lines` 会统一裁掉再补留白）。
fn settings_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(sub) = &app.settings_selector.submenu {
        render_submenu(&mut lines, app, sub, width);
    } else {
        render_main_list(&mut lines, app, width);
    }
    lines
}

/// 渲染选择器（settings_selector.active 时占据输入框区域）
pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width as usize;
    let border = app.theme.style("border", "#5f87ff");
    let lines = settings_lines(app, width);
    super::panel::render_panel_lines(frame, area, width, border, lines);
}

/// 向 `lines` 追加子菜单内容：标题、按宽度折行的说明、居中滚动的选项列表与底部按键提示。
/// `app` 仅用于取主题样式。
fn render_submenu(lines: &mut Vec<Line<'static>>, app: &App, sub: &SubmenuState, width: usize) {
    let accent = app.theme.style("accent", "#8abeb7");
    let muted = app.theme.style("muted", "#808080");
    let bold = Style::default().add_modifier(Modifier::BOLD);

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        sub.title.clone(),
        bold.patch(accent),
    )));
    lines.push(Line::from(""));

    if !sub.description.is_empty() {
        for chunk in wrap_text(&sub.description, width.saturating_sub(4)) {
            lines.push(Line::from(Span::styled(format!(" {}", chunk), muted)));
        }
        lines.push(Line::from(""));
    }

    // 选项列表（居中滚动）
    let total = sub.options.len();
    let max = total.min(MAX_VISIBLE);
    let start = sub
        .selected
        .saturating_sub(max / 2)
        .min(total.saturating_sub(max));
    let end = (start + max).min(total);

    for i in start..end {
        let opt = &sub.options[i];
        let selected = i == sub.selected;
        let prefix = if selected { "→ " } else { "  " };
        let label_style = if selected { accent } else { Style::default() };

        // 勾选列表：`[x] name`（对齐 /extension 面板）；单选列表用选项自带 label
        let text = match sub.kind {
            SubmenuKind::Toggle => {
                format!("[{}] {}", if opt.checked { "x" } else { " " }, opt.value)
            }
            SubmenuKind::Select => opt.label.clone(),
        };

        let mut spans = vec![Span::styled(format!("{}{}", prefix, text), label_style)];
        if !opt.description.is_empty() && width > 40 {
            let used = 4 + utils::display::display_width(&text);
            let remain = width.saturating_sub(used);
            if remain > 10 {
                let (d, _) = utils::display::truncate_display(&opt.description, remain);
                spans.push(Span::styled(format!("  {}", d), muted));
            }
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    let hint = match sub.kind {
        SubmenuKind::Toggle => "  Space/Enter to toggle · Esc to go back",
        SubmenuKind::Select => "  Enter to select · Esc to go back",
    };
    lines.push(Line::from(Span::styled(hint, muted)));
    lines.push(Line::from(""));
}

/// 向 `lines` 追加主列表内容：搜索输入行 + 过滤后的设置项；无匹配时只输出空态与提示。
fn render_main_list(lines: &mut Vec<Line<'static>>, app: &App, width: usize) {
    let s = &app.settings_selector;
    let muted = app.theme.style("muted", "#808080");

    // 搜索输入行（`> ` 前缀）
    let (view, _) = s.filter.visible_window(width.saturating_sub(4));
    let (f, _) = utils::display::truncate_display(&view, width.saturating_sub(4));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("> ", Style::default()),
        Span::styled(f, Style::default()),
    ]));
    lines.push(Line::from(""));

    let total = s.filtered.len();
    if total == 0 {
        lines.push(Line::from(Span::styled("  No matching settings", muted)));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Type to search · Enter/Space to change · Esc to cancel",
            muted,
        )));
        lines.push(Line::from(""));
        return;
    }

    // 列表 + 选中项描述 + 底部 hint
    render_setting_items(lines, app, width);
}

/// 渲染设置列表条目、选中项描述与底部 hint
fn render_setting_items(lines: &mut Vec<Line<'static>>, app: &App, width: usize) {
    let s = &app.settings_selector;
    let accent = app.theme.style("accent", "#8abeb7");
    let muted = app.theme.style("muted", "#808080");
    let total = s.filtered.len();

    // 列表：label 对齐（最宽 ≤30，对齐 pi maxLabelWidth）
    let max_label = s
        .items
        .iter()
        .map(|it| utils::display::display_width(&it.label))
        .max()
        .unwrap_or(0)
        .min(30);

    let start = s
        .selected
        .saturating_sub(MAX_VISIBLE / 2)
        .min(total.saturating_sub(MAX_VISIBLE));
    let end = (start + MAX_VISIBLE).min(total);
    for i in start..end {
        let item = &s.items[s.filtered[i]];
        let selected = i == s.selected;
        let prefix = if selected { "→ " } else { "  " };
        let label_padded = item.label.clone()
            + &" ".repeat(max_label.saturating_sub(utils::display::display_width(&item.label)));
        let label_style = if selected { accent } else { Style::default() };
        let used = 2 + max_label + 2;
        let value_max = width.saturating_sub(used).saturating_sub(2);
        let (value, _) = utils::display::truncate_display(&item.current_value, value_max);
        let value_style = if selected { accent } else { muted };
        lines.push(Line::from(vec![
            Span::styled(prefix.to_string() + &label_padded, label_style),
            Span::styled("  ", Style::default()),
            Span::styled(value, value_style),
        ]));
    }

    // 页码（仅滚动时）
    if start > 0 || end < total {
        lines.push(Line::from(Span::styled(
            format!("  ({}/{})", s.selected + 1, total),
            muted,
        )));
    }
    lines.push(Line::from(""));

    // 选中项描述（自动换行，2 空格前缀）
    if let Some(&idx) = s.filtered.get(s.selected) {
        let desc = &s.items[idx].description;
        if !desc.is_empty() {
            for chunk in wrap_text(desc, width.saturating_sub(4)) {
                lines.push(Line::from(Span::styled(format!("  {}", chunk), muted)));
            }
            lines.push(Line::from(""));
        }
    }

    // 底部 hint
    lines.push(Line::from(Span::styled(
        "  Type to search · Enter/Space to change · Esc to cancel",
        muted,
    )));
    lines.push(Line::from(""));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent() -> crate::modes::interactive::agent_actor::WorkerHandle {
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::modes::interactive::agent_actor::WorkerHandle::null()
    }

    fn open_sel_on(app: &mut App, _agent: &crate::modes::interactive::agent_actor::WorkerHandle) {
        let (show_thinking, theme_name, autocomplete_max) = (
            app.show_thinking,
            app.theme.name.clone(),
            app.autocomplete_max_visible,
        );
        app.settings_selector.open(
            show_thinking,
            &theme_name,
            autocomplete_max,
            app.thinking_level.as_deref().unwrap_or("off"),
        );
    }

    #[test]
    fn height_includes_search_list_desc_and_hint() {
        // open_sel 读共享 settings.json：与写盘测试互斥（见 settings_selector tests 注释）
        let _ad = crate::test_support::AgentDirGuard::temp();
        let agent = test_agent();
        let mut app = App::new();
        open_sel_on(&mut app, &agent);
        let h = selector_height(&app, 80) as usize;
        // 边框2 + 空/搜索/空 + 列表8 + 空 + 描述wrap行 + 空 + 提示 + 空
        assert!(h >= 9 + 8, "height too small: {}", h);
        // 选中项描述较长时高度增加（换行行数计入）
        app.settings_selector.items[0].description = "x".repeat(200);
        let h2 = selector_height(&app, 80) as usize;
        assert!(h2 > h, "描述行应增加高度: {} > {}", h2, h);
    }

    #[test]
    fn render_layout_matches_pi_settings_list() {
        // 对齐 pi 截图：label 左对齐（最宽≤30）、value 右对齐、选中行 → 前缀、
        // 描述在选中项下方、底部 hint Type to search · Enter/Space to change · Esc to cancel
        let _ad = crate::test_support::AgentDirGuard::temp();
        let agent = test_agent();
        let mut app = App::new();
        open_sel_on(&mut app, &agent);
        app.settings_selector.selected = 0; // Auto-compact（第一个，description 一行）
        let h = selector_height(&app, 80);
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(80, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, h);
                render(f, area, &app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let row = |y: u16| {
            (0..80u16)
                .map(|x| {
                    buf.cell((x, y))
                        .map(|c| c.symbol().to_string())
                        .unwrap_or_default()
                })
                .collect::<String>()
        };
        // 搜索行（边框1 + 空1 之后）
        assert!(row(2).starts_with("> "), "search row: {:?}", row(2));
        // 选中行（Auto-compact）→ 前缀（列表第 1 行 = 空/搜索/空 后第 4 行）
        let r4 = row(4);
        assert!(
            r4.trim_start().starts_with("→ Auto-compact"),
            "row4: {:?}",
            r4
        );
        // 26 项列表：第 3 行起为列表第 1 项；selected=0 时可见前 10 项，
        // 最后可见项为 Retry max attempts（Compact reserve tokens 及之后的滚动出可见区）
        let mut last_found = false;
        for y in 3..20u16 {
            if row(y).contains("Retry max attempts") {
                last_found = true;
                break;
            }
        }
        assert!(last_found, "未找到最后可见设置项");
        // value 对齐：Auto-compact 行右侧应出现 true
        let mut auto_row: Option<String> = None;
        for y in 3..20u16 {
            let r = row(y);
            if r.trim_start().starts_with("  Auto-compact")
                || r.trim_start().starts_with("→ Auto-compact")
            {
                auto_row = Some(r);
                break;
            }
        }
        let v = auto_row.expect("Auto-compact 行");
        let trimmed: String = v
            .trim_end()
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        assert_eq!(trimmed, "true", "value on right: {:?}", trimmed);
        // 描述在选中项下方（Auto-compact 的 description）
        let mut desc_found = false;
        for y in 3..20u16 {
            if row(y).contains("Automatically compact context") {
                desc_found = true;
                break;
            }
        }
        assert!(desc_found, "未找到描述");
        // 底部 hint
        assert!(
            (1..h)
                .any(|y| row(y).contains("Type to search · Enter/Space to change · Esc to cancel")),
            "hint missing"
        );
    }

    #[test]
    fn toggle_submenu_renders_extension_style_checkboxes() {
        // 勾选子面板（Built-in tools）：`[x] name` 勾选态 + 工具说明 + 切换提示，
        // 对齐 /extension 面板的 checkbox 写法
        let _ad = crate::test_support::AgentDirGuard::temp();
        let agent = test_agent();
        let mut app = App::new();
        open_sel_on(&mut app, &agent);
        app.settings_selector.submenu = Some(SubmenuState {
            title: "Built-in tools".to_string(),
            description: String::new(),
            kind: SubmenuKind::Toggle,
            options: vec![
                crate::modes::interactive::settings_selector::SubmenuOption {
                    value: "read".to_string(),
                    label: "read".to_string(),
                    description: "Read files and images".to_string(),
                    checked: true,
                },
                crate::modes::interactive::settings_selector::SubmenuOption {
                    value: "grep".to_string(),
                    label: "grep".to_string(),
                    description: "Search file contents".to_string(),
                    checked: false,
                },
            ],
            selected: 1,
            item_index: 0,
        });

        let w = 80u16;
        let h = selector_height(&app, w as usize);
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(f, Rect::new(0, 0, w, h), &app))
            .unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| {
                        buf.cell((x, y))
                            .map(|c| c.symbol().to_string())
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .collect();
        let text = rows.join("\n");
        assert!(text.contains("Built-in tools"), "{text}");
        assert!(text.contains("[x] read"), "勾选态: {text}");
        assert!(text.contains("→ [ ] grep"), "未勾选项带 → 前缀: {text}");
        assert!(text.contains("Search file contents"), "工具说明: {text}");
        assert!(
            text.contains("Space/Enter to toggle · Esc to go back"),
            "底部提示: {text}"
        );
    }

    #[test]
    fn selector_has_exactly_one_blank_line_around_content() {
        // 统一留白：/settings 与其它 `/` 面板一致，内容上下各恰好 1 行空行。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let agent = test_agent();
        let mut app = App::new();
        open_sel_on(&mut app, &agent);
        let w = 80u16;
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
}
