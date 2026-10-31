//! 侧栏：会话列表（读 `agent_dir()/sessions`），按修改时间分组。
//!
//! 顶部的搜索框按标题过滤列表。
//!
//! 列表分两段算：目录列举（[`paths`]，只看 mtime，快）留在主线程，
//! 逐条解析会话元数据（整文件读，慢）交给 [`ProgressiveMetas`] 的后台线程，
//! 渲染时只取已就绪的那部分。

use super::{
    components::{dot, section_label},
    theme::{
        CONTROL_H_LG, CONTROL_H_SM, DOT_SIZE, FONT_CAPTION, FONT_SMALL, HEADER_H, RADIUS_MD,
        RADIUS_SM, SPACE_MD, SPACE_SM, SPACE_XS, SPACE_XXS, Theme, px,
    },
};
use crate::{
    cli::session_picker::ProgressiveMetas,
    core::{
        session_manager::{default_session_dir, list_sessions},
        settings_manager::agent_dir,
    },
    modes::interactive::agent_actor::{AgentCommand, CommandTx},
    utils::{display::fit_display, paths::dir_name},
};
use gpui::{
    AnyElement, IntoElement, MouseButton, ParentElement, ScrollHandle, SharedString, Styled, div,
    prelude::*,
};
use std::{path::PathBuf, rc::Rc};

/// 点击类回调（视图层挂事件用）。
type ClickHandler = Box<dyn Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App)>;

/// 会话行右键回调：参数是会话文件路径。
type ContextHandler = Rc<dyn Fn(&str, &gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App)>;

/// 会话按最近活动时间分的组。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    /// 24 小时内
    Today,
    /// 一周内
    Week,
    /// 更早
    Older,
}

impl Group {
    /// 分组标题。
    fn label(self) -> &'static str {
        match self {
            Group::Today => "Today",
            Group::Week => "This week",
            Group::Older => "Older",
        }
    }
}

/// 侧栏里的一行会话。
pub struct SessionRow {
    /// 会话文件路径（切会话用）
    pub path: String,
    /// 展示标题：会话名优先，否则首条用户消息，再否则会话 id
    pub title: String,
    /// 相对修改时间（`now` / `5m` / `2d`…）
    pub age: String,
    /// 消息条数
    pub messages: usize,
    /// 分组
    group: Group,
}

/// 列出当前 cwd 的会话文件（按最近修改倒序，不解析内容）。
pub fn paths(cwd: &str) -> Vec<PathBuf> {
    list_sessions(&default_session_dir(cwd, &agent_dir()))
}

/// 把已解析完的元数据组装成侧栏行（未就绪的会话不出现）。
///
/// 消息数为 0 的会话也列出：空会话可能是刚建的、或还没写入内容，藏起来会让人
/// 以为列表坏了（标题退回到文件名）。
pub fn rows(loader: &ProgressiveMetas) -> Vec<SessionRow> {
    loader
        .visible()
        .map(|(_, path, meta)| {
            let title = meta
                .name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .or_else(|| {
                    let first = meta.first_message.trim();
                    (!first.is_empty()).then(|| fit_display(first, 32))
                })
                .unwrap_or_else(|| {
                    path.file_stem()
                        .map(|s| fit_display(&s.to_string_lossy(), 18))
                        .unwrap_or_default()
                });
            SessionRow {
                path: path.to_string_lossy().to_string(),
                title,
                group: group_of(&meta.modified_age),
                age: meta.modified_age.clone(),
                messages: meta.message_count,
            }
        })
        .collect()
}

/// 相对时长 → 分组。
fn group_of(age: &str) -> Group {
    if age == "now" || age.ends_with('m') || age.ends_with('h') {
        Group::Today
    } else if age.ends_with('d') || age.ends_with('w') {
        Group::Week
    } else {
        Group::Older
    }
}

/// 当前会话在列表中的下标（按文件路径匹配）。
pub fn current_index(rows: &[&SessionRow], current: Option<&str>) -> Option<usize> {
    let current = current?;
    rows.iter().position(|row| row.path == current)
}

/// 侧栏渲染入参：列表数据、滚动句柄与交互回调。
///
/// 滚动条由调用方按 [`ScrollHandle`] 生成后传进来，这样拖动事件与滚动句柄待在
/// 同一处（视图层），侧栏本身只负责摆位置。
pub struct SidebarCtx<'a> {
    /// 设计令牌
    pub theme: &'a Theme,
    /// 当前工作目录（底部展示）
    pub cwd: &'a str,
    /// 当前 git 分支（非 git 仓库时为 None）
    pub branch: Option<&'a str>,
    /// 侧栏宽度（逻辑像素，可拖拽改变）
    pub width: f32,
    /// 会话行
    pub rows: &'a [SessionRow],
    /// 搜索框内容（空串显示占位文案）
    pub query: &'a str,
    /// 搜索框是否聚焦（显示光标与高亮边框）
    pub search_focused: bool,
    /// 点搜索框：聚焦它
    pub focus_search: ClickHandler,
    /// 当前会话文件路径
    pub current: Option<&'a str>,
    /// 是否还有会话元数据在读
    pub loading: bool,
    /// 列表滚动句柄
    pub scroll: &'a ScrollHandle,
    /// 列表滚动条元素
    pub scrollbar: AnyElement,
    /// 命令下发端（点行切会话用）
    pub cmd_tx: CommandTx,
    /// 点「+ 新会话」
    pub new_session: ClickHandler,
    /// 会话行右键：参数是会话文件路径
    pub context_menu: ContextHandler,
    /// 点底部工作目录：切目录
    pub change_cwd: ClickHandler,
}

