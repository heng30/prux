//! 渲染层：状态栏 / 消息区 / 停靠面板 / 输入框（含选择器与扩展设置面板）/ 底栏。
//!
//! 垂直布局：[消息区(Min) | 待发送区(动态) | 停靠面板(动态) | 状态栏(1) | 输入区(动态：输入框 / 选择器 / 扩展设置面板) | 内联覆盖层(动态) | 底栏(动态)]。
//! 全屏覆盖层（会话查看器）打开时改为：[banner? | 覆盖层(Min) | 分隔线? | 底栏?]。

mod dock;
mod ext_settings;
mod history;
mod hyperlink;
mod import;
mod input;
mod latex;
mod markdown;
mod mermaid;
mod pending;
mod session_selector;
mod settings_selector;
mod status;
mod suggestion;
mod syntax_hl;

pub(crate) mod image;
pub(crate) mod messages;
pub(crate) mod overlay;
pub(crate) mod panel;
pub(crate) mod startup;
pub(crate) mod trust;

use self::history::message_lines;
use super::{
    app::{App, ClickRow, LinkRow, MouseSel},
    panel::PanelKind,
    theme::{Theme, color},
};
use crate::{
    core::extensions::{BannerLine, FooterLine, OverlaySize},
    utils::{glyphs::DEF_SCROLLBAR_THUMB, terminal_caps::hyperlinks_enabled, terminal_image},
};
use image::ImageRect;
use messages::{ClickTarget, ImageSlot, LinkSpan};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect, Size},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use ratatui_image::sliced::{SignedPosition, SlicedImage};
use std::sync::Arc;

pub use markdown::render_markdown;
pub(crate) use panel::{oauth_url_lines, wrap_text};

/// [`collect_visible_lines`] 的结果：可见行 / 内容总行数 / 最大滚动量 / 是否需要滚动条 /
/// 可折叠条目命中区 / 链接区域 / 内联图片槽位（后三者都是内容全局行坐标）。
type VisibleLines = (
    Vec<Line<'static>>,
    usize,
    usize,
    bool,
    Vec<ClickTarget>,
    Vec<LinkSpan>,
    Vec<ImageSlot>,
);

/// 内联图片相对消息文本区的左内边距（列）：与消息正文的 1 列内边距一致。
const IMAGE_LEFT_PAD: u16 = 1;

/// 布局高度计算（可测试）：返回 (状态区行数, 待发送区行数, 输入区行数)
/// 状态区固定 1 行，仅在项目信任询问时按内容撑高；待发送区仅在存在排队 steer 时占行；
/// 停靠面板行数由渲染层单独计算（dock::dock_render + dock_height），不在此处汇总；
/// 扩展设置面板与 `/settings` 一样占据输入区（打开时隐藏输入框）。
pub fn layout_heights(app: &App, width: usize) -> (u16, u16, u16) {
    // 输入区：扩展设置面板 / 会话选择器 / 面板激活时由各自高度决定；否则为编辑器内容行数 + 上下边框 2 行
    let mut input_lines = if app.ext_settings.visible {
        ext_settings::panel_height(app, width)
    } else if app.settings_selector.active {
        settings_selector::selector_height(app, width)
    } else if app.session_selector.active {
        session_selector::selector_height(app, width)
    } else if app.panel.active {
        panel::panel_height(app, width)
    } else {
        // 编辑器内容 + 上下边框 + 候选列表行
        input::content_height(app, width) as u16 + 2 + suggestion::suggestion_height(app, width)
    };

    // 面板样式的覆盖层（如 FleetView）占据输入区：隐藏主页输入框，自身带上下分割线。
    if matches!(
        app.overlay.as_ref().map(|o| &o.view.size),
        Some(OverlaySize::Panel { .. })
    ) {
        input_lines = 0;
    }

    // 状态区固定 1 行：消息区与输入框之间仅保留 1 行间隔（忙碌时显示 spinner+状态）
    let status_lines = 1u16;

    // 待发送区：无排队为 0（不占布局）
    let pending_lines = pending::pending_lines(app) as u16;
    (status_lines, pending_lines, input_lines)
}

/// 渲染一帧（事件循环的 terminal.draw 闭包）
pub fn render_frame(frame: &mut Frame, app: &mut App) {
    render_frame_widgets(frame, app);

    // 超链接必须是最后一步：等所有 widget 落进 buffer 后再改写 cell symbol，
    // 否则后续 widget 会把粘好的转义序列盖掉（见 `hyperlink` 模块说明）。
    let url = app.oauth_login.as_ref().map(|login| login.auth_url());
    hyperlink::apply(
        frame,
        hyperlinks_enabled(),
        url.as_deref(),
        &app.mouse.links,
    );

    // 内联图片排在超链接之后：图片 cell（占位字符 / 转义序列）不能被后续步骤改写。
    draw_images(frame, app);
}

/// 绘制本帧收集到的内联图片（消息区渲染时已算好位置）。
///
/// 协议数据按内容指纹缓存在 [`image`] 模块里（后台线程编码，见 [`image::request`]），
/// 滚动时由 `SlicedImage` 按锚点裁剪；编码还没落地就画 `[image]` 占位，
/// 终端不支持图片或编码失败时保持预留的空白行，不画任何内容。
fn draw_images(frame: &mut Frame, app: &mut App) {
    let Some(picker) = terminal_image::image_picker() else {
        return;
    };

    // 取出来只为错开 `image_rects` 与 `app.theme` 的借用；画完放回去
    // （区域每帧重建，保留它是为了让渲染结果可断言、可诊断）。
    let rects = std::mem::take(&mut app.image_rects);
    for rect in &rects {
        match image::request(picker, rect.key, &rect.data, rect.size, rect.pad) {
            Some(protocol) => {
                frame.render_widget(SlicedImage::new(&protocol, rect.position), rect.area);
            }
            None => draw_image_placeholder(frame, rect, &app.theme),
        }
    }
    app.image_rects = rects;
}

/// 后台编码未完成的图片：在预留区的第一个可见行画 `[image]` 占位。
///
/// 图片上端滚出视口时占位落在第一个可见行；整张图都在视口之外时什么都不画。
fn draw_image_placeholder(frame: &mut Frame, rect: &ImageRect, theme: &Theme) {
    let bottom = rect.position.y + rect.size.height as i16;
    if rect.position.y >= rect.size.height as i16 || bottom <= 0 {
        return;
    }

    let row = Rect {
        x: rect.area.x,
        y: rect.area.y + rect.position.y.max(0) as u16,
        width: rect.area.width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(messages::render_image_placeholder(theme)),
        row,
    );
}

/// 渲染一帧的 widget 部分（不含末尾的超链接哨兵翻译）。
fn render_frame_widgets(frame: &mut Frame, app: &mut App) {
    let size = frame.area();
    let width = size.width.max(20) as usize;

    // 折叠条目 / 链接命中区每帧重建（全屏覆盖层接管消息区时为空 → 不残留上一帧区域）
    app.mouse.click_targets.clear();
    app.mouse.links.clear();
    app.image_rects.clear();

    // 覆盖层内容每帧拉取（扩展持有真值；返回 None 时自动关闭）
    super::overlay::sync(app, size.width);

    // banner 扩展渲染行（非干净启动/无扩展/窄矮终端为空 → 不渲染、不占布局）
    let banner_lines = status::collect_banner_lines(app, width, size.height as usize);
    let banner_height = banner_lines.len() as u16;

    // footer 扩展渲染行（无扩展时为空 → 底栏不渲染、不留空行）
    app.refresh_git_branch();
    let footer_lines = status::collect_footer_lines(app, width);
    let footer_height = footer_lines.len() as u16;

    // 输入区高度：全部输入行的折行数累计（上限 input::MAX_INPUT_LINES）+ 上下边框；状态区固定 1 行
    let (status_lines, pending_lines, input_lines) = layout_heights(app, width);

    // 停靠面板（状态栏上方）：隐藏或内容为空时不渲染，并清区域（防止滚轮命中残留区域）
    let dock = dock::dock_render(app);
    if app.dock_visible && dock.is_none() {
        app.dock_visible = false;
        app.dock_offset = 0;
        app.dock_area = None;
    }
    let dock_h = dock.as_ref().map(dock::dock_height).unwrap_or(0);

    // 布局：banner(可选) + 消息区(grow) + 待发送区 + 停靠面板 + 状态区 + 输入框(或选择器/扩展设置面板) + footer(可选)
    let mut constraints = vec![
        Constraint::Min(1),
        Constraint::Length(pending_lines),
        Constraint::Length(dock_h),
        Constraint::Length(status_lines),
        Constraint::Length(input_lines),
    ];

    let has_banner = banner_height > 0;
    if has_banner {
        constraints.insert(0, Constraint::Length(banner_height));
    }

    let has_footer = footer_height > 0;
    if has_footer {
        constraints.push(Constraint::Length(footer_height));
    }

    // 全屏覆盖层：接管消息区/停靠/状态/输入（banner 与底栏保留）
    if let Some(OverlaySize::Fullscreen) = app.overlay.as_ref().map(|o| o.view.size) {
        render_fullscreen_overlay(frame, size, app, &banner_lines, &footer_lines);
        return;
    }

    // 内联覆盖层（输入框下方、底栏上方）
    let overlay_h = insert_inline_overlay(&mut constraints, app, has_footer);

    let chunks = Layout::vertical(constraints).split(size);

    let mut idx = 0usize;
    if has_banner {
        status::render_top_bar(frame, chunks[idx], app, &banner_lines);
        idx += 1;
    }
    render_message_area(frame, chunks[idx], app, width);
    idx += 1;
    pending::render_pending(frame, chunks[idx], app);
    idx += 1;
    if let Some(d) = &dock {
        dock::render_dock(frame, chunks[idx], app, d);
    }
    idx += 1;
    status::render_status_bar(frame, chunks[idx], app);
    idx += 1;
    let input_chunk = chunks[idx];
    render_input_region(frame, input_chunk, app);
    idx += 1;

    if overlay_h > 0 {
        overlay::render(
            frame,
            chunks[idx],
            app,
            OverlaySize::Inline { max_rows: u16::MAX },
        );
        idx += 1;
    }

    if has_footer {
        status::render_bottom_bar(frame, chunks[idx], app, &footer_lines);
    }

    // 输入光标：/settings 选择器定位到搜索行；会话选择器定位到搜索行；
    // 面板激活时定位到选择器搜索行；否则为编辑器光标。
    render_cursor(frame, input_chunk, app, width);
}

