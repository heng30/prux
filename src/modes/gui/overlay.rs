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
    /// 执行一条配置类斜杠命令（只从 Ctrl-M 面板进）
    ConfigCommand(String),
    /// 把一段文本写回输入框（历史面板）
    InsertText(String),
    /// 技能面板：在草稿里把某个技能切到目标启停态
    ToggleSkill {
        /// 技能名
        name: String,
        /// true = 打开，false = 关闭
        enable: bool,
    },
    /// 切到某个工作目录（最近目录列表）
    SwitchCwdTo(String),
    /// 打开系统目录选择器
    BrowseCwd,
    /// 打开可编辑的设置面板
    Settings,
    /// 显示/隐藏左侧会话列表
    ToggleSidebar,
    /// 打开登录面板（选 provider 填 key）
    Login,
    /// 打开登出面板（列出已存 key 的 provider）
    Logout,
    /// 为某个 provider 填 API key
    LoginProvider(String),
    /// 清除某个 provider 的 API key
    LogoutProvider(String),
    /// 显示当前版本的更新日志
    Changelog,
    /// 打开扩展管理面板
    Extensions,
    /// 打开技能面板
    Skills,
    /// 打开模型循环名单（/scoped-models）
    ScopedModels,
    /// 切换某个扩展的启用状态
    ToggleExtension(String),
    /// 切换某个模型是否进入 Ctrl-P 循环名单
    ToggleScopedModel(String),
    /// 切换设置面板里的一个开关（改完立即落盘）
    ToggleSetting(SettingKey),
    /// 插入附件引用（`@路径`）：`images` 为真时补全只列图片
    Attach {
        /// 是否只列图片
        images: bool,
    },
}

/// 设置面板里能直接切换的开关项。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingKey {
    /// 上下文接近上限时自动压缩
    AutoCompact,
    /// 失败请求自动重试
    Retry,
    /// 隐藏模型的思考内容
    HideThinking,
    /// 代码块语法高亮
    SyntaxHighlight,
    /// 图片超限自动缩放
    ImagesAutoResize,
    /// 拦截图片输入
    ImagesBlockImages,
    /// 把会话环境变量暴露给工具
    ExposeSessionEnvironment,
    /// 缓存未命中提示
    ShowCacheMissNotices,
    /// 转向（steering）模式：逐个循环 queued / immediateInterrupt / immediate
    SteeringMode,
    /// 追问（follow-up）模式：循环排队 / 立即
    FollowUpMode,
    /// 缓存预热模式：循环 auto / always / never
    CacheWarming,
    /// 启动安静级别：循环 off / notice / info / debug
    QuietStartup,
    /// 失败重试次数（数字输入）
    RetryMaxRetries,
    /// temperature 覆盖（数字输入，空 = 清除）
    Temperature,
    /// 压缩预留 token 数（数字输入）
    CompactReserve,
    /// 压缩保留最近 token 数（数字输入）
    CompactKeepRecent,
}

impl SettingKey {
    /// 是否需要用输入框改数值（而不是 Enter 直接切换）。
    pub fn is_number(self) -> bool {
        matches!(
            self,
            SettingKey::RetryMaxRetries
                | SettingKey::Temperature
                | SettingKey::CompactReserve
                | SettingKey::CompactKeepRecent
        )
    }
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
    /// 本会话历史输入
    History,
}

/// 内置信息页。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InfoKind {
    /// 用量与费用
    Usage,

    /// 当前会话信息（模型 / 目录 / 工具 / 会话文件）
    Session,
    /// 快捷键
    Help,
}

/// 列表面板的行高（逻辑像素，行内容高度）。
const ROW_HEIGHT: f32 = 30.0;

/// 列表面板的行间距（[`rows_container`] 的 `gap`，补全面板也用同一个值）。
pub(crate) const ROW_GAP: f32 = 2.0;

/// 行尾说明文字的最大宽度（逻辑像素）：技能/扩展描述很长，超了就截断。
const DETAIL_MAX_WIDTH: f32 = 260.0;

/// 列表项。
pub struct Entry {
    /// 主文案
    pub label: String,
    /// 右侧次要说明
    pub detail: String,
    /// 选中后的动作
    pub action: Action,
    /// 勾选框状态：`Some` 时行首画一个方框（扩展/技能面板的启停态）
    pub checked: Option<bool>,
}