/// 渲染侧栏。
pub fn render(ctx: SidebarCtx<'_>) -> AnyElement {
    let SidebarCtx {
        theme,
        cwd,
        branch,
        width,
        rows,
        current,
        loading,
        query,
        search_focused,
        focus_search,
        scroll,
        scrollbar,
        cmd_tx,
        new_session,
        context_menu,
        change_cwd,
    } = ctx;

    let mut column = div()
        .w(px(width))
        .h_full()
        .flex_none()
        .flex()
        .flex_col()
        .bg(theme.sidebar)
        .border_r_1()
        .border_color(theme.border);

    // 品牌行
    column = column.child(brand_row(theme, new_session));

    // 会话搜索框
    column = column.child(search_box(theme, query, search_focused, focus_search));

    let needle = query.trim().to_lowercase();
    let rows = filter(rows, &needle);
    let active = current_index(&rows, current);

    // 会话列表：与滚动条同放一层相对容器，滚动条盖在列表右侧、不占布局宽度
    column = column.child(
        div()
            .relative()
            .flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(session_list(
                theme,
                &rows,
                active,
                loading,
                query,
                scroll,
                cmd_tx,
                context_menu,
            ))
            .child(scrollbar),
    );

    // 底部：工作目录 + git 分支（点击弹系统目录选择器）
    column = column.child(footer_block(theme, cwd, branch, change_cwd));

    column.into_any_element()
}

/// 品牌行：产品名 + 「+ New session」按钮。
fn brand_row(theme: &Theme, new_session: ClickHandler) -> AnyElement {
    let bar = div()
        .flex()
        .items_center()
        .gap(px(SPACE_SM))
        .h(px(HEADER_H))
        .px(px(SPACE_MD))
        .child(dot(theme.accent, DOT_SIZE))
        .child(
            div()
                .text_size(px(FONT_SMALL))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(SharedString::from("prux")),
        )
        .child(div().flex_1())
        .child(
            div()
                .id("new-session")
                .h(px(CONTROL_H_SM))
                .px(px(SPACE_SM))
                .rounded(px(RADIUS_SM))
                .flex()
                .items_center()
                .text_size(px(FONT_CAPTION))
                .bg(theme.overlay)
                .text_color(theme.text_secondary)
                .cursor_pointer()
                .hover(|style| style.bg(theme.overlay_strong).text_color(theme.text))
                .on_click(new_session)
                .child(SharedString::from("+ New session")),
        );
    bar.into_any_element()
}

/// 按标题过滤会话行（大小写不敏感）；`needle` 需已 trim + 转小写，空串不过滤。
fn filter<'a>(rows: &'a [SessionRow], needle: &str) -> Vec<&'a SessionRow> {
    rows.iter()
        .filter(|row| needle.is_empty() || row.title.to_lowercase().contains(needle))
        .collect()
}

/// 会话搜索框：点一下开始输入，框内显示光标。
fn search_box(theme: &Theme, query: &str, focused: bool, focus: ClickHandler) -> AnyElement {
    let empty = query.is_empty();
    div()
        .id("session-search")
        .mx(px(SPACE_XS))
        .mb(px(SPACE_XS))
        .h(px(CONTROL_H_LG))
        .px(px(SPACE_SM))
        .rounded(px(RADIUS_MD))
        .flex()
        .items_center()
        .bg(theme.inset)
        .border_1()
        .border_color(if focused {
            theme.border_strong
        } else {
            theme.border
        })
        .text_size(px(FONT_SMALL))
        .text_color(if empty { theme.text_ghost } else { theme.text })
        .cursor_pointer()
        .on_click(focus)
        .child(
            div()
                .flex()
                .items_center()
                .child(SharedString::from(if empty {
                    "Search sessions…".to_string()
                } else {
                    query.to_string()
                }))
                .child(
                    div()
                        .w(px(if focused && !empty { 1.5 } else { 0. }))
                        .h(px(FONT_SMALL))
                        .ml(px(SPACE_XXS))
                        .bg(theme.accent),
                ),
        )
        .into_any_element()
}

