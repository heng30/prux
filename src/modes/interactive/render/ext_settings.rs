//! 扩展设置面板渲染：占据输入框区域的设置视图（单扩展，对齐 `/settings`）。
//!
//! 行结构（与 `/settings` 一致）：搜索行 `> query` → 列表 `→ label <对齐> value`
//! （选中行 accent 高亮）→ 选中项描述（按宽度折行完整展示）→ 底部提示；上下边框，
//! 顶部边框左侧留 4 列后嵌扩展名（`---- xxx ------`，名称与左右边框各隔一列空格）。
//!
//! 状态与动作在 [`crate::modes::interactive::ext_settings`]，键盘处理在
//! `handlers::ext_settings`。

use crate::{
    modes::interactive::{app::App, ext_settings::MAX_VISIBLE},
    utils::display::{display_width, truncate_display, wrap_words},
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

/// 可见项窗口 `[start, end)`（`panel_height` 与 `render` 共用，保证布局与绘制一致）。
fn visible_window(app: &App) -> (usize, usize) {
    let p = &app.ext_settings;
    let total = p.filtered.len();
    let start = p.offset.min(total);
    let end = (start + MAX_VISIBLE).min(total);
    (start, end)
}

/// 面板总高度（含上下分割线与内容上下各 1 行空行）；不可见时为 0（不占布局）。
///
/// 与 [`render`] 同源：都来自 [`ext_settings_lines`]（`内容行数 + 4`），
/// 保证「分割线 ↔ 内容」恰好 1 行空行间隔。
pub fn panel_height(app: &App, width: usize) -> u16 {
    if !app.ext_settings.visible {
        return 0;
    }
    super::panel::trim_panel_blanks(ext_settings_lines(app, width)).len() as u16 + 4
}

/// 设置视图内容行（可能自带首尾空行；`render_panel_lines` 会统一裁掉再补留白）。
fn ext_settings_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let accent = app.theme.style("accent", "#8abeb7");
    let muted = app.theme.style("muted", "#808080");

    // label 对齐宽度（与 /settings 一致：最宽 ≤30）
    let max_label = app
        .ext_settings
        .items
        .iter()
        .map(|it| display_width(&it.label))
        .max()
        .unwrap_or(0)
        .min(30);

    let selected = app.ext_settings.selected;
    let total = app.ext_settings.filtered.len();
    let mut lines: Vec<Line<'static>> = Vec::new();

    // 搜索行（`> ` 前缀）
    let (view, _) = app
        .ext_settings
        .filter
        .visible_window(width.saturating_sub(4));
    let (view, _) = truncate_display(&view, width.saturating_sub(4));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("> ", Style::default()),
        Span::styled(view, Style::default()),
    ]));
    lines.push(Line::from(""));

    if total == 0 {
        lines.push(Line::from(Span::styled("  No matching settings", muted)));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Type to search · Enter/Space to change · Esc/Ctrl+C to hide",
            muted,
        )));
        lines.push(Line::from(""));
        return lines;
    }

    // 列表（选中窗口居中滚动）
    let (start, end) = visible_window(app);
    for i in start..end {
        let item = &app.ext_settings.items[app.ext_settings.filtered[i]];
        let is_selected = i == selected;
        let prefix = if is_selected { "→ " } else { "  " };
        let padded =
            item.label.clone() + &" ".repeat(max_label.saturating_sub(display_width(&item.label)));
        let label_style = if is_selected {
            accent
        } else {
            Style::default()
        };
        let value_style = if is_selected { accent } else { muted };
        let used = 2 + max_label + 2;
        let value_max = width.saturating_sub(used).saturating_sub(1);
        let (value, _) = truncate_display(&item.value, value_max.max(1));
        lines.push(Line::from(vec![
            Span::styled(prefix.to_string() + &padded, label_style),
            Span::styled("  ", Style::default()),
            Span::styled(value, value_style),
        ]));
    }

    // 页码（仅滚动时）
    if start > 0 || end < total {
        lines.push(Line::from(Span::styled(
            format!("  ({}/{})", selected + 1, total),
            muted,
        )));
    }
    lines.push(Line::from(""));

    // 选中项描述（按宽度折行完整展示，不过早截断）
    if let Some(item) = app.ext_settings.current()
        && !item.description.is_empty()
    {
        for l in wrap_words(&item.description, width.saturating_sub(4).max(1)) {
            lines.push(Line::from(Span::styled(format!("  {l}"), muted)));
        }

        lines.push(Line::from(""));
    }

    // 提示行（应用失败时优先显示错误）
    let hint = match &app.ext_settings.error {
        Some(err) => format!("  {err}"),
        None => "  Type to search · Enter/Space to change · Esc/Ctrl+C to hide".to_string(),
    };
    lines.push(Line::from(Span::styled(
        hint,
        if app.ext_settings.error.is_some() {
            app.theme.style("error", "#cc6666")
        } else {
            muted
        },
    )));
    lines.push(Line::from(""));

    lines
}

