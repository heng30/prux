//! /import 确认面板渲染（对齐 pi ImportCommand 的 ExtensionConfirm）：
//! 标题 `Import session` + `Replace current session with <path>?` + Yes/No 列表。
//!
//! 布局由 `panel.rs` 的 [`super::panel::render_panel_lines`] 统一：内容上下各 1 行空行。
//! 内容顺序：
//!    标题 Import session（accent）
//!    空行
//!    Replace current session with（muted）
//!    路径（按宽度换行，muted）
//!    空行
//!    → Yes /   No（选中 accent）
//!    空行
//!    操作提示（warning）

use super::{
    super::app::{App, truncate_display},
    panel::{panel_hint, wrap_text},
};
use crate::modes::interactive::panel::PanelLayer;
use ratatui::{
    style::Style,
    text::{Line, Span},
};

/// /import 确认面板内容行（高度由 `panel_height` 从同一内容推导）。
pub fn import_confirm_lines(
    app: &App,
    layer: &PanelLayer,
    width: usize,
    warning: Style,
    muted: Style,
    accent: Style,
) -> Vec<Line<'static>> {
    let path = app
        .pending_import
        .as_ref()
        .map(|p| p.source.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" {}", layer.title),
        accent,
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Replace current session with",
        muted,
    )));
    for wl in wrap_text(&path, width.saturating_sub(4)) {
        let (t, _) = truncate_display(&wl, width.saturating_sub(4));
        lines.push(Line::from(Span::styled(format!(" {}", t), muted)));
    }
    lines.push(Line::from(""));

    let filtered = app.panel.filtered_items();
    let sel = layer.selected.min(filtered.len().saturating_sub(1));
    for (i, item) in filtered.iter().enumerate() {
        let selected = i == sel;
        let prefix = if selected { "→ " } else { "  " };
        let style = if selected { accent } else { Style::default() };
        lines.push(Line::from(Span::styled(
            format!("{}{}", prefix, item.label),
            style,
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" {}", panel_hint(layer)),
        warning,
    )));
    lines.push(Line::from(""));
    lines
}

/// /import 会话 cwd 不一致确认面板内容行：
/// 标题 + 说明（缺失 = “cwd from session file does not exist”/
/// 不同 = “session cwd differs from current cwd”）+ 原 cwd + 空行 +
/// “continue in current cwd” + 当前 cwd + 空行 + Yes/No + 提示。
pub fn import_cwd_confirm_lines(
    app: &App,
    layer: &PanelLayer,
    width: usize,
    warning: Style,
    muted: Style,
    accent: Style,
) -> Vec<Line<'static>> {
    let (session_cwd, current_cwd, cwd_missing) = app
        .pending_import_cwd
        .as_ref()
        .map(|p| (p.session_cwd.clone(), app.cwd.clone(), p.cwd_missing))
        .unwrap_or_default();

    // 原 cwd 缺失 → 沿用 pi 文案；存在但不同 → 更准确的表述
    let headline = if cwd_missing {
        " cwd from session file does not exist"
    } else {
        " session cwd differs from current cwd"
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" {}", layer.title),
        accent,
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(headline, muted)));
    for wl in wrap_text(&session_cwd, width.saturating_sub(4)) {
        let (t, _) = truncate_display(&wl, width.saturating_sub(4));
        lines.push(Line::from(Span::styled(format!(" {}", t), muted)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(" continue in current cwd", muted)));
    for wl in wrap_text(&current_cwd, width.saturating_sub(4)) {
        let (t, _) = truncate_display(&wl, width.saturating_sub(4));
        lines.push(Line::from(Span::styled(format!(" {}", t), muted)));
    }
    lines.push(Line::from(""));

    let filtered = app.panel.filtered_items();
    let sel = layer.selected.min(filtered.len().saturating_sub(1));
    for (i, item) in filtered.iter().enumerate() {
        let selected = i == sel;
        let prefix = if selected { "→ " } else { "  " };
        let style = if selected { accent } else { Style::default() };
        lines.push(Line::from(Span::styled(
            format!("{}{}", prefix, item.label),
            style,
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" {}", panel_hint(layer)),
        warning,
    )));
    lines.push(Line::from(""));
    lines
}