/// 会话列表：按时间分组，点击切会话、右键弹行菜单。
fn session_list(
    theme: &Theme,
    rows: &[&SessionRow],
    active: Option<usize>,
    loading: bool,
    query: &str,
    scroll: &ScrollHandle,
    cmd_tx: CommandTx,
    context_menu: ContextHandler,
) -> AnyElement {
    let mut list = div()
        .id("sessions")
        .flex()
        .flex_col()
        .flex_1()
        .h_full()
        .w_full()
        .min_h_0()
        .gap(px(SPACE_XS))
        .px(px(SPACE_XS))
        .py(px(SPACE_XS))
        .pb(px(SPACE_MD))
        .overflow_y_scroll()
        .track_scroll(scroll);

    if rows.is_empty() {
        list = list.child(
            div()
                .px(px(SPACE_SM))
                .py(px(SPACE_XS))
                .text_size(px(FONT_SMALL))
                .text_color(theme.text_ghost)
                .child(SharedString::from(if !query.trim().is_empty() {
                    "No matching sessions"
                } else if loading {
                    "Loading sessions…"
                } else {
                    "No sessions yet"
                })),
        );
    }

    let mut last_group: Option<Group> = None;
    for (index, row) in rows.iter().enumerate() {
        if last_group != Some(row.group) {
            last_group = Some(row.group);
            list = list.child(
                div()
                    .mt(px(SPACE_XS))
                    .child(section_label(theme, row.group.label())),
            );
        }

        let mut entry = div()
            .w_full()
            .px(px(SPACE_SM))
            .py(px(SPACE_XS))
            .rounded(px(RADIUS_MD))
            .flex()
            .flex_col()
            .gap(px(SPACE_XXS))
            .cursor_pointer()
            .hover(|style| style.bg(theme.overlay));

        if active == Some(index) {
            entry = entry.bg(theme.overlay_strong);
        }

        entry = entry
            .child(
                div()
                    .text_size(px(FONT_SMALL))
                    .text_color(if active == Some(index) {
                        theme.text
                    } else {
                        theme.text_secondary
                    })
                    .truncate()
                    .child(SharedString::from(row.title.clone())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(SPACE_XS))
                    .text_size(px(FONT_CAPTION))
                    .text_color(theme.text_ghost)
                    .child(SharedString::from(row.age.clone()))
                    .child(SharedString::from(format!("· {} messages", row.messages))),
            );

        let tx = cmd_tx.clone();
        let path = row.path.clone();
        let menu = context_menu.clone();
        let menu_path = row.path.clone();

        list = list.child(
            entry
                .id(("session", index))
                .on_mouse_down(MouseButton::Right, move |event, window, cx| {
                    menu(&menu_path, event, window, cx);
                })
                .on_click(move |_, _, _| {
                    _ = tx.send(AgentCommand::ResumeSession { path: path.clone() });
                }),
        );
    }
    list.into_any_element()
}

/// 底部：git 分支与工作目录，整块可点（弹目录选择器）。
fn footer_block(
    theme: &Theme,
    cwd: &str,
    branch: Option<&str>,
    change_cwd: ClickHandler,
) -> AnyElement {
    let mut footer = div()
        .id("cwd")
        .w_full()
        .px(px(SPACE_XS))
        .py(px(SPACE_XS))
        .rounded(px(RADIUS_MD))
        .flex()
        .flex_col()
        .gap(px(SPACE_XXS))
        .cursor_pointer()
        .hover(|style| style.bg(theme.overlay))
        .on_click(change_cwd);
    if let Some(branch) = branch {
        footer = footer.child(
            div()
                .flex()
                .items_center()
                .gap(px(SPACE_XS))
                .text_size(px(FONT_CAPTION))
                .text_color(theme.text_ghost)
                .child(SharedString::from("    "))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_size(px(FONT_CAPTION))
                        .text_color(theme.text_ghost)
                        .child(SharedString::from(branch.to_string())),
                ),
        );
    }

    footer = footer.child(
        div()
            .flex()
            .items_center()
            .gap(px(SPACE_XS))
            .child(
                div()
                    .flex_none()
                    .text_size(px(FONT_CAPTION))
                    .text_color(theme.text_ghost)
                    .child(SharedString::from("📁 ")),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(px(FONT_CAPTION))
                    .text_color(theme.text_secondary)
                    .child(SharedString::from(dir_name(cwd))),
            ),
    );
    footer.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一行会话（只填过滤用到的字段）。
    fn row(title: &str) -> SessionRow {
        SessionRow {
            path: String::new(),
            title: title.to_string(),
            age: "now".to_string(),
            messages: 0,
            group: Group::Today,
        }
    }

    /// 标题过滤：大小写不敏感，空查询全留。
    #[test]
    fn filter_matches_titles_ignoring_case() {
        let rows = vec![row("git commit"), row("Refactor Sidebar")];
        assert_eq!(filter(&rows, "").len(), 2);
        assert_eq!(filter(&rows, "git")[0].title, "git commit");
        assert_eq!(filter(&rows, "sidebar")[0].title, "Refactor Sidebar");
        assert!(filter(&rows, "nope").is_empty());
    }
}
