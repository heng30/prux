//! 覆盖层面板：可过滤列表（命令面板 / 模型 / 会话 / 思考级别 / 主题）、只读信息页
//! 与扩展覆盖层（扩展自持内容，GUI 只负责绘制与回传按键）。

use super::{
    app::GuiApp,
    composer,
    text_edit::Editor,
    theme::{
        CONTROL_H_LG, FONT_BODY, FONT_CAPTION, FONT_SMALL, ICON_SIZE, RADIUS_LG, RADIUS_MD,
        RADIUS_SM, SPACE_MD, SPACE_SM, SPACE_XS, SPACE_XXS, Theme, px,
    },
};
use crate::{
    core::extensions::{self, DockLine, DockSpan, OverlayView},
    modes::interactive::agent_actor::AgentCommand,
    utils::geometry,
};
use gpui::{
    AnyElement, Div, FontWeight, InteractiveElement, IntoElement, ParentElement, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, div,
};

/// 列表弹层宽度（命令面板 / 模型 / 会话选择 / 思考级别）。
const LIST_W: f32 = 560.;
/// 列表弹层高度上限。
const LIST_MAX_H: f32 = 460.;
/// 信息卡片宽度（usage / help / about）。
const INFO_W: f32 = 600.;
/// 信息卡片高度上限。
const INFO_MAX_H: f32 = 468.;
/// 拉取扩展覆盖层一帧内容时报告给扩展的「终端宽度」。
///
/// 扩展按它自己折行（ask-user-question 的题干/描述就用这个宽度换行），
/// 桌面窗口宽度差异很大，取一个通用值即可。
const OVERLAY_COLS: u16 = 100;

/// 列表项被选中后要做的事。
#[derive(Clone)]
pub enum Action {
    /// 不做事（纯展示）
    None,
    /// 下发一条命令（装箱：AgentCommand 体积远大于其它变体）
    Command(Box<AgentCommand>),
    /// 切换到某个会话文件
    LoadSession(String),
    /// 切换明暗主题
    SetDark(bool),
    /// 打开另一个列表面板
    Open(ListKind),
    /// 刷新会话列表
    RefreshSessions,
    /// 设置界面缩放系数（1.0 = 设计稿尺寸）
    Scale(f32),
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
    /// 为某个 provider 填 API key
    LoginProvider(String),
    /// 清除某个 provider 的 API key
    LogoutProvider(String),
    /// 打开扩展管理面板
    Extensions,
    /// 打开技能面板
    Skills,
    /// 切换某个扩展的启用状态
    ToggleExtension(String),
    /// 切换某个模型是否进入 Ctrl-P 循环名单
    ToggleScopedModel(String),
    /// 切换设置面板里的一个开关（改完立即落盘）
    ToggleSetting(SettingKey),
    /// 扩展留下的选择面板（`ExtensionUiRequest::Select`）：把选中的选项文本回传；
    /// `text` 为 None 表示用户取消（Esc）。
    UiChoice {
        /// 请求 id（`submit_ui_choice` 按它匹配发起方）
        id: u64,
        /// 选项文本；None = 取消
        text: Option<String>,
    },
    /// `/trust` 面板里的一个信任决策（取值同 TUI：trust / trust_parent / distrust / clear）。
    TrustDecision(String),
    /// 扩展设置面板：把某一项设置循环到下一个候选值
    CycleExtensionSetting {
        /// 扩展名（`/extension` 面板里显示的名字）
        ext: String,
        /// 设置项的稳定键（`ExtensionSetting::key`）
        key: String,
    },
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
    /// 桌面窗口明暗主题（切换时写回 `theme-gui`）
    GuiTheme,
}

