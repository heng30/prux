//! 选择器渲染（/model /session /theme /login /extension …），显示在输入框区域
//!
//! # 统一布局（所有 `/` 面板共用）
//!
//! 内容上下各留 **恰好 1 行** 空行，再包上下两条动态分割线：
//!
//! ```text
//! ──（上分割线，border 色）
//!    空行
//!    内容（各面板自定：提示 / 搜索 / 列表 / 描述 / 页码 / 键位提示 …）
//!    空行
//! ──（下分割线）
//! ```
//!
//! 高度与绘制**同源**：[`panel_height`] 与 [`render_panel_selector`] 都从
//! [`panel_content`] 推导（`内容行数 + 4`），由 [`render_panel_lines`] 统一补留白，
//! 因此不会出现某个面板底部多出空行、另一个面板内容贴住下分割线的漂移。

use super::{
    super::{
        app::{App, display_width, truncate_display},
        panel::PanelKind,
    },
    hyperlink, import, trust,
};
use crate::{
    core::{self, keybindings},
    modes::interactive::panel::{PanelItem, PanelLayer},
    utils::{
        display::wrap_words,
        glyphs::{DEF_ARROW_UP_DOWN, DEF_CURSOR, DEF_DONE},
    },
};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

/// 面板列表最大展示行数
pub const MAX_PANEL_LINES: usize = 10;

/// 选择器总高度（含上下分割线与内容上下各 1 行空行）。
///
/// 与 [`render_panel_selector`] 同源（都来自 [`panel_content`]）：`内容行数 + 4`。
pub fn panel_height(app: &App, width: usize) -> u16 {
    let Some(layer) = app.panel.top() else {
        return 0;
    };
    trim_panel_blanks(panel_content(app, layer, width)).len() as u16 + 4
}

/// 面板内容行（可能自带首尾空行；[`render_panel_lines`] 会统一裁掉再补留白）。
///
/// 这里是**唯一**的内容来源：布局高度与绘制都调用它，二者不可能再漂移。
fn panel_content(app: &App, layer: &PanelLayer, width: usize) -> Vec<Line<'static>> {
    let warning = app.theme.style("warning", "#ffff00");
    let muted = app.theme.style("muted", "#808080");
    let accent = app.theme.style("accent", "#8abeb7");
    let success = app.theme.style("success", "#b5bd68");

    match layer.kind {
        PanelKind::ExtensionDetail => {
            extension_detail_lines(app, layer, width, warning, muted, accent, success)
        }
        PanelKind::SkillDetail => {
            skill_detail_lines(app, layer, width, warning, muted, accent, success)
        }
        PanelKind::Skill => skill_lines(app, layer, width, warning, muted, accent, success),
        PanelKind::LoginOauth => login_oauth_lines(app, layer, width, warning, muted, accent),
        PanelKind::LoginKey => login_key_lines(app, layer, width, warning, muted, accent),
        PanelKind::Thinking => thinking_lines(app, layer, width, warning, muted, accent, success),
        PanelKind::ScopedModels => scoped_models_lines(app, layer, width, muted, accent, success),
        PanelKind::Extension => extension_lines(app, layer, width, muted, accent, success),
        PanelKind::ImportConfirm => {
            import::import_confirm_lines(app, layer, width, warning, muted, accent)
        }
        PanelKind::ImportCwdConfirm => {
            import::import_cwd_confirm_lines(app, layer, width, warning, muted, accent)
        }
        PanelKind::SettingsOverwrite => {
            let detail = app
                .pending_settings_overwrite
                .as_ref()
                .map(|p| p.detail.clone())
                .unwrap_or_default();
            detail_confirm_lines(
                app,
                layer,
                width,
                warning,
                muted,
                accent,
                &detail,
                "Overwriting keeps recoverable settings; unrecoverable content will be lost.",
            )
        }
        PanelKind::HistoryClearConfirm => {
            let detail = app
                .pending_history_clear
                .as_ref()
                .map(|p| p.detail.clone())
                .unwrap_or_default();
            detail_confirm_lines(
                app,
                layer,
                width,
                warning,
                muted,
                accent,
                &detail,
                "Deleting prompt history cannot be undone.",
            )
        }
        PanelKind::ProjectTrust | PanelKind::ProjectTrustDialog => {
            trust::project_trust_lines(app, layer, width, warning, muted, accent)
        }
        PanelKind::TrustClearConfirm => detail_confirm_lines(
            app,
            layer,
            width,
            warning,
            muted,
            accent,
            &trust_clear_detail(),
            "Removing saved trust decisions cannot be undone.",
        ),
        PanelKind::BugReportHint
        | PanelKind::BugReportTranscript
        | PanelKind::BugReportSummary
        | PanelKind::BugReportDelivery => {
            bug_report_lines(app, layer, width, warning, muted, accent)
        }
        // 通用选择面板（model / session / theme / login 认证 / 自定义 …）
        _ => generic_lines(app, layer, width, warning, muted, accent, success),
    }
}

/// 渲染选择器（panel.active 时调用，占据输入框区域）
pub fn render_panel_selector(frame: &mut Frame, area: Rect, app: &App) {
    let Some(layer) = app.panel.top() else { return };
    let width = area.width as usize;
    let border = app.theme.style("border", "#5f87ff");
    let content = panel_content(app, layer, width);
    render_panel_lines(frame, area, width, border, content);
}

/// 统一绘制面板：内容首尾空行裁掉后，上下各补 **恰好 1 行** 空行，再包上下分割线。
///
/// 所有 `/` 面板（选择器 / 会话选择 / 设置 / 扩展设置）共用，保证样式一致。
pub(crate) fn render_panel_lines(
    frame: &mut Frame,
    area: Rect,
    width: usize,
    border: Style,
    content: Vec<Line<'static>>,
) {
    let content = trim_panel_blanks(content);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(content.len() + 2);
    lines.push(Line::from(""));
    lines.extend(content);
    lines.push(Line::from(""));
    render_clipped_lines(frame, area, width, border, lines);
}

/// 裁掉内容首尾的空行（统一留白交给 [`render_panel_lines`]，避免底部多出空行）。
pub(crate) fn trim_panel_blanks(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let is_blank = |l: &Line<'static>| l.spans.iter().all(|s| s.content.trim().is_empty());
    let start = lines
        .iter()
        .position(|l| !is_blank(l))
        .unwrap_or(lines.len());
    let end = lines
        .iter()
        .rposition(|l| !is_blank(l))
        .map(|i| i + 1)
        .unwrap_or(start);
    lines[start..end].to_vec()
}

/// /login API key 输入面板（对齐 pi LoginDialogComponent）：
/// 标题 / `Enter {provider} API key` 提示 / `> ` 输入行 / 底部取消提示
#[allow(clippy::too_many_arguments)]
fn login_key_lines(
    app: &App,
    layer: &PanelLayer,
    width: usize,
    warning: Style,
    muted: Style,
    accent: Style,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" {}", layer.title),
        accent,
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" Enter {} API key", app.login_provider_name),
        muted,
    )));
    let (view, _) = layer.filter.visible_window(width.saturating_sub(4));
    let (f, _) = truncate_display(&view, width.saturating_sub(4));
    lines.push(Line::from(vec![
        Span::styled("> ", Style::default()),
        Span::styled(f, Style::default()),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " (escape/ctrl+c to cancel, enter to submit)",
        warning,
    )));
    lines.push(Line::from(""));
    lines
}

/// 通用选择面板内容（model / session / theme / login 认证 / extension / 自定义）：
/// 提示 + 搜索输入行 + 滚动列表 + 页码 + Model 底部信息 + Model 底部键位提示
#[allow(clippy::too_many_arguments)]
fn generic_lines(
    app: &App,
    layer: &PanelLayer,
    width: usize,
    warning: Style,
    muted: Style,
    accent: Style,
    success: Style,
) -> Vec<Line<'static>> {
    let filtered = app.panel.filtered_items();
    let total = filtered.len();

    let hint = panel_hint(layer);

    // 操作提示：login 面板放底部，其他面板保持顶部
    let hint_at_bottom = matches!(
        layer.kind,
        PanelKind::LoginAuthType
            | PanelKind::LoginMethod
            | PanelKind::LoginProvider
            | PanelKind::LogoutProvider
            | PanelKind::ExtensionDetail
    );

    // 无搜索输入行的面板：认证方式 / 扩展详情 / 会话修复 / 僵尸 op 提示 / 确认框 / 自定选择
    // （与事件处理的 [`PanelKind::has_filter_input`] 共用同一判定，避免名单漂移）
    let show_filter = layer.kind.has_filter_input();

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(""));
    if !hint_at_bottom {
        for l in hint_wrapped_lines(&hint, width) {
            lines.push(Line::from(Span::styled(l, warning)));
        }
        lines.push(Line::from(""));
    }

    // 搜索输入行（Input：`> ` 前缀 + filter，水平滚动窗口跟随光标）
    if show_filter {
        let (view, _) = layer.filter.visible_window(width.saturating_sub(4));
        let (f, _) = truncate_display(&view, width.saturating_sub(4));
        lines.push(Line::from(vec![
            Span::styled("> ", Style::default()),
            Span::styled(f, Style::default()),
        ]));
        lines.push(Line::from(""));
    }

    // 列表（滚动居中），sel/start/end 供页码与 Model 底部信息使用
    let (sel, start, end) = append_list_rows(
        &mut lines, &filtered, layer, app, total, width, accent, muted, success,
    );

    // 页码（仅滚动时）
    if start > 0 || end < total {
        let page = format!("  ({}/{})", sel + 1, total);
        let (page, _) = truncate_display(&page, width.saturating_sub(2));
        lines.push(Line::from(Span::styled(page, muted)));
    }

    // Model Name 行 + 刷新状态行（model 面板）
    if layer.kind == PanelKind::Model && total > 0 {
        append_model_footer(&mut lines, app, &filtered, sel, total, muted, success);
    }
    lines.push(Line::from(""));

    // login 面板：操作提示放列表底部
    if hint_at_bottom {
        for l in hint_wrapped_lines(&hint, width) {
            lines.push(Line::from(Span::styled(l, warning)));
        }
    }

    // /model 面板：底部快捷键提示（列表顶部已显示 provider 警告，键位提示放底部）
    if layer.kind == PanelKind::Model {
        lines.push(Line::from(Span::styled(
            "  Enter to select · Ctrl+S to set as default · Escape/Ctrl+C to cancel",
            warning,
        )));
    }

    lines
}

/// /thinking 选择面板（对齐 pi ThinkingSelectorComponent）：
/// 标题 / 副标题（Shift+Tab cycles thinking levels in-session）/ 搜索行 `> ` /
/// 列表（label 列宽对齐 + desc + 当前级别 ✓ 标记）/ 底部操作提示。
#[allow(clippy::too_many_arguments)]
#[allow(clippy::vec_init_then_push)]
fn thinking_lines(
    app: &App,
    layer: &PanelLayer,
    width: usize,
    warning: Style,
    muted: Style,
    accent: Style,
    success: Style,
) -> Vec<Line<'static>> {
    let filtered = app.panel.filtered_items();
    let total = filtered.len();

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("Thinking Level", accent)));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Shift+Tab cycles thinking levels in-session",
        muted,
    )));
    lines.push(Line::from(""));

    // 搜索输入行（Input：`> ` 前缀 + filter，水平滚动窗口跟随光标）
    let (view, _) = layer.filter.visible_window(width.saturating_sub(4));
    let (f, _) = truncate_display(&view, width.saturating_sub(4));
    lines.push(Line::from(vec![
        Span::styled("> ", Style::default()),
        Span::styled(f, Style::default()),
    ]));
    lines.push(Line::from(""));

    // 列表（滚动居中；label 列宽对齐：clamp [12,32]，对齐 pi SelectList 主列宽）
    let max_display = total.min(MAX_PANEL_LINES);
    let sel = layer.selected.min(total.saturating_sub(1));
    let start = if total > 0 {
        sel.saturating_sub(max_display / 2)
            .min(total.saturating_sub(max_display))
    } else {
        0
    };
    let end = (start + max_display).min(total);
    let label_w = filtered
        .iter()
        .map(|it| it.label.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(12, 32);

    for (i, item) in filtered.iter().enumerate().skip(start).take(end - start) {
        let selected = i == sel;
        let prefix = if selected { DEF_CURSOR } else { "  " };
        let label_style = if selected { accent } else { Style::default() };
        let padded = format!("{:<width$}", item.label, width = label_w);

        let mut spans = vec![
            Span::styled(prefix.to_string(), label_style),
            Span::styled(padded, label_style),
        ];

        if !item.desc.is_empty() {
            spans.push(Span::styled(item.desc.clone(), muted));
        }
        // 持久化默认级别标记（仅在默认与当前相同时才可能出现 ✓，顺序为 `· default ✓`）
        if app.default_thinking_level.as_deref() == Some(item.value.as_str()) {
            spans.push(Span::styled(" · default", muted));
        }
        if app.thinking_level.as_deref() == Some(item.value.as_str()) {
            spans.push(Span::styled(format!(" {DEF_DONE}"), success));
        }
        lines.push(Line::from(spans));
    }

    let page = usize::from(start > 0 || end < total);
    if page > 0 {
        let page_text = format!("  ({}/{})", sel + 1, total);
        let (page_text, _) = truncate_display(&page_text, width.saturating_sub(2));
        lines.push(Line::from(Span::styled(page_text, muted)));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Enter to select · Ctrl+S to set as default · Escape/Ctrl+C to cancel",
        warning,
    )));

    lines
}

/// 拆分复选框标签 `[x] name` / `[ ] name` → (复选框, 名称)。
/// 按前缀匹配而非按空格切分：后者会把 `[ ] id` 拆成 `[` 与 `] id`，
/// 导致方括号落到两个不同样式里（未选中时颜色不一致）。
fn split_check_label(label: &str) -> (&str, &str) {
    if let Some(rest) = label.strip_prefix("[x] ") {
        ("[x]", rest)
    } else if let Some(rest) = label.strip_prefix("[ ] ") {
        ("[ ]", rest)
    } else {
        ("", label)
    }
}

/// /extension 底部键位提示（一行放不下由 [`hint_wrapped_lines`] 拆两行）
fn extension_hint() -> String {
    format!(
        "{DEF_ARROW_UP_DOWN} navigate · space toggle · ctrl+a all · ctrl+x clear · enter details · tab mode · esc back"
    )
}

