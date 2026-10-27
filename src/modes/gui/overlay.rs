//! 覆盖层面板：可过滤列表（命令面板 / 模型 / 会话 / 思考级别 / 主题）与只读信息页。

use super::text_edit::Editor;
use super::theme::{Theme, px};
use crate::modes::interactive::agent_actor::AgentCommand;
use gpui::{
    AnyElement, InteractiveElement, IntoElement, ParentElement, ScrollHandle, SharedString,
    StatefulInteractiveElement, Styled, div,
};

/// 列表项被选中后要做的事。
pub enum Action {
    /// 不做事（纯展示）
    None,
    /// 下发一条命令（装箱：AgentCommand 体积远大于其它变体）
    Command(Box<AgentCommand>),
    /// 切换到某个会话文件
    LoadSession(String),
    /// 切换明暗主题
    SetDark(bool),
    /// 新建会话
    NewSession,
    /// 打开另一个列表面板
    Open(ListKind),
    /// 打开一个信息页
    Info(InfoKind),
    /// 刷新会话列表
    RefreshSessions,
    /// 设置界面缩放系数（1.0 = 设计稿尺寸）
    Scale(f32),
    /// 退出
    Quit,
    /// 插入附件引用（`@路径`）：`images` 为真时补全只列图片
    Attach {
        /// 是否只列图片
        images: bool,
    },
}

/// 可打开的列表面板种类。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    /// 模型
    Model,
    /// 会话
    Session,
    /// 思考级别
    Thinking,
    /// 外观（明/暗）
    Appearance,
    /// 右侧面板：变更
    PanelChanges,
    /// 右侧面板：文件
    PanelFiles,
}

/// 内置信息页。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InfoKind {
    /// 用量与费用
    Usage,
    /// 技能列表
    Skills,
    /// settings.json 现状
    Settings,
    /// 快捷键
    Help,
}

/// 列表项。
pub struct Entry {
    /// 主文案
    pub label: String,
    /// 右侧次要说明
    pub detail: String,
    /// 选中后的动作
    pub action: Action,
}

/// 可过滤列表。
pub struct List {
    /// 面板标题
    pub title: String,
    /// 过滤输入
    pub query: Editor,
    /// 全部条目
    pub entries: Vec<Entry>,
    /// 当前高亮项（在过滤后的列表里的下标）
    pub selected: usize,
    /// 列表视口（键盘移动选中项时要把它滚进视野）
    scroll: ScrollHandle,
}

impl List {
    /// 新建列表（过滤框为空）。
    pub fn new(title: impl Into<String>, entries: Vec<Entry>) -> Self {
        Self {
            title: title.into(),
            query: Editor::new(),
            entries,
            selected: 0,
            scroll: ScrollHandle::new(),
        }
    }