impl SettingKey {
    /// 是否需要用输入框改数值（而不是 Space 直接切换）。
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
    /// 思考级别
    Thinking,
    /// 外观（明/暗）
    Appearance,
    /// 右侧面板：文件
    PanelFiles,
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

/// 详情页表格左列（属性名）宽度（逻辑像素）。
const INFO_LABEL_W: f32 = 132.0;

/// 详情卡片里可滚动主体的高度上限（逻辑像素）：超出后主体内部滚动，标题与提示行留在卡片里。
const INFO_BODY_MAX_H: f32 = 360.0;

/// 详情页「滚动一行」的位移（逻辑像素）：值会折行、行高不固定，用它把方向键换算成位移。
const INFO_SCROLL_STEP: f32 = 20.0;

/// 设置面板表格列宽（逻辑像素）：设置项名。
pub(crate) const TABLE_SETTING_W: f32 = 200.0;

/// 设置面板表格列宽（逻辑像素）：当前值（`on` / `off` / 数字 / 模式名）。
pub(crate) const TABLE_VALUE_W: f32 = 110.0;

/// 表格第一列（名称）宽度（逻辑像素）：技能与扩展面板共用，两个面板列宽对得上。
pub(crate) const TABLE_NAME_W: f32 = 140.0;

/// 表格来源列宽度（逻辑像素）：技能面板的 `source`（发现根目录，超宽截断）。
pub(crate) const TABLE_SOURCE_W: f32 = 150.0;

/// 表格模式列宽度（逻辑像素）：扩展面板的可用模式。
pub(crate) const TABLE_MODES_W: f32 = 150.0;

/// 表格 provider 列宽度（逻辑像素）：模型名单面板的 provider 名。
pub(crate) const TABLE_PROVIDER_W: f32 = 110.0;

/// 表格列：表头标题 + 固定宽度。
pub struct Column {
    /// 表头文字
    pub title: String,
    /// 列宽（逻辑像素）；`None` = 吃剩余宽度（表格里最多一个）
    pub width: Option<f32>,
}

impl Column {
    /// 定宽列。
    pub fn fixed(title: impl Into<String>, width: f32) -> Self {
        Self {
            title: title.into(),
            width: Some(width),
        }
    }

    /// 伸缩列（占满剩余宽度）。
    pub fn flex(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            width: None,
        }
    }
}

/// 列表项。
pub struct Entry {
    /// 主文案（表格模式下就是第一列，即名称）
    pub label: String,
    /// 右侧次要说明（表格模式下作行尾说明用；表格面板一般留空）
    pub detail: String,
    /// 选中后的动作
    pub action: Action,
    /// 勾选框状态：`Some` 时行首画一个方框（扩展/技能/模型名单面板的启停态）
    pub checked: Option<bool>,
    /// 表格模式下除第一列以外的单元格（与 [`List::columns`] 从第二列起一一对应）；
    /// 空 = 普通两列布局
    pub cells: Vec<String>,
}

