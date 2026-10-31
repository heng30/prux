//! 项目信任面板渲染：启动期的信任选择器 + `/trust` 对话框。
//!
//! 信任面板的视图全在本模块：内容（标题/选项）与底部提示。
//! - [`project_trust_panel_content`] / [`project_trust_dialog_content`]：面板内容，
//!   由 `handlers::trust::open_project_trust_panel` / `open_project_trust_dialog`
//!   取用后打开面板（状态流转在 handlers/trust.rs）；
//! - [`project_trust_hint`]：被 `panel.rs` 的 `panel_hint` 分发；
//! - [`project_trust_lines`]：内容行，被 `panel.rs` 的 `panel_content` 分发。
//!
//! 内容布局（上下留白由 `panel.rs` 的 `render_panel_lines` 统一补 1 行）：
//!    标题块（首行 accent、其余 muted，按宽度折行，显式空行保留）
//!    空行
//!    `→ ` + [`✓ `]（`/trust` 对话框有当前列）+ 选项（选中 accent）
//!    空行
//!    操作提示（warning）

use super::{
    super::{
        app::{App, truncate_display},
        panel::{PanelItem, PanelKind, PanelLayer},
    },
    panel::panel_hint,
};
use crate::{
    core::{project_trust, settings_manager::agent_dir},
    utils::{
        display::wrap_words,
        glyphs::{DEF_ARROW_UP_DOWN, DEF_CURSOR, DEF_DONE},
        paths::normalize_cwd,
    },
};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use std::path::Path;

/// 启动期项目信任选择面板内容（对齐 pi `getProjectTrustOptions`）：
/// Trust / Trust parent folder (…) / Trust (this session only) / Do not trust /
/// Do not trust (this session only)。
pub fn project_trust_panel_content(app: &App) -> (String, Vec<PanelItem>) {
    let cwd = app.cwd.clone();
    let title = format!(
        "Trust project folder?\n{cwd}\n\nThis allows {} to load {} settings and resources, install missing project packages, and execute project extensions.",
        crate::APP_NAME,
        crate::PROJECT_SCOPE_NAME,
    );
    let item = |label: String, value: &str| PanelItem {
        label,
        value: value.to_string(),
        desc: String::new(),
        name: String::new(),
        ..Default::default()
    };

    let mut items = vec![item("Trust".to_string(), "trust")];
    if let Some(parent) = Path::new(&cwd).parent()
        && !parent.as_os_str().is_empty()
    {
        items.push(item(
            format!("Trust parent folder ({})", parent.display()),
            "trust_parent",
        ));
    }
    items.push(item(
        "Trust (this session only)".to_string(),
        "trust_session",
    ));
    items.push(item("Do not trust".to_string(), "distrust"));
    items.push(item(
        "Do not trust (this session only)".to_string(),
        "distrust_session",
    ));

    (title, items)
}

/// `/trust` 对话框内容（对齐 pi `TrustSelectorComponent`）：标题（cwd + 已保存决策 +
/// 当前会话状态）、选项（Trust / Trust parent folder / Do not trust /
/// Remove all saved trust decisions，无仅本次选项）与初始选中项（已保存决策命中的选项，
/// 无匹配则第一项 Trust）。
///
/// 命中已保存决策的选项 `desc` 标记为 `current`（渲染为 ✓）。
pub fn project_trust_dialog_content(app: &App) -> (String, Vec<PanelItem>, usize) {
    let cwd = app.cwd.clone();
    let cwd_norm = normalize_cwd(Path::new(&cwd));
    let saved = project_trust::trust_decision(Path::new(&cwd), &agent_dir());

    let saved_text = match &saved {
        None => "none".to_string(),
        Some((path, decision)) => {
            let label = if *decision { "trusted" } else { "untrusted" };
            if *path == cwd_norm {
                format!("{label} ({path})")
            } else {
                format!("{label} (inherited from {path})")
            }
        }
    };

    let session = if app.reload_ctx.trusted {
        "trusted"
    } else {
        "untrusted"
    };

    let title =
        format!("Project trust\n{cwd}\n\nSaved decision: {saved_text}\nCurrent session: {session}");

    // 已保存决策命中的选项打 `current` 标记（渲染为 ✓）
    let is_current = |saved_path: &str, trusted: bool| {
        saved
            .as_ref()
            .is_some_and(|(p, d)| p == saved_path && *d == trusted)
    };

    let mk = |label: String, value: &str, saved_path: Option<(&str, bool)>| PanelItem {
        label,
        value: value.to_string(),
        desc: match saved_path {
            Some((p, t)) if is_current(p, t) => "current".to_string(),
            _ => String::new(),
        },
        name: String::new(),
        ..Default::default()
    };

    let parent_norm = Path::new(&cwd)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(normalize_cwd);

    let mut items = vec![mk(
        "Trust".to_string(),
        "trust",
        Some((cwd_norm.as_str(), true)),
    )];

    if let Some(parent) = &parent_norm {
        items.push(mk(
            format!("Trust parent folder ({parent})"),
            "trust_parent",
            Some((parent.as_str(), true)),
        ));
    }

    items.push(mk(
        "Do not trust".to_string(),
        "distrust",
        Some((cwd_norm.as_str(), false)),
    ));

    // 清空 trust.json 全部内容（所有路径决策）：另弹 Yes/No 确认面板，不直接执行
    items.push(mk(
        "Remove all saved trust decisions".to_string(),
        "clear",
        None,
    ));

    // 初始选中已保存的选项（无匹配则第一项 Trust）
    let selected = items.iter().position(|i| i.desc == "current").unwrap_or(0);
    (title, items, selected)
}