/// 渲染设置视图（上下边框 + 搜索行 + 列表 + 描述 + 提示；标题为扩展名）。
pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width as usize;
    let border = app.theme.style("border", "#5f87ff");
    let lines = ext_settings_lines(app, width);
    super::panel::render_panel_lines(frame, area, width, border, lines);

    // 顶部边框：左侧 2 列边框 → ` ext ` → 右侧边框填满（如 `-- xxx ------`）
    if area.height > 0 && width >= 4 && !app.ext_settings.ext.is_empty() {
        let (ext, _) = truncate_display(&app.ext_settings.ext, width.saturating_sub(4));
        let ext_w = display_width(&ext);
        let right = width.saturating_sub(2 + 1 + ext_w + 1);
        let top = Line::from(vec![
            Span::styled("─".repeat(2), border),
            Span::raw(" "),
            Span::styled(ext, border.add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled("─".repeat(right), border),
        ]);
        frame.render_widget(
            Paragraph::new(top),
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: 1,
            },
        );
    }
}

/// 按宽度截断/填充行并渲染（与 settings_selector 同款：上下边框 + 定制扩展名顶边）。
#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]
    use super::*;
    use crate::core::extensions::{Extension, ExtensionSetting, ExtensionTool};

    struct FakeExt;

    impl Extension for FakeExt {
        fn name(&self) -> &str {
            "fake-render-settings"
        }
        fn tools(&self) -> Vec<ExtensionTool> {
            Vec::new()
        }
        fn settings(&self) -> Vec<ExtensionSetting> {
            vec![
                ExtensionSetting::cycle("a", "Alpha", "first setting", "one", ["one", "two"]),
                ExtensionSetting::cycle("b", "Beta", "second setting", "x", ["x", "y"]),
            ]
        }
    }

    struct LongDescExt;

    impl Extension for LongDescExt {
        fn name(&self) -> &str {
            "fake-long-desc-settings"
        }
        fn tools(&self) -> Vec<ExtensionTool> {
            Vec::new()
        }
        fn settings(&self) -> Vec<ExtensionSetting> {
            vec![ExtensionSetting::cycle(
                "join",
                "Group notifications",
                LONG_DESC,
                "smart",
                ["smart", "group", "async"],
            )]
        }
    }

    /// `/agents settings` 里的一条真实长度描述（配置说明行）
    const LONG_DESC: &str = "Batch background completions into one notification (smart: 30s window, group: 15s, async: one per agent)";

    fn frame_text(app: &App, width: u16, height: u16) -> Vec<String> {
        use ratatui::backend::TestBackend;
        let mut terminal = ratatui::Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| render(f, Rect::new(0, 0, width, height), app))
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn renders_search_rows_title_and_hint() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::core::extensions::register_extension(FakeExt);

        let mut app = App::new();
        app.ext_settings.open("fake-render-settings");
        let h = panel_height(&app, 60);
        assert!(h >= 9, "面板高度应含搜索/列表/描述/提示: {h}");

        let rows = frame_text(&app, 60, h);
        let joined = rows.join("\n");
        assert!(
            joined.contains("fake-render-settings"),
            "边框标题缺失: {joined}"
        );
        assert!(
            joined.contains("── fake-render-settings ─"),
            "扩展名应在左侧 2 列边框之后: {joined}"
        );
        assert!(joined.contains("> "), "搜索行缺失: {joined}");
        assert!(joined.contains("→ Alpha"), "{joined}");
        assert!(
            joined.contains("first setting"),
            "选中项描述应展示: {joined}"
        );
        assert!(joined.contains("Type to search"), "提示行缺失: {joined}");

        crate::core::extensions::unregister_extension("fake-render-settings");
    }

    #[test]
    fn panel_has_exactly_one_blank_line_around_content() {
        // 统一留白：扩展设置面板与 /settings 一致，内容上下各恰好 1 行空行。
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::core::extensions::register_extension(FakeExt);

        let mut app = App::new();
        app.ext_settings.open("fake-render-settings");
        let h = panel_height(&app, 60);
        let rows = frame_text(&app, 60, h);
        assert!(rows[0].starts_with('─'), "上分割线: {:?}", rows[0]);
        assert!(
            rows[(h - 1) as usize].starts_with('─'),
            "下分割线: {:?}",
            rows[(h - 1) as usize]
        );
        let inner = &rows[1..(h - 1) as usize];
        let lead = inner.iter().take_while(|r| r.trim().is_empty()).count();
        let trail = inner
            .iter()
            .rev()
            .take_while(|r| r.trim().is_empty())
            .count();
        assert_eq!(lead, 1, "顶部恰好 1 行空行: {rows:?}");
        assert_eq!(trail, 1, "底部恰好 1 行空行: {rows:?}");

        crate::core::extensions::unregister_extension("fake-render-settings");
    }

    #[test]
    fn search_row_shows_query_and_no_match_state() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::core::extensions::register_extension(FakeExt);

        let mut app = App::new();
        app.ext_settings.open("fake-render-settings");
        app.ext_settings.filter.set_value("beta");
        app.ext_settings.recompute();
        let h = panel_height(&app, 60);
        let joined = frame_text(&app, 60, h).join("\n");
        assert!(joined.contains("> beta"), "搜索行应显示查询: {joined}");
        assert!(joined.contains("Beta"), "{joined}");
        assert!(!joined.contains("Alpha"), "被过滤的项不应出现: {joined}");

        app.ext_settings.filter.set_value("zzz");
        app.ext_settings.recompute();
        let joined = frame_text(&app, 60, panel_height(&app, 60)).join("\n");
        assert!(joined.contains("No matching settings"), "{joined}");

        crate::core::extensions::unregister_extension("fake-render-settings");
    }

    #[test]
    fn long_description_wraps_and_panel_grows() {
        // `/agents settings` 配置说明行：过长时按宽度折行完整展示（曾单行截断）
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::core::extensions::register_extension(LongDescExt);

        let mut app = App::new();
        app.ext_settings.open("fake-long-desc-settings");

        let per_line = wrap_words(LONG_DESC, 60 - 4);
        assert!(per_line.len() > 1, "描述应折行: {per_line:?}");
        // 1 项列表 + 无页码 + 描述 per_line 行
        assert_eq!(panel_height(&app, 60) as usize, 9 + 1 + per_line.len());

        let rows = frame_text(&app, 60, panel_height(&app, 60));
        for l in &per_line {
            assert!(
                rows.iter().any(|r| r.contains(l.as_str())),
                "描述行被截断/丢失: {l:?}\n{}",
                rows.join("\n")
            );
        }
        // 末行（含 `one per agent`）完整可见
        assert!(rows.join("\n").contains("one per agent)"), "{rows:?}");

        crate::core::extensions::unregister_extension("fake-long-desc-settings");
    }

    #[test]
    fn height_is_zero_when_inactive() {
        let app = App::new();
        assert_eq!(panel_height(&app, 60), 0);
    }
}