impl Default for Entry {
    fn default() -> Self {
        Self {
            label: String::new(),
            detail: String::new(),
            action: Action::None,
            checked: None,
            cells: Vec::new(),
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
    /// Enter 打开的详情页；`Some` 时面板渲染成只读信息页
    pub detail: Option<Info>,
    /// 详情页取哪份数据（[`DetailSource::None`] 时 Enter 无效）
    pub detail_source: DetailSource,
    /// 表格列定义（空 = 普通两列布局）；第一列渲染 [`Entry::label`]
    pub columns: Vec<Column>,
    /// 列表视口（键盘移动选中项时要把它滚进视野）
    scroll: ScrollHandle,
    /// 这条列表在回答哪个扩展的 `Select` 请求：收起时按「取消」回传
    pub choice: Option<u64>,
    /// 底部提示行覆盖（`None` 时按列表种类自动挑一句）
    pub hint: Option<String>,
    /// Space 是否直接触发条目动作（设置 / 扩展 / 技能面板）：为 true 时空格不再写进过滤框
    pub space_activates: bool,
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
            columns: Vec::new(),
            scroll: ScrollHandle::new(),
            choice: None,
            hint: None,
            space_activates: false,
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
                    || entry
                        .cells
                        .iter()
                        .any(|cell| cell.to_lowercase().contains(query))
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
    /// 逐行文本（`rows` 为空时按普通文本渲染）
    pub lines: Vec<String>,
    /// 两列表格内容（属性 / 值）：非空时按表格渲染，值里是 URL 的可点击
    pub rows: Vec<InfoRow>,
    /// 滚动位置：方向键与滚轮共用同一个句柄
    pub scroll: ScrollHandle,
}

/// 详情页表格的一行：属性名 + 值。
pub struct InfoRow {
    /// 属性名（左列）
    pub key: String,
    /// 值（右列）；以 `http://` / `https://` 开头时渲染成可点击链接
    pub value: String,
}

impl Info {
    /// 新建信息页。
    pub fn new(title: impl Into<String>, lines: Vec<String>) -> Self {
        Self {
            title: title.into(),
            lines,
            rows: Vec::new(),
            scroll: ScrollHandle::new(),
        }
    }

    /// 新建表格型详情页（扩展 / 技能的 Tab 详情）。
    pub fn with_rows(title: impl Into<String>, rows: Vec<InfoRow>) -> Self {
        Self {
            title: title.into(),
            lines: Vec::new(),
            rows,
            scroll: ScrollHandle::new(),
        }
    }

    /// 按行滚动：正数向下、负数向上，换算成像素后夹到实际可滚范围。
    pub fn scroll_by(&mut self, rows: f32) {
        let top = f32::from(self.scroll.offset().y);
        self.scroll_to(top + rows * INFO_SCROLL_STEP);
    }

    /// 滚到纵坐标 `y`（夹到 `0..=max_offset`）；传 `f32::MAX` 即滚到底。
    pub fn scroll_to(&mut self, y: f32) {
        let max = f32::from(self.scroll.max_offset().height);
        let y = y.clamp(0., max);
        self.scroll
            .set_offset(gpui::point(gpui::px(0.), gpui::px(y)));
    }
}

/// 当前打开的覆盖层。
pub enum Overlay {
    /// 可过滤列表
    List(List),
    /// 只读信息
    Info(Info),
    /// 扩展覆盖层（内容每帧从扩展拉取）
    Extension(ExtOverlay),
}

/// 扩展覆盖层：内容由扩展持有，GUI 只缓存最近一帧、绘制并把按键回传。
pub struct ExtOverlay {
    /// 扩展分配的覆盖层 id
    pub id: u64,
    /// 最近一帧内容（拉取失败时保留上一帧，避免闪空）
    pub view: OverlayView,
    /// 内联输入行（`view.input`）：真值在扩展，这里只是编辑缓冲
    pub input: Editor,
    /// 输入行是否聚焦（扩展请求 `input_focus` 或用户开始打字）
    pub input_active: bool,
    /// 内容滚动（滚轮）
    pub scroll: ScrollHandle,
    /// 上一帧扩展声明的选中行：只有它**变了**才把该行滚进视口（滚轮不会被拉回）
    last_selected: Option<usize>,
}

impl ExtOverlay {
    /// 按扩展当前状态建一帧；扩展不再认领该 id 时返回 None。
    pub fn open(id: u64) -> Option<Self> {
        let view = extensions::overlay_view(id, OVERLAY_COLS)?;
        let input_active = view.input_focus;
        let last_selected = view.selected;
        let mut overlay = Self {
            id,
            view,
            input: Editor::new(),
            input_active,
            scroll: ScrollHandle::new(),
            last_selected,
        };
        overlay.sync_input();
        Some(overlay)
    }

    /// 重新拉取内容；返回 false 表示扩展已经不再认领（调用方应关闭覆盖层）。
    pub fn refresh(&mut self) -> bool {
        let Some(view) = extensions::overlay_view(self.id, OVERLAY_COLS) else {
            return false;
        };

        let gained = self.view.input.is_none() && view.input.is_some();
        let lost = self.view.input.is_some() && view.input.is_none();
        self.view = view;
        if lost {
            self.input_active = false;
            self.input.clear();
        }
        if gained {
            // 新出现的输入行可能带初值（编辑已有备注/密码框重开）：聚焦并装进缓冲
            self.input_active = true;
            self.input.clear();
            self.sync_input();
        }
        // 未在编辑时以扩展的值为准（提交后扩展会把输入行置空）
        if !self.input_active {
            self.sync_input();
        }
        // 键盘移动了选中项才跟着滚；滚动条自己动过就不打扰
        if self.view.selected != self.last_selected {
            self.last_selected = self.view.selected;
            if let Some(selected) = self.view.selected {
                reveal_row(&self.scroll, selected);
            }
        }
        true
    }

    /// 把扩展给的输入值同步进编辑缓冲（不覆盖正在敲的内容）。
    fn sync_input(&mut self) {
        let Some(input) = &self.view.input else {
            return;
        };
        if self.input.text() != input.value {
            self.input.clear();
            self.input.insert(&input.value);
        }
    }
}

/// 一行扩展文本（主题键 + 兜底色 + 内容）→ GUI 元素；扩展覆盖层与停靠面板共用。
pub(crate) fn render_dock_line(line: &DockLine, theme: &Theme) -> AnyElement {
    let mut row = div().w_full().flex().flex_wrap();
    for span in line {
        row = row.child(
            div()
                .text_color(dock_span_color(span, theme))
                .child(SharedString::from(span.text.clone())),
        );
    }
    row.into_any_element()
}

/// 扩展文本片段的颜色：先按主题键查，认不出再用兜底的 `#RRGGBB`，
/// 都没有就退回主文本色（扩展给的键是终端主题名，桌面端只认常见几个）。
fn dock_span_color(span: &DockSpan, theme: &Theme) -> gpui::Hsla {
    let by_key = match span.key {
        "accent" => Some(theme.accent),
        "text" => Some(theme.text),
        "dim" | "muted" => Some(theme.text_tertiary),
        "success" | "ok" => Some(theme.success),
        "warning" | "warn" => Some(theme.warning),
        "error" | "danger" => Some(theme.danger),
        "link" => Some(theme.link),
        _ => None,
    };
    by_key
        .or_else(|| parse_hex_color(&span.fallback))
        .unwrap_or(theme.text)
}

/// `#RRGGBB` / `#RGB` → 颜色；其它写法（终端色名）返回 None。
fn parse_hex_color(spec: &str) -> Option<gpui::Hsla> {
    let hex = spec.trim().trim_start_matches('#');
    let expand = |s: &str| u8::from_str_radix(s, 16).ok();
    let (r, g, b) = match hex.len() {
        6 => (
            expand(&hex[0..2])?,
            expand(&hex[2..4])?,
            expand(&hex[4..6])?,
        ),
        3 => {
            let d = |i: usize| expand(&hex[i..i + 1]).map(|v| v * 17);
            (d(0)?, d(1)?, d(2)?)
        }
        _ => return None,
    };
    Some(gpui::rgba(((r as u32) << 24) | ((g as u32) << 16) | ((b as u32) << 8) | 0xFF).into())
}

/// 渲染覆盖层（居中卡片 + 半透明遮罩由调用方提供）。
pub fn render(overlay: &Overlay, theme: &Theme, cx: &mut gpui::Context<GuiApp>) -> AnyElement {
    match overlay {
        Overlay::List(list) => match &list.detail {
            Some(detail) => render_info(detail, theme),
            None => render_list(list, theme, cx),
        },
        Overlay::Info(info) => render_info(info, theme),
        Overlay::Extension(ext) => render_extension(ext, theme),
    }
}

/// 扩展覆盖层卡片：标题 + 固定区 + 内容（选中行高亮）+ 键位提示 + 内联输入行。
fn render_extension(ext: &ExtOverlay, theme: &Theme) -> AnyElement {
    let mut column = div()
        .w(px(LIST_W))
        .max_h(px(LIST_MAX_H))
        .flex()
        .flex_col()
        .gap(px(SPACE_XS))
        .p(px(SPACE_MD))
        .rounded(px(RADIUS_LG))
        .bg(theme.raised)
        .border_1()
        .border_color(theme.border_strong)
        .text_size(px(FONT_SMALL))
        .child(
            div()
                .text_size(px(FONT_SMALL))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(SharedString::from(ext.view.title.clone())),
        );

    // 固定区：标签条、题干等（不随内容滚动）
    for line in &ext.view.header {
        column = column.child(render_dock_line(line, theme));
    }

    // 内容区：选中行整行铺一层强调底色
    let mut body = div()
        .id("ext-overlay-body")
        .min_h_0()
        .flex_1()
        .flex()
        .flex_col()
        .overflow_y_scroll()
        .track_scroll(&ext.scroll);

    for (index, line) in ext.view.lines.iter().enumerate() {
        let selected = ext.view.selected == Some(index);
        let mut row = div().w_full().rounded(px(RADIUS_SM)).px(px(SPACE_XXS));
        if selected {
            row = row.bg(theme.overlay_strong);
        }
        body = body.child(row.child(render_dock_line(line, theme)));
    }
    column = column.child(body);

    if !ext.view.footer.is_empty() {
        let hints: Vec<String> = ext
            .view
            .footer
            .iter()
            .map(|(key, action)| format!("{key} {action}"))
            .collect();
        column = column.child(
            div()
                .text_size(px(FONT_CAPTION))
                .text_color(theme.text_ghost)
                .child(SharedString::from(hints.join(" · "))),
        );
    }

    if let Some(input) = &ext.view.input {
        let mut row = div()
            .w_full()
            .px(px(SPACE_SM))
            .py(px(SPACE_XS))
            .rounded(px(RADIUS_SM))
            .bg(theme.inset)
            .border_1()
            .border_color(if ext.input_active {
                theme.accent
            } else {
                theme.border
            })
            .flex()
            .items_center()
            .gap(px(SPACE_SM))
            .child(
                div()
                    .text_size(px(FONT_CAPTION))
                    .text_color(theme.text_tertiary)
                    .child(SharedString::from(input.label.clone())),
            );
        let text = ext.input.text();
        row = if text.is_empty() && !ext.input_active {
            row.child(
                div()
                    .text_color(theme.text_ghost)
                    .child(SharedString::from(input.placeholder.clone())),
            )
        } else {
            row.child(
                div()
                    .min_w_0()
                    .flex_1()
                    .text_color(theme.text)
                    .child(composer::render(&ext.input, theme)),
            )
        };
        column = column.child(row);
    }

    column.into_any_element()
}

/// 一行的内容与状态：主文案、对齐列、右侧说明、勾选态、是否选中、行高。
pub(crate) struct RowSpec<'a> {
    /// 主文案
    pub label: &'a str,
    /// 表格列定义（空 = 普通两列布局）
    pub columns: &'a [Column],
    /// 表格行除第一列以外的单元格（与 `columns` 从第二列起对应）
    pub cells: &'a [String],
    /// 次要说明（空串表示不显示）
    pub detail: &'a str,
    /// 说明是否紧跟主文案（`true` 用于补全面板）：`false` 时主文案撑满、说明右对齐，
    /// `true` 时说明吃掉剩余宽度并左对齐，命令与说明之间不留大片空白
    pub detail_inline: bool,
    /// 勾选框状态（`Some` 时行首画方框）
    pub checked: Option<bool>,
    /// 是否为当前选中行
    pub selected: bool,
    /// 行高（逻辑像素）
    pub height: f32,
}

/// 列表里的一行：勾选框 + 主文案 + 对齐列 + 右侧说明（面板与 `/`、`@` 补全面板共用）。
///
/// `columns` 非空时切到表格版式（名称定宽、每列定宽，描述占剩余宽），
/// 空时保持「名称撑满 + 说明右对齐」，`detail_inline` 为真则改成「名称自然宽 + 说明吃剩余宽」。
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
        columns,
        cells,
        detail,
        detail_inline,
        checked,
        selected,
        height,
    } = spec;
    let table = !columns.is_empty();
    let mut row = div()
        .id(id)
        .cursor_pointer()
        .w_full()
        .h(px(height))
        .px(px(SPACE_SM))
        .rounded(px(RADIUS_MD))
        .flex()
        .items_center()
        .gap(px(SPACE_SM))
        .text_size(px(FONT_BODY))
        .on_click(on_click);

    if let Some(checked) = checked {
        row = row.child(
            div()
                .flex_none()
                .w(px(ICON_SIZE))
                .h(px(ICON_SIZE))
                .rounded(px(RADIUS_SM))
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
                .text_size(px(FONT_CAPTION))
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

    if table {
        // 表格：第一列是名称（主色、正常字号），其余列按列宽定长，超宽截断
        row = row.child(table_cell(theme, label, columns[0].width, false));
        for (text, column) in cells.iter().zip(&columns[1..]) {
            row = row.child(table_cell(theme, text, column.width, true));
        }
        return row.into_any_element();
    }

    let label_cell = div()
        .min_w_0()
        .truncate()
        .child(SharedString::from(label.to_string()));

    row = row.child(if detail_inline {
        label_cell.flex_none().max_w(px(DETAIL_MAX_WIDTH))
    } else {
        label_cell.flex_1()
    });

    if !detail.is_empty() {
        let detail_cell = div()
            .min_w_0()
            .truncate()
            .text_size(px(FONT_CAPTION))
            .text_color(theme.text_ghost)
            .child(SharedString::from(detail.to_string()));
        row = row.child(if detail_inline {
            detail_cell.flex_1()
        } else {
            detail_cell.flex_none().max_w(px(DETAIL_MAX_WIDTH))
        });
    }
    row.into_any_element()
}

/// 表格单元格：定宽列截断，`None` 宽度吃剩余空间；`muted` 用次要色与小字号。
fn table_cell(theme: &Theme, text: &str, width: Option<f32>, muted: bool) -> AnyElement {
    let cell = div()
        .min_w_0()
        .truncate()
        .child(SharedString::from(text.to_string()));
    let cell = match width {
        Some(width) => cell.flex_none().w(px(width)),
        None => cell.flex_1(),
    };
    if muted {
        cell.text_size(px(FONT_CAPTION))
            .text_color(theme.text_tertiary)
            .into_any_element()
    } else {
        cell.into_any_element()
    }
}

/// 表头：与 [`list_row`] 的表格行同宽同列；`checked` 为真时留出勾选框的位置。
fn table_header(theme: &Theme, columns: &[Column], checked: bool) -> AnyElement {
    let mut header = div()
        .w_full()
        .h(px(ROW_HEIGHT))
        .px(px(SPACE_SM))
        .flex()
        .items_center()
        .gap(px(SPACE_SM))
        .border_b_1()
        .border_color(theme.border)
        .text_size(px(FONT_CAPTION))
        .text_color(theme.text_ghost);

    if checked {
        header = header.child(div().flex_none().w(px(ICON_SIZE)).h(px(ICON_SIZE)));
    }

    for column in columns {
        header = header.child(table_cell(theme, &column.title, column.width, true));
    }

    header.into_any_element()
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
    let next = geometry::offset_revealing(offset, top, height, viewport, max);
    if next != offset {
        scroll.set_offset(gpui::point(gpui::px(0.), gpui::px(next)));
    }
}

/// 列表卡片。
fn render_list(list: &List, theme: &Theme, cx: &mut gpui::Context<GuiApp>) -> AnyElement {
    let filtered = list.filtered();
    let mut column = div()
        .w(px(LIST_W))
        .max_h(px(LIST_MAX_H))
        .flex()
        .flex_col()
        .gap(px(SPACE_SM))
        .p(px(SPACE_MD))
        .rounded(px(RADIUS_LG))
        .bg(theme.raised)
        .border_1()
        .border_color(theme.border_strong);

    column = column.child(
        div()
            .flex()
            .items_center()
            .justify_between()
            .px(px(SPACE_XXS))
            .text_size(px(FONT_SMALL))
            .text_color(theme.text_tertiary)
            .child(SharedString::from(list.title.clone()))
            .child(SharedString::from(format!("{} items", filtered.len()))),
    );

    // 搜索框
    column = column.child(
        div()
            .w_full()
            .h(px(CONTROL_H_LG))
            .px(px(SPACE_SM))
            .rounded(px(RADIUS_MD))
            .flex()
            .items_center()
            .bg(theme.inset)
            .border_1()
            .border_color(theme.border)
            .text_size(px(FONT_BODY))
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
                            .h(px(ICON_SIZE))
                            .ml(px(SPACE_XXS))
                            .bg(theme.accent),
                    ),
            ),
    );

    // 和搜索框之间留一道空隙：所有列表面板（Ctrl-M、/history、模型、会话…）共用同一个版式
    let mut rows = rows_container("overlay-rows", &list.scroll, 340.)
        .flex_1()
        .mt(px(SPACE_XS));

    // 表头不随行滚动：贴在列表上方，列宽与行内单元格一致
    if !list.columns.is_empty() {
        let checked = list.entries.iter().any(|entry| entry.checked.is_some());
        column = column.child(table_header(theme, &list.columns, checked));
    }

    if filtered.is_empty() {
        rows = rows.child(
            div()
                .px(px(SPACE_SM))
                .py(px(SPACE_SM))
                .text_size(px(FONT_SMALL))
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
                columns: &list.columns,
                cells: &entry.cells,
                detail: &entry.detail,
                detail_inline: false,
                checked: entry.checked,
                selected: index == list.selected,
                height: ROW_HEIGHT,
            },
            ("overlay-row", index),
            cx.listener(move |this, _, _, cx| this.pick_entry(index, cx)),
        ));
    }
    column = column.child(rows);

    let hint: String = if let Some(hint) = &list.hint {
        hint.clone()
    } else if list.detail_source != DetailSource::None {
        "↑↓ select · Space toggle · Enter detail · Esc close".to_string()
    } else if list.space_activates {
        "↑↓ select · Space change value · Esc close".to_string()
    } else if list.entries.iter().any(|entry| entry.checked.is_some()) {
        "↑↓ select · Enter toggle · Esc close".to_string()
    } else {
        "↑↓ select · Enter confirm · Esc close".to_string()
    };
    column = column.child(
        div()
            .px(px(SPACE_XXS))
            .text_size(px(FONT_CAPTION))
            .text_color(theme.text_ghost)
            .child(SharedString::from(hint)),
    );
    column.into_any_element()
}