/// /extension 选择面板（布局对齐 /scoped-models）：
/// 标题 `Extension Configuration` / 副标题（当前模式 + Ctrl+S 存盘）/ 搜索行 `> ` /
/// 列表（[x] 名称 + ℹ）/ 底部键位提示（一行放不下拆两行）+ (unsaved)。
#[allow(clippy::too_many_arguments)]
fn extension_lines(
    app: &App,
    layer: &PanelLayer,
    width: usize,
    muted: Style,
    accent: Style,
    success: Style,
) -> Vec<Line<'static>> {
    let filtered = app.panel.filtered_items();
    let total = filtered.len();

    let mut lines: Vec<Line<'static>> = vec![Line::from("")];
    lines.push(Line::from(Span::styled("Extension Configuration", accent)));
    lines.push(Line::from(Span::styled(
        format!(
            "mode: {}  Ctrl+S to save to settings.",
            core::extensions::current_extension_mode().as_str()
        ),
        muted,
    )));
    lines.push(Line::from(""));

    // 搜索输入行
    let (view, _) = layer.filter.visible_window(width.saturating_sub(4));
    let (f, _) = truncate_display(&view, width.saturating_sub(4));
    lines.push(Line::from(vec![
        Span::styled("> ", Style::default()),
        Span::styled(f, Style::default()),
    ]));
    lines.push(Line::from(""));

    // 列表（滚动居中）
    let (sel, start, end) = append_list_rows(
        &mut lines, &filtered, layer, app, total, width, accent, muted, success,
    );

    // 页码（仅滚动时）
    if start > 0 || end < total {
        let page = format!("  ({}/{})", sel + 1, total);
        let (page, _) = truncate_display(&page, width.saturating_sub(2));
        lines.push(Line::from(Span::styled(page, muted)));
    }
    lines.push(Line::from(""));

    // 底部：键位提示 + 启用计数（一行放不下拆两行）+ 未保存标记
    let mut footer = extension_hint();
    footer.push_str(&format!(" · {}", enabled_count_text(&layer.items)));
    if !app.extension_draft.is_empty() {
        footer.push_str(" · (unsaved)");
    }
    for l in hint_wrapped_lines(&footer, width.saturating_sub(2)) {
        lines.push(Line::from(Span::styled(format!("  {l}"), muted)));
    }

    lines
}

/// /scoped-models 选择面板（对齐 pi ScopedModelsSelectorComponent）：
/// 标题 / 副标题（Session-only. Ctrl+S to save）/ 搜索行 `> ` /
/// 列表（[x]/[ ] + id + [provider]，unavailable 删除线）/ Model Name 行 /
/// footer 键位 + 计数（n/m enabled · k unavailable）+ (unsaved)。
#[allow(clippy::too_many_arguments)]
fn scoped_models_lines(
    app: &App,
    layer: &PanelLayer,
    width: usize,
    muted: Style,
    accent: Style,
    success: Style,
) -> Vec<Line<'static>> {
    let filtered = app.panel.filtered_items();
    let total = filtered.len();

    let mut lines: Vec<Line<'static>> = vec![Line::from("")];
    lines.push(Line::from(Span::styled("Model Configuration", accent)));
    lines.push(Line::from(Span::styled(
        "Session-only. Ctrl+S to save to settings.",
        muted,
    )));
    lines.push(Line::from(""));

    // 搜索输入行
    let (view, _) = layer.filter.visible_window(width.saturating_sub(4));
    let (f, _) = truncate_display(&view, width.saturating_sub(4));
    lines.push(Line::from(vec![
        Span::styled("> ", Style::default()),
        Span::styled(f, Style::default()),
    ]));
    lines.push(Line::from(""));

    // 列表（滚动居中）
    let max_display = total.min(MAX_PANEL_LINES);
    let sel = layer.selected.min(total.saturating_sub(1));
    let start = if total > 0 {
        sel.saturating_sub(max_display / 2)
            .min(total.saturating_sub(max_display))
    } else {
        0
    };
    let end = (start + max_display).min(total);

    for (i, item) in filtered.iter().enumerate().skip(start).take(end - start) {
        let selected = i == sel;
        let prefix = if selected { DEF_CURSOR } else { "  " };
        let name_style = if selected { accent } else { Style::default() };
        let (check, name) = split_check_label(&item.label);
        let check_style = if check == "[x]" { success } else { muted };
        let unavailable = item.desc == "[unavailable]";
        let mut spans = vec![
            Span::styled(prefix.to_string(), name_style),
            Span::styled(format!("{check} "), check_style),
        ];
        if unavailable {
            spans.push(Span::styled(
                name.to_string(),
                muted.add_modifier(ratatui::style::Modifier::CROSSED_OUT),
            ));
        } else {
            spans.push(Span::styled(name.to_string(), name_style));
        }
        if !item.desc.is_empty() {
            spans.push(Span::styled(format!(" {}", item.desc), muted));
        }
        // 持久化默认模型标记（禁用条目也显示：默认与是否纳入 Ctrl+P 循环名单无关）
        if item_is_default_model(app, item) {
            spans.push(Span::styled(" · default", muted));
        }
        lines.push(Line::from(spans));
    }

    // Model Name 行（选中可用模型）
    if total > 0 {
        let selected_item = &filtered[sel.min(total - 1)];
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            if selected_item.name.is_empty() {
                "  Model unavailable".to_string()
            } else {
                format!("  Model Name: {}", selected_item.name)
            },
            muted,
        )));
    }

    // 页码（仅滚动时）
    if start > 0 || end < total {
        let page = format!("  ({}/{})", sel + 1, total);
        let (page, _) = truncate_display(&page, width.saturating_sub(2));
        lines.push(Line::from(Span::styled(page, muted)));
    }

    // footer：键位 + 计数 + (unsaved)；一行放不下则在 `·` 处拆两行
    lines.push(Line::from(""));
    let footer = scoped_footer_text(&layer.items, app.model_scope_dirty);
    for l in hint_wrapped_lines(&footer, width.saturating_sub(2)) {
        lines.push(Line::from(Span::styled(format!("  {l}"), muted)));
    }

    lines
}

/// 键位提示排版：一行放得下就一行；放不下则在 `·` 处拆成两行，
/// 拆分点取使左右两行宽度最均衡的分隔符（分隔符留在首行末尾）。
fn hint_wrapped_lines(hint: &str, width: usize) -> Vec<String> {
    let total = display_width(hint);
    if total <= width {
        return vec![hint.to_string()];
    }

    /// 键位提示拆行时依据的分隔符，用于选取左右宽度最均衡的断点。
    const SEP: &str = " · ";
    let half = (total / 2) as isize;
    let split = hint
        .match_indices(SEP)
        .min_by_key(|(i, _)| (display_width(&hint[..*i]) as isize - half).abs());

    match split {
        Some((i, _)) => vec![
            format!("{} ·", &hint[..i]),
            hint[i + SEP.len()..].to_string(),
        ],
        None => vec![hint.to_string()],
    }
}

/// 启用计数文本：n/m enabled · k unavailable（全启用时为 `all enabled`）。
/// /scoped-models 与 /extension 底部共用。
fn enabled_count_text(items: &[PanelItem]) -> String {
    let total = items.iter().filter(|i| !i.value.starts_with('\0')).count();
    let enabled: Vec<&PanelItem> = items
        .iter()
        .filter(|i| i.label.starts_with("[x]"))
        .collect();
    let unavailable = enabled.iter().filter(|i| i.desc == "[unavailable]").count();
    let enabled_available = enabled.len() - unavailable;
    if total > 0 && enabled_available == total {
        "all enabled".to_string()
    } else {
        format!(
            "{enabled_available}/{total} enabled{}",
            if unavailable > 0 {
                format!(" · {unavailable} unavailable")
            } else {
                String::new()
            }
        )
    }
}

/// footer 计数文本：n/m enabled · k unavailable · (unsaved)
fn scoped_footer_text(items: &[PanelItem], dirty: bool) -> String {
    let count_text = enabled_count_text(items);
    let keys =
        "enter toggle · ctrl+a all · ctrl+x clear · ctrl+p provider · alt+↑/↓ reorder · esc back";
    if dirty {
        format!("{keys} · {count_text} · (unsaved)")
    } else {
        format!("{keys} · {count_text}")
    }
}

/// 清空 trust.json 确认面板的详情文本（要清空哪个文件；`\n` 为硬换行，与 `wrap_text` 的硬换行规则一致）。
fn trust_clear_detail() -> String {
    let path = core::project_trust::trust_file_path(&core::settings_manager::agent_dir());
    format!(
        "All saved project trust decisions will be removed from\n{}",
        path.display()
    )
}

/// 「标题 + 详情（折行）+ 不可恢复警告 + 二选项列表 + 提示」的确认面板外壳。
///
/// settings.json 覆写确认与 `/history` 删除确认共用这个形状：两者都是
/// 「先把要发生的后果写清楚，再让用户选 Yes/Overwrite 还是 No/Cancel」。
#[allow(clippy::too_many_arguments)]
fn detail_confirm_lines(
    app: &App,
    layer: &PanelLayer,
    width: usize,
    warning: Style,
    muted: Style,
    accent: Style,
    detail: &str,
    warning_text: &str,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" {}", layer.title),
        accent,
    )));
    lines.push(Line::from(""));
    for wl in wrap_text(detail, width.saturating_sub(4)) {
        let (t, _) = truncate_display(&wl, width.saturating_sub(4));
        lines.push(Line::from(Span::styled(format!(" {}", t), muted)));
    }
    lines.push(Line::from(Span::styled(
        format!(" {}", warning_text),
        warning,
    )));
    lines.push(Line::from(""));

    let filtered = app.panel.filtered_items();
    let sel = layer.selected.min(filtered.len().saturating_sub(1));
    for (i, item) in filtered.iter().enumerate() {
        let selected = i == sel;
        let prefix = if selected { DEF_CURSOR } else { "  " };
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

/// `/bug` 面板：标题 / 说明（免责或当前选择汇总）/ （描述面板的）输入行 /
/// 选项列表 / 底部键位提示。
fn bug_report_lines(
    app: &App,
    layer: &PanelLayer,
    width: usize,
    warning: Style,
    muted: Style,
    accent: Style,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" {}", layer.title),
        accent,
    )));
    lines.push(Line::from(""));

    for note in bug_report_note_lines(app, width) {
        let (text, _) = truncate_display(&note, width.saturating_sub(4));
        lines.push(Line::from(Span::styled(format!(" {text}"), muted)));
    }

    // 描述面板：filter 行即描述输入框
    if layer.kind == PanelKind::BugReportHint {
        lines.push(Line::from(""));
        let (view, _) = layer.filter.visible_window(width.saturating_sub(4));
        let (filter, _) = truncate_display(&view, width.saturating_sub(4));
        lines.push(Line::from(vec![
            Span::styled("> ", Style::default()),
            Span::styled(filter, Style::default()),
        ]));
    }

    let filtered = app.panel.filtered_items();
    let sel = layer.selected.min(filtered.len().saturating_sub(1));
    if !filtered.is_empty() {
        lines.push(Line::from(""));
    }

    for (i, item) in filtered.iter().enumerate() {
        let selected = i == sel;
        let prefix = if selected { DEF_CURSOR } else { "  " };
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

/// 面板操作提示文案（按面板类型）
pub(crate) fn panel_hint(layer: &PanelLayer) -> String {
    match layer.kind {
        PanelKind::Model => {
            "Only showing models from configured providers. Use /login to add providers.".to_string()
        }
        PanelKind::Session => "Session history.".to_string(),
        PanelKind::Theme => "Select a theme.".to_string(),
        PanelKind::Thinking => {
            format!("{DEF_ARROW_UP_DOWN} navigate  type to filter  enter select  escape/ctrl+c cancel")
        }
        PanelKind::ScopedModels => {
            format!(
                "{DEF_ARROW_UP_DOWN} navigate · enter toggle · ctrl+a all · ctrl+x clear · ctrl+p provider · alt+↑/↓ reorder · esc back"
            )
        }
        PanelKind::LoginKey => "Enter API key (enter to save, escape/ctrl+c cancel)".to_string(),
        PanelKind::LoginOauth => {
            "Paste the authorization code / redirect URL with ctrl+shift+v (enter to submit, escape/ctrl+c cancel)"
                .to_string()
        }
        // /extension：模式与保存方式在标题下副标题行，键位提示在底部（见 extension_lines）
        PanelKind::Extension => extension_hint(),
        PanelKind::ExtensionDetail => "escape/ctrl+c back".to_string(),
        PanelKind::Skill => skill_hint(),
        PanelKind::SkillDetail => "escape/ctrl+c back".to_string(),
        PanelKind::ProjectTrust | PanelKind::ProjectTrustDialog => trust::project_trust_hint(layer),
        PanelKind::Custom(_) | PanelKind::SessionRepair | PanelKind::ZombieOperations
            | PanelKind::ImportConfirm | PanelKind::ImportCwdConfirm | PanelKind::SettingsOverwrite
            | PanelKind::HistoryClearConfirm
            | PanelKind::TrustClearConfirm
            | PanelKind::LoginAuthType
            | PanelKind::LoginMethod
            | PanelKind::LoginProvider | PanelKind::LogoutProvider
            => format!("{DEF_ARROW_UP_DOWN} navigate  enter select  escape/ctrl+c cancel"),
        PanelKind::BugReportHint => "enter continue  escape/ctrl+c cancel".to_string(),
        PanelKind::BugReportTranscript
        | PanelKind::BugReportSummary
        | PanelKind::BugReportDelivery => {
            format!("{DEF_ARROW_UP_DOWN} navigate  enter select  escape/ctrl+c cancel")
        }
    }
}

/// 追加列表行（滚动居中 + 选中高亮 + 扩展 checkbox / 状态标签）；
/// 返回 (选中索引, 起始行, 结束行) 供页码与 Model 底部信息使用
#[allow(clippy::too_many_arguments)]
fn append_list_rows(
    lines: &mut Vec<Line<'static>>,
    filtered: &[&PanelItem],
    layer: &PanelLayer,
    app: &App,
    total: usize,
    width: usize,
    accent: Style,
    muted: Style,
    success: Style,
) -> (usize, usize, usize) {
    let (start, end) = list_window(total, layer.selected, MAX_PANEL_LINES);
    let sel = layer.selected.min(total.saturating_sub(1));

    for (i, item) in filtered
        .iter()
        .enumerate()
        .skip(start)
        .take(end.saturating_sub(start))
    {
        let selected = i == sel;
        let prefix = if selected { DEF_CURSOR } else { "  " };
        let is_current = row_is_current(layer, item, app);
        let is_default = layer.kind == PanelKind::Model && item_is_default_model(app, item);

        // 逐项展示色（扩展 Select 声明的 fg）：有色的行始终穿该色（选中与否都保留状态色，
        // 选中由 `→` 前缀标识）；无 fg 的行维持旧行为（选中 = accent）。
        let name_style = match &item.fg {
            Some(spec) => Style::default().fg(app.theme.resolve_color(spec)),
            None if selected => accent,
            None => Style::default(),
        };
        let mut spans = Vec::new();

        if let PanelKind::Custom(_) = layer.kind {
            // 扩展自定义面板：条目文本很长（如 `/agents types` 的`类型名 — 描述 [来源] · 工具`），
            // 按宽度折行完整显示；续行缩进 2 列，与首行文本（去掉 `→ `/`  ` 前缀）对齐；
            // 折行条目之间空一行，否则多行描述贴在一起太密集
            let chunks = custom_row_lines(&item.label, width);
            for (k, text) in chunks.iter().enumerate() {
                let indent = if k == 0 { prefix } else { "  " };
                lines.push(Line::from(Span::styled(
                    format!("{}{}", indent, text),
                    name_style,
                )));
            }

            if custom_gap_after(i - start, end - start, chunks.len()) {
                lines.push(Line::from(""));
            }
            continue;
        }

        if matches!(layer.kind, PanelKind::Extension | PanelKind::Skill) {
            // checkbox（label 前缀 `[x]`/`[ ]`）+ 名称 + 注记（desc）
            let (check, name) = split_check_label(&item.label);
            let check_style = if check == "[x]" { success } else { muted };
            let name_style = if selected { accent } else { Style::default() };
            spans.push(Span::styled(format!("{}{}", prefix, check), check_style));
            spans.push(Span::styled(format!(" {}", name), name_style));
            if !item.desc.is_empty() {
                spans.push(Span::styled(format!("   {}", item.desc), muted));
            }
        } else {
            spans.push(Span::styled(
                format!("{}{}", prefix, item.label),
                name_style,
            ));

            if !item.desc.is_empty() {
                let gap = if layer.kind == PanelKind::Model { 1 } else { 3 };
                // login 面板的状态标签：`✓ stored` 用 success 色，`• not configured` 用 muted
                let desc_style = if item.desc.starts_with('✓') {
                    success
                } else {
                    muted
                };
                spans.push(Span::styled(
                    format!("{}{}", " ".repeat(gap), item.desc),
                    desc_style,
                ));
            }
        }

        if is_default {
            spans.push(Span::styled(" · default", muted));
        }
        if is_current {
            spans.push(Span::styled(format!(" {DEF_DONE}"), success));
        }
        lines.push(Line::from(spans));
    }
    (sel, start, end)
}

/// Model 面板底部：Model Name 行 + 刷新状态行（状态非空时）
fn append_model_footer(
    lines: &mut Vec<Line<'static>>,
    app: &App,
    filtered: &[&PanelItem],
    sel: usize,
    total: usize,
    muted: Style,
    success: Style,
) {
    lines.push(Line::from(""));
    let item = &filtered[sel.min(total - 1)];
    lines.push(Line::from(Span::styled(
        format!("  Model Name: {}", item.name),
        muted,
    )));

    //根据刷新状态显示不同提示
    if !app.refresh_status_message.is_empty() {
        let status_style = if app.refresh_status_success {
            success
        } else {
            muted
        };
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  {}", app.refresh_status_message),
            status_style,
        )));
    }
}