impl Default for Entry {
    fn default() -> Self {
        Self {
            label: String::new(),
            detail: String::new(),
            action: Action::None,
            checked: None,
        }
    }
}

/// 列表项详情页的数据来源（Tab 打开）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DetailSource {
    /// 没有详情页（多数列表面板）
    None,
    /// 扩展详情（描述 / 模式 / 工具 / fork 来源）
    Extension,
    /// 技能详情（SKILL.md 头信息 + 来源 + 路径）
    Skill,
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
    /// Tab 打开的详情页；`Some` 时面板渲染成只读信息页
    pub detail: Option<Info>,
    /// 详情页取哪份数据（[`DetailSource::None`] 时 Tab 无效）
    pub detail_source: DetailSource,
    /// 列表视口（键盘移动选中项时要把它滚进视野）
    scroll: ScrollHandle,
}

impl List {
    /// 新建列表（过滤框为空、无详情页）。
    pub fn new(title: impl Into<String>, entries: Vec<Entry>) -> Self {
        Self {
            title: title.into(),
            query: Editor::new(),
            entries,
            selected: 0,
            detail: None,
            detail_source: DetailSource::None,
            scroll: ScrollHandle::new(),
        }
    }

    /// 当前选中行的主文案（空列表时为 None）。
    pub fn selected_label(&self) -> Option<&str> {
        let filtered = self.filtered();
        let index = *filtered.get(self.selected)?;
        Some(&self.entries[index].label)
    }

    /// 用新条目重算列表：保留过滤框与选中行（勾选切换后不跳回第一行）。
    pub fn refresh(&mut self, title: impl Into<String>, entries: Vec<Entry>) {
        self.title = title.into();
        self.entries = entries;
        self.selected = self.selected.min(self.filtered().len().saturating_sub(1));
        self.move_selection(0);
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
        reveal_row(&self.scroll, self.selected);
    }

    /// 直接跳到第 `index` 行（`tui.select.first/last`：home/end），并把它滚进视口。
    pub fn select_index(&mut self, index: usize) {
        let len = self.filtered().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = index.min(len - 1);
        reveal_row(&self.scroll, self.selected);
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
        Overlay::List(list) => match &list.detail {
            Some(detail) => render_info(detail, theme),
            None => render_list(list, theme, cx),
        },
        Overlay::Info(info) => render_info(info, theme),
    }
}

/// 一行的内容与状态：主文案、右侧说明、勾选态、是否选中、行高。
pub(crate) struct RowSpec<'a> {
    /// 主文案
    pub label: &'a str,
    /// 右侧次要说明（空串表示不显示）
    pub detail: &'a str,
    /// 勾选框状态（`Some` 时行首画方框）
    pub checked: Option<bool>,
    /// 是否为当前选中行
    pub selected: bool,
    /// 行高（逻辑像素）
    pub height: f32,
}

/// 列表里的一行：勾选框 + 主文案 + 右侧说明（面板与 `/`、`@` 补全面板共用）。
///
/// `height` 是行高（面板 30、补全面板 26），`id` 的前半段区分不同列表，
/// 避免两个列表的同一行号抢同一个元素 id。
pub(crate) fn list_row(
    theme: &Theme,
    spec: RowSpec<'_>,
    id: (&'static str, usize),
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let RowSpec {
        label,
        detail,
        checked,
        selected,
        height,
    } = spec;
    let mut row = div()
        .id(id)
        .cursor_pointer()
        .w_full()
        .h(px(height))
        .px(px(10.))
        .rounded(px(7.))
        .flex()
        .items_center()
        .gap(px(10.))
        .text_size(px(13.))
        .on_click(on_click);
    if let Some(checked) = checked {
        row = row.child(
            div()
                .flex_none()
                .w(px(14.))
                .h(px(14.))
                .rounded(px(3.))
                .border_1()
                .border_color(if checked {
                    theme.accent
                } else {
                    theme.border_strong
                })
                .bg(if checked { theme.accent } else { theme.inset })
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(10.))
                .text_color(theme.on_inverse)
                .child(SharedString::from(if checked { "✓" } else { "" })),
        );
    }
    row = if selected {
        row.bg(theme.overlay_strong).text_color(theme.text)
    } else {
        row.text_color(theme.text_secondary)
            .hover(|style| style.bg(theme.overlay).text_color(theme.text))
    };
    row = row.child(
        div()
            .min_w_0()
            .flex_1()
            .truncate()
            .child(SharedString::from(label.to_string())),
    );
    if !detail.is_empty() {
        row = row.child(
            div()
                .flex_none()
                .max_w(px(DETAIL_MAX_WIDTH))
                .min_w_0()
                .truncate()
                .text_size(px(11.))
                .text_color(theme.text_ghost)
                .child(SharedString::from(detail.to_string())),
        );
    }
    row.into_any_element()
}