/// 信息卡片：标题 + 可滚动主体（表格或纯文本）+ 底部提示。
fn render_info(info: &Info, theme: &Theme) -> AnyElement {
    let mut column = div()
        .w(px(INFO_W))
        .max_h(px(INFO_MAX_H))
        .flex()
        .flex_col()
        .gap(px(SPACE_SM))
        .p(px(SPACE_MD))
        .rounded(px(RADIUS_LG))
        .bg(theme.raised)
        .border_1()
        .border_color(theme.border_strong)
        .child(
            div()
                .text_size(px(FONT_SMALL))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(SharedString::from(info.title.clone())),
        );

    if info.rows.is_empty() {
        let mut body = info_body(&info.scroll);
        for line in &info.lines {
            body = body.child(
                div()
                    .text_size(px(FONT_SMALL))
                    .text_color(theme.text_secondary)
                    .child(SharedString::from(line.clone())),
            );
        }
        column = column.child(body);
    } else {
        column =
            column
                .child(info_header(theme))
                .child(info_table(&info.rows, theme, &info.scroll));
    }

    column
        .child(
            div()
                .mt(px(SPACE_XS))
                .text_size(px(FONT_CAPTION))
                .text_color(theme.text_ghost)
                .child(SharedString::from("↑↓ scroll · Esc close")),
        )
        .into_any_element()
}