/// 通用列表的滚动窗口 `[start, end)`（选中项居中）：
/// `panel_height` 与 [`append_list_rows`] 共用，保证布局与绘制不漂移。
fn list_window(total: usize, selected: usize, max_display: usize) -> (usize, usize) {
    if total == 0 {
        return (0, 0);
    }
    let max_display = max_display.min(total);
    let sel = selected.min(total - 1);
    let start = sel
        .saturating_sub(max_display / 2)
        .min(total.saturating_sub(max_display));
    (start, (start + max_display).min(total))
}

/// 扩展自定义选择面板条目的折行：按内容宽度（去掉 2 列前缀）按词折行；
/// 续行由渲染侧缩进 2 列与首行文本对齐。
fn custom_row_lines(label: &str, width: usize) -> Vec<String> {
    wrap_words(label, width.saturating_sub(2).max(1))
}

/// 折行条目之后是否空一行（`/agents types` 这类多行描述贴在一起太密集）；
/// 末项之后不加，避免列表与页码/提示之间出现双空行。
fn custom_gap_after(idx: usize, visible: usize, row_lines: usize) -> bool {
    row_lines > 1 && idx + 1 < visible
}

/// 条目是否为持久化默认模型（value 编码 provider\0model_id，与 App 缓存匹配）
fn item_is_default_model(app: &App, item: &PanelItem) -> bool {
    item.value
        .split_once('\0')
        .map(|(provider, model_id)| {
            app.default_provider.as_deref() == Some(provider)
                && app.default_model.as_deref() == Some(model_id)
        })
        .unwrap_or(false)
}

/// 当前项是否勾选 ✓：Model 按 provider+model 完整匹配，Session 按路径，其余无勾选
fn row_is_current(layer: &PanelLayer, item: &PanelItem, app: &App) -> bool {
    match layer.kind {
        PanelKind::Model => {
            // value 编码为 provider\0model_id：按 provider+model 完整匹配，避免不同 provider 的同名模型全部勾选
            item.value
                .split_once('\0')
                .map(|(provider, model_id)| {
                    app.current_provider.as_deref() == Some(provider)
                        && app.current_model.as_deref() == Some(model_id)
                })
                .unwrap_or(false)
        }
        PanelKind::Session => app.current_session_path.as_deref() == Some(item.value.as_str()),
        PanelKind::Thinking => app.thinking_level.as_deref() == Some(item.value.as_str()),
        PanelKind::Theme => app.theme.name == item.value,
        PanelKind::LoginAuthType
        | PanelKind::LoginMethod
        | PanelKind::LoginProvider
        | PanelKind::LoginKey
        | PanelKind::LoginOauth
        | PanelKind::LogoutProvider => false,
        PanelKind::Extension | PanelKind::ExtensionDetail | PanelKind::Custom(_) => false,
        PanelKind::Skill | PanelKind::SkillDetail => false,
        PanelKind::ScopedModels => false,
        PanelKind::SessionRepair
        | PanelKind::ZombieOperations
        | PanelKind::ImportConfirm
        | PanelKind::ImportCwdConfirm
        | PanelKind::SettingsOverwrite
        | PanelKind::HistoryClearConfirm
        | PanelKind::ProjectTrust
        | PanelKind::ProjectTrustDialog
        | PanelKind::TrustClearConfirm => false,
        PanelKind::BugReportHint
        | PanelKind::BugReportTranscript
        | PanelKind::BugReportSummary
        | PanelKind::BugReportDelivery => false,
    }
}

/// 按宽度换行：把长文本切成不超过 width 的行（ASCII/等宽场景）。
/// crate 内共享（panel / settings_selector 描述换行用）。
pub(crate) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }

    // 显式换行（`\n`）是硬换行：调用方（如 `/history @clear` 的确认面板）用它把
    // 「计数摘要」与「文件路径」分成两块。旧的逐字符折行把 `\n` 当普通字符吞进同一行，
    // 渲染时两块会粘连（`3417 entries…/home/…`），所以这里先按 `\n` 切段再各自折行。
    let mut out = Vec::new();
    for logical in text.split('\n') {
        let mut current = String::new();
        for c in logical.chars() {
            if display_width(&current) + display_width(&c.to_string()) > width {
                out.push(std::mem::take(&mut current));
            }
            current.push(c);
        }
        out.push(current);
    }
    out
}

/// OAuth 授权 URL 在面板中的展示行数（渲染与鼠标命中共用同一公式）
pub(crate) fn oauth_url_lines(url: &str, width: usize) -> usize {
    wrap_text(url, width.saturating_sub(4)).len().max(1)
}

/// `/bug` 说明文本按面板宽度折行的结果（渲染与光标定位共用同一公式，避免行数漂移）。
fn bug_report_note_lines(app: &App, width: usize) -> Vec<String> {
    wrap_text(&app.bug_report_note(), width.saturating_sub(4))
}

/// `/bug` 面板说明文本折行后的行数（渲染与光标定位共用同一公式，避免漂移）。
/// 说明在标题下方、输入行上方，故描述面板的输入行位置随它的行数波动。
pub(crate) fn bug_report_note_line_count(app: &App, width: usize) -> usize {
    bug_report_note_lines(app, width).len()
}

/// 面板里的一行链接文本（授权 URL / device-code 验证 URI 的其中一段折行）。
///
/// 首尾各放一个哨兵（[`hyperlink::LINK_START`] / [`hyperlink::LINK_END`]），由
/// [`hyperlink::apply`] 在 buffer 定型时换成 OSC8 转义序列；每个折行段独立开闭链接，
/// 因此长 URL 换行后每一行都可点击。
///
/// 两个哨兵之间的前导空格也落在链接范围内（无害），这样 URL 前面与改造前一样只占 1 列。
fn link_line(chunk: &str, style: Style) -> Line<'static> {
    Line::from(Span::styled(
        format!("{} {chunk}{}", hyperlink::LINK_START, hyperlink::LINK_END),
        style,
    ))
}

/// /login OAuth 登录面板
/// 授权码流与复制授权码流显示完整授权 URL + 手动粘贴输入行；
/// device-code 流显示 user_code + verification_uri。
fn login_oauth_lines(
    app: &App,
    layer: &PanelLayer,
    width: usize,
    warning: Style,
    muted: Style,
    accent: Style,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(" {}", layer.title),
        accent,
    )));
    lines.push(Line::from(""));
    let awaiting = app.oauth_login.as_ref().is_some_and(|o| o.awaiting_input());

    // AwaitInput 步骤（copilot 企业域名）：标题 → 提问 → 占位示例 → 输入行
    // 独立于 device code 初始化（该阶段 user_code 尚未生成）。
    if let Some(prompt) = app.oauth_login.as_ref().and_then(|o| o.prompt_text()) {
        return login_oauth_prompt_lines(layer, app, width, warning, muted, accent, &prompt);
    }

    // device-code 流：展示用户码与验证 URI
    // 初始化中（user_code 尚未生成）：显示等待文案，无输入行（面板更矮，收到 code 后撑高）
    if app.oauth_login.as_ref().is_some_and(|o| o.is_device_code()) && !awaiting {
        if let Some((user_code, verification_uri)) =
            app.oauth_login.as_ref().and_then(|o| o.device_code_info())
        {
            return login_oauth_device_code_lines(
                lines,
                width,
                warning,
                muted,
                accent,
                user_code,
                verification_uri,
            );
        }

        // 初始化中（设备授权请求尚未返回）：等待文案，无输入行
        return login_oauth_initializing_lines(lines, width, warning, muted);
    }

    // 授权 URL：完整展示（自动换行），可 Ctrl+click 打开
    login_oauth_authorization_url_lines(lines, app, layer, width, warning, muted, accent)
}

/// AwaitInput 步骤（copilot 企业域名）：标题 → 提问 → 占位示例 → 输入行
#[allow(clippy::too_many_arguments)]
fn login_oauth_prompt_lines(
    layer: &PanelLayer,
    app: &App,
    width: usize,
    warning: Style,
    muted: Style,
    accent: Style,
    prompt: &str,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(" {}", layer.title),
        accent,
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(format!(" {}", prompt), muted)));

    let placeholder = app
        .oauth_login
        .as_ref()
        .map(|o| o.prompt_placeholder())
        .unwrap_or_default();

    if !placeholder.is_empty() {
        lines.push(Line::from(Span::styled(format!(" {}", placeholder), muted)));
    }
    let (view, _) = layer.filter.visible_window(width.saturating_sub(4));
    let (f, _) = truncate_display(&view, width.saturating_sub(4));

    lines.push(Line::from(vec![
        Span::styled("> ", Style::default()),
        Span::styled(f, Style::default()),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " (escape/ctrl+c to cancel, enter to submit)",
        warning,
    )));
    lines
}

/// device-code 流：展示用户码与验证 URI（标题行由调用方先绘制）
#[allow(clippy::too_many_arguments)]
fn login_oauth_device_code_lines(
    mut lines: Vec<Line<'static>>,
    width: usize,
    warning: Style,
    muted: Style,
    accent: Style,
    user_code: String,
    verification_uri: String,
) -> Vec<Line<'static>> {
    lines.push(Line::from(vec![
        Span::styled(" Code: ", muted),
        Span::styled(user_code, accent),
    ]));
    lines.push(Line::from(""));
    for chunk in wrap_text(&verification_uri, width.saturating_sub(4)) {
        lines.push(link_line(&chunk, accent));
    }
    lines.push(Line::from(Span::styled(" Ctrl+click to open", muted)));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Enter the code above at the URL in your browser, then return here.",
        muted,
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Waiting for authorization... (escape/ctrl+c to cancel)",
        warning,
    )));
    lines.push(Line::from(""));
    lines
}

/// device-code 初始化中（设备授权请求尚未返回）：等待文案，无输入行
fn login_oauth_initializing_lines(
    mut lines: Vec<Line<'static>>,
    _width: usize,
    warning: Style,
    muted: Style,
) -> Vec<Line<'static>> {
    lines.push(Line::from(Span::styled(" Requesting device code.", muted)));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " (escape/ctrl+c to cancel)",
        warning,
    )));
    lines.push(Line::from(""));
    lines
}