/// 渲染全屏覆盖层：布局为 [banner? | 覆盖层(Min) | 分隔线? | 底栏?]。
/// 覆盖层接管消息区/停靠面板/状态栏/输入区，banner 与底栏保留。
///
/// 覆盖层与底栏之间插一行分割线：两者都是提示行，直接贴合会糊成一片。
fn render_fullscreen_overlay(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    banner_lines: &[BannerLine],
    footer_lines: &[FooterLine],
) {
    let banner_height = banner_lines.len() as u16;
    let footer_height = footer_lines.len() as u16;

    // 只在与主页底栏相邻时才需要分割线（无底栏时下面就是终端边缘）
    let separator_height = u16::from(footer_height > 0);

    let mut constraints = vec![Constraint::Min(1)];
    if banner_height > 0 {
        constraints.insert(0, Constraint::Length(banner_height));
    }
    if separator_height > 0 {
        constraints.push(Constraint::Length(separator_height));
    }
    if footer_height > 0 {
        constraints.push(Constraint::Length(footer_height));
    }

    let chunks = Layout::vertical(constraints).split(area);
    let mut idx = 0usize;
    if banner_height > 0 {
        status::render_top_bar(frame, chunks[idx], app, banner_lines);
        idx += 1;
    }
    overlay::render(frame, chunks[idx], app, OverlaySize::Fullscreen);
    idx += 1;
    if separator_height > 0 {
        render_overlay_separator(frame, chunks[idx], app);
        idx += 1;
    }
    if footer_height > 0 {
        status::render_bottom_bar(frame, chunks[idx], app, footer_lines);
    }
}

/// 全屏覆盖层与主页底栏之间的分割线（`─`，dim 色）。
fn render_overlay_separator(frame: &mut Frame, area: Rect, app: &App) {
    let style = app.theme.style("dim", "#666666");
    let line = Line::from(Span::styled("─".repeat(area.width as usize), style));
    frame.render_widget(Paragraph::new(line), area);
}

/// 将内联覆盖层高度约束插到底栏之前（约束顺序：… 输入区, 覆盖层, 底栏）。
/// 无内联覆盖层时不做改动，返回 0；否则返回插入的覆盖层行数。
fn insert_inline_overlay(constraints: &mut Vec<Constraint>, app: &App, has_footer: bool) -> u16 {
    let overlay_h = super::overlay::outer_rows(app) as u16;
    if overlay_h == 0 {
        return 0;
    }
    let footer_idx = constraints.len() - usize::from(has_footer);
    constraints.insert(footer_idx, Constraint::Length(overlay_h));
    overlay_h
}

/// 渲染输入框区域：按激活的选择器/面板/编辑器分派，并维护面板区域记录。
fn render_input_region(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.ext_settings.visible {
        // 扩展设置面板占据输入框区域
        ext_settings::render(frame, area, app);
        app.panel_area = None;
    } else if app.settings_selector.active {
        // /settings 设置选择器占据输入框区域
        settings_selector::render(frame, area, app);
        app.panel_area = None;
    } else if app.session_selector.active {
        // 会话选择器占据输入框区域
        session_selector::render(frame, area, app);
    } else if app.panel.active {
        // 记录面板区域（鼠标 Ctrl+click 打开 OAuth URL 用）
        app.panel_area = Some(area);
        // 选择器占据输入框区域
        panel::render_panel_selector(frame, area, app);
    } else {
        input::render_input_box(frame, area, app);
        app.panel_area = None;
    }
}

/// 设置输入光标位置：按激活的选择器/面板/编辑器分派。
fn render_cursor(frame: &mut Frame, area: Rect, app: &mut App, width: usize) {
    // 面板样式的覆盖层占据输入区：主页输入框已隐藏 → 不显示编辑器光标
    if matches!(
        app.overlay.as_ref().map(|o| &o.view.size),
        Some(OverlaySize::Panel { .. })
    ) {
        return;
    }

    if app.ext_settings.visible {
        // 搜索行：边框1 + 空1
        let (_, col) = app
            .ext_settings
            .filter
            .visible_window(width.saturating_sub(4));
        let cx = area.x + 2 + col as u16;
        let cy = area.y + 2;
        frame.set_cursor_position((cx, cy));
    } else if app.settings_selector.active {
        if app.settings_selector.submenu.is_none() {
            // 搜索行：边框1 + 空1
            let s = &app.settings_selector;
            let (_, col) = s.filter.visible_window(width.saturating_sub(4));
            let cx = area.x + 2 + col as u16;
            let cy = area.y + 2;
            frame.set_cursor_position((cx, cy));
        }
    } else if app.session_selector.active {
        let s = &app.session_selector;
        if s.rename_mode {
            // rename 输入行：边框1 + 空1 + 标题1 + 空1
            let (_, col) = s.rename_input.visible_window(width.saturating_sub(2));
            let cx = area.x + 2 + col as u16;
            let cy = area.y + 4;
            frame.set_cursor_position((cx, cy));
        } else {
            // 搜索行：边框1 + 空1 + header3 + 空1
            let (_, col) = s.filter.visible_window(width.saturating_sub(2));
            let cx = area.x + 2 + col as u16;
            let cy = area.y + 6;
            frame.set_cursor_position((cx, cy));
        }
    } else if app.panel.active {
        let kind = app.panel.top().map(|l| l.kind);
        let show_cursor = kind.is_some_and(|k| k.has_filter_input());

        if show_cursor {
            let input = app.panel.top().map(|l| &l.filter);
            if let Some(input) = input {
                let (_, col) = input.visible_window(width.saturating_sub(4));
                let cx = area.x + 2 + col as u16;
                // login 面板提示在底部 → 搜索行是第 2 行；其余面板提示在顶部 → 第 4 行
                let cy = if matches!(
                    kind,
                    Some(PanelKind::LoginProvider | PanelKind::LogoutProvider)
                ) {
                    area.y + 2
                } else if kind == Some(PanelKind::LoginKey) {
                    // API key 输入面板：输入行位于 空/标题/空/提示 之后（内容第 5 行）
                    area.y + 5
                } else if kind == Some(PanelKind::Thinking) {
                    // /thinking 面板：输入行位于 空/标题/空/副标题/空 之后（内容第 6 行）
                    area.y + 6
                } else if kind == Some(PanelKind::ScopedModels) {
                    // /scoped-models 面板：输入行位于 空/标题/副标题/空 之后（内容第 5 行）
                    area.y + 5
                } else if kind == Some(PanelKind::Extension) {
                    // /extension 面板：输入行与 /scoped-models 同布局（内容第 5 行）
                    area.y + 5
                } else if kind == Some(PanelKind::Skill) {
                    // /skills 面板：与 /extension 同布局（内容第 5 行），但错误行夹在
                    // 副标题与输入行之间且会折行 → 加上其行数（与渲染共用同一个计算）
                    area.y + 5 + panel::skills_error_line_count(app, width) as u16
                } else if kind == Some(PanelKind::BugReportHint) {
                    // /bug 描述面板：输入行在 空/标题/空/说明 N 行/空 之后，
                    // N 随面板宽度折行（与渲染共用 [`panel::bug_report_note_line_count`]）
                    area.y + 5 + panel::bug_report_note_line_count(app, width) as u16
                } else if kind == Some(PanelKind::LoginOauth) {
                    // OAuth 面板：输入行位于 空/标题/空/URL n行/Ctrl/空/说明2/空 之后
                    // n 按面板实际宽度换行（与渲染 oauth_url_lines 同公式）；边框在 area 内 → +1
                    // device-code 流（含初始化中）无输入行 → 不设置光标
                    let awaiting = app.oauth_login.as_ref().is_some_and(|o| o.awaiting_input());
                    if app.oauth_login.as_ref().is_some_and(|o| o.is_device_code()) && !awaiting {
                        return;
                    }
                    if awaiting {
                        // AwaitInput 面板：输入行在 标题/空/提问/占位 之后（内容第 5 行）
                        area.y + 5
                    } else {
                        let n = app
                            .oauth_login
                            .as_ref()
                            .map(|o| {
                                crate::modes::interactive::render::oauth_url_lines(
                                    &o.auth_url(),
                                    area.width as usize,
                                )
                            })
                            .unwrap_or(1) as u16;
                        area.y + 9 + n
                    }
                } else {
                    area.y + 4
                };
                frame.set_cursor_position((cx, cy));
            }
        }
    } else if let Some((x, y)) = input::input_cursor_pos(area, app) {
        frame.set_cursor_position((x, y));
    }

    // 模态选择面板已移除：选择器在输入框区域渲染（对齐 pi showSelector）
}