/// 详情卡片主体：有高度上限、纵向滚动的容器（表格与纯文本共用同一个滚动句柄）。
fn info_body(scroll: &ScrollHandle) -> gpui::Stateful<Div> {
    rows_container("overlay-info-body", scroll, INFO_BODY_MAX_H).mt(px(SPACE_XS))
}

/// 详情表格的表头（属性 / 值）：贴在滚动主体上方，列宽与行内单元格一致。
fn info_header(theme: &Theme) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(SPACE_SM))
        .px(px(SPACE_XS))
        .pb(px(SPACE_XS))
        .border_b_1()
        .border_color(theme.border)
        .text_size(px(FONT_CAPTION))
        .text_color(theme.text_ghost)
        .child(
            div()
                .flex_none()
                .w(px(INFO_LABEL_W))
                .child(SharedString::from("Property")),
        )
        .child(div().flex_1().min_w_0().child(SharedString::from("Value")))
        .into_any_element()
}

/// 详情页表格主体：「属性 / 值」两列，值按列宽折行（不截断），值是 URL 时用链接色。
fn info_table(rows: &[InfoRow], theme: &Theme, scroll: &ScrollHandle) -> gpui::Stateful<Div> {
    let mut table = info_body(scroll);

    for (index, row) in rows.iter().enumerate() {
        let link = is_url(&row.value);
        let value = div().flex_1().min_w_0().text_size(px(FONT_SMALL));
        let value = if link {
            let url = row.value.clone();
            value
                .id(("info-link", index))
                .text_color(theme.link)
                .cursor_pointer()
                .hover(|style| style.underline())
                .on_click(move |_, _, _| open_url(&url))
                .child(SharedString::from(row.value.clone()))
                .into_any_element()
        } else {
            value
                .text_color(theme.text_secondary)
                .child(SharedString::from(row.value.clone()))
                .into_any_element()
        };
        table = table.child(
            div()
                .flex()
                .items_start()
                .gap(px(SPACE_SM))
                .px(px(SPACE_XS))
                .text_size(px(FONT_SMALL))
                .child(
                    div()
                        .flex_none()
                        .w(px(INFO_LABEL_W))
                        .truncate()
                        .text_color(theme.text_tertiary)
                        .child(SharedString::from(row.key.clone())),
                )
                .child(value),
        );
    }
    table
}