/// 授权 URL 流程：完整 URL（自动换行）+ Ctrl+click 提示 + 提供商说明 + 手动粘贴输入行。
/// 浏览器登录与复制授权码登录共用布局，只有说明与输入行文案不同。
#[allow(clippy::too_many_arguments)]
fn login_oauth_authorization_url_lines(
    mut lines: Vec<Line<'static>>,
    app: &App,
    layer: &PanelLayer,
    width: usize,
    warning: Style,
    muted: Style,
    accent: Style,
) -> Vec<Line<'static>> {
    if let Some(oauth) = app.oauth_login.as_ref() {
        for chunk in wrap_text(&oauth.auth_url(), width.saturating_sub(4)) {
            lines.push(link_line(&chunk, accent));
        }
    }

    // 提示行带上复制键位（按实际绑定显示；整条 URL 的每一行都能 Ctrl+click 打开）
    let click_hint = if cfg!(target_os = "macos") {
        "Cmd+click to open"
    } else {
        "Ctrl+click to open"
    };
    let copy_hint = match keybindings::key_display("app.message.copy") {
        Some(key) => format!("{key} to copy"),
        None => "to copy".to_string(),
    };
    lines.push(Line::from(Span::styled(
        format!(" {click_hint} • {copy_hint}"),
        muted,
    )));
    lines.push(Line::from(""));

    // 说明（复制授权码流粘的是页面上的 code#state，anthropic 浏览器流用 Complete login，其余 sign-in）
    let provider_id = app
        .oauth_login
        .as_ref()
        .map(|o| o.provider_id.as_str())
        .unwrap_or("");
    let copy_code = app.oauth_login.as_ref().is_some_and(|o| o.is_copy_code());

    let (instruction, input_label) = if copy_code {
        (
            "Approve access in your browser, then paste the code shown on the page (looks like code#state).",
            " Paste the code shown on the page (code#state):",
        )
    } else if provider_id == "anthropic" {
        (
            "Complete login in your browser. If the browser is on another machine, paste the final redirect URL here.",
            " Complete login in your browser, or paste the authorization code / redirect URL here:",
        )
    } else {
        (
            "Complete sign-in in your browser. If the browser is on another machine, paste the final redirect URL here.",
            " Complete sign-in in your browser, or paste the authorization code / redirect URL here:",
        )
    };

    // 完整一句：终端宽度不足时自动折行
    for chunk in wrap_text(instruction, width.saturating_sub(4)) {
        lines.push(Line::from(Span::styled(format!(" {}", chunk), muted)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(input_label, muted)));

    let (view, _) = layer.filter.visible_window(width.saturating_sub(4));
    let (f, _) = truncate_display(&view, width.saturating_sub(4));
    lines.push(Line::from(vec![
        Span::styled("> ", Style::default()),
        Span::styled(f, Style::default()),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " (escape/ctrl+c to cancel)",
        warning,
    )));
    lines.push(Line::from(""));
    lines
}

/// /skills 底部键位提示（一行放不下由 [`hint_wrapped_lines`] 拆两行）
fn skill_hint() -> String {
    format!("{DEF_ARROW_UP_DOWN} navigate · space toggle · ctrl+s save · enter details · esc back")
}

/// /skills 选择面板（布局对齐 /extension）：标题 / 副标题（Ctrl+S 存盘口径 + 错误行）/
/// 搜索行 `> ` / 列表（[x] 名称 + 注记）/ 底部键位提示 + 启用计数 + (unsaved)。
#[allow(clippy::too_many_arguments)]
fn skill_lines(
    app: &App,
    layer: &PanelLayer,
    width: usize,
    warning: Style,
    muted: Style,
    accent: Style,
    success: Style,
) -> Vec<Line<'static>> {
    let filtered = app.panel.filtered_items();
    let total = filtered.len();

    let mut lines: Vec<Line<'static>> = vec![Line::from("")];
    lines.push(Line::from(Span::styled("Skills Configuration", accent)));
    lines.push(Line::from(Span::styled(
        "Ctrl+S to save to settings.json; enable installs bundled skills into agent_dir/skills.",
        muted,
    )));

    if let Some(err) = &app.skill_panel_error {
        for l in hint_wrapped_lines(err, width.saturating_sub(2)) {
            lines.push(Line::from(Span::styled(format!("  {l}"), warning)));
        }
    }
    lines.push(Line::from(""));

    // 搜索输入行
    let (view, _) = layer.filter.visible_window(width.saturating_sub(4));
    let (f, _) = truncate_display(&view, width.saturating_sub(4));
    lines.push(Line::from(vec![
        Span::styled("> ", Style::default()),
        Span::styled(f, Style::default()),
    ]));
    lines.push(Line::from(""));

    // 列表（滚动居中；`[x]` 勾选态在 append_list_rows 里按 Skill/Extension 统一处理）
    let (sel, start, end) = append_list_rows(
        &mut lines, &filtered, layer, app, total, width, accent, muted, success,
    );

    if start > 0 || end < total {
        let page = format!("  ({}/{})", sel + 1, total);
        let (page, _) = truncate_display(&page, width.saturating_sub(2));
        lines.push(Line::from(Span::styled(page, muted)));
    }
    lines.push(Line::from(""));

    let mut footer = skill_hint();
    footer.push_str(&format!(" · {}", enabled_count_text(&layer.items)));
    if !app.skill_draft.is_empty() {
        footer.push_str(" · (unsaved)");
    }

    for l in hint_wrapped_lines(&footer, width.saturating_sub(2)) {
        lines.push(Line::from(Span::styled(format!("  {l}"), muted)));
    }

    lines
}

/// /skills 面板搜索输入行上方的错误行数（无错误时为 0）。
///
/// 错误行插在副标题与搜索行之间且按宽度折行，光标行号（见 `render::render_cursor`）
/// 必须减去同一个数，否则会把光标留在错误行上。
pub(crate) fn skills_error_line_count(app: &App, width: usize) -> usize {
    app.skill_panel_error
        .as_ref()
        .map(|e| hint_wrapped_lines(e, width.saturating_sub(2)).len())
        .unwrap_or(0)
}

/// /skills 二级菜单：SKILL.md 头信息 + 来源 + 路径（按宽度折行完整展示）。
#[allow(clippy::too_many_arguments)]
fn skill_detail_lines(
    app: &App,
    layer: &PanelLayer,
    width: usize,
    warning: Style,
    muted: Style,
    accent: Style,
    success: Style,
) -> Vec<Line<'static>> {
    let enabled = app.skill_state(&layer.title);
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ".to_string(), Style::default()),
        Span::styled(layer.title.clone(), accent),
        Span::styled(
            if enabled {
                "  [enabled]"
            } else {
                "  [disabled]"
            }
            .to_string(),
            if enabled { success } else { muted },
        ),
    ]));
    lines.push(Line::from(""));

    for l in &app.skill_detail {
        for chunk in wrap_text(l, width.saturating_sub(4)) {
            lines.push(Line::from(Span::styled(chunk, Style::default())));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("  escape/ctrl+c back", warning)));
    lines.push(Line::from(""));
    lines
}

/// /extension 二级菜单：扩展描述信息（对齐 /model 面板边框与配色）
/// 行布局：标题+状态徽标 / 空 / 描述行 / 空行 / fork URL（accent 色 + Ctrl+click 提示）/ 空 / 操作提示
#[allow(clippy::too_many_arguments)]
fn extension_detail_lines(
    app: &App,
    layer: &PanelLayer,
    width: usize,
    warning: Style,
    muted: Style,
    accent: Style,
    success: Style,
) -> Vec<Line<'static>> {
    let enabled = core::extensions::is_extension_enabled(&layer.title);
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ".to_string(), Style::default()),
        Span::styled(layer.title.clone(), accent),
        Span::styled(
            if enabled {
                "  [enabled]"
            } else {
                "  [disabled]"
            }
            .to_string(),
            if enabled { success } else { muted },
        ),
    ]));
    lines.push(Line::from(""));

    // 描述行按宽度换行；信息完整展示
    let mut has_fork_url = false;
    for l in &app.extension_detail {
        // fork url 行用 accent 色高亮，并追加 Ctrl+click 提示
        if l.starts_with("  fork url: ") {
            has_fork_url = true;
            for chunk in wrap_text(l, width.saturating_sub(4)) {
                lines.push(Line::from(Span::styled(chunk, accent)));
            }
        } else {
            for chunk in wrap_text(l, width.saturating_sub(4)) {
                lines.push(Line::from(Span::styled(chunk, Style::default())));
            }
        }
    }

    let hint_line = format!(
        "  escape/ctrl+c back{}",
        if has_fork_url {
            "  Ctrl+click to open in browser"
        } else {
            ""
        }
    );

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(hint_line, warning)));
    lines.push(Line::from(""));
    lines
}

