//! 侧栏：会话列表（读 `agent_dir()/sessions`），按修改时间分组。

use super::components::{dot, ellipsis, section_label};
use super::theme::Theme;
use crate::core::session_manager::{default_session_dir, list_sessions, session_meta};
use crate::core::settings_manager::agent_dir;
use crate::modes::interactive::agent_actor::{AgentCommand, CommandTx};
use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, prelude::*, px};

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
            Group::Today => "今天",
            Group::Week => "本周",
            Group::Older => "更早",
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

/// 读取当前 cwd 的会话列表（按最近修改倒序）。
pub fn load(cwd: &str) -> Vec<SessionRow> {
    let dir = default_session_dir(cwd, &agent_dir());
    list_sessions(&dir)
        .into_iter()
        .filter_map(|path| {
            let meta = session_meta(&path)?;
            let title = meta
                .name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .or_else(|| {
                    let first = meta.first_message.trim();
                    (!first.is_empty()).then(|| truncate(first, 32))
                })
                .unwrap_or_else(|| {
                    path.file_stem()
                        .map(|s| truncate(&s.to_string_lossy(), 18))
                        .unwrap_or_default()
                });
            Some(SessionRow {
                path: path.to_string_lossy().to_string(),
                title,
                group: group_of(&meta.modified_age),
                age: meta.modified_age,
                messages: meta.message_count,
            })
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
pub fn current_index(rows: &[SessionRow], current: Option<&str>) -> Option<usize> {
    let current = current?;
    rows.iter().position(|row| row.path == current)
}

/// 渲染侧栏。
pub fn render(
    theme: &Theme,
    cwd: &str,
    rows: &[SessionRow],
    current: Option<&str>,
    selected: usize,
    cmd_tx: CommandTx,
    on_new_session: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let active = current_index(rows, current);
    let mut column = div()
        .w(px(252.))
        .h_full()
        .flex_none()
        .flex()
        .flex_col()
        .bg(theme.sidebar)
        .border_r_1()
        .border_color(theme.border);

    // 品牌行
    column = column.child(
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(40.))
            .px(px(12.))
            .child(dot(theme.accent, 7.))
            .child(
                div()
                    .text_size(px(12.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from("prux")),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("new-session")
                    .h(px(20.))
                    .px(px(7.))
                    .rounded(px(5.))
                    .flex()
                    .items_center()
                    .text_size(px(11.))
                    .bg(theme.overlay)
                    .text_color(theme.text_secondary)
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.overlay_strong).text_color(theme.text))
                    .on_click(on_new_session)
                    .child(SharedString::from("+ 新会话")),
            ),
    );

    // 会话列表
    let mut list = div()
        .id("sessions")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .gap(px(2.))
        .px(px(6.))
        .py(px(4.))
        .overflow_y_scroll();
    list = list.child(section_label(theme, "会话"));
    if rows.is_empty() {
        list = list.child(
            div()
                .px(px(8.))
                .py(px(6.))
                .text_size(px(11.5))
                .text_color(theme.text_ghost)
                .child(SharedString::from("还没有会话")),
        );
    }
    let mut last_group: Option<Group> = None;
    for (index, row) in rows.iter().enumerate() {
        if last_group != Some(row.group) {
            last_group = Some(row.group);
            list = list.child(
                div()
                    .mt(px(6.))
                    .child(section_label(theme, row.group.label())),
            );
        }
        let mut entry = div()
            .w_full()
            .px(px(8.))
            .py(px(6.))
            .rounded(px(6.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .cursor_pointer();
        if index == selected && active != Some(index) {
            entry = entry.bg(theme.overlay);
        }
        if active == Some(index) {
            entry = entry.bg(theme.overlay_strong);
        }
        entry = entry
            .child(
                div()
                    .text_size(px(12.5))
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
                    .gap(px(6.))
                    .text_size(px(10.5))
                    .text_color(theme.text_ghost)
                    .child(SharedString::from(row.age.clone()))
                    .child(SharedString::from(format!("· {} 条", row.messages))),
            );
        let tx = cmd_tx.clone();
        let path = row.path.clone();
        list = list.child(entry.id(("session", index)).on_click(move |_, _, _| {
            let _ = tx.send(AgentCommand::ResumeSession { path: path.clone() });
        }));
    }
    column = column.child(list);

    // 底部：当前工作目录
    column = column.child(
        div()
            .border_t_1()
            .border_color(theme.border)
            .px(px(12.))
            .py(px(8.))
            .child(ellipsis(basename(cwd), theme.text_ghost, 10.5)),
    );
    column.into_any_element()
}

/// 路径最后一段（项目名）。
fn basename(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// 按字符截断并补省略号。
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}