/// 详情页里可点击的值：只看 `http(s)://` 开头的绝对链接。
fn is_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

/// 用系统默认浏览器打开链接（`webbrowser` crate，与 TUI 的 Ctrl+click 同一条路径）。
fn open_url(url: &str) {
    _ = webbrowser::open(url);
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

    use crate::core::extensions::{Extension, ExtensionHook, OverlayEvent, register_extension};

    /// 认领 id=7 的假扩展：覆盖层内容由扩展提供，GUI 只缓存一帧。
    struct FakeOverlay;

    /// 假扩展「当前选中的行」（`usize::MAX` 表示没有选中项）。
    static SELECTED: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(usize::MAX);

    impl Extension for FakeOverlay {
        /// 扩展名（避免与其它测试冲突）。
        fn name(&self) -> &str {
            "gui-overlay-test"
        }

        /// 不提供工具。
        fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
            Vec::new()
        }

        /// 只声明覆盖层钩子。
        fn hooks(&self) -> Vec<ExtensionHook> {
            vec![ExtensionHook::Overlay]
        }

        /// 只认领 id=7；选中行由 [`SELECTED`] 控制，便于断言跟随行为。
        fn overlay_view(&self, id: u64, _cols: u16) -> Option<OverlayView> {
            if id != 7 {
                return None;
            }
            let mut view = OverlayView::inline("t", Vec::new(), 5);
            let selected = SELECTED.load(std::sync::atomic::Ordering::Relaxed);
            view.selected = (selected != usize::MAX).then_some(selected);
            Some(view)
        }

        /// 测试里不消费任何按键。
        fn on_overlay_event(&self, _id: u64, _ev: &OverlayEvent) -> bool {
            false
        }
    }

    /// 扩展覆盖层按 id 拉取内容；没人认领的 id 返回 None（调用方据此收起）。
    #[test]
    fn ext_overlay_pulls_the_extension_view() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        register_extension(FakeOverlay);

        let overlay = ExtOverlay::open(7).expect("认领 id=7");
        assert_eq!(overlay.view.title, "t");
        assert!(!overlay.input_active);
        assert!(ExtOverlay::open(8).is_none());
    }

    /// 选中行变了才跟随：`refresh` 把新选中项记账（真正的滚动由 `reveal_row` 施加）。
    #[test]
    fn ext_overlay_tracks_the_selected_row() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        register_extension(FakeOverlay);

        SELECTED.store(2, std::sync::atomic::Ordering::Relaxed);
        let mut overlay = ExtOverlay::open(7).expect("认领 id=7");
        assert_eq!(overlay.last_selected, Some(2));

        SELECTED.store(5, std::sync::atomic::Ordering::Relaxed);
        assert!(overlay.refresh(), "扩展仍认领该 id");
        assert_eq!(overlay.last_selected, Some(5));

        // 同一次选中重复拉取不会反复滚动
        assert!(overlay.refresh());
        assert_eq!(overlay.last_selected, Some(5));
        SELECTED.store(usize::MAX, std::sync::atomic::Ordering::Relaxed);
    }
}