/// 信任面板底部提示：`/trust` 对话框 Enter 保存决策（重启后生效）；
/// 启动选择器（含仅本次选项）Enter 直接选择。
pub(crate) fn project_trust_hint(layer: &PanelLayer) -> String {
    match layer.kind {
        // `/trust`：Enter 保存决策（重启后生效）
        PanelKind::ProjectTrustDialog => {
            format!("{DEF_ARROW_UP_DOWN} navigate  enter save  escape/ctrl+c cancel")
        }
        _ => format!("{DEF_ARROW_UP_DOWN} navigate  enter select  escape/ctrl+c cancel"),
    }
}

/// 信任面板内容行：标题块 + 选项列表 + 底部提示（上下留白由 `panel.rs` 统一补）。
#[allow(clippy::too_many_arguments)]
pub fn project_trust_lines(
    app: &App,
    layer: &PanelLayer,
    width: usize,
    warning: Style,
    muted: Style,
    accent: Style,
) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(2).max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(""));

    // 标题块：首行 accent，cwd/说明 muted；显式空行保留
    for (i, raw) in layer.title.split('\n').enumerate() {
        if raw.is_empty() {
            lines.push(Line::from(""));
            continue;
        }
        let style = if i == 0 { accent } else { muted };
        for wl in wrap_words(raw, inner) {
            let (t, _) = truncate_display(&wl, inner);
            lines.push(Line::from(Span::styled(format!(" {}", t), style)));
        }
    }
    lines.push(Line::from(""));

    let filtered = app.panel.filtered_items();
    let sel = layer.selected.min(filtered.len().saturating_sub(1));
    // `/trust` 对话框有 ✓ 当前列；启动选择器只有箭头列（对齐 pi 两种组件）
    let show_marker = layer.kind == PanelKind::ProjectTrustDialog;
    for (i, item) in filtered.iter().enumerate() {
        let selected = i == sel;
        // `desc == "current"`：该选项与已保存决策一致（`/trust` 面板渲染 ✓）
        let is_current = item.desc == "current";
        let prefix = if selected { DEF_CURSOR } else { "  " };
        let marker = if is_current {
            format!("{DEF_DONE} ")
        } else {
            "  ".to_string()
        };
        let label_style = if selected { accent } else { Style::default() };
        let prefix_style = if selected { accent } else { Style::default() };
        let mut spans = vec![
            Span::raw(" "),
            Span::styled(prefix.to_string(), prefix_style),
        ];

        if show_marker {
            spans.push(Span::styled(marker.to_string(), accent));
        }

        spans.push(Span::styled(item.label.clone(), label_style));
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" {}", panel_hint(layer)),
        warning,
    )));
    lines.push(Line::from(""));
    lines
}

#[cfg(test)]
mod tests {
    use super::super::super::app::App;
    use super::super::super::panel::PanelKind;
    use super::super::panel::{panel_height, render_panel_selector};
    use ratatui::layout::Rect;

    #[test]
    fn project_trust_panel_renders_pi_style() {
        // 对齐 pi 启动选择器（ExtensionSelectorComponent）：标题块 + 5 个选项
        // （Trust / parent / session / Do not trust / session-only）+ 底部导航提示，上下各一条动态边框。
        let mut app = App::new();
        app.cwd = "/home/blue/wayshot-fork/xxx".to_string();
        app.open_project_trust_panel();

        assert!(app.ask_trust, "应置位信任门控（首个消息在决定前不派发）");
        assert_eq!(
            app.panel.top().map(|l| l.kind),
            Some(PanelKind::ProjectTrust)
        );

        let w = 80u16;
        let h = panel_height(&app, w as usize);
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_panel_selector(f, Rect::new(0, 0, w, h), &app))
            .unwrap();
        let buf = terminal.backend().buffer();
        let row_text = |y: u16| -> String {
            (0..w)
                .map(|x| {
                    buf.cell((x, y))
                        .map(|c| c.symbol().to_string())
                        .unwrap_or_default()
                })
                .collect()
        };
        let all: String = (0..h).map(row_text).collect::<Vec<_>>().join("\n");