/// 消息区：历史 + 流式缓冲 → 裁剪可见行 → Paragraph + 滚动条（右侧 1 列）。
/// 历史部分走消息级缓存（命中零重渲染，只做指针拼接与裁剪定位）；
/// 流式段每帧重新渲染（内容持续变化，无法缓存）。
/// 滚动条状态（存在/不存在）决定首选宽度，避免 width ↔ width-1 抖动反复重建缓存。
fn render_message_area(frame: &mut Frame, area: Rect, app: &mut App, width: usize) {
    let msg_area = area.height as usize;
    if msg_area == 0 {
        app.mouse.visible.clear();
        app.mouse.links.clear();
        app.image_rects.clear();
        app.mouse.log_area_h = 0;
        app.mouse.log_total = 0;
        app.mouse.scroll_thumb = None;
        app.max_scroll = 0;
        app.scroll = 0;
        app.last_render_total = 0;
        return;
    }
    // 流式内容按值取出（避免借用 app 期间调用 &mut 的渲染管线）
    let streaming: Option<(String, Vec<String>, String)> = if app.busy {
        Some((
            app.streaming_thinking.clone(),
            app.streaming_tools.clone(),
            app.streaming_text.clone(),
        ))
    } else {
        None
    };
    let streaming_ref = streaming
        .as_ref()
        .map(|(t, tools, txt)| (t.as_str(), tools.as_slice(), txt.as_str()));

    // 首选宽度：有滚动条 → width-1（历史会话几乎总有滚动条，避免首帧双渲染）
    let last_scrollbar = app.last_has_scrollbar;
    let init_width = if last_scrollbar {
        width.saturating_sub(1).max(1)
    } else {
        width
    };

    // 渲染历史 + 流式段并裁剪出可见行
    let (mut visible, total, max_scroll, needs_scrollbar, click_targets, links, images) =
        collect_visible_lines(
            app,
            msg_area,
            width,
            init_width,
            last_scrollbar,
            streaming_ref,
        );

    // 记录可见行文本（鼠标命中测试用）与消息区位置/滚动条状态（拖动比例用）
    app.mouse.log_area = area.y;
    app.mouse.log_area_h = area.height;
    app.mouse.log_total = total;
    // 视口顶行的内容全局行号（选中以内容坐标存储，滚动/流式时反算回视口行）
    app.mouse.view_start = total.saturating_sub(msg_area).saturating_sub(app.scroll);
    // 渲染宽度：与 collect_visible_lines 内部一致（滚动条存在时文本区宽 width-1）
    app.mouse.view_width = if needs_scrollbar {
        width.saturating_sub(1).max(1)
    } else {
        width
    };
    app.mouse.visible = visible.iter().map(line_text).collect();

    // 可折叠条目命中区：内容全局行 → 屏幕行（只保留与视口相交的部分）
    let view_end = app.mouse.view_start + app.mouse.visible.len();
    app.mouse.click_targets = click_targets
        .iter()
        .filter_map(|t| {
            let s = t.start.max(app.mouse.view_start);
            let e = t.end.min(view_end);
            (s < e).then(|| ClickRow {
                start: area.y + (s - app.mouse.view_start) as u16,
                end: area.y + (e - app.mouse.view_start) as u16,
                id: t.id,
                default_expanded: t.default_expanded,
            })
        })
        .collect();

    // 选中高亮（selectedBg）
    apply_selection_highlight(&mut visible, &app.mouse, &app.theme);

    // 文本区：有滚动条时右侧留 1 列，无滚动条时占满整个区域
    let text_area = if needs_scrollbar {
        Rect {
            width: area.width.saturating_sub(1).max(1),
            ..area
        }
    } else {
        area
    };

    // 链接命中区：内容全局行 → 屏幕行（只保留与视口相交的部分），列加上文本区左边界并裁到区内。
    app.mouse.links = links
        .iter()
        .filter(|l| l.line >= app.mouse.view_start && l.line < view_end)
        .filter_map(|l| {
            let col_start = text_area.x.saturating_add(l.start_col as u16);
            let col_end = text_area
                .x
                .saturating_add(l.end_col as u16)
                .min(text_area.right());
            (col_start < col_end).then(|| LinkRow {
                row: area.y + (l.line - app.mouse.view_start) as u16,
                col_start,
                col_end,
                url: l.url.clone(),
            })
        })
        .collect();

    let para = Paragraph::new(visible)
        .block(Block::default().borders(Borders::NONE))
        .style(Style::default());
    frame.render_widget(para, text_area);

    // 内联图片：内容全局行 → 相对文本区的锚点（可为负，表示图片上端滚到视口之上）。
    // 绘制本身在帧末尾（超链接之后），这里只算位置。
    app.image_rects = images
        .iter()
        .filter(|slot| {
            let end = slot.line + slot.rows as usize;
            slot.line < view_end && end > app.mouse.view_start
        })
        .map(|slot| ImageRect {
            area: text_area,
            position: SignedPosition {
                x: IMAGE_LEFT_PAD as i16,
                y: slot.line as i16 - app.mouse.view_start as i16,
            },
            key: slot.key,
            size: Size::new(slot.cols, slot.rows),
            data: slot.data.clone(),
            pad: slot.pad,
        })
        .collect();

    render_scrollbar(frame, area, app, msg_area, total, max_scroll);
}

/// 渲染历史 + 流式段，按 segment 前缀二分裁剪出可见行，并更新滚动状态。
fn collect_visible_lines(
    app: &mut App,
    msg_area: usize,
    width: usize,
    init_width: usize,
    last_scrollbar: bool,
    streaming_ref: Option<(&str, &[String], &str)>,
) -> VisibleLines {
    // 渲染历史 + 流式段；滚动条状态切换时换另一宽度重建缓存（最多一次）
    let (hist, stream_lines, stream_links, needs_scrollbar): (
        messages::HistoryRender,
        Vec<Line<'static>>,
        Vec<messages::LinkSpan>,
        bool,
    ) = {
        let mut hist = app.render_history_lines(init_width);
        let mut pending_all = hist.pending.clone();
        let mut stream_links: Vec<LinkSpan> = Vec::new();
        let stream = match streaming_ref {
            Some((t, tools, txt)) => messages::render_streaming(
                t,
                tools,
                txt,
                init_width,
                &app.theme,
                app.expand_all,
                app.show_thinking,
                &mut pending_all,
                &app.tool_started_at,
                &mut stream_links,
            ),
            None => Vec::new(),
        };
        let needs = hist.total + stream.len() > msg_area;
        if needs != last_scrollbar {
            let w2 = if needs {
                width.saturating_sub(1).max(1)
            } else {
                width
            };
            app.invalidate_render();
            hist = app.render_history_lines(w2);
            pending_all = hist.pending.clone();
            let mut stream2_links: Vec<LinkSpan> = Vec::new();
            let stream2 = match streaming_ref {
                Some((t, tools, txt)) => messages::render_streaming(
                    t,
                    tools,
                    txt,
                    w2,
                    &app.theme,
                    app.expand_all,
                    app.show_thinking,
                    &mut pending_all,
                    &app.tool_started_at,
                    &mut stream2_links,
                ),
                None => Vec::new(),
            };
            (hist, stream2, stream2_links, needs)
        } else {
            (hist, stream, stream_links, needs)
        }
    };
    app.last_has_scrollbar = needs_scrollbar;

    // 可折叠条目命中区：流式段插在锚点之前时，锚点之后的条目行号整体下移
    let hist_stream_len = stream_lines.len();
    let click_targets: Vec<messages::ClickTarget> = match hist.anchor_end {
        Some(anchor_line) if hist_stream_len > 0 && anchor_line <= hist.total => hist
            .click_targets
            .iter()
            .map(|t| {
                let shift = if t.start >= anchor_line {
                    hist_stream_len
                } else {
                    0
                };
                messages::ClickTarget {
                    start: t.start + shift,
                    end: t.end + shift,
                    id: t.id,
                    default_expanded: t.default_expanded,
                }
            })
            .collect(),
        _ => hist.click_targets.clone(),
    };

    // 滚动裁剪：按 segment 前缀二分定位可见区间，只 clone 窗口内的行；
    // 渲染模型以“距底部偏移”定位视口（scroll=0 贴底跟随）。用户向上滚动暂停跟随后，
    // 视口顶行 start = total - msg_area - scroll 必须锚定：末尾内容增长会把 start 往下推，
    // 末尾内容缩减则反之。因此 scroll>0 时按 total 增减同步调整 scroll 抵消位移。
    let total = hist.total + stream_lines.len();
    let max_scroll = total.saturating_sub(msg_area);
    app.max_scroll = max_scroll;
    if app.scroll > 0 {
        let prev_total = app.last_render_total;
        if total > prev_total {
            app.scroll = app.scroll.saturating_add(total - prev_total);
        } else {
            app.scroll = app.scroll.saturating_sub(prev_total - total);
        }
    }
    app.last_render_total = total;
    app.scroll = app.scroll.min(max_scroll);

    let start = total.saturating_sub(msg_area).saturating_sub(app.scroll);
    let end = total.min(start + msg_area).max(start);
    let mut visible: Vec<Line<'static>> = Vec::new();

    // 流式段插入：若历史渲染返回了锚点行号（assistant 开始位置），
    // 把流式输出插到锚点之后，避免后注入的 steer 消息显示在流式输出前。
    let stream_len = stream_lines.len();
    if let Some(anchor_line) = hist.anchor_end
        && stream_len > 0
        && anchor_line <= hist.total
    {
        let mut placed = false;
        let mut out: Vec<messages::HistorySegment> = Vec::with_capacity(hist.segments.len() + 1);

        for (seg_start, lines, pending) in &hist.segments {
            if !placed && *seg_start >= anchor_line {
                out.push((
                    anchor_line,
                    Arc::new(stream_lines.clone()),
                    Arc::new(Vec::new()),
                ));
                placed = true;
            }

            let shift = if *seg_start >= anchor_line {
                stream_len
            } else {
                0
            };

            out.push((*seg_start + shift, lines.clone(), pending.clone()));
        }

        if !placed {
            out.push((
                anchor_line,
                Arc::new(stream_lines.clone()),
                Arc::new(Vec::new()),
            ));
        }

        for (seg_start, lines, _) in &out {
            let seg_end = seg_start + lines.len();
            if seg_end <= start || *seg_start >= end {
                continue;
            }
            let a = start.saturating_sub(*seg_start);
            let b = (end - *seg_start).min(lines.len());
            for i in a..b {
                visible.push(lines[i].clone());
            }
        }
    } else {
        for (seg_start, lines, _) in &hist.segments {
            let seg_end = seg_start + lines.len();
            if seg_end <= start || *seg_start >= end {
                continue;
            }

            let a = start.saturating_sub(*seg_start);
            let b = (end - *seg_start).min(lines.len());
            for i in a..b {
                visible.push(lines[i].clone());
            }
        }

        if end > hist.total && start < total {
            let a = start.saturating_sub(hist.total);
            let b = (end - hist.total).min(stream_lines.len());
            for line in stream_lines.iter().take(b).skip(a) {
                visible.push(line.clone());
            }
        }
    }

    // 链接区域：历史部分在流式段插到前面时整体下移，流式部分从插入点起算
    // （口径与 click_targets 一致）；这里只做内容坐标换算，屏幕坐标由调用方算。
    let mut links: Vec<LinkSpan> = Vec::new();
    let mut images: Vec<ImageSlot> = Vec::new();
    for link in hist.links.iter() {
        let mut link = link.clone();
        if stream_len > 0
            && let Some(anchor_line) = hist.anchor_end
            && link.line >= anchor_line
        {
            link.line += stream_len;
        }
        links.push(link);
    }

    // 图片槽位：与链接同一套下移口径（流式段无图片）
    for slot in hist.images.iter() {
        let mut slot = slot.clone();
        if stream_len > 0
            && let Some(anchor_line) = hist.anchor_end
            && slot.line >= anchor_line
        {
            slot.line += stream_len;
        }
        images.push(slot);
    }

    if stream_len > 0 {
        let base = match hist.anchor_end {
            Some(anchor_line) if anchor_line <= hist.total => anchor_line,
            _ => hist.total,
        };
        links.extend(stream_links.iter().cloned().map(|mut link| {
            link.line += base;
            link
        }));
    }

    (
        visible,
        total,
        max_scroll,
        needs_scrollbar,
        click_targets,
        links,
        images,
    )
}