    /// 过滤后的条目下标。
    pub fn filtered(&self) -> Vec<usize> {
        let query = self.query.text().to_lowercase();
        let query = query.trim();
        if query.is_empty() {
            return (0..self.entries.len()).collect();
        }
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.label.to_lowercase().contains(query)
                    || entry.detail.to_lowercase().contains(query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// 上/下移动高亮项，并把新选中项滚进视口。
    pub fn move_selection(&mut self, delta: i32) {
        let len = self.filtered().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let current = self.selected.min(len - 1) as i32 + delta;
        self.selected = current.clamp(0, len as i32 - 1) as usize;
        self.scroll.scroll_to_item(self.selected);
    }

    /// 当前选中的动作（没有条目时为 None）。
    pub fn take_action(&self) -> Option<&Action> {
        let filtered = self.filtered();
        let index = *filtered.get(self.selected.min(filtered.len().checked_sub(1)?))?;
        Some(&self.entries[index].action)
    }
}

/// 只读信息页。
pub struct Info {
    /// 面板标题
    pub title: String,
    /// 逐行文本
    pub lines: Vec<String>,
    /// 滚动偏移（行）
    pub scroll: usize,
}

impl Info {
    /// 新建信息页。
    pub fn new(title: impl Into<String>, lines: Vec<String>) -> Self {
        Self {
            title: title.into(),
            lines,
            scroll: 0,
        }
    }

    /// 滚动（夹到有效范围）。
    pub fn scroll_by(&mut self, delta: i32, visible: usize) {
        let max = self.lines.len().saturating_sub(visible);
        let next = self.scroll as i32 + delta;
        self.scroll = next.clamp(0, max as i32) as usize;
    }
}

/// 当前打开的覆盖层。
pub enum Overlay {
    /// 可过滤列表
    List(List),
    /// 只读信息
    Info(Info),
}

/// 渲染覆盖层（居中卡片 + 半透明遮罩由调用方提供）。
pub fn render(
    overlay: &Overlay,
    theme: &Theme,
    cx: &mut gpui::Context<super::app::GuiApp>,
) -> AnyElement {
    match overlay {
        Overlay::List(list) => render_list(list, theme, cx),
        Overlay::Info(info) => render_info(info, theme),
    }
}

/// 列表卡片。
fn render_list(
    list: &List,
    theme: &Theme,
    cx: &mut gpui::Context<super::app::GuiApp>,
) -> AnyElement {
    let filtered = list.filtered();
    let mut column = div()
        .w(px(560.))
        .max_h(px(460.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .p(px(12.))
        .rounded(px(12.))
        .bg(theme.raised)
        .border_1()
        .border_color(theme.border_strong);

    column = column.child(
        div()
            .flex()
            .items_center()
            .justify_between()
            .px(px(2.))
            .text_size(px(11.5))
            .text_color(theme.text_tertiary)
            .child(SharedString::from(list.title.clone()))
            .child(SharedString::from(format!("{} 项", filtered.len()))),
    );

    // 搜索框
    column = column.child(
        div()
            .w_full()
            .h(px(32.))
            .px(px(10.))
            .rounded(px(8.))
            .flex()
            .items_center()
            .bg(theme.inset)
            .border_1()
            .border_color(theme.border)
            .text_size(px(13.))
            .text_color(if list.query.is_empty() {
                theme.text_ghost
            } else {
                theme.text
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(SharedString::from(if list.query.is_empty() {
                        "搜索…".to_string()
                    } else {
                        list.query.text()
                    }))
                    .child(
                        div()
                            .w(px(if list.query.is_empty() { 0. } else { 1.5 }))
                            .h(px(15.))
                            .ml(px(1.))
                            .bg(theme.accent),
                    ),
            ),
    );

    let mut rows = div()
        .id("overlay-rows")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .gap(px(2.))
        .overflow_y_scroll()
        .track_scroll(&list.scroll);
    if filtered.is_empty() {
        rows = rows.child(
            div()
                .px(px(10.))
                .py(px(8.))
                .text_size(px(12.))
                .text_color(theme.text_ghost)
                .child(SharedString::from("没有匹配项")),
        );
    }
    for (visible_index, entry_index) in filtered.iter().enumerate() {
        let entry = &list.entries[*entry_index];
        let is_selected = visible_index == list.selected;
        let mut row = div()
            .w_full()
            .h(px(30.))
            .px(px(10.))
            .rounded(px(7.))
            .flex()
            .items_center()
            .gap(px(10.))
            .text_size(px(13.));
        if is_selected {
            row = row.bg(theme.overlay_strong).text_color(theme.text);
        } else {
            row = row
                .text_color(theme.text_secondary)
                .hover(|style| style.bg(theme.overlay).text_color(theme.text));
        }
        row = row.child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .child(SharedString::from(entry.label.clone())),
        );
        if !entry.detail.is_empty() {
            row = row.child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(theme.text_ghost)
                    .child(SharedString::from(entry.detail.clone())),
            );
        }
        rows = rows.child(
            row.id(("overlay-row", visible_index))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.pick_entry(visible_index, cx))),
        );
    }
    column = column.child(rows);

    column = column.child(
        div()
            .px(px(2.))
            .text_size(px(10.5))
            .text_color(theme.text_ghost)
            .child(SharedString::from("↑↓ 选择 · Enter 确认 · Esc 关闭")),
    );
    column.into_any_element()
}

/// 信息卡片。
fn render_info(info: &Info, theme: &Theme) -> AnyElement {
    let mut column = div()
        .w(px(600.))
        .max_h(px(470.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .p(px(14.))
        .rounded(px(12.))
        .bg(theme.raised)
        .border_1()
        .border_color(theme.border_strong)
        .child(
            div()
                .text_size(px(12.5))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(SharedString::from(info.title.clone())),
        );

    for line in info.lines.iter().skip(info.scroll).take(16) {
        column = column.child(
            div()
                .text_size(px(12.5))
                .text_color(theme.text_secondary)
                .child(SharedString::from(line.clone())),
        );
    }

    column
        .child(
            div()
                .mt(px(4.))
                .text_size(px(10.5))
                .text_color(theme.text_ghost)
                .child(SharedString::from("↑↓ 滚动 · Esc 关闭")),
        )
        .into_any_element()
}