        assert!(all.contains("Trust project folder?"), "标题: {all}");
        assert!(all.contains("/home/blue/wayshot-fork/xxx"), "cwd: {all}");
        assert!(
            all.contains("This allows prux to load .prux settings"),
            "说明: {all}"
        );
        assert!(all.contains("→ Trust"), "默认选中 Trust: {all}");
        assert!(
            all.contains("Trust parent folder (/home/blue/wayshot-fork)"),
            "父目录选项: {all}"
        );
        assert!(all.contains("Trust (this session only)"));
        assert!(all.contains("Do not trust"));
        assert!(all.contains("Do not trust (this session only)"));
        assert!(
            all.contains("↑↓ navigate  enter select  escape/ctrl+c cancel"),
            "底部提示: {all}"
        );
        // 上下动态边框（DynamicBorder）横跨整个宽度
        assert!(row_text(0).starts_with('─'), "上边框: {:?}", row_text(0));
        assert!(
            row_text(h - 1).starts_with('─'),
            "下边框: {:?}",
            row_text(h - 1)
        );
    }

    #[test]
    fn project_trust_panel_without_parent_omits_parent_option() {
        // 根目录无父目录：不展示 Trust parent folder 选项
        let mut app = App::new();
        app.cwd = "/".to_string();
        app.open_project_trust_panel();
        let labels: Vec<String> = app
            .panel
            .filtered_items()
            .iter()
            .map(|i| i.label.clone())
            .collect();
        assert_eq!(labels.len(), 4, "根目录选项: {labels:?}");
        assert!(!labels.iter().any(|l| l.starts_with("Trust parent folder")));
    }

    #[test]
    fn project_trust_dialog_renders_saved_and_current() {
        // 对齐 pi TrustSelectorComponent：`/trust` 面板展示已保存决策/当前会话，
        // 3 个选项（无仅本次），当前决策打 ✓，提示为 enter save。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("pi");
        std::fs::create_dir_all(&cwd).unwrap();
        let agent_dir = crate::core::settings_manager::agent_dir();
        crate::core::project_trust::set_project_trust(&cwd, &agent_dir, false);

        let mut app = App::new();
        app.cwd = cwd.to_string_lossy().to_string();
        app.open_project_trust_dialog();

        assert_eq!(
            app.panel.top().map(|l| l.kind),
            Some(PanelKind::ProjectTrustDialog)
        );
        let items = app.panel.filtered_items();
        assert_eq!(
            items.len(),
            4,
            "无仅本次选项: {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
        let distrust = items
            .iter()
            .find(|i| i.label == "Do not trust")
            .expect("应有 Do not trust");
        assert_eq!(distrust.desc, "current", "已保存项应标记 current");
        assert_eq!(
            items.last().unwrap().label,
            "Remove all saved trust decisions",
            "末项为清空 trust.json 的选项"
        );
        assert_eq!(items.last().unwrap().desc, "", "清空选项不带 current 标记");
        // 初始选中 = 已保存的 Do not trust
        assert_eq!(app.panel.top().unwrap().selected, 2);

        let w = 90u16;
        let h = panel_height(&app, w as usize);
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_panel_selector(f, Rect::new(0, 0, w, h), &app))
            .unwrap();
        let buf = terminal.backend().buffer();
        let row_text = |y: u16| -> String {
            (0..w)
                .map(|x| {
                    buf.cell((x, y))
                        .map(|c| c.symbol().to_string())
                        .unwrap_or_default()
                })
                .collect()
        };
        let all: String = (0..h).map(row_text).collect::<Vec<_>>().join("\n");

        assert!(all.contains("Project trust"), "标题: {all}");
        assert!(
            all.contains("Saved decision: untrusted"),
            "已保存决策: {all}"
        );
        assert!(
            all.contains("Current session: untrusted"),
            "当前会话: {all}"
        );
        assert!(all.contains("→ ✓ Do not trust"), "选中+当前标记: {all}");
        assert!(
            all.contains("Remove all saved trust decisions"),
            "清空选项: {all}"
        );
        assert!(
            all.contains("↑↓ navigate  enter save  escape/ctrl+c cancel"),
            "提示: {all}"
        );
        assert!(row_text(0).starts_with('─'));
        assert!(row_text(h - 1).starts_with('─'));
    }
}