/// 渲染右侧滚动条并记录 thumb 相对区间（鼠标拖动比例用）。
fn render_scrollbar(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    msg_area: usize,
    total: usize,
    max_scroll: usize,
) {
    // 滚动条：内容超出可视区时显示；thumb 位置与 scroll 线性映射：
    // scroll=0（内容底部）→ thumb 贴 track 底；scroll=max_scroll → thumb 贴顶。
    // ratatui 的 thumb_start = position*track/(content-1+viewport)，贴底需 position=content-1，
    // 因此按 (total-1)*(1 - scroll/max_scroll) 换算，避免 thumb 到不了底部。
    let scroll_x = area.x + area.width.saturating_sub(1);
    if max_scroll > 0 && area.width >= 2 {
        let thumb_style = app.theme.style("scrollbarThumb", "#6a6a78");
        let scrollbar =
            ratatui::widgets::Scrollbar::new(ratatui::widgets::ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_symbol(DEF_SCROLLBAR_THUMB)
                .thumb_style(thumb_style);
        let position =
            ((total - 1) as f64 * (1.0 - app.scroll as f64 / max_scroll as f64)).round() as usize;
        let mut state = ratatui::widgets::ScrollbarState::new(total)
            .position(position)
            .viewport_content_length(msg_area);
        frame.render_stateful_widget(
            scrollbar,
            Rect {
                x: scroll_x,
                width: 1,
                ..area
            },
            &mut state,
        );
        // 记录 thumb 相对区间（与 ratatui part_lengths 同公式）：
        // thumb_len = vp*track/(total-1+vp)；thumb_start = position*track/(total-1+vp)
        let denom = (total - 1 + msg_area).max(1);
        let thumb_len = ((msg_area as f64 * msg_area as f64 / denom as f64).round() as usize)
            .clamp(1, msg_area);
        let thumb_start = ((position as f64 * msg_area as f64 / denom as f64).round() as usize)
            .min(msg_area.saturating_sub(thumb_len));
        app.mouse.scroll_thumb = Some((thumb_start, thumb_len));
        app.mouse.scroll_x = Some(scroll_x);
    } else {
        app.mouse.scroll_thumb = None;
        app.mouse.scroll_x = None;
    }
}

/// 供测试：把消息列表渲染为纯文本（简化断言）
pub fn render_messages_text(app: &App, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for msg in &app.messages {
        for line in message_lines(msg, width, &app.theme, app.expand_all) {
            out.push(line_text(&line));
        }
    }
    out
}

/// 行拼接文本
fn line_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// 在可见行上叠加选中高亮（反色：背景 = 该处文本色、文字 = 该处背景色，对齐终端选区；按字符区间，支持反向选择）
/// `sel` 存内容全局行号：先按 `view_start` 反算到视口行，只处理与视口相交的区间，滚动后高亮跟随内容而非视口。
fn apply_selection_highlight(visible: &mut [Line<'static>], mouse: &MouseSel, _theme: &Theme) {
    use ratatui::text::Span;
    let Some((ls, cs, le, ce)) = mouse.sel else {
        return;
    };
    if mouse.visible.is_empty() {
        return;
    }
    let (ls, cs, le, ce) = if (ls, cs) <= (le, ce) {
        (ls, cs, le, ce)
    } else {
        (le, ce, ls, cs)
    };
    let start = mouse.view_start;

    // 选中区与视口（内容行 [start, start+visible.len())）完全不相交时直接跳过。
    // 起点整体在视口下方 → 旧检查 vs >= visible.len() 已覆盖；
    // 终点整体在视口上方（le < start）此前漏检：vs/ve 被 saturating_sub 双双钳成 0，
    // 视口第一行被当成相交首行整行染成选中背景（滚动/流式把旧选择推出视口后“顶部一行被选中”）。
    let rows = visible.len();
    if le < start || ls >= start.saturating_add(rows) {
        return;
    }

    // 内容行 → 视口行；选中起点在视口上方时 vs=0（只显示相交部分），终点在视口下方时由 ve > visible.len() 自然截断
    let vs = ls.saturating_sub(start);
    let ve = le.saturating_sub(start);
    let sel_style = Style::default()
        .fg(ratatui::style::Color::Black)
        .bg(color("#a0a0a0"));
    for (i, line) in visible.iter_mut().enumerate() {
        if i < vs || i > ve {
            continue;
        }
        let g = start + i;
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
        // 重建行：clip 选中字符区间，套 selectedBg
        let mut new_spans: Vec<ratatui::text::Span<'static>> = Vec::new();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::app::App;

    /// 内联图片：开启 `showImages` 后整帧渲染把图片槽位换算成屏幕区域并在区域里写出
    /// 图片 cell（半块字符 / kitty 占位字符 / sixel 转义序列，视终端协议而定）；
    /// 关闭时整块不渲染——既不产生图片区域，也不留 `[image]` 占位行。
    /// 图片区域里是否写出了协议 cell（非空白、非占位）。
    fn image_cells_present(
        terminal: &ratatui::Terminal<ratatui::backend::TestBackend>,
        app: &App,
    ) -> bool {
        let Some(rect) = app.image_rects.first() else {
            return false;
        };
        let x = rect.area.x + rect.position.x.max(0) as u16;
        let y = rect.area.y + rect.position.y.max(0) as u16;
        let buf = terminal.backend().buffer();
        (y..y + rect.size.height).any(|row| {
            (x..x + rect.size.width).any(|col| {
                buf.cell((col, row))
                    .map(|c| !c.symbol().trim().is_empty() && !c.symbol().contains("[image]"))
                    .unwrap_or(false)
            })
        })
    }

    /// 等异步编码落地并重绘，直到真图画出来（并行测试会清全局缓存，故重试几次）。
    fn draw_until_image(
        terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
        app: &mut App,
    ) -> bool {
        for _ in 0..10 {
            assert!(
                super::image::wait_idle(std::time::Duration::from_secs(30)),
                "后台编码应在超时前结束"
            );
            app.dirty = true;
            terminal.draw(|f| render_frame(f, app)).unwrap();
            if image_cells_present(terminal, app) {
                return true;
            }
        }
        false
    }

    #[test]
    fn show_images_renders_inline_image_cells() {
        use crate::core::provider::ContentBlock;
        use ratatui::backend::TestBackend;

        // 图片内容指纹就是图片协议缓存的键：本用例要求首帧必为冷缓存，
        // 数据尺寸必须与其它用图用例不同，否则会被并行用例预热（首帧直接画出真图）
        let data = crate::test_support::png_base64_two_tone(600, 200);
        let build = |show: bool| {
            let mut app = App::new();
            app.set_show_images(show);
            let mut msg = crate::core::provider::AgentMessage::user_text("");
            msg.role = "assistant".to_string();
            msg.content = vec![ContentBlock::Image {
                data: data.clone(),
                mime_type: "image/png".to_string(),
            }];
            app.messages.push(msg);
            app
        };

        // 关闭（默认）：整块不渲染
        let mut off = build(false);
        let mut terminal = ratatui::Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| render_frame(f, &mut off)).unwrap();
        assert!(off.image_rects.is_empty());
        assert!(
            !off.mouse.visible.iter().any(|l| l.contains("[image]")),
            "关闭时不应出现占位行: {:?}",
            off.mouse.visible
        );

        // 开启：区域非空；编码在后台跑，第一帧先画 `[image]` 占位
        let mut on = build(true);
        let mut terminal = ratatui::Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| render_frame(f, &mut on)).unwrap();
        assert_eq!(on.image_rects.len(), 1, "槽位应换算成一个屏幕区域");

        let (x, y, width) = {
            let rect = &on.image_rects[0];
            (
                rect.area.x + rect.position.x.max(0) as u16,
                rect.area.y + rect.position.y.max(0) as u16,
                rect.size.width,
            )
        };
        let placeholder: String = (x..x + width)
            .map(|col| {
                terminal
                    .backend()
                    .buffer()
                    .cell((col, y))
                    .map(|c| c.symbol().to_string())
                    .unwrap_or_default()
            })
            .collect();
        assert!(
            placeholder.contains("[image]"),
            "编码未完成时应先画占位: {placeholder:?}"
        );

        // 后台编码落地后重绘：区域内写出协议 cell（占位被真图覆盖）
        assert!(
            draw_until_image(&mut terminal, &mut on),
            "图片区域内应写出协议 cell"
        );
    }

    /// 后台编码完成会经 `run_in_event_loop` 把界面标脏（这里没有事件循环，退化为不报错），
    /// 且同参数只起一个编码任务：重复请求在任务跑完前一直返回 `None`。
    #[test]
    fn image_requests_are_deduplicated_while_encoding() {
        use ratatui::layout::Size;

        let Some(picker) = crate::utils::terminal_image::image_picker() else {
            return;
        };
        super::image::reset_cache();

        // 尺寸与 show_images 用例错开：同一份数据会共享缓存条目，
        // 对着已被预热的键断言“首次请求必为未命中”必然时灵时不灵
        let data: std::sync::Arc<str> =
            std::sync::Arc::from(crate::test_support::png_base64_two_tone(600, 220).as_str());
        let key = crate::utils::terminal_image::image_key(&data);
        let size = Size::new(60, 10);

        let first = super::image::request(picker, key, &data, size, None);
        assert!(first.is_none(), "首次请求应登记后台任务并返回 None");
        let second = super::image::request(picker, key, &data, size, None);
        assert!(second.is_none(), "任务在跑时重复请求不应再起线程");

        assert!(
            super::image::wait_idle(std::time::Duration::from_secs(30)),
            "后台编码应在超时前结束"
        );
        // 并行测试可能刚好清掉全局缓存，重试到命中为止
        let hit = (0..10).any(|_| {
            if super::image::request(picker, key, &data, size, None).is_some() {
                return true;
            }
            _ = super::image::wait_idle(std::time::Duration::from_secs(30));
            false
        });
        assert!(hit, "编码完成后应命中缓存");
    }

    /// halfblocks 没有 alpha：透明补边必须按槽位底色填色，否则图片底部会多出一条黑边
    /// （sixel 走 P2=1 透明，看到的是真实背景色）。这里用纯色图 + 高对比补边色验证
    /// 底色真的传到了编码器。
    #[test]
    fn halfblocks_padding_uses_the_slot_background_color() {
        use ratatui::layout::Size;
        use ratatui_image::picker::Picker;

        // 固定用 halfblocks picker：本机可能处于 tmux / 设置了 PRUX_IMAGE_PROTOCOL，
        // 全局 picker 的协议不可控（泄漏成 'static 满足 request 的签名）。
        let picker: &'static Picker = Box::leak(Box::new(Picker::halfblocks()));
        let font = picker.font_size();
        let data: std::sync::Arc<str> =
            std::sync::Arc::from(crate::test_support::png_base64(301, 50).as_str());
        let key = crate::utils::terminal_image::image_key(&data);

        // 301×50 放进 31×3 格（10×20 字体）：宽度/高度都不是格子尺寸的整数倍，
        // 库会按 `Resize::Fit` 补边（底部 10px，即最后一格的下半格）。
        let cols = 301u16.div_ceil(font.width);
        let rows = 50u16.div_ceil(font.height);
        let size = Size::new(cols, rows);

        // 最后一行的所有格子颜色（halfblocks 的上下半格就是 fg / bg）
        let bottom_row_colors = |pad: crate::utils::terminal_image::PadRgb| {
            super::image::reset_cache();
            let mut protocol = None;
            for _ in 0..10 {
                if let Some(p) = super::image::request(picker, key, &data, size, pad) {
                    protocol = Some(p);
                    break;
                }
                assert!(
                    super::image::wait_idle(std::time::Duration::from_secs(30)),
                    "后台编码应在超时前结束"
                );
            }
            let protocol = protocol.expect("编码完成后应命中缓存");

            let area = ratatui::layout::Rect::new(0, 0, size.width, size.height);
            let mut buf = ratatui::buffer::Buffer::empty(area);
            let widget = ratatui_image::sliced::SlicedImage::new(&protocol, (0, 0).into());
            ratatui::widgets::Widget::render(widget, area, &mut buf);
            (0..size.width)
                .flat_map(|x| {
                    let c = buf.cell((x, size.height - 1)).unwrap();
                    [c.fg, c.bg]
                })
                .filter_map(|c| match c {
                    ratatui::style::Color::Rgb(r, g, b) => Some((r, g, b)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        // 补边填绿：下半格应明显偏绿
        let padded = bottom_row_colors(Some([0, 255, 0]));
        assert!(
            padded.iter().any(|(_, g, _)| *g >= 128),
            "补边应填成槽位底色（绿），实际 {padded:?}"
        );
        // 不填（透明 → 黑）：不应有绿色
        let plain = bottom_row_colors(None);
        assert!(
            plain.iter().all(|(_, g, _)| *g < 128),
            "不填底色时补边不该有绿色，实际 {plain:?}"
        );
    }

    /// 内联图片滚动到半出视口：锚点行号为负，`SlicedImage` 只画可见的那几行
    /// （kitty 的占位符 / halfblocks 的行切片都按行偏移裁剪），且不 panic。
    #[test]
    fn inline_image_is_clipped_when_scrolled_half_out() {
        use crate::core::provider::ContentBlock;
        use ratatui::backend::TestBackend;

        let mut app = App::new();
        app.set_show_images(true);
        let mut msg = crate::core::provider::AgentMessage::user_text("");
        msg.role = "assistant".to_string();
        msg.content = vec![ContentBlock::Image {
            data: crate::test_support::png_base64_two_tone(600, 2000),
            mime_type: "image/png".to_string(),
        }];
        app.messages.push(msg);

        // 高图：60×30 单元格上限里按高度顶满（18 列 × 30 行），内容超出消息区才能滚动
        let mut terminal = ratatui::Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        assert!(app.max_scroll > 0, "内容应超出视口");
        assert_eq!(app.image_rects.len(), 1);

        // 图片编码在后台：先等它落地，断言的才是真图而不是占位
        assert!(draw_until_image(&mut terminal, &mut app));

        app.scroll = app.max_scroll / 2;
        app.dirty = true;
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();

        let (negative, x) = {
            let rect = &app.image_rects[0];
            assert!(
                rect.position.y < 0,
                "图片上端应滚到视口之上: {:?}",
                rect.position
            );
            (
                rect.position.y < 0,
                rect.area.x + rect.position.x.max(0) as u16,
            )
        };
        assert!(negative);
        // 可见部分仍写出协议 cell
        let buf = terminal.backend().buffer();
        let drawn = (app.image_rects[0].area.y..app.image_rects[0].area.bottom()).any(|row| {
            (x..x + app.image_rects[0].size.width).any(|col| {
                buf.cell((col, row))
                    .map(|c| !c.symbol().trim().is_empty())
                    .unwrap_or(false)
            })
        });
        assert!(drawn, "滚动后可见部分仍应画出图片");
    }

    #[test]
    fn message_links_become_clickable_rows() {
        // 整帧渲染：markdown 链接要被记成屏幕命中区（行/列 + URL），供 Ctrl+click 使用。
        use ratatui::backend::TestBackend;
        let mut app = App::new();
        app.messages
            .push(crate::core::provider::AgentMessage::user_text(
                "see [docs](https://example.com/docs) now",
            ));
        let w = 80u16;
        let h = 20u16;
        let backend = TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();

        let links = &app.mouse.links;
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(links[0].url, "https://example.com/docs");
        // 列区间内的任意一列都命中，区间外不命中（列区间含 ` (url)` 提示）。
        assert!(
            app.mouse
                .link_at(links[0].col_start, links[0].row)
                .is_some()
        );
        assert!(
            app.mouse
                .link_at(links[0].col_end - 1, links[0].row)
                .is_some()
        );
        assert!(
            app.mouse
                .link_at(links[0].col_start.saturating_sub(1), links[0].row)
                .is_none()
        );
        assert!(
            app.mouse
                .link_at(links[0].col_start, links[0].row + 1)
                .is_none()
        );
    }

    #[test]
    fn links_from_several_messages_keep_their_own_rows() {
        // 行号累加：第二条消息里的链接必须排在第一条之后（跨条目全局行号换算）。
        use ratatui::backend::TestBackend;
        let mut app = App::new();
        app.messages
            .push(crate::core::provider::AgentMessage::user_text(
                "[first](https://first.example)",
            ));
        app.messages
            .push(crate::core::provider::AgentMessage::user_text(
                "[second](https://second.example)",
            ));
        let backend = TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();

        let links = &app.mouse.links;
        assert_eq!(links.len(), 2, "{links:?}");
        assert_eq!(links[0].url, "https://first.example");
        assert_eq!(links[1].url, "https://second.example");
        assert!(
            links[0].row < links[1].row,
            "第二条消息的链接应在更下面: {links:?}"
        );
    }

    #[test]
    fn pending_region_sits_between_messages_and_status() {
        // 整帧渲染：排队 steer 时待发送区占据消息区与状态区之间的行（对齐 pi
        // dock：transcript → pendingMessagesContainer → statusContainer）
        use ratatui::backend::TestBackend;
        let mut app = App::new();
        app.messages
            .push(crate::core::provider::AgentMessage::user_text("hi"));
        app.runtime_steer_inbox.lock().unwrap().push_back(
            crate::core::provider::AgentMessage::user_text("continue faster"),
        );
        app.status = "Working...".to_string();
        app.busy = true;
        let h = 30u16;
        let backend = TestBackend::new(60, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (0..60)
                .map(|c| buf.cell((c, y)).unwrap().symbol().to_string())
                .collect()
        };
        // 固定区（待发送 + 状态 + 输入）贴底，消息区占其余空间；
        // 测试 App::new() 无 footer 扩展注册，底栏不渲染、不占行；
        // 待发送区位于状态区正上方
        let (status_h, pending_h, input_h) = layout_heights(&app, 60);
        let pend_start = h - (pending_h + status_h + input_h);
        assert_eq!(pending_h, 3, "空行 + Steering + 提示行");
        let msg_rows: Vec<String> = (0..pend_start).map(&row).collect();
        assert!(msg_rows.join("\n").contains("hi"), "消息区在待发送区之上");
        assert!(
            row(pend_start).trim().is_empty(),
            "待发送区首行空（对齐 pi Spacer(1)）: {:?}",
            row(pend_start)
        );
        assert!(
            row(pend_start + 1).contains("Steering: continue faster"),
            "row {}: {:?}",
            pend_start + 1,
            row(pend_start + 1)
        );
        assert!(
            row(pend_start + 2).contains("↳ Alt+Up to edit all queued messages"),
            "row {}: {:?}",
            pend_start + 2,
            row(pend_start + 2)
        );
        // 状态区（braille spinner + Working...）紧贴待发送区之下（固定 1 行）
        assert!(
            row(pend_start + pending_h).contains("Working..."),
            "状态区位于待发送区之下: {:?}",
            row(pend_start + pending_h)
        );
    }

    /// 面板样式的覆盖层（如 FleetView）占据输入区：隐藏主页输入框。
    #[test]
    fn panel_overlay_hides_the_main_input_region() {
        use crate::core::extensions::{OverlaySize, OverlayView};

        let mut app = App::new();
        let (_, _, with_input) = layout_heights(&app, 80);
        assert!(with_input > 0, "无覆盖层时输入区有高度");

        crate::modes::interactive::overlay::open(
            &mut app,
            1,
            OverlayView {
                size: OverlaySize::Panel { max_rows: 10 },
                ..OverlayView::inline("Fleet", Vec::new(), 10)
            },
        );
        let (_, _, without) = layout_heights(&app, 80);
        assert_eq!(without, 0, "面板样式应隐藏主页输入框");
    }

    /// 全屏覆盖层底部与主页底栏之间要有一行分割线（`─`），
    /// 否则覆盖层的键位提示与主页底栏贴在一起、看成一堆。
    #[test]
    fn fullscreen_overlay_has_a_separator_before_the_bottom_bar() {
        use crate::core::extensions::{DockSpan, FooterSpan, OverlaySize, OverlayView};
        use ratatui::backend::TestBackend;

        let mut app = App::new();
        let view = OverlayView {
            title: "Viewer".to_string(),
            lines: vec![vec![DockSpan::plain("transcript")]],
            footer: vec![("Esc".to_string(), "close".to_string())],
            input: None,
            editor: None,
            size: OverlaySize::Fullscreen,
            header: Vec::new(),
            messages: Vec::new(),
            selected: None,
            input_focus: false,
        };
        crate::modes::interactive::overlay::open(&mut app, 1, view);

        let footer_lines = vec![vec![FooterSpan::new("dim", "#666666", "model · cwd")]];
        let h = 10u16;
        let w = 40u16;
        let backend = TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let area = Rect::new(0, 0, w, h);
        terminal
            .draw(|f| render_fullscreen_overlay(f, area, &mut app, &[], &footer_lines))
            .unwrap();
        let buf = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (0..w)
                .map(|c| buf.cell((c, y)).unwrap().symbol().to_string())
                .collect()
        };
        // 布局：覆盖层(8) | 分割线(1) | 底栏(1)
        assert!(row(0).contains("Viewer"), "覆盖层在顶: {:?}", row(0));
        assert!(row(8).starts_with('─'), "分割线应在底栏之上: {:?}", row(8));
        assert!(
            row(8).chars().all(|c| c == '─'),
            "整行都是分割线: {:?}",
            row(8)
        );
        assert!(row(9).contains("model · cwd"), "底栏在最下: {:?}", row(9));
    }

    #[test]
    fn input_area_height_is_exactly_cursor_line() {
        let mut app = App::new();
        // 含上下边框：输入区高度 = 内容行数 + 2
        let (_, _, input_h) = layout_heights(&app, 80);
        assert_eq!(input_h, 3, "empty input = 1 content line + 2 borders");

        // 单行输入不折行
        app.editor.insert_text("short");
        let (_, _, input_h) = layout_heights(&app, 80);
        assert_eq!(input_h, 3, "single-line input + 2 borders");

        // 长行输入在窄终端折行：高度 = 折行数
        app.editor.clear();
        app.editor.insert_text(&"x".repeat(30));
        let (_, _, input_h) = layout_heights(&app, 10);
        assert!(
            input_h >= 3,
            "30 chars should wrap to 3+ lines at width 10, actual {}",
            input_h
        );
    }

    #[test]
    fn status_height_fixed_one_line() {
        let mut app = App::new();
        app.status = "single-line status".to_string();
        let (status_h, _, _) = layout_heights(&app, 80);
        assert_eq!(
            status_h, 1,
            "状态区固定 1 行（消息区与输入框之间 1 行间隔）"
        );
        app.status = "first line\nsecond line".to_string();
        let (status_h, _, _) = layout_heights(&app, 80);
        assert_eq!(status_h, 1, "普通状态多行不撑高状态区");
        // 项目信任改为输入区面板，不再撑高状态区
        app.ask_trust = true;
        let (status_h, _, _) = layout_heights(&app, 80);
        assert_eq!(status_h, 1, "项目信任面板不占状态区");
    }

    #[test]
    fn pending_queue_reserves_layout_lines() {
        let app = App::new();
        let (_, pending_h, _) = layout_heights(&app, 80);
        assert_eq!(pending_h, 0, "无排队不占布局");
        app.runtime_steer_inbox
            .lock()
            .unwrap()
            .push_back(crate::core::provider::AgentMessage::user_text("a"));
        app.runtime_steer_inbox
            .lock()
            .unwrap()
            .push_back(crate::core::provider::AgentMessage::user_text("b"));
        let (_, pending_h, _) = layout_heights(&app, 80);
        assert_eq!(pending_h, 4, "空行 + 2 条消息 + 提示行");
    }

    #[test]
    fn scrollbar_appears_right_and_drag_maps_position() {
        // 内容超过可视高度时右侧 1 列渲染滚动条（thumb 顶部 = 内容顶部），
        // 文本区宽度减 1；拖动映射：track 顶端 → scroll=max_scroll（内容顶部）
        use ratatui::backend::TestBackend;
        let mut app = App::new();
        for i in 0..30 {
            app.messages
                .push(crate::core::provider::AgentMessage::user_text(&format!(
                    "msg {}",
                    i
                )));
        }
        let backend = TestBackend::new(50, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let col49: Vec<String> = (0..10u16)
            .map(|y| buf.cell((49, y)).unwrap().symbol().to_string())
            .collect();
        assert!(
            col49.iter().any(|s| s == "█"),
            "滚动条 thumb 出现在右侧列: {:?}",
            col49
        );
        assert!(
            app.mouse.scroll_x == Some(49),
            "滚动条列已记录: {:?}",
            app.mouse.scroll_x
        );
        assert!(
            app.mouse.log_total > 30,
            "总行数已记录: {}",
            app.mouse.log_total
        );
        // 文本不越过滚动条列（内容宽 = 49）
        for y in 0..10u16 {
            let s = (0..49u16)
                .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect::<String>();
            assert!(
                s.trim_end().chars().count() <= 49,
                "row{} 文本超宽: {:?}",
                y,
                s
            );
        }
    }

    #[test]
    fn scroll_to_top_renders_first_message() {
        // 用户把滚动条拖到最上面：scroll=max_scroll 时第一条消息必须显示在消息区顶部
        // （回归：顶部内容丢失，开头几行永远看不到）
        use ratatui::backend::TestBackend;
        let mut app = App::new();
        for i in 0..30 {
            app.messages
                .push(crate::core::provider::AgentMessage::user_text(&format!(
                    "msg {:02}",
                    i
                )));
        }
        let backend = TestBackend::new(50, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        // 与 scrollbar_drag_to 拖到 track 顶的端点一致：scroll = log_total - log_area_h
        app.scroll = app
            .mouse
            .log_total
            .saturating_sub(app.mouse.log_area_h as usize);
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        // 第一条消息是用户消息（Box 上下 padding）→ padding 行在 y=0，正文应在 y=1
        let row1: String = (0..50)
            .map(|x| buf.cell((x, 1)).unwrap().symbol().to_string())
            .collect();
        assert!(
            row1.contains("msg 00"),
            "顶部区域应显示第一条消息: {:?} total={} h={}",
            row1,
            app.mouse.log_total,
            app.mouse.log_area_h
        );
        let all: String = (0..10)
            .flat_map(|y| (0..50).map(move |x| buf.cell((x, y)).unwrap().symbol().to_string()))
            .collect();
        assert!(!all.contains("msg 29"), "滚到顶部后不应显示最后一条消息");
    }

    #[test]
    fn scroll_to_bottom_renders_last_message() {
        // 滚动条贴底（scroll=0）：最后一条消息必须在消息区底部显示（跟随最新）
        use ratatui::backend::TestBackend;
        let mut app = App::new();
        for i in 0..30 {
            app.messages
                .push(crate::core::provider::AgentMessage::user_text(&format!(
                    "msg {:02}",
                    i
                )));
        }
        let backend = TestBackend::new(50, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let all: String = (0..10)
            .flat_map(|y| (0..50).map(move |x| buf.cell((x, y)).unwrap().symbol().to_string()))
            .collect();
        assert!(all.contains("msg 29"), "贴底时最后一条消息可见");
        assert!(!all.contains("msg 00"), "贴底时不应看到第一条消息");
    }

    #[test]
    fn scroll_anchors_content_when_output_grows() {
        // 回归：用户滚上去阅读历史时，新输出（total 增长）不得把视口往下推——
        // 渲染以 scroll（距底部偏移）定位，需按新增行数补偿 scroll 锚定内容行。
        use ratatui::backend::TestBackend;
        let mut app = App::new();
        for i in 0..30 {
            app.messages
                .push(crate::core::provider::AgentMessage::user_text(&format!(
                    "msg {:02}",
                    i
                )));
        }
        let backend = TestBackend::new(50, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();

        // 用户向上滚动（暂停跟随），记下视口顶行锚定的内容行
        app.scroll = app
            .mouse
            .log_total
            .saturating_sub(app.mouse.log_area_h as usize)
            / 2;
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        let anchor = app.mouse.view_start;
        assert!(anchor > 0, "上滚后视口应离开底部: anchor={anchor}");

        // 新输出到达：total 增长，视口必须钉在原内容行上（不得下滚一个文本块）
        app.messages
            .push(crate::core::provider::AgentMessage::user_text(
                "new output block",
            ));
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        assert_eq!(
            app.mouse.view_start, anchor,
            "新输出不得把视口往下推: start {} → {}",
            anchor, app.mouse.view_start
        );

        // 内容增长后用户的向上滚动仍有效
        let before = app.mouse.view_start;
        app.scroll = app.scroll.saturating_add(3);
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        assert!(
            app.mouse.view_start < before,
            "上滚后视口应继续上移: {} → {}",
            before,
            app.mouse.view_start
        );
    }

    #[test]
    fn scroll_anchors_content_when_streaming_grows_and_shrinks() {
        // 回归：用户滚上去阅读时，流式输出末尾内容增长/缩减/清空都不得挪动视口——
        // 暂停跟随下 scroll 必须随 total 双向补偿，保持 start 锚定。
        use ratatui::backend::TestBackend;
        let mut app = App::new();
        for i in 0..30 {
            app.messages
                .push(crate::core::provider::AgentMessage::user_text(&format!(
                    "msg {:02}",
                    i
                )));
        }
        let backend = TestBackend::new(50, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();

        // 用户向上滚动（暂停跟随），记下视口顶行锚定的内容行
        app.scroll = app
            .mouse
            .log_total
            .saturating_sub(app.mouse.log_area_h as usize)
            / 2;
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        let anchor = app.mouse.view_start;
        assert!(anchor > 0, "上滚后视口应离开底部: anchor={anchor}");

        // 流式输出增长：末尾内容变多，视口不得下滚
        app.busy = true;
        app.streaming_text = "long streaming output line\nanother line\n".to_string();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        assert_eq!(
            app.mouse.view_start, anchor,
            "末尾内容变多时视口不得下滚: {} → {}",
            anchor, app.mouse.view_start
        );

        // 末尾文本内容变少（如 pending 样式替换/流式回退），视口不得滚动
        app.streaming_text = "short".to_string();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        assert_eq!(
            app.mouse.view_start, anchor,
            "末尾内容变少时视口不得滚动: {} → {}",
            anchor, app.mouse.view_start
        );

        // 流式结束清空（内容重组为正式消息）：视口不得被推到顶部或贴底
        app.streaming_text.clear();
        app.busy = false;
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        assert!(
            app.mouse.view_start > 0,
            "清空后视口不应跳到底部/顶部: start={}",
            app.mouse.view_start
        );
    }

    #[test]
    fn scrollbar_hidden_when_content_fits() {
        // 内容行数 ≤ 消息区高度时右侧列不渲染滚动条（问题 1 复现检查）
        use ratatui::backend::TestBackend;
        let mut app = App::new();
        // 单条消息 + 空行；消息区高度 10
        app.messages
            .push(crate::core::provider::AgentMessage::user_text("short"));
        let backend = TestBackend::new(50, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let col49: Vec<String> = (0..10u16)
            .map(|y| buf.cell((49, y)).unwrap().symbol().to_string())
            .collect();
        eprintln!(
            "fits col49: {:?} total={} h={}",
            col49, app.mouse.log_total, app.mouse.log_area_h
        );
        assert!(
            app.mouse.scroll_x.is_none(),
            "内容可容纳时不应有滚动条: total={} h={}",
            app.mouse.log_total,
            app.mouse.log_area_h
        );
        // 无滚动条时不预留右侧列：消息背景应铺满到最后一列（x=49），
        // 而不是右侧空置一列（回归：此前无条件 content_width-1）
        let c = buf.cell((49, 0)).unwrap();
        assert_ne!(
            c.style().bg,
            None,
            "内容 fits 时文本区应占满右侧列, style={:?}",
            c.style()
        );
    }

    #[test]
    fn selection_follows_content_across_scroll() {
        // 回归（bug 1）：选中文本后滚动，选择必须钉在内容上而非视口。
        // 贴顶视口选中第一条正文行（内容行 1），下滚 1 行后 start 0→1：
        // 该行从视口行 1 移到行 0，高亮跟随内容，提取文本不变。
        use ratatui::backend::TestBackend;
        let mut app = App::new();
        for i in 0..30 {
            app.messages
                .push(crate::core::provider::AgentMessage::user_text(&format!(
                    "msg {:02}",
                    i
                )));
        }
        let backend = TestBackend::new(50, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        app.scroll = app
            .mouse
            .log_total
            .saturating_sub(app.mouse.log_area_h as usize);
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        assert_eq!(app.mouse.view_start, 0, "贴顶视口顶行 = 内容行 0");

        let texts = render_messages_text(&app, app.mouse.view_width.max(1));
        let sel_line = 1usize;
        assert!(
            !texts[sel_line].trim().is_empty(),
            "内容行 1 应为正文: {:?}",
            texts[sel_line]
        );
        app.mouse.sel = Some((sel_line, 0, sel_line, usize::MAX));
        let before = app.selected_text().unwrap();
        assert_eq!(before, texts[sel_line], "提取 = 该行全文");

        // 下滚 1 行：start 0→1，选中行应移到视口行 0
        app.scroll = app.scroll.saturating_sub(1);
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        assert_eq!(app.mouse.view_start, 1);
        assert_eq!(
            app.selected_text(),
            Some(before),
            "滚动后选中文本不变（钉在内容上）"
        );
        let buf = terminal.backend().buffer();
        // 与生产取色同入口：真彩终端为 Rgb、256 色终端为 Indexed，断言不依赖运行终端
        let sel_bg = Some(color("#a0a0a0"));
        let row0_has = (0..50).any(|x| buf.cell((x, 0)).unwrap().style().bg == sel_bg);
        assert!(row0_has, "滚动后高亮应跟随内容出现在视口行 0");
        let row1_has = (0..50).any(|x| buf.cell((x, 1)).unwrap().style().bg == sel_bg);
        assert!(!row1_has, "视口行 1 已是新内容，不应带原选择高亮");
    }

    #[test]
    fn selection_fully_above_viewport_highlights_nothing() {
        // 回归（bug）：选中旧内容后滚动/LLM 持续输出把选择整体推到视口上方
        // （sel 终点行 < view_start）时，旧实现把 vs/ve 双双 saturating_sub 钳成 0，
        // 视口第一行被整行染成选中背景（表现为"顶部一行被选中"），即使选择根本不在视口内。
        use ratatui::backend::TestBackend;
        let mut app = App::new();
        for i in 0..40 {
            app.messages
                .push(crate::core::provider::AgentMessage::user_text(&format!(
                    "msg {:02}",
                    i
                )));
        }
        let backend = TestBackend::new(50, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        // 贴顶视口：start=0，选中第 2 行整行
        app.scroll = app
            .mouse
            .log_total
            .saturating_sub(app.mouse.log_area_h as usize);
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        assert_eq!(app.mouse.view_start, 0, "贴顶视口顶行 = 内容行 0");
        app.mouse.sel = Some((1, 0, 1, usize::MAX));

        // 滚回底部（LLM 输出贴底跟随的等价状态）：选中的内容行已整体移到视口上方
        app.scroll = 0;
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        assert!(
            app.mouse.view_start > 1,
            "选中行应整体滚出视口上方: start={}",
            app.mouse.view_start
        );
        let buf = terminal.backend().buffer();
        let sel_bg = Some(color("#a0a0a0"));
        let any_sel =
            (0..10u16).any(|y| (0..50).any(|x| buf.cell((x, y)).unwrap().style().bg == sel_bg));
        assert!(!any_sel, "选择整体在视口上方时不得高亮任何行");
    }

    #[test]
    fn selection_pushed_above_viewport_highlights_nothing() {
        // 回归（bug 直接函数级）：选中旧内容后滚动 / LLM 持续输出把选择整体推到视口上方
        // （sel 终点行 < view_start）时，旧实现把 vs/ve 双双 saturating_sub 钳成 0，
        // 视口第一行被整行染成选中背景（"顶部一行被选中"），即使选择根本不在视口内。
        // 直接构造受控可见行断言（与集成渲染无关，空行/消息分隔不掩盖行为）。
        let mk = |rows: usize| -> Vec<Line<'static>> {
            (0..rows).map(|i| Line::from(format!("row {i}"))).collect()
        };
        let sel_bg = Some(color("#a0a0a0"));
        let has_sel = |lines: &[Line<'static>]| {
            lines
                .iter()
                .any(|l| l.spans.iter().any(|s| s.style.bg == sel_bg))
        };

        // 选择整体在视口上方（行 1 < view_start 100）：不得高亮任何行
        let mut lines = mk(5);
        let mouse = MouseSel {
            visible: vec!["x".to_string(); 5],
            view_start: 100,
            sel: Some((1, 0, 1, usize::MAX)),
            ..Default::default()
        };
        apply_selection_highlight(&mut lines, &mouse, &Theme::default());
        assert!(
            !has_sel(&lines),
            "选择整体在视口上方时不得高亮任何行（尤其顶部行）"
        );

        // 对照：选择起点在视口上方、终点落入视口时，顶部相交行仍应高亮（正确裁剪）
        let mut lines = mk(5);
        let mouse = MouseSel {
            visible: vec!["x".to_string(); 5],
            view_start: 100,
            sel: Some((99, 0, 102, usize::MAX)),
            ..Default::default()
        };
        apply_selection_highlight(&mut lines, &mouse, &Theme::default());
        assert!(
            has_sel(&lines),
            "选区延伸到视口内时应从顶部行起高亮相交部分"
        );
        // 选择整体在视口下方：不得高亮
        let mut lines = mk(5);
        let mouse = MouseSel {
            visible: vec!["x".to_string(); 5],
            view_start: 100,
            sel: Some((110, 0, 120, usize::MAX)),
            ..Default::default()
        };
        apply_selection_highlight(&mut lines, &mouse, &Theme::default());
        assert!(!has_sel(&lines), "选择整体在视口下方时不得高亮任何行");
    }

    #[test]
    fn selection_reverse_drag_normalizes() {
        // 反向拖选（终点早于起点）与正向提取结果一致；跨多行同理
        use ratatui::backend::TestBackend;
        let mut app = App::new();
        for i in 0..30 {
            app.messages
                .push(crate::core::provider::AgentMessage::user_text(&format!(
                    "msg {:02}",
                    i
                )));
        }
        let backend = TestBackend::new(50, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        let start = app.mouse.view_start;
        let h = app.mouse.log_area_h as usize;
        debug_assert!(h >= 3, "测试视口至少 3 行");
        let (a, b) = (start + 1, start + h - 1);
        let texts = render_messages_text(&app, 50);
        let ca = texts[a].chars().count().saturating_sub(2);
        let cb = 2usize;
        let forward = {
            app.mouse.sel = Some((a, cb, b, ca));
            app.selected_text().unwrap()
        };
        // 反向：sel 起点换到 (b, ca)，规范化后必须与正向一致
        app.mouse.sel = Some((b, ca, a, cb));
        assert_eq!(app.selected_text(), Some(forward), "反向拖拽提取方向一致");
    }

    #[test]
    fn bug_report_hint_cursor_sits_on_description_input_line() {
        // 回归：/bug 描述面板的说明在输入行上方且会折行；光标曾固定按默认 +4 定位，
        // 落在说明文字中间。现在光标行随说明折行数移动。
        use crate::modes::interactive::app::PendingBugReport;
        use crate::modes::interactive::panel::PanelKind;
        use ratatui::backend::TestBackend;
        let w = 60usize;
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

        let (_, pending_h, input_h) = layout_heights(&app, w);
        let mut terminal =
            ratatui::Terminal::new(TestBackend::new(w as u16, 2 + pending_h + input_h)).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();

        let input_y = 1 + pending_h + 1; // 消息区 1 + 状态区 1
        let pos = terminal.get_cursor_position().unwrap();
        // 输入行紧跟说明折行之后：空/标题/空/N 行说明/空/输入行
        let notes = crate::modes::interactive::render::panel::bug_report_note_line_count(&app, w);
        assert_eq!(
            pos.y,
            input_y + 5 + notes as u16,
            "光标应在描述输入行: input_y={input_y} notes={notes} pos={pos:?}"
        );
        assert_eq!(
            pos.x,
            2 + "crash on start".len() as u16,
            "cursor x 应在描述末尾: pos={pos:?}"
        );

        // 光标行上确实是 `> ` 开头的输入行（而非说明文字）
        let buffer = terminal.backend().buffer();
        let row: String = (0..w as u16)
            .map(|x| buffer[(x, pos.y)].symbol().to_string())
            .collect();
        assert!(row.starts_with("> crash on start"), "输入行: {row:?}");
    }

    #[test]
    fn scoped_models_cursor_sits_on_search_line() {
        // 回归：/scoped-models 光标曾落在搜索行上一行（+4）；
        // 该面板搜索行在 空/标题/副标题/空 之后（内容第 5 行，边框+1 → +5）。
        use crate::modes::interactive::panel::{PanelItem, PanelKind};
        use ratatui::backend::TestBackend;
        let w = 60usize;
        let mut app = App::new();
        app.panel.open(
            PanelKind::ScopedModels,
            "Model Configuration".to_string(),
            vec![
                PanelItem {
                    label: "[x] deepseek-v4-pro".to_string(),
                    value: "deepseek\0deepseek-v4-pro".to_string(),
                    desc: "[deepseek]".to_string(),
                    name: "deepseek-v4-pro".to_string(),
                    ..Default::default()
                },
                PanelItem {
                    label: "[ ] deepseek-flash".to_string(),
                    value: "deepseek\0deepseek-flash".to_string(),
                    desc: "[deepseek]".to_string(),
                    name: "deepseek-flash".to_string(),
                    ..Default::default()
                },
            ],
        );
        app.panel.top_mut().unwrap().filter.set_value("a");
        let (_, pending_h, input_h) = layout_heights(&app, w);
        // 满一帧：消息区(min 1) + 待发送 + dock(0) + 状态(1) + 输入区
        let mut terminal =
            ratatui::Terminal::new(TestBackend::new(w as u16, 2 + pending_h + input_h)).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        let input_y = 1 + pending_h + 1; // 消息区 1 + 状态区 1
        let pos = terminal.get_cursor_position().unwrap();
        assert_eq!(
            pos.y,
            input_y + 5,
            "光标应在搜索输入行: input_y={input_y} pos={pos:?}"
        );
        assert_eq!(pos.x, 3, "cursor x 应跟随过滤文本: pos={pos:?}");
    }

    #[test]
    fn extension_cursor_sits_on_search_line() {
        // 回归：/extension 光标曾落在搜索行上一行（+4）；
        // 该面板搜索行在 空/标题/副标题/空 之后（内容第 5 行，边框+1 → +5）。
        use crate::modes::interactive::panel::{PanelItem, PanelKind};
        use ratatui::backend::TestBackend;
        let w = 60usize;
        let mut app = App::new();
        app.panel.open(
            PanelKind::Extension,
            "/extension".to_string(),
            vec![PanelItem {
                label: "[x] footer(normal)".to_string(),
                value: "footer(normal)".to_string(),
                desc: "ℹ".to_string(),
                ..Default::default()
            }],
        );
        app.panel.top_mut().unwrap().filter.set_value("a");
        let (_, pending_h, input_h) = layout_heights(&app, w);
        let mut terminal =
            ratatui::Terminal::new(TestBackend::new(w as u16, 2 + pending_h + input_h)).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        let input_y = 1 + pending_h + 1;
        let pos = terminal.get_cursor_position().unwrap();
        assert_eq!(
            pos.y,
            input_y + 5,
            "光标应在搜索输入行: input_y={input_y} pos={pos:?}"
        );
        assert_eq!(pos.x, 3, "cursor x 应跟随过滤文本: pos={pos:?}");
    }

    #[test]
    fn skill_cursor_sits_on_search_line() {
        // 回归：/skills 光标曾落在搜索行上一行（+4）；
        // 该面板搜索行在 空/标题/副标题/空 之后（内容第 5 行，边框+1 → +5）。
        use crate::modes::interactive::panel::{PanelItem, PanelKind};
        use ratatui::backend::TestBackend;
        let w = 60usize;
        let mut app = App::new();
        app.panel.open(
            PanelKind::Skill,
            "/skills".to_string(),
            vec![PanelItem {
                label: "[x] grill-me".to_string(),
                value: "grill-me".to_string(),
                desc: "[model-off]".to_string(),
                ..Default::default()
            }],
        );
        app.panel.top_mut().unwrap().filter.set_value("a");
        let (_, pending_h, input_h) = layout_heights(&app, w);
        let mut terminal =
            ratatui::Terminal::new(TestBackend::new(w as u16, 2 + pending_h + input_h)).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        let input_y = 1 + pending_h + 1;
        let pos = terminal.get_cursor_position().unwrap();
        assert_eq!(
            pos.y,
            input_y + 5,
            "光标应在搜索输入行: input_y={input_y} pos={pos:?}"
        );
        assert_eq!(pos.x, 3, "cursor x 应跟随过滤文本: pos={pos:?}");
    }

    #[test]
    fn skill_cursor_follows_error_lines() {
        // 存盘失败提示插在副标题与搜索行之间（可能折行）→ 光标必须跟着下移，
        // 否则会停在错误行上。
        use crate::modes::interactive::panel::{PanelItem, PanelKind};
        use ratatui::backend::TestBackend;
        let w = 40usize;
        let mut app = App::new();
        app.skill_panel_error = Some(
            "failed to save skills: permission denied writing settings.json backup copy"
                .to_string(),
        );
        app.panel.open(
            PanelKind::Skill,
            "/skills".to_string(),
            vec![PanelItem {
                label: "[x] grill-me".to_string(),
                value: "grill-me".to_string(),
                ..Default::default()
            }],
        );
        let (_, pending_h, input_h) = layout_heights(&app, w);
        let mut terminal =
            ratatui::Terminal::new(TestBackend::new(w as u16, 2 + pending_h + input_h)).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        let input_y = 1 + pending_h + 1;
        let pos = terminal.get_cursor_position().unwrap();
        assert_eq!(
            pos.y,
            input_y + 5 + panel::skills_error_line_count(&app, w) as u16,
            "光标应随错误行下移: input_y={input_y} pos={pos:?}"
        );
    }

    #[test]
    fn history_clear_confirm_hides_cursor() {
        // 回归：删除历史确认面板没有输入行，不得把光标留在面板区域内
        // （此前未列入隐藏名单，光标会停在无输入行的地方）。
        use crate::modes::interactive::panel::{PanelItem, PanelKind};
        use ratatui::backend::TestBackend;
        let w = 60usize;
        let mut app = App::new();
        app.pending_history_clear = Some(crate::modes::interactive::app::PendingHistoryClear {
            all: false,
            detail: "proj · 3 entries\n/tmp/prux/history/proj.jsonl".to_string(),
        });
        app.panel.open(
            PanelKind::HistoryClearConfirm,
            "Clear prompt history for this project".to_string(),
            vec![
                PanelItem::new("Yes".to_string(), "yes".to_string()),
                PanelItem::new("No".to_string(), "no".to_string()),
            ],
        );
        let (_, pending_h, input_h) = layout_heights(&app, w);
        let mut terminal =
            ratatui::Terminal::new(TestBackend::new(w as u16, 2 + pending_h + input_h)).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        assert!(
            !terminal.backend().cursor_visible(),
            "删除历史确认面板应隐藏光标"
        );
    }

    #[test]
    fn ext_settings_panel_replaces_input_area() {
        // 扩展设置面板与 /settings 一样占据输入区：隐藏输入框，输入区高度 = 面板高度。
        use crate::core::extensions::{Extension, ExtensionSetting, ExtensionTool};
        use ratatui::backend::TestBackend;

        struct FakeExt;
        impl Extension for FakeExt {
            fn name(&self) -> &str {
                "fake-frame-settings"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
            fn settings(&self) -> Vec<ExtensionSetting> {
                vec![ExtensionSetting::cycle(
                    "m",
                    "Mode",
                    "a settable mode",
                    "a",
                    ["a", "b"],
                )]
            }
        }

        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::core::extensions::register_extension(FakeExt);

        let mut app = App::new();
        // 先记录普通输入框高度（3 行：上下边框 + 1 内容）
        let (_, pending_h, plain_input_h) = layout_heights(&app, 60);
        assert_eq!(plain_input_h, 3, "空输入框 = 1 内容行 + 2 边框");

        app.ext_settings.open("fake-frame-settings");
        let ph = ext_settings::panel_height(&app, 60);
        let (status_h, pending_h2, input_h) = layout_heights(&app, 60);
        assert_eq!(pending_h, pending_h2);
        assert_eq!(input_h, ph, "打开面板后输入区应等于面板高度（输入框隐藏）");
        assert!(input_h > plain_input_h, "面板应高于普通输入框");

        let h = 1 + pending_h + status_h + input_h;
        let mut terminal = ratatui::Terminal::new(TestBackend::new(60, h)).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (0..60u16)
                .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect()
        };

        // 面板标题（扩展名）在输入区顶部边框上，搜索行在其下一行（边框 + 空行）
        let input_y = 1 + pending_h + status_h;
        assert!(
            row(input_y).contains("fake-frame-settings"),
            "面板标题应在顶部边框: {:?}",
            row(input_y)
        );
        assert!(
            row(input_y + 2).starts_with("> "),
            "搜索行应位于边框下第二行: {:?}",
            row(input_y + 2)
        );
        assert!(
            row(input_y + input_h - 1).contains('─'),
            "面板下边框应贴输入区底部: {:?}",
            row(input_y + input_h - 1)
        );

        // 关闭面板后恢复普通输入框高度
        app.ext_settings.close();
        let (_, _, restored) = layout_heights(&app, 60);
        assert_eq!(restored, plain_input_h, "关闭后恢复输入框");

        crate::core::extensions::unregister_extension("fake-frame-settings");
    }
}