/// 按宽度截断/填充行并渲染（选择器与详情共用；输入已含上下各 1 行留白）
pub(crate) fn render_clipped_lines(
    frame: &mut Frame,
    area: Rect,
    width: usize,
    border: Style,
    lines: Vec<Line<'static>>,
) {
    let mut final_lines: Vec<Line<'static>> = Vec::new();
    for line in lines.into_iter().take(area.height as usize) {
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

    let para = Paragraph::new(final_lines)
        .block(
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(border),
        )
        .style(Style::default());
    frame.render_widget(para, area);
}
/// 测试专用：按当前面板层渲染出纯文本行（与 `render_panel` 同一套布局规则），
/// 供断言直接比对；没有打开的面板层时返回空 Vec。
#[cfg(test)]
pub fn render_panel_text(app: &App, width: usize) -> Vec<String> {
    let Some(layer) = app.panel.top() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let filtered = app.panel.filtered_items();
    let total = filtered.len();
    let (start, end) = list_window(total, layer.selected, MAX_PANEL_LINES);
    for (i, item) in filtered.iter().enumerate().skip(start).take(end - start) {
        let selected = i == layer.selected;
        let prefix = if selected { DEF_CURSOR } else { "  " };
        let is_current = match layer.kind {
            PanelKind::Model => item
                .value
                .split_once('\0')
                .map(|(provider, model_id)| {
                    app.current_provider.as_deref() == Some(provider)
                        && app.current_model.as_deref() == Some(model_id)
                })
                .unwrap_or(false),
            PanelKind::Session => app.current_session_path.as_deref() == Some(item.value.as_str()),
            PanelKind::Thinking => app.thinking_level.as_deref() == Some(item.value.as_str()),
            PanelKind::Theme => app.theme.name == item.value,
            PanelKind::LoginAuthType
            | PanelKind::LoginMethod
            | PanelKind::LoginProvider
            | PanelKind::LoginKey
            | PanelKind::LoginOauth
            | PanelKind::LogoutProvider => false,
            PanelKind::Extension | PanelKind::ExtensionDetail | PanelKind::Custom(_) => false,
            PanelKind::Skill | PanelKind::SkillDetail => false,
            PanelKind::ScopedModels => false,
            PanelKind::SessionRepair
            | PanelKind::ZombieOperations
            | PanelKind::ImportConfirm
            | PanelKind::ImportCwdConfirm
            | PanelKind::SettingsOverwrite
            | PanelKind::HistoryClearConfirm
            | PanelKind::ProjectTrust
            | PanelKind::ProjectTrustDialog
            | PanelKind::TrustClearConfirm => false,
            PanelKind::BugReportHint
            | PanelKind::BugReportTranscript
            | PanelKind::BugReportSummary
            | PanelKind::BugReportDelivery => false,
        };
        let check = if is_current {
            format!(" {DEF_DONE}")
        } else {
            String::new()
        };
        // 持久化默认标记：Model/ScopedModels 按 provider+model，Thinking 按级别字符串
        let is_default = match layer.kind {
            PanelKind::Model | PanelKind::ScopedModels => item_is_default_model(app, item),
            PanelKind::Thinking => {
                app.default_thinking_level.as_deref() == Some(item.value.as_str())
            }
            _ => false,
        };
        let default_badge = if is_default { " · default" } else { "" };
        if let PanelKind::Custom(_) = layer.kind {
            // 与渲染一致：条目文本按宽度折行，续行缩进 2 列，折行条目间空一行
            let chunks = custom_row_lines(&item.label, width);
            for (k, text) in chunks.iter().enumerate() {
                let indent = if k == 0 { prefix } else { "  " };
                out.push(format!("{}{}", indent, text));
            }
            if custom_gap_after(i - start, end - start, chunks.len()) {
                out.push(String::new());
            }
            continue;
        }
        if layer.kind == PanelKind::Extension {
            // 一行：checkbox + 名称 + ℹ（无前缀箭头简化为空格）
            let (cb, name) = split_check_label(&item.label);
            out.push(format!("{}{} {} {}", prefix, cb, name, item.desc));
        } else if layer.kind == PanelKind::ScopedModels {
            // 一行：[x]/[ ] + 名称 + [provider]/[unavailable] + `· default`
            let (cb, name) = split_check_label(&item.label);
            let suffix = if item.desc == "[unavailable]" {
                " [unavailable]".to_string()
            } else {
                format!(" {}", item.desc)
            };
            out.push(format!(
                "{}{} {}{}{}",
                prefix, cb, name, suffix, default_badge
            ));
        } else if layer.kind == PanelKind::Model {
            out.push(format!(
                "{}{}{}{} {}",
                prefix, item.label, default_badge, check, item.desc
            ));
        } else if layer.kind == PanelKind::Thinking {
            // label 列宽 clamp [12,32]，desc 紧贴列宽后，默认/当前级别追加 ` · default` ` ✓`
            let label_w = filtered
                .iter()
                .map(|it| it.label.chars().count())
                .max()
                .unwrap_or(0)
                .clamp(12, 32);
            out.push(format!(
                "{}{:<width$}{}{}{}",
                prefix,
                item.label,
                item.desc,
                default_badge,
                check,
                width = label_w
            ));
        } else if layer.kind == PanelKind::Theme {
            out.push(format!("{}{}{}", prefix, item.label, check));
        } else if item.desc.is_empty() {
            out.push(format!("{}{}", prefix, item.label));
        } else {
            out.push(format!("{}{}   {}", prefix, item.label, item.desc));
        }
    }
    if total > MAX_PANEL_LINES {
        out.push(format!("  ({}/{})", layer.selected + 1, total));
    }
    if layer.kind == PanelKind::Model && total > 0 {
        out.push(format!("  Model Name: {}", filtered[layer.selected].name));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::app::App;
    use crate::modes::interactive::panel::{PanelItem, PanelKind};

    #[test]
    fn wrap_text_honors_explicit_newlines() {
        // 显式 `\n` 是硬换行：不能把它当普通字符吞进同一行
        assert_eq!(
            wrap_text("a\nb", 80),
            vec!["a".to_string(), "b".to_string()]
        );
        // 每段仍按宽度软折行
        assert_eq!(wrap_text("abcd\nef", 2), vec!["ab", "cd", "ef"]);
        // 空段（连续换行 / 结尾换行）保留为空行
        assert_eq!(wrap_text("a\n\nb", 80), vec!["a", "", "b"]);
        // 无换行时与旧行为一致
        assert_eq!(wrap_text("", 80), vec![String::new()]);
        assert_eq!(wrap_text("abc", 80), vec!["abc".to_string()]);
    }

    #[test]
    fn extension_detail_fork_url_height_has_no_extra_bottom_blank() {
        // 回归：fork url 的 Ctrl+click 提示与底部 escape 提示同一行，
        // panel_height 不得再为它额外 +1（否则面板底部多一行空行）。
        let mut app = App::new();
        app.extension_detail = vec![
            "  desc".to_string(),
            "".to_string(),
            "  forked from: pi-goal v0.1.7".to_string(),
            "  fork url: https://github.com/Michaelliv/pi-goal".to_string(),
            "  mode: dev, creator".to_string(),
            "  type: standard extension".to_string(),
        ];
        app.panel
            .push(PanelKind::ExtensionDetail, "goal".to_string(), Vec::new());
        let w = 60u16;
        let h = panel_height(&app, w as usize);
        // 边框2 + 空1 + 标题1 + 空1 + 详情6 + 空1 + 提示1 + 空1 = 14
        assert_eq!(h, 14, "fork url 面板不应多算一行");

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
        // 紧贴下边框的上一行是按设计留出的空行；提示行在它上面
        let hint_row = h - 3;
        let hint = row_text(hint_row);
        // 提示行与上方详情文本左对齐（详情行均以 2 空格缩进）
        assert!(
            hint.starts_with("  escape/ctrl+c back"),
            "提示行左对齐(2空格): {hint:?}"
        );
        assert!(hint.contains("escape/ctrl+c back"), "提示行: {hint:?}");
        assert!(
            hint.contains("Ctrl+click to open in browser"),
            "fork 提示应与 escape 同行: {hint:?}"
        );
        assert!(
            row_text(h - 2).trim().is_empty(),
            "底部仅保留设计内 1 行空行: {:?}",
            row_text(h - 2)
        );
    }

    /// `/bug` 四个面板：标题 / 说明 / 输入行（仅描述面板）/ 选项 / 键位提示
    #[test]
    fn bug_report_panels_render_note_input_and_options() {
        use crate::modes::interactive::app::PendingBugReport;

        // 1) 描述面板：filter 行即输入框，预填描述
        let mut app = App::new();
        app.pending_bug_report = Some(PendingBugReport {
            hint: "crash on start".to_string(),
            include_session: false,
            include_summary: false,
        });
        app.panel.open(
            PanelKind::BugReportHint,
            "Report a bug — what went wrong? (optional)".to_string(),
            Vec::new(),
        );
        app.panel
            .top_mut()
            .unwrap()
            .filter
            .set_value("crash on start");
        let rows = render_panel_rows(&app, 80);
        let all = rows.join("\n");
        // 说明文字会按面板宽度折行：断言前把连续空白归一化
        let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat(&all).contains("Report a bug"), "{all}");
        assert!(flat(&all).contains("not uploaded anywhere"), "{all}");
        assert!(all.contains("> crash on start"), "预填输入行: {all}");
        assert!(all.contains("enter continue"), "{all}");

        // 2) transcript 面板：说明 + Yes/No
        app.panel.open(
            PanelKind::BugReportTranscript,
            "Include the session transcript?".to_string(),
            vec![
                PanelItem::new("Yes, include the transcript", "yes"),
                PanelItem::new("No", "no"),
            ],
        );
        let all = render_panel_rows(&app, 80).join("\n");
        assert!(
            flat(&all).contains("transcript contains your messages"),
            "{all}"
        );
        assert!(all.contains("Yes, include the transcript"), "{all}");
        assert!(all.contains("→ No") || all.contains("→ Yes"), "{all}");

        // 3) 导出面板：汇总说明（含描述/transcript/摘要三行）
        app.pending_bug_report = Some(PendingBugReport {
            hint: "crash on start".to_string(),
            include_session: false,
            include_summary: true,
        });
        app.panel.open(
            PanelKind::BugReportDelivery,
            "Bug report".to_string(),
            vec![
                PanelItem::new("Export as Zip", "zip"),
                PanelItem::new("Cancel", "cancel"),
            ],
        );
        let all = render_panel_rows(&app, 80).join("\n");
        assert!(flat(&all).contains("Description: crash on start"), "{all}");
        assert!(flat(&all).contains("Transcript: not included"), "{all}");
        assert!(
            flat(&all).contains("Summary: written by the current model"),
            "{all}"
        );
        assert!(
            all.contains("Export as Zip") && all.contains("Cancel"),
            "{all}"
        );
    }

    fn panel_app(kind: PanelKind, items: Vec<(String, String)>) -> App {
        let mut app = App::new();
        let items: Vec<PanelItem> = items
            .into_iter()
            .map(|(label, desc)| PanelItem {
                label: label.clone(),
                // Model 面板的 value 编码为 provider\0model_id
                value: if kind == PanelKind::Model {
                    format!("opencode-go\0{}", label)
                } else {
                    label.clone()
                },
                desc,
                name: format!("{} full name", label),
                ..Default::default()
            })
            .collect();
        app.panel.open(kind, "test".to_string(), items);
        app
    }

    /// 走真实 ratatui 渲染路径，把面板区域按行导出为文本（行首/行尾空白保留）
    fn render_panel_rows(app: &App, width: u16) -> Vec<String> {
        let h = panel_height(app, width as usize);
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(width, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_panel_selector(f, Rect::new(0, 0, width, h), app))
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..h)
            .map(|y| {
                (0..width)
                    .map(|x| {
                        buf.cell((x, y))
                            .map(|c| c.symbol().to_string())
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn model_and_thinking_panels_show_default_key_hint() {
        // 回归：/model 与 /thinking 底部都应有 `Ctrl+S to set as default` 键位提示
        let hint = "Enter to select · Ctrl+S to set as default · Escape/Ctrl+C to cancel";

        let app = panel_app(
            PanelKind::Model,
            vec![("m1".to_string(), "[p]".to_string())],
        );
        let rows = render_panel_rows(&app, 100);
        assert!(
            rows.iter().any(|l| l.contains(hint)),
            "/model 面板缺键位提示: {rows:?}"
        );

        let app = panel_app(
            PanelKind::Thinking,
            vec![("off".to_string(), "No reasoning".to_string())],
        );
        let rows = render_panel_rows(&app, 100);
        assert!(
            rows.iter().any(|l| l.contains(hint)),
            "/thinking 面板缺键位提示: {rows:?}"
        );
    }

    #[test]
    fn model_selector_rows_match_pi_layout() {
        let mut app = panel_app(
            PanelKind::Model,
            vec![
                ("deepseek-flash".to_string(), "[opencode-go]".to_string()),
                ("minimax-m3".to_string(), "[opencode-go]".to_string()),
            ],
        );
        app.current_model = Some("deepseek-flash".to_string());
        app.current_provider = Some("opencode-go".to_string());
        let out = render_panel_text(&app, 80);
        // 选中行：→ 前缀 + 当前模型 ✓ + [provider]
        assert!(out[0].starts_with("→ "), "row0: {:?}", out[0]);
        assert!(
            out[0].contains(" ✓"),
            "current model checkmark: {:?}",
            out[0]
        );
        assert!(
            out[0].contains("[opencode-go]"),
            "provider badge: {:?}",
            out[0]
        );
        assert!(out[1].starts_with("  "), "unselected: {:?}", out[1]);
        // Model Name 行
        assert!(
            out.iter()
                .any(|l| l.contains("Model Name: deepseek-flash full name"))
        );
    }

    #[test]
    fn thinking_selector_rows_match_pi_layout() {
        // 对齐 pi ThinkingSelectorComponent：
        //   →/空格 + label 列宽 12 + desc 紧贴 + 当前级别 ✓ 标记
        let mut app = panel_app(
            PanelKind::Thinking,
            vec![
                ("off".to_string(), "No reasoning".to_string()),
                (
                    "low".to_string(),
                    "Light reasoning (~2k tokens)".to_string(),
                ),
                (
                    "high".to_string(),
                    "Deep reasoning (~16k tokens)".to_string(),
                ),
                ("max".to_string(), "Maximum reasoning".to_string()),
            ],
        );
        app.thinking_level = Some("off".to_string());
        let out = render_panel_text(&app, 80);

        // 选中项带 → 前缀，label 列宽 12，desc 紧贴列宽后；当前思考级别 off 带 ✓ 标记
        assert_eq!(out[0], "→ off         No reasoning ✓", "row0: {:?}", out[0]);
        assert_eq!(
            out[1], "  low         Light reasoning (~2k tokens)",
            "row1: {:?}",
            out[1]
        );
        assert_eq!(
            out[2], "  high        Deep reasoning (~16k tokens)",
            "row2: {:?}",
            out[2]
        );
        assert_eq!(
            out[3], "  max         Maximum reasoning",
            "row3: {:?}",
            out[3]
        );
    }

    #[test]
    fn model_selector_checks_only_current_provider() {
        // 同名模型跨 provider：只有当前 provider 的那一行勾选 ✓
        let mut app = App::new();
        let items: Vec<PanelItem> = vec![
            PanelItem {
                label: "kimi-k2.6".to_string(),
                value: "deepseek\0kimi-k2.6".to_string(),
                desc: "[deepseek]".to_string(),
                name: "DeepSeek Kimi".to_string(),
                ..Default::default()
            },
            PanelItem {
                label: "kimi-k2.6".to_string(),
                value: "opencode\0kimi-k2.6".to_string(),
                desc: "[opencode]".to_string(),
                name: "OpenCode Kimi".to_string(),
                ..Default::default()
            },
        ];
        app.panel
            .open(PanelKind::Model, "/model".to_string(), items);
        app.current_model = Some("kimi-k2.6".to_string());
        app.current_provider = Some("opencode".to_string());

        let out = render_panel_text(&app, 80);
        // 只有 opencode 那行带 ✓
        assert!(
            out.iter()
                .any(|l| l.contains("[opencode]") && l.contains(" ✓")),
            "{:?}",
            out
        );
        assert!(
            !out.iter()
                .any(|l| l.contains("[deepseek]") && l.contains(" ✓")),
            "{:?}",
            out
        );
    }

    #[test]
    fn page_indicator_when_many_models() {
        let items: Vec<(String, String)> = (0..15)
            .map(|i| (format!("model-{}", i), "[opencode-go]".to_string()))
            .collect();
        let app = panel_app(PanelKind::Model, items);
        let out = render_panel_text(&app, 80);
        assert!(out.len() > MAX_PANEL_LINES, "scrollable list");
        assert!(out.iter().any(|l| l.trim_start().starts_with('(')));
    }

    #[test]
    fn default_badge_before_checkmark() {
        // /model：持久化默认模型行标记 ` · default`，且位于 ` ✓` 之前
        let mut app = panel_app(
            PanelKind::Model,
            vec![("deepseek-flash".to_string(), "[opencode-go]".to_string())],
        );
        app.current_model = Some("deepseek-flash".to_string());
        app.current_provider = Some("opencode-go".to_string());
        app.default_provider = Some("opencode-go".to_string());
        app.default_model = Some("deepseek-flash".to_string());
        let out = render_panel_text(&app, 80);
        assert_eq!(
            out[0], "→ deepseek-flash · default ✓ [opencode-go]",
            "默认标记应在 ✓ 之前: {:?}",
            out[0]
        );

        // /thinking：默认级别同标记，且位于 ` ✓` 之前
        let mut app = panel_app(
            PanelKind::Thinking,
            vec![("off".to_string(), "No reasoning".to_string())],
        );
        app.thinking_level = Some("off".to_string());
        app.default_thinking_level = Some("off".to_string());
        let out = render_panel_text(&app, 80);
        assert_eq!(
            out[0], "→ off         No reasoning · default ✓",
            "thinking 默认标记应在 ✓ 之前: {:?}",
            out[0]
        );
    }

    #[test]
    fn scoped_default_badge_on_disabled_row() {
        // /scoped-models：默认模型即使未勾选（[ ]）也带 ` · default`
        let mut app = App::new();
        app.panel.open(
            PanelKind::ScopedModels,
            "Model Configuration".to_string(),
            vec![PanelItem {
                label: "[ ] deepseek-v4-pro".to_string(),
                value: "deepseek\0deepseek-v4-pro".to_string(),
                desc: "[deepseek]".to_string(),
                name: "deepseek-v4-pro full name".to_string(),
                ..Default::default()
            }],
        );
        app.default_provider = Some("deepseek".to_string());
        app.default_model = Some("deepseek-v4-pro".to_string());
        let out = render_panel_text(&app, 80);
        assert_eq!(
            out[0], "→ [ ] deepseek-v4-pro [deepseek] · default",
            "禁用默认行也应带标记: {:?}",
            out[0]
        );
    }

    #[test]
    fn session_rows_have_three_space_gap() {
        let app = panel_app(
            PanelKind::Session,
            vec![("abc".to_string(), "session".to_string())],
        );
        let out = render_panel_text(&app, 80);
        assert!(out[0].contains("abc   session"), "{:?}", out[0]);
    }

    #[test]
    fn extension_rows_show_checkbox_and_info_icon() {
        use crate::modes::interactive::panel::PanelItem;
        let mut app = App::new();
        app.panel.open(
            PanelKind::Extension,
            "/extension".to_string(),
            vec![
                PanelItem {
                    label: "[x] footer(normal)".to_string(),
                    value: "footer(normal)".to_string(),
                    desc: "ℹ".to_string(),
                    name: String::new(),
                    ..Default::default()
                },
                PanelItem {
                    label: "[ ] other-ext".to_string(),
                    value: "other-ext".to_string(),
                    desc: "ℹ".to_string(),
                    name: String::new(),
                    ..Default::default()
                },
            ],
        );
        let out = render_panel_text(&app, 80);
        assert!(out[0].starts_with("→ [x] footer(normal)"), "{:?}", out[0]);
        assert!(out[0].contains("ℹ"), "详情图标: {:?}", out[0]);
        assert!(out[1].starts_with("  [ ] other-ext"), "{:?}", out[1]);
    }

    #[test]
    fn skill_rows_show_checkbox_and_notes() {
        use crate::modes::interactive::panel::PanelItem;
        let mut app = App::new();
        app.panel.open(
            PanelKind::Skill,
            "/skills".to_string(),
            vec![
                PanelItem {
                    label: "[x] grill-me".to_string(),
                    value: "grill-me".to_string(),
                    desc: "[model-off]".to_string(),
                    name: String::new(),
                    ..Default::default()
                },
                PanelItem {
                    label: "[ ] handoff".to_string(),
                    value: "handoff".to_string(),
                    desc: "[not installed] [model-off]".to_string(),
                    name: String::new(),
                    ..Default::default()
                },
            ],
        );
        let out = render_panel_rows(&app, 80).join("\n");
        assert!(out.contains("→ [x] grill-me"), "{out}");
        assert!(out.contains("[model-off]"), "{out}");
        assert!(out.contains("[ ] handoff"), "{out}");
        assert!(out.contains("[not installed]"), "{out}");
        assert!(out.contains("ctrl+s save"), "底部键位提示: {out}");
    }

    #[test]
    fn skill_detail_shows_frontmatter_source_and_back_hint() {
        let mut app = App::new();
        app.skill_detail = vec![
            "  name: grill-me".to_string(),
            "  description: grill the user".to_string(),
            "  disable-model-invocation: true".to_string(),
            "  source: ~/.agents/skills".to_string(),
            "  path: /tmp/skills/grill-me/SKILL.md".to_string(),
        ];
        app.panel
            .push(PanelKind::SkillDetail, "grill-me".to_string(), Vec::new());
        let out = render_panel_rows(&app, 80).join("\n");
        assert!(out.contains("name: grill-me"), "{out}");
        assert!(out.contains("description: grill the user"), "{out}");
        assert!(out.contains("disable-model-invocation: true"), "{out}");
        assert!(out.contains("source: ~/.agents/skills"), "{out}");
        assert!(out.contains("path: /tmp/skills/grill-me/SKILL.md"), "{out}");
        assert!(out.contains("escape/ctrl+c back"), "{out}");
    }

    #[test]
    fn extension_panel_layout_matches_scoped_models() {
        // /extension 面板：标题 `Extension Configuration` / 副标题（mode + Ctrl+S 保存）/
        // 搜索行 / 列表 / 底部键位提示（一行放不下拆两行，中点号分隔）
        let mut app = App::new();
        app.panel.open(
            PanelKind::Extension,
            "/extension".to_string(),
            vec![PanelItem::new(
                "[x] footer(normal)".to_string(),
                "footer(normal)".to_string(),
            )],
        );

        let rows = render_panel_rows(&app, 200);
        let title = rows
            .iter()
            .position(|r| r.contains("Extension Configuration"))
            .unwrap_or_else(|| panic!("缺标题: {rows:?}"));
        assert!(
            rows[title].starts_with("Extension Configuration"),
            "{rows:?}"
        );
        assert!(
            rows[title + 1].starts_with(&format!(
                "mode: {}  Ctrl+S to save to settings.",
                crate::core::extensions::current_extension_mode().as_str()
            )),
            "副标题应为 mode + 保存提示: {:?}",
            rows[title + 1]
        );

        // 键位提示在列表之后（底部），不在标题下方
        let hint_row = rows
            .iter()
            .position(|r| r.contains("ctrl+a all"))
            .unwrap_or_else(|| panic!("缺键位提示: {rows:?}"));
        let list_row = rows
            .iter()
            .position(|r| r.contains("footer(normal)"))
            .unwrap_or_else(|| panic!("缺列表项: {rows:?}"));
        assert!(hint_row > list_row, "提示应在列表底部: {rows:?}");

        let hint = rows[hint_row].trim();
        for key in [
            "navigate",
            "space toggle",
            "ctrl+a all",
            "ctrl+x clear",
            "enter details",
            "tab mode",
            "esc back",
            "all enabled",
        ] {
            assert!(hint.contains(key), "提示缺 {key:?}: {hint:?}");
        }
        assert!(!hint.contains("ctrl+s"), "已移除 ctrl+s 提示: {hint:?}");
        assert!(!hint.contains("escape"), "已移除 escape 提示: {hint:?}");
        assert_eq!(hint.matches('·').count(), 7, "键位用中点号隔开: {hint:?}");
        assert!(!hint.contains("  "), "不应有双空格键位: {hint:?}");

        // 窄终端：提示拆两行，每行不超宽且键位一个不少
        let rows = render_panel_rows(&app, 80);
        let hint_rows: Vec<String> = rows
            .iter()
            .map(|r| r.trim_end().to_string())
            .filter(|r| r.contains("navigate ·") || r.contains("enter details"))
            .collect();
        assert_eq!(hint_rows.len(), 2, "窄终端应两行: {rows:?}");
        assert!(hint_rows[0].ends_with('·'), "{hint_rows:?}");
        for r in &hint_rows {
            assert!(r.starts_with("  "), "提示行需缩进: {r:?}");
            assert!(display_width(r) <= 80, "提示行不应超宽: {r:?}");
        }
        let joined = hint_rows.join(" ");
        for key in [
            "space toggle",
            "ctrl+a all",
            "ctrl+x clear",
            "esc back",
            "all enabled",
        ] {
            assert!(joined.contains(key), "拆两行后键位丢失 {key:?}: {joined:?}");
        }

        // 未保存草稿：底部追加 `· (unsaved)`
        app.extension_draft
            .push(("footer(normal)".to_string(), false));
        let rows = render_panel_rows(&app, 200);
        assert!(
            rows.iter().any(|r| r.contains("(unsaved)")),
            "未保存时应有标记: {rows:?}"
        );
    }

    #[test]
    fn hint_wrapped_lines_splits_only_when_too_wide() {
        let hint = "a · b · cccccccccc · dddddddddd · e";
        assert_eq!(hint_wrapped_lines(hint, 200), vec![hint.to_string()]);

        let two = hint_wrapped_lines(hint, 20);
        assert_eq!(two.len(), 2, "{two:?}");
        assert!(two[0].ends_with('·'), "{two:?}");
        assert_eq!(two.join(" "), hint, "拆行不丢字符: {two:?}");
        assert!(two.iter().all(|l| display_width(l) <= 20), "{two:?}");

        // 无分隔符可拆：保持单行（交给渲染层裁剪）
        assert_eq!(hint_wrapped_lines("abcdefghijklmno", 5).len(), 1);
    }

    #[test]
    fn scoped_models_footer_wraps_into_two_lines_when_narrow() {
        // /scoped-models footer：宽终端一行，窄终端拆两行（计数与键位都不丢）
        let app = panel_app(
            PanelKind::ScopedModels,
            vec![("[x] deepseek-v4-pro".to_string(), "[deepseek]".to_string())],
        );

        let rows = render_panel_rows(&app, 200);
        let footers: Vec<&String> = rows.iter().filter(|r| r.contains("enter toggle")).collect();
        assert_eq!(footers.len(), 1, "宽终端应一行: {rows:?}");
        assert!(
            !footers[0].contains("ctrl+s"),
            "已移除 ctrl+s 提示: {}",
            footers[0]
        );
        assert!(footers[0].contains("enabled"), "{:?}", footers[0]);

        let rows = render_panel_rows(&app, 80);
        let first = rows
            .iter()
            .position(|r| r.contains("enter toggle"))
            .unwrap_or_else(|| panic!("缺 footer 首行: {rows:?}"));
        let second = rows
            .iter()
            .position(|r| r.contains("alt+↑/↓ reorder"))
            .unwrap_or_else(|| panic!("缺 footer 次行: {rows:?}"));
        assert_eq!(second, first + 1, "footer 应连续两行: {rows:?}");
        for r in [&rows[first], &rows[second]] {
            assert!(r.starts_with("  "), "footer 行需保留缩进: {r:?}");
            assert!(display_width(r) <= 80, "footer 行不应超宽: {r:?}");
        }
    }

    #[test]
    fn extension_detail_height_fits_content() {
        let mut app = App::new();
        app.extension_detail = vec!["  line1".to_string(), "  line2".to_string()];
        app.panel.push(
            PanelKind::ExtensionDetail,
            "footer(normal)".to_string(),
            Vec::new(),
        );
        let h = panel_height(&app, 80);
        // 边框2 + 空1 + 标题1 + 空1 + 描述2 + 空1 + 提示1 + 空1
        assert_eq!(h, 10, "detail height: {}", h);
    }

    #[test]
    fn scoped_rows_render_brackets_in_one_style() {
        // 回归：`[ ] name` 曾被按空格切分 → `[` muted、`]` 默认色（同一对括号两种颜色）。
        // 未选中行时 `[` 与 `]` 必须是同一种样式；选中行同理（accent）。
        let mut app = App::new();
        app.panel.open(
            PanelKind::ScopedModels,
            "Model Configuration".to_string(),
            vec![
                PanelItem {
                    label: "[x] deepseek-v4-pro".to_string(),
                    value: "deepseek\0deepseek-v4-pro".to_string(),
                    desc: "[deepseek]".to_string(),
                    name: "deepseek-v4-pro full name".to_string(),
                    ..Default::default()
                },
                PanelItem {
                    label: "[ ] deepseek-flash".to_string(),
                    value: "deepseek\0deepseek-flash".to_string(),
                    desc: "[deepseek]".to_string(),
                    name: "deepseek-flash full name".to_string(),
                    ..Default::default()
                },
            ],
        );
        // 未选中：落在第二行（selected=0，首行选中）
        let w = 60u16;
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
        let y = (0..h)
            .find(|&yy| row_text(yy).contains("[ ]"))
            .unwrap_or_else(|| panic!("未找到未选中条目行"));
        let row = row_text(y);
        // 行末 `[ ]` 前缀：遇到 3 个空格后是 checkbox
        let x = row.find("[ ]").unwrap() as u16;
        let fg_open = buf.cell((x, y)).unwrap().fg;
        let fg_close = buf.cell((x + 2, y)).unwrap().fg;
        assert_eq!(
            fg_open, fg_close,
            "`[ ]` 两个括号颜色应一致: open={fg_open:?} close={fg_close:?}"
        );
        // 勾选行：`[x]` 两个字符也同色（成功色）
        let yx = (0..h)
            .find(|&yy| row_text(yy).contains("[x]"))
            .unwrap_or_else(|| panic!("未找到勾选条目行"));
        let rowx = row_text(yx);
        let xx = rowx.find("[x]").unwrap() as u16;
        assert_eq!(
            buf.cell((xx, yx)).unwrap().fg,
            buf.cell((xx + 1, yx)).unwrap().fg,
            "`[x]` 两个字符颜色应一致"
        );
    }

    #[test]
    fn split_check_label_prefixes() {
        assert_eq!(split_check_label("[x] gpt-4o"), ("[x]", "gpt-4o"));
        assert_eq!(split_check_label("[ ] gpt-4o"), ("[ ]", "gpt-4o"));
        assert_eq!(split_check_label("plain"), ("", "plain"));
        // 名称含空格的条目不应被二次切分
        let (cb, name) = split_check_label("[ ] gpt-4o turbo");
        assert_eq!(cb, "[ ]");
        assert_eq!(name, "gpt-4o turbo");
    }

    #[test]
    fn extension_detail_long_description_wraps_not_truncates() {
        // 回归：长描述按宽度换行、完整显示（曾固定 clamp 8 行 + 横向截断）
        let long = format!("  {}", "word ".repeat(60)); // 约 300 字符
        let mut app = App::new();
        app.extension_detail = vec![long.clone()];
        app.panel.push(
            PanelKind::ExtensionDetail,
            "plan-mode".to_string(),
            Vec::new(),
        );
        // 40 列 → wrap(36) 下 300 字符 ≈ 9 行，面板应被撑高（而非固定 9 行上限截断）
        let h = panel_height(&app, 40);
        assert!(h > 10, "长描述应撑高面板, got {h}");
        assert_eq!(h as usize, 8 + 9, "边框2+内容+描述换行9行");

        // 渲染后每一行描述都不丢（逐行统计 word 出现行数）
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(40, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, h);
                render_panel_selector(f, area, &app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut word_rows = 0usize;
        for y in 0..h {
            let row: String = (0..40u16)
                .map(|x| {
                    buf.cell((x, y))
                        .map(|c| c.symbol().to_string())
                        .unwrap_or_default()
                })
                .collect();
            if row.contains("word") {
                word_rows += 1;
            }
        }
        assert!(word_rows >= 8, "完整换行显示, word 行 {word_rows}");
    }

    #[test]
    fn custom_panel_wraps_long_option_and_grows_height() {
        // `/agents types` 这类扩展自定义面板：选项文本（类型名 — 描述 [来源] · 工具）
        // 过长时按宽度折行完整显示，面板高度随折行行数撑高
        let long = "general-purpose — General-purpose agent for researching complex questions, \
                    searching for code, and executing multi-step tasks. Use it for open-ended \
                    work that benefits from an isolated context window. [global] · read, bash, \
                    edit, write, glob, grep";
        let mut app = App::new();
        app.panel.open(
            PanelKind::Custom(7),
            "Agent types".to_string(),
            vec![
                PanelItem::new("short".to_string(), "short".to_string()),
                PanelItem::new(long.to_string(), long.to_string()),
            ],
        );

        let w = 80usize;
        let chunks = custom_row_lines(long, w);
        assert!(chunks.len() > 1, "长条目应折行: {chunks:?}");
        assert_eq!(chunks.join(" "), long, "折行不丢字: {chunks:?}");
        assert_eq!(
            panel_height(&app, w) as usize,
            7 + chunks.len(),
            "高度 = 提示1+空1+列表(短项1+长项 {} 行)+上下分割线2+上下留白2",
            chunks.len()
        );

        // 渲染：选中行前缀 `→ `，长条目续行缩进 2 列与首行文本对齐
        let out = render_panel_text(&app, w);
        assert_eq!(out.len(), 1 + chunks.len());
        assert!(out[0].starts_with("→ short"), "首行选中: {out:?}");
        let expected: Vec<String> = chunks.iter().map(|t| format!("  {t}")).collect();
        assert_eq!(&out[1..], &expected[..], "长条目每行缩进 2 列: {out:?}");

        // 完整渲染到终端：末行不越宽、不被面板高度裁掉
        use ratatui::backend::TestBackend;
        let h = panel_height(&app, w);
        let backend = TestBackend::new(w as u16, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, w as u16, h);
                render_panel_selector(f, area, &app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..h)
            .map(|y| {
                (0..w as u16)
                    .map(|x| {
                        buf.cell((x, y))
                            .map(|c| c.symbol().to_string())
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .collect();
        let last = chunks.last().unwrap();
        assert!(
            rows.iter().any(|r| r.contains(last.as_str())),
            "末行未被裁掉: {rows:?}"
        );
    }

    #[test]
    fn custom_panel_separates_wrapped_entries_with_blank_row() {
        // `/agents types`：多行描述贴在一起太密集，折行的条目之间空一行
        // （末项之后不加，避免与页码/提示之间双空行）
        let a = "general-purpose — General-purpose agent for researching complex questions, \
                 searching for code, and executing multi-step tasks.";
        let b = "Explore — Fast read-only search agent for locating code. Use it to find files \
                 by pattern, grep for symbols or keywords, or answer questions about the code.";
        let mk = |label: &str| PanelItem::new(label.to_string(), label.to_string());
        let mut app = App::new();
        app.panel.open(
            PanelKind::Custom(11),
            "Agent types".to_string(),
            vec![mk(a), mk(b)],
        );

        let w = 80usize;
        let (ra, rb) = (custom_row_lines(a, w).len(), custom_row_lines(b, w).len());
        assert!(ra > 1 && rb > 1, "两条都应折行: {ra}/{rb}");
        assert_eq!(
            panel_height(&app, w) as usize,
            7 + ra + rb,
            "高度含条目间空行"
        );

        let out = render_panel_text(&app, w);
        assert_eq!(out.len(), ra + rb + 1);
        assert!(out[ra].is_empty(), "首条之后应空一行: {out:?}");
        assert!(!out.last().unwrap().is_empty(), "末项之后不补空行: {out:?}");
    }

    /// 逐项展示色（扩展 Select 的 `fg`）：行文本按声明的主题键着色，
    /// 被选中的行也保留状态色（选中由 `→` 前缀标识）；`"text"` = 终端默认前景。
    #[test]
    fn custom_panel_colors_rows_by_item_fg() {
        use ratatui::style::Color;

        let mut app = App::new();
        app.panel.open(
            PanelKind::Custom(9),
            "Tasks".to_string(),
            vec![
                PanelItem::styled(
                    "task:1  ✔ #1 [completed] A",
                    "task:1  ✔ #1 [completed] A",
                    "success",
                ),
                PanelItem::styled(
                    "task:2  ◼ #2 [in_progress] B",
                    "task:2  ◼ #2 [in_progress] B",
                    "accent",
                ),
                PanelItem::styled(
                    "task:3  ◻ #3 [pending] C",
                    "task:3  ◻ #3 [pending] C",
                    "text",
                ),
                PanelItem::new("plain row", "plain row"),
            ],
        );

        let width = 80u16;
        let h = panel_height(&app, width as usize);
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(width, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_panel_selector(f, Rect::new(0, 0, width, h), &app))
            .unwrap();
        let buf = terminal.backend().buffer();

        // 找到含 needle 的行，返回该处单元格的前景色
        let row_fg = |needle: &str| -> Color {
            for y in 0..h {
                let row: String = (0..width)
                    .map(|x| {
                        buf.cell((x, y))
                            .map(|c| c.symbol().to_string())
                            .unwrap_or_default()
                    })
                    .collect();
                if let Some(x) = row.find(needle) {
                    return buf
                        .cell((x as u16, y))
                        .and_then(|c| c.style().fg)
                        .unwrap_or(Color::Reset);
                }
            }
            panic!("row not found: {needle}");
        };

        // 首行是选中行（`→`）且是 completed：状态色不被选中高亮覆盖
        let success = app.theme.resolve_color("success");
        let accent = app.theme.resolve_color("accent");
        assert_eq!(row_fg("completed"), success);
        assert_eq!(row_fg("in_progress"), accent);
        assert_eq!(row_fg("pending"), Color::Reset, "\"text\" = 终端默认前景");
        assert_eq!(
            row_fg("plain row"),
            Color::Reset,
            "未声明 fg 的行跟默认配色"
        );
        assert_ne!(success, accent, "三个状态颜色应两两不同");
        assert_ne!(accent, Color::Reset, "三个状态颜色应两两不同");
    }

    #[test]
    fn custom_panel_height_counts_scroll_page_row() {
        // 超过 MAX_PANEL_LINES 项时列表可滚动，高度需含页码行（否则末行被裁）
        let mut app = App::new();
        let items: Vec<PanelItem> = (0..MAX_PANEL_LINES + 1)
            .map(|i| PanelItem::new(format!("option-{i}"), format!("option-{i}")))
            .collect();
        app.panel
            .open(PanelKind::Custom(8), "Pick".to_string(), items);
        assert_eq!(
            panel_height(&app, 80) as usize,
            7 + MAX_PANEL_LINES,
            "10 行列表 + 页码"
        );
    }

    #[test]
    fn custom_panel_renders_options_with_hint() {
        // 扩展 UI 请求的通用选择面板：选项 + 导航提示（对齐 pi ui.select）
        let mut app = App::new();
        app.panel.open(
            PanelKind::Custom(42),
            "Plan mode - what next?".to_string(),
            vec![
                PanelItem::new(
                    "Execute the plan (track progress)".to_string(),
                    "Execute the plan (track progress)".to_string(),
                ),
                PanelItem::new(
                    "Stay in plan mode".to_string(),
                    "Stay in plan mode".to_string(),
                ),
                PanelItem::new("Refine the plan".to_string(), "Refine the plan".to_string()),
            ],
        );
        assert_eq!(app.panel.top().unwrap().kind, PanelKind::Custom(42));
        let out = render_panel_text(&app, 80);
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(
            out[0].starts_with("→ Execute the plan"),
            "选中首项: {out:?}"
        );
        assert!(out[1].contains("Stay in plan mode"));
        assert!(out[2].contains("Refine the plan"));
        // hint 文案（render_panel_selector 顶层渲染路径）
        let h = panel_height(&app, 80);
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(80, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, h);
                render_panel_selector(f, area, &app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut found_hint = false;
        for y in 0..h {
            let row: String = (0..80u16)
                .map(|x| {
                    buf.cell((x, y))
                        .map(|c| c.symbol().to_string())
                        .unwrap_or_default()
                })
                .collect();
            if row.contains("enter select") {
                found_hint = true;
            }
        }
        assert!(found_hint, "Custom 面板提示行缺失");

        // Custom 面板无输入框（输入框无用——Enter 确认的是选中项而非输入文本）：
        // 面板高度不含搜索行，渲染不得出现 `> ` 过滤行，光标也不得定位到输入行
        assert_eq!(h, 9, "无搜索行高度: {}", h);
        for y in 0..h {
            let row: String = (0..80u16)
                .map(|x| {
                    buf.cell((x, y))
                        .map(|c| c.symbol().to_string())
                        .unwrap_or_default()
                })
                .collect();
            assert!(
                !row.trim_start().starts_with('>'),
                "Custom 面板不应绘制输入框: {:?}",
                row
            );
        }
    }

    #[test]
    fn settings_overwrite_panel_shows_choices_and_hides_input() {
        // settings.json 损坏覆写确认：标题 + 路径/原因 + Overwrite/Cancel，无输入框
        let mut app = App::new();
        app.pending_settings_overwrite =
            Some(crate::modes::interactive::app::PendingSettingsOverwrite {
                path: "/home/u/.prux/settings.json".to_string(),
                detail: "/home/u/.prux/settings.json is not valid JSON".to_string(),
            });
        app.panel.open(
            PanelKind::SettingsOverwrite,
            "settings.json is invalid".to_string(),
            vec![
                PanelItem::new("Overwrite".to_string(), "overwrite".to_string()),
                PanelItem::new("Cancel".to_string(), "cancel".to_string()),
            ],
        );

        // 高度：边框2 + 空1 + 标题1 + 空1 + 原因1 + 警告1 + 空1 + 列表2 + 空1 + 提示1 + 空1 = 12 + n
        let h = panel_height(&app, 80);
        assert_eq!(h, 13, "无搜索行高度: {h}");

        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(80, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, h);
                render_panel_selector(f, area, &app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..h)
            .map(|y| {
                (0..80u16)
                    .map(|x| {
                        buf.cell((x, y))
                            .map(|c| c.symbol().to_string())
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .collect();
        assert!(
            rows.iter().any(|r| r.contains("settings.json is invalid")),
            "标题缺失: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("is not valid JSON")),
            "原因缺失: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("→ Overwrite")),
            "Overwrite 选项缺失: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("will be lost")),
            "覆写警告缺失: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("  Cancel")),
            "Cancel 选项缺失: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("enter select")),
            "提示行缺失: {rows:?}"
        );
        // 无输入框：不得出现 `> ` 过滤行
        for r in &rows {
            assert!(
                !r.trim_start().starts_with('>'),
                "覆写确认面板不应绘制输入框: {r:?}"
            );
        }
    }

    #[test]
    fn history_clear_confirm_shows_blast_radius_and_hides_input() {
        // /history @clear-all：标题 + 爆炸半径 + 「不可恢复」警告 + Yes/No，无输入框
        let mut app = App::new();
        app.pending_history_clear = Some(crate::modes::interactive::app::PendingHistoryClear {
            all: true,
            detail: "12 files · 3417 entries (every project)\n/home/u/.prux/history".to_string(),
        });
        app.panel.open(
            PanelKind::HistoryClearConfirm,
            "Clear ALL prompt history".to_string(),
            vec![
                PanelItem::new("Yes".to_string(), "yes".to_string()),
                PanelItem::new("No".to_string(), "no".to_string()),
            ],
        );

        // 边框2 + 空1 + 标题1 + 空1 + 详情2（计数 + 路径各一行） + 警告1 + 空1 + 列表2 + 空1 + 提示1 + 空1 = 14
        let h = panel_height(&app, 80);
        assert_eq!(h, 14, "详情显式换行后应占两行: {h}");

        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(80, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, h);
                render_panel_selector(f, area, &app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..h)
            .map(|y| {
                (0..80u16)
                    .map(|x| {
                        buf.cell((x, y))
                            .map(|c| c.symbol().to_string())
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .collect();
        for needle in [
            "Clear ALL prompt history",
            "3417 entries",
            "/home/u/.prux/history",
            "cannot be undone",
            "→ Yes",
            "  No",
            "enter select",
        ] {
            assert!(
                rows.iter().any(|r| r.contains(needle)),
                "缺少 {needle:?}: {rows:?}"
            );
        }
        // 计数与路径之间必须有间隔（显式 `\n`）：不得有任何一行同时包含两者
        assert!(
            !rows
                .iter()
                .any(|r| r.contains("3417 entries") && r.contains("/home/u/.prux/history")),
            "计数与路径必须在不同行: {rows:?}"
        );
        // 无输入框：不得出现 `> ` 过滤行
        for r in &rows {
            assert!(
                !r.trim_start().starts_with('>'),
                "删除确认面板不应绘制输入框: {r:?}"
            );
        }
    }

    #[test]
    fn trust_clear_confirm_renders_file_and_yes_no() {
        // /trust「Remove all saved trust decisions」确认面板：标题 + 将清空的 trust.json 路径
        // + 「不可恢复」警告 + Yes/No，无输入框。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let mut app = App::new();
        app.panel.open(
            PanelKind::TrustClearConfirm,
            "Remove all saved trust decisions?".to_string(),
            vec![
                PanelItem::new("Yes".to_string(), "yes".to_string()),
                PanelItem::new("No".to_string(), "no".to_string()),
            ],
        );

        // 边框2 + 空1 + 标题1 + 空1 + 详情2（说明 + 路径）+ 警告1 + 空1 + 列表2 + 空1 + 提示1 + 空1 ≥ 14
        let h = panel_height(&app, 80);
        assert!(h >= 14, "高度应容纳说明/路径/警告/列表: {h}");

        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(80, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, h);
                render_panel_selector(f, area, &app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..h)
            .map(|y| {
                (0..80u16)
                    .map(|x| {
                        buf.cell((x, y))
                            .map(|c| c.symbol().to_string())
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .collect();
        for needle in [
            "Remove all saved trust decisions?",
            "All saved project trust decisions will be removed from",
            "trust.json",
            "cannot be undone",
            "→ Yes",
            "  No",
            "enter select",
        ] {
            assert!(
                rows.iter().any(|r| r.contains(needle)),
                "缺少 {needle:?}: {rows:?}"
            );
        }
        // 无输入框：不得出现 `> ` 过滤行（Yes/No 不会被字符静默过滤）
        for r in &rows {
            assert!(
                !r.trim_start().starts_with('>'),
                "清空确认面板不应绘制输入框: {r:?}"
            );
        }
    }

    #[test]
    fn login_key_panel_renders_pi_dialog_layout() {
        // 对齐 pi LoginDialogComponent：标题/空行/Enter {provider} API key/输入行/底部提示
        let mut app = App::new();
        app.login_provider_name = "DeepSeek".to_string();
        app.panel.push(
            PanelKind::LoginKey,
            "Login to DeepSeek".to_string(),
            Vec::new(),
        );
        let h = panel_height(&app, 80);
        assert_eq!(h, 10, "边框2 + 内容6 + 上下留白2");
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(60, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 60, h);
                render_panel_selector(f, area, &app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let row = |y: u16| {
            (0..60u16)
                .map(|x| {
                    buf.cell((x, y))
                        .map(|c| c.symbol().to_string())
                        .unwrap_or_default()
                })
                .collect::<String>()
        };
        assert!(row(2).contains("Login to DeepSeek"), "row2: {:?}", row(2));
        assert!(
            row(4).contains("Enter DeepSeek API key"),
            "row4: {:?}",
            row(4)
        );
        assert!(row(5).starts_with('>'), "row5: {:?}", row(5));
        assert!(
            row(7).contains("(escape/ctrl+c to cancel, enter to submit)"),
            "row7: {:?}",
            row(7)
        );
    }

    #[test]
    fn thinking_panel_height_tracks_level_count() {
        // /thinking 面板高度随可用思考级别数量动态变化：
        // 内容 = 标题/空/副标题/空/搜索/空/列表n/空/提示 = 8 + n，总高 = 12 + n。
        // 否则不同模型的不同级别数会导致最后一行操作提示被下分割线裁掉。
        let mut app = App::new();
        // 覆盖全部合法级别（7 个）：高度 = 12 + 7 = 19
        let items: Vec<PanelItem> = crate::cli::args::VALID_THINKING_LEVELS
            .iter()
            .map(|l| PanelItem::new(l.to_string(), l.to_string()))
            .collect();
        app.panel
            .open(PanelKind::Thinking, "/thinking".to_string(), items);
        assert_eq!(panel_height(&app, 80), 19, "7 个级别: 12+7");

        // 4 个级别（如只支持 off/low/high/max 的模型）：高度 = 12 + 4 = 16
        app.panel.close();
        app.panel.open(
            PanelKind::Thinking,
            "/thinking".to_string(),
            vec![
                PanelItem::new("off".to_string(), "off".to_string()),
                PanelItem::new("low".to_string(), "low".to_string()),
                PanelItem::new("high".to_string(), "high".to_string()),
                PanelItem::new("max".to_string(), "max".to_string()),
            ],
        );
        assert_eq!(panel_height(&app, 80), 16, "4 个级别: 12+4");

        // 超过一屏（12 个）：列表 clamp 到 MAX_PANEL_LINES + 页码行 = 12 + 10 + 1 = 23
        app.panel.close();
        let many: Vec<PanelItem> = (0..12)
            .map(|i| PanelItem::new(format!("level-{}", i), format!("level-{}", i)))
            .collect();
        app.panel
            .open(PanelKind::Thinking, "/thinking".to_string(), many);
        assert_eq!(panel_height(&app, 80), 23, "滚动: 12+10+1");
    }

    #[test]
    fn login_oauth_copilot_skips_prompt_and_uses_device_flow() {
        // copilot 登录不再询问企业域名：直接以 Init 起步，首个 tick 发起设备授权。
        // device-code 布局渲染行 = 3共享 + Code1 + 空1 + URL n + Ctrl1 + 空1 + 说明1 + 空1 + 等待1 + 空1；
        // panel_height 必须 ≥ 行数 + 边框2，否则底部等待提示会被裁掉（回归：曾固定返回 12 导致截断）。
        use crate::core::oauth::LoginKind;
        use crate::core::oauth::device_code::DeviceStep;
        let mut app = App::new();
        app.oauth_login = Some(crate::core::oauth::copilot::start_device_flow());
        app.login_provider_id = "github-copilot".to_string();
        app.login_provider_name = "GitHub Copilot".to_string();
        app.panel.push(
            PanelKind::LoginOauth,
            "Login to GitHub Copilot".to_string(),
            Vec::new(),
        );
        {
            let oauth = app.oauth_login.as_ref().unwrap();
            assert!(!oauth.awaiting_input(), "不应再有企业域名提问");
            assert!(oauth.prompt_text().is_none(), "提问文案应移除");
            assert!(oauth.is_device_code());
            assert!(
                oauth.device_code_info().is_none(),
                "初始化中尚无 device code"
            );
        }
        // 初始化中（尚无 device code）：内容 = 标题/空/请求中/空/取消提示 = 5 → 高度 9
        assert_eq!(
            panel_height(&app, 80),
            9,
            "init height: {}",
            panel_height(&app, 80)
        );
        // 模拟设备授权返回：Polling 高度稳定，仍有等待提示空间
        {
            let oauth = app.oauth_login.as_mut().unwrap();
            if let LoginKind::DeviceCode { flow, .. } = &mut oauth.kind {
                flow.user_code = "GH-1234".to_string();
                flow.verification_uri = "https://github.com/login/device".to_string();
                flow.step = DeviceStep::Polling;
            }
        }
        assert_eq!(
            app.oauth_login.as_ref().unwrap().device_code_info(),
            Some((
                "GH-1234".to_string(),
                "https://github.com/login/device".to_string()
            ))
        );
        // 设备授权返回后：内容 = 标题/空/Code/空/URL/提示/空/说明/空/等待 = 10 → 高度 14
        assert_eq!(
            panel_height(&app, 80),
            14,
            "polling height: {}",
            panel_height(&app, 80)
        );
    }
}

#[cfg(test)]
mod layout_invariant_tests {
    //! 回归：所有 `/` 面板的「上/下分割线 ↔ 内容」必须恰好 1 行空行间隔。
    //!
    //! 曾经的漂移：`panel_height` 与各渲染函数各自计数，导致有的面板底部多出 2~4 行空行，
    //! 有的面板提示行直接贴住下分割线。现在高度与绘制同源（[`panel_content`]），
    //! 本测试对每一种面板都断言 `lead == 1 && trail == 1`。

    use super::*;
    use crate::modes::interactive::app::{
        App, PendingHistoryClear, PendingImport, PendingImportCwd, PendingSettingsOverwrite,
    };
    use crate::modes::interactive::panel::{PanelItem, PanelKind};
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    /// 渲染面板并把内容区首尾空行数返回（`(lead, trail)`；均应为 1）。
    fn margins(app: &App, w: u16) -> (usize, usize) {
        let h = panel_height(app, w as usize);
        assert!(h >= 4, "面板至少要有上下分割线 + 上下留白: {h}");
        let backend = TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_panel_selector(f, Rect::new(0, 0, w, h), app))
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
        // 上/下分割线
        assert!(row_text(0).starts_with('─'), "上分割线缺失");
        assert!(row_text(h - 1).starts_with('─'), "下分割线缺失");
        let inner: Vec<String> = (1..h - 1).map(row_text).collect();
        let lead = inner.iter().take_while(|r| r.trim().is_empty()).count();
        let trail = inner
            .iter()
            .rev()
            .take_while(|r| r.trim().is_empty())
            .count();
        (lead, trail)
    }

    fn assert_margins(name: &str, app: &App, w: u16) {
        let (lead, trail) = margins(app, w);
        assert_eq!(lead, 1, "{name}: 顶部应有恰好 1 行空行");
        assert_eq!(trail, 1, "{name}: 底部应有恰好 1 行空行");
    }

    fn items(n: usize) -> Vec<PanelItem> {
        (0..n)
            .map(|i| PanelItem::new(format!("item-{i}"), format!("item-{i}")))
            .collect()
    }

    fn open(kind: PanelKind, n: usize) -> App {
        let mut app = App::new();
        app.panel.open(kind, "T".to_string(), items(n));
        app
    }

    #[test]
    fn every_panel_has_exactly_one_blank_line_around_content() {
        let w = 80u16;

        // 通用选择面板（带搜索行 / 无搜索行 / 提示在底部 / Model 底部信息 / 自定义折行）
        for kind in [
            PanelKind::Theme,
            PanelKind::Model,
            PanelKind::Session,
            PanelKind::LoginAuthType,
            PanelKind::LoginMethod,
            PanelKind::LoginProvider,
            PanelKind::LogoutProvider,
            PanelKind::SessionRepair,
            PanelKind::ZombieOperations,
            PanelKind::TrustClearConfirm,
            PanelKind::Custom(1),
            PanelKind::Extension,
            PanelKind::ScopedModels,
            PanelKind::Thinking,
        ] {
            assert_margins(&format!("{kind:?}"), &open(kind, 3), w);
        }

        // 空列表（过滤后无匹配）也要保持留白一致
        for kind in [
            PanelKind::Theme,
            PanelKind::Model,
            PanelKind::LoginProvider,
            PanelKind::Custom(2),
        ] {
            assert_margins(&format!("{kind:?}(empty)"), &open(kind, 0), w);
        }

        // 滚动列表（页码行）
        assert_margins("Model(scroll)", &open(PanelKind::Model, 15), w);
        assert_margins("Thinking(scroll)", &open(PanelKind::Thinking, 15), w);

        // 折行内容
        {
            let long = "general-purpose — General-purpose agent for researching complex questions, \
                        searching for code, and executing multi-step tasks. [global] · read, bash, \
                        edit, write, glob, grep";
            let mut app = App::new();
            app.panel.open(
                PanelKind::Custom(7),
                "Agent types".to_string(),
                vec![
                    PanelItem::new("short".to_string(), "short".to_string()),
                    PanelItem::new(long.to_string(), long.to_string()),
                ],
            );
            assert_margins("Custom(wrapped)", &app, w);
        }

        // /login API key 输入
        {
            let mut app = App::new();
            app.login_provider_name = "DeepSeek".to_string();
            app.panel
                .push(PanelKind::LoginKey, "Login to DeepSeek".to_string(), vec![]);
            assert_margins("LoginKey", &app, w);
        }

        // /extension 详情（含折行描述）
        {
            let mut app = App::new();
            app.extension_detail = vec![
                "  line1".to_string(),
                "  line2".to_string(),
                "  fork url: https://example.com/very/long/url".to_string(),
            ];
            app.panel
                .push(PanelKind::ExtensionDetail, "ext".to_string(), vec![]);
            assert_margins("ExtensionDetail", &app, w);
        }

        // /import 确认
        {
            let mut app = App::new();
            app.pending_import = Some(PendingImport {
                source: std::path::PathBuf::from("/tmp/some/session.jsonl"),
            });
            app.panel.open(
                PanelKind::ImportConfirm,
                "Import session".to_string(),
                items(2),
            );
            assert_margins("ImportConfirm", &app, w);
        }

        // /import cwd 确认
        {
            let mut app = App::new();
            app.cwd = "/home/u/proj".to_string();
            app.pending_import_cwd = Some(PendingImportCwd {
                destination: std::path::PathBuf::from("/tmp/x.jsonl"),
                session_cwd: "/gone/away".to_string(),
                cwd_missing: true,
            });
            app.panel.open(
                PanelKind::ImportCwdConfirm,
                "Import session".to_string(),
                items(2),
            );
            assert_margins("ImportCwdConfirm", &app, w);
        }

        // settings.json 覆写确认
        {
            let mut app = App::new();
            app.pending_settings_overwrite = Some(PendingSettingsOverwrite {
                path: "/tmp/settings.json".to_string(),
                detail: "not valid JSON".to_string(),
            });
            app.panel.open(
                PanelKind::SettingsOverwrite,
                "settings.json is invalid".to_string(),
                items(2),
            );
            assert_margins("SettingsOverwrite", &app, w);
        }

        // /history 删除确认
        {
            let mut app = App::new();
            app.pending_history_clear = Some(PendingHistoryClear {
                all: true,
                detail: "12 files · 3417 entries\n/home/u/.prux/history".to_string(),
            });
            app.panel.open(
                PanelKind::HistoryClearConfirm,
                "Clear ALL prompt history".to_string(),
                items(2),
            );
            assert_margins("HistoryClearConfirm", &app, w);
        }

        // /trust 面板与清空确认
        {
            let _ad = crate::test_support::AgentDirGuard::temp();
            let mut app = App::new();
            app.cwd = "/home/u/proj".to_string();
            app.open_project_trust_panel();
            assert_margins("ProjectTrust", &app, w);

            let mut app = App::new();
            app.cwd = "/home/u/proj".to_string();
            app.open_project_trust_dialog();
            assert_margins("ProjectTrustDialog", &app, w);
        }
    }

    /// 2.3：复制授权码登录面板——展示 provider 侧回调 URL（不含本地端口），
    /// 并明确要求粘贴页面上的 `code#state`。
    #[test]
    fn login_oauth_copy_code_panel_asks_for_code_state() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let mut app = App::new();
        app.oauth_login = Some(crate::core::oauth::anthropic::start_copy_code_flow().unwrap());
        app.login_provider_name = "Anthropic".to_string();
        app.panel.push(
            PanelKind::LoginOauth,
            "Login to Anthropic".to_string(),
            vec![],
        );

        let layer = app.panel.top().unwrap().clone();
        // 折行处的链接哨兵不属于可见文本（真实渲染会被换成转义序列）。
        let joined = hyperlink::strip(
            &panel_content(&app, &layer, 80)
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
        assert!(
            joined.contains("code#state"),
            "应提示粘贴 code#state: {joined}"
        );
        // URL 会按宽度折行，去空白后再比对（%2F 为 urlencode 后的分隔符）
        let flat: String = joined.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            flat.contains("platform.claude.com%2Foauth%2Fcode%2Fcallback"),
            "应展示 provider 侧回调地址: {joined}"
        );
        assert!(
            !joined.contains("127.0.0.1"),
            "复制授权码流不得展示本地回调地址: {joined}"
        );
    }

    #[test]
    fn login_oauth_url_is_rendered_as_osc8_hyperlink() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let mut app = App::new();
        app.oauth_login = Some(crate::core::oauth::openrouter::start_authorize_flow().unwrap());
        app.login_provider_name = "OpenRouter".to_string();
        app.panel.push(
            PanelKind::LoginOauth,
            "Login to OpenRouter".to_string(),
            vec![],
        );
        let url = app.oauth_login.as_ref().unwrap().auth_url();

        // `apply` 显式传 enabled=true：不依赖测试所在终端的 OSC8 能力探测。
        let w = 80u16;
        let h = panel_height(&app, w as usize);
        let backend = TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render_panel_selector(f, Rect::new(0, 0, w, h), &app);
                hyperlink::apply(f, true, Some(&url), &[]);
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        let start_sequence = format!("\x1b]8;;{url}\x1b\\");
        let mut link_cells = 0usize;
        for y in 0..h {
            for x in 0..w {
                let symbol = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or_default();
                if symbol.contains(&start_sequence) {
                    link_cells += 1;
                }
                assert!(
                    !symbol.contains(hyperlink::LINK_START)
                        && !symbol.contains(hyperlink::LINK_END),
                    "哨兵不得残留到 buffer：{symbol:?}"
                );
            }
        }
        // 折行后每个 URL 行各有一个起始序列 → 长 URL 的每一段都可点。
        assert_eq!(
            link_cells,
            super::oauth_url_lines(&url, w as usize),
            "每个折行段都该带 OSC8 起始序列"
        );
    }

    #[test]
    fn login_oauth_variants_have_exactly_one_blank_line_around_content() {
        use crate::core::oauth::LoginKind;
        use crate::core::oauth::device_code::DeviceStep;
        let w = 80u16;

        // 授权 URL 流（默认）：标题 / URL / Ctrl+click / 说明 / 粘贴输入行 / 取消提示
        {
            let mut app = App::new();
            app.oauth_login = Some(crate::core::oauth::openrouter::start_authorize_flow().unwrap());
            app.login_provider_name = "OpenRouter".to_string();
            app.panel.push(
                PanelKind::LoginOauth,
                "Login to OpenRouter".to_string(),
                vec![],
            );
            assert_margins("LoginOauth(url)", &app, w);
        }

        // device-code 初始化中
        {
            let mut app = App::new();
            app.oauth_login = Some(crate::core::oauth::copilot::start_device_flow());
            app.login_provider_name = "GitHub Copilot".to_string();
            app.panel.push(
                PanelKind::LoginOauth,
                "Login to GitHub Copilot".to_string(),
                vec![],
            );
            assert_margins("LoginOauth(init)", &app, w);
        }

        // device-code 轮询中
        {
            let mut app = App::new();
            app.oauth_login = Some(crate::core::oauth::copilot::start_device_flow());
            app.login_provider_name = "GitHub Copilot".to_string();
            app.panel.push(
                PanelKind::LoginOauth,
                "Login to GitHub Copilot".to_string(),
                vec![],
            );
            if let Some(oauth) = app.oauth_login.as_mut()
                && let LoginKind::DeviceCode { flow, .. } = &mut oauth.kind
            {
                flow.user_code = "GH-1234".to_string();
                flow.verification_uri = "https://github.com/login/device".to_string();
                flow.step = DeviceStep::Polling;
            }
            assert_margins("LoginOauth(poll)", &app, w);
        }
    }
}