/// 可滚动行容器（行数超过 `max_h` 时用滚轮滚动）。
pub(crate) fn rows_container(
    id: &'static str,
    scroll: &ScrollHandle,
    max_h: f32,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .w_full()
        .min_h_0()
        .max_h(px(max_h))
        .gap(px(ROW_GAP))
        .overflow_y_scroll()
        .track_scroll(scroll)
}

/// 把第 `index` 行滚进视口：键盘移动选中项后调用，保证选中行始终可见。
///
/// 行位置取容器的真实子元素边界（`bounds_for_item`），不按「行高 × 行号」推算：
/// 列表面板是 flex 容器，行会被压缩到内容最小高度，推算出来的位置会比实际偏低，
/// 往上游标时就滚过头、把选中行顶出视口。
///
/// 容器还没登记子元素边界（面板刚打开的那一帧）时什么也不做，下一帧自然会跟上。
pub(crate) fn reveal_row(scroll: &ScrollHandle, index: usize) {
    let (Some(row), area) = (scroll.bounds_for_item(index), scroll.bounds()) else {
        return;
    };
    let viewport = f32::from(area.size.height);
    let max = f32::from(scroll.max_offset().height);
    let offset = f32::from(scroll.offset().y);
    let top = f32::from(row.top() - area.top());
    let height = f32::from(row.size.height);
    let next = crate::utils::geometry::offset_revealing(offset, top, height, viewport, max);
    if next != offset {
        scroll.set_offset(gpui::point(gpui::px(0.), gpui::px(next)));
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
            .child(SharedString::from(format!("{} items", filtered.len()))),
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
                        "Search…".to_string()
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

    // 和搜索框之间留一道空隙：所有列表面板（Ctrl-M、/history、模型、会话…）共用同一个版式
    let mut rows = rows_container("overlay-rows", &list.scroll, 340.)
        .flex_1()
        .mt(px(6.));
    if filtered.is_empty() {
        rows = rows.child(
            div()
                .px(px(10.))
                .py(px(8.))
                .text_size(px(12.))
                .text_color(theme.text_ghost)
                .child(SharedString::from("No matches")),
        );
    }
    for (visible_index, entry_index) in filtered.iter().enumerate() {
        let entry = &list.entries[*entry_index];
        let index = visible_index;
        rows = rows.child(list_row(
            theme,
            RowSpec {
                label: &entry.label,
                detail: &entry.detail,
                checked: entry.checked,
                selected: index == list.selected,
                height: ROW_HEIGHT,
            },
            ("overlay-row", index),
            cx.listener(move |this, _, _, cx| this.pick_entry(index, cx)),
        ));
    }
    column = column.child(rows);

    let hint = if list.detail_source == DetailSource::None {
        "↑↓ select · Enter confirm · Esc close"
    } else {
        "↑↓ select · Enter toggle · Tab detail · Esc close"
    };
    column = column.child(
        div()
            .px(px(2.))
            .text_size(px(10.5))
            .text_color(theme.text_ghost)
            .child(SharedString::from(hint)),
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
                .child(SharedString::from("↑↓ scroll · Esc close")),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(label: &str, checked: bool) -> Entry {
        Entry {
            label: label.to_string(),
            checked: Some(checked),
            ..Entry::default()
        }
    }

    /// 勾选后重建列表：选中行与过滤框都保留，不跳回第一行。
    #[test]
    fn refresh_keeps_query_and_selected_row() {
        let mut list = List::new("Extensions", vec![entry("a", false), entry("b", false)]);
        list.selected = 1;
        list.refresh("Extensions", vec![entry("a", false), entry("b", true)]);
        assert_eq!(list.selected, 1);
        assert_eq!(list.selected_label(), Some("b"));

        list.query.insert("a");
        list.refresh("Extensions", vec![entry("a", true), entry("b", true)]);
        assert_eq!(list.query.text(), "a");
        assert_eq!(list.filtered(), vec![0]);
        assert_eq!(list.selected, 0);
    }
}
