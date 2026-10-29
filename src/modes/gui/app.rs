//! GUI 根视图：自绘标题栏 + 侧栏 + 对话流 + 输入卡片 + 面板。
//!
//! 本文件只做「状态 + 渲染 + 按键分发」，与 agent 的一切交互都经 [`CommandTx`] 下发、
//! 经 [`GuiEvent`] 上行，视图不持有 agent。

use super::bridge::{GuiEvent, Snapshot};
use super::components::{chip, dot};
use super::composer;
use super::overlay::{
    self, Action, DetailSource, Entry, Info, InfoKind, List, ListKind, Overlay, SettingKey,
};
use super::panel::{self, PanelTab, RightPanel};
use super::sidebar::{self, SessionRow};
use super::suggest::{self, Suggest, Trigger};
use super::text_edit::{self, Editor};
use super::theme::{self, Theme, px};
use super::transcript::{self, Note, Transcript};
use crate::cli::session_picker::ProgressiveMetas;
use crate::core::provider::AgentMessage;
use crate::core::settings_manager;
use crate::modes::interactive::agent_actor::{AgentCommand, CommandTx};
use gpui::{
    AnyElement, App, Bounds, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    IntoElement, KeyDownEvent, MouseButton, Pixels, Render, ScrollHandle, SharedString, Task,
    UTF16Selection, Window, canvas, div, prelude::*,
};
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

/// 正文列最大宽度：宽屏下不铺满整屏，阅读更稳。
const CONTENT_MAX_WIDTH: f32 = 760.0;
/// 标题栏高度。
const CHROME_HEIGHT: f32 = 40.0;
/// 对话流顶部留白。
const FLOW_PADDING_TOP: f32 = 20.0;
/// 对话流底部留白：滚到底时最后一行不贴输入卡片。
const FLOW_PADDING_BOTTOM: f32 = 72.0;
/// 对话流滚动条宽度（滑块本身比这个热区窄，方便抓取）。
const SCROLLBAR_WIDTH: f32 = 8.0;
/// 滚动条热区宽度：滑块附近都算拖动范围，不然 6px 太难点中。
const SCROLLBAR_HIT_WIDTH: f32 = 14.0;
/// 右侧面板默认宽度（逻辑像素）
const PANEL_DEFAULT_WIDTH: f32 = 320.0;
/// 右侧面板最窄
const PANEL_MIN_WIDTH: f32 = 240.0;
/// 右侧面板最宽
const PANEL_MAX_WIDTH: f32 = 620.0;

/// 下拉菜单离窗口边缘至少留出的距离（逻辑像素）
const MENU_EDGE_MARGIN: f32 = 12.0;

/// 下拉菜单每行的高度（逻辑像素，估高度用）
const DROPDOWN_ROW_HEIGHT: f32 = 24.0;

/// 下拉菜单的最大高度（超过就滚动）
const DROPDOWN_MAX_HEIGHT: f32 = 360.0;

/// 循环取下一个下标：`dir` 为 ±1，`current` 未知（还没对上号）时取第一个。
///
/// `len` 必须非空：调用方都先判过空列表。
fn cycle_index(current: Option<usize>, len: usize, dir: i32) -> usize {
    let Some(current) = current else {
        return 0;
    };
    ((current as i32 + dir).rem_euclid(len as i32)) as usize
}

/// `Ctrl+P` 循环的候选模型：`scoped` 非空时严格按 scope 顺序、跳过目录里不可用的；
/// 为空时才退回全部已配置模型（与 TUI 的模型循环一致）。
fn cycle_models(
    scoped: &[crate::core::model_scope::ScopedModel],
    all: &[(String, String)],
) -> Vec<(String, String)> {
    if scoped.is_empty() {
        return all.to_vec();
    }
    scoped
        .iter()
        .filter_map(|scoped| {
            all.iter()
                .find(|(provider, id)| provider == &scoped.provider && id == &scoped.id)
                .cloned()
        })
        .collect()
}

/// 下拉菜单上边距：贴按钮下方，但不许越过窗口底部（底部留 `MENU_EDGE_MARGIN`）。
fn clamp_menu_top(anchor_y: f32, height: f32, viewport_height: f32) -> f32 {
    let max_top = (viewport_height - height - MENU_EDGE_MARGIN).max(MENU_EDGE_MARGIN);
    anchor_y.clamp(MENU_EDGE_MARGIN, max_top)
}

/// 下拉菜单左边距：贴着按钮，但不许越过窗口右侧（右侧留 `MENU_EDGE_MARGIN`）。
fn clamp_menu_left(anchor_x: f32, width: f32, viewport_width: f32) -> f32 {
    let max_left = (viewport_width - width - MENU_EDGE_MARGIN).max(MENU_EDGE_MARGIN);
    anchor_x.clamp(MENU_EDGE_MARGIN, max_left)
}

/// 对话流正文的行高（相对字号；默认 φ = 1.618 会让行内高亮背景过高并连成一片）
const TRANSCRIPT_LINE_HEIGHT: f32 = 1.32;

/// 输入卡片每侧的内边距（逻辑像素，对应卡片的 `px(12.)`）：算可用折行宽度时两侧各扣一份
const COMPOSER_H_PADDING: f32 = 12.0;

/// prompt 历史条目的标签宽度上限（显示宽度，CJK 字符按 2 算）。
const HISTORY_LABEL_WIDTH: usize = 60;

/// toast 停留时长
const TOAST_DURATION: std::time::Duration = std::time::Duration::from_millis(3600);
/// 同时最多保留几条 toast
const TOAST_MAX: usize = 4;

/// 侧栏默认宽度（逻辑像素）
const SIDEBAR_DEFAULT_WIDTH: f32 = 252.0;
/// 侧栏最窄：再窄标题就全被截断
const SIDEBAR_MIN_WIDTH: f32 = 180.0;
/// 侧栏最宽：留出主区
const SIDEBAR_MAX_WIDTH: f32 = 460.0;
/// 拖拽热区宽度（骑在右边框上）
const SIDEBAR_HANDLE_WIDTH: f32 = 6.0;
/// 补全面板里除候选行以外的固定高度（输入框 + 内边距，逻辑像素）
const SUGGEST_CHROME_HEIGHT: f32 = 46.0;

/// 补全面板最多显示几行。
const SUGGEST_ROWS: usize = 8;
/// 补全面板单行高度（逻辑像素）。
const SUGGEST_ROW_HEIGHT: f32 = 26.0;
/// 补全面板与输入卡片之间的间隙。
const SUGGEST_GAP: f32 = 8.0;

/// 滚动条滑块最小高度。
const SCROLLBAR_MIN_THUMB: f32 = 24.0;

/// 可拖拽滚动条所在的滚动区。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScrollArea {
    /// 对话流
    Transcript,
    /// 侧栏会话列表
    Sidebar,
}

impl ScrollArea {
    /// 元素 id 用的编号（同一帧里两个滚动条不能撞 id）。
    fn index(self) -> usize {
        match self {
            ScrollArea::Transcript => 0,
            ScrollArea::Sidebar => 1,
        }
    }
}

/// 下拉菜单里的一项。
struct MenuItem {
    /// 主文本
    label: String,
    /// 右侧说明
    detail: String,
    /// 选中后执行的动作
    action: Action,
}

/// 贴按钮下方弹出的下拉菜单。
struct Dropdown {
    /// 菜单项
    items: Vec<MenuItem>,
    /// 锚点（窗口坐标，菜单贴它下方）
    anchor: gpui::Point<Pixels>,
    /// 菜单宽度（逻辑像素）
    width: f32,
    /// 内容超高时滚动（模型列表很长）
    scroll: bool,
}

/// `@` 补全的文件路径缓存。
#[derive(Default)]
struct FilesCache {
    /// 缓存对应的目录
    cwd: String,
    /// 是否正在后台列文件
    loading: bool,
    /// 相对路径
    paths: Vec<String>,
}

/// 侧栏会话行的右键菜单。
struct SessionMenu {
    /// 会话文件路径
    path: String,
    /// 菜单锚点（右键位置，窗口坐标）
    position: gpui::Point<Pixels>,
}

/// 会话对话框：重命名 / 删除确认。
enum SessionDialog {
    /// 重命名：编辑器里是新名字
    Rename {
        /// 会话文件路径
        path: String,
        /// 名字输入框
        editor: Editor,
    },
    /// `/bug` 的复现描述：编辑器里是描述文本
    BugHint {
        /// 描述输入框
        editor: Editor,
    },
    /// 给当前会话树叶子打标签：编辑器里是标签文本（空 = 清除）
    Label {
        /// 标签输入框
        editor: Editor,
    },
    /// 导入会话：编辑器里是 jsonl 路径
    Import {
        /// 路径输入框
        editor: Editor,
    },
    /// 填 API key：编辑器里是 key 原文
    ApiKey {
        /// provider 名
        provider: String,
        /// key 输入框
        editor: Editor,
    },
    /// 改数值型设置：编辑器里是当前值
    Number {
        /// 改哪一项
        key: SettingKey,
        /// 数值输入框
        editor: Editor,
    },
    /// 删除前的二次确认
    Delete {
        /// 会话文件路径
        path: String,
        /// 展示用的会话标题
        title: String,
    },
}

/// 滚动条拖动状态。
#[derive(Clone, Copy)]
struct ScrollDrag {
    /// 正在拖哪个滚动区
    area: ScrollArea,
    /// 首次鼠标移动时的 y（与施加拖拽的位置同源，`None` = 还没定位）
    anchor_y: Option<Pixels>,
    /// 按下时的滚动偏移
    offset: Pixels,
}

/// toast 级别：决定配色与位置。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    /// 中性提示
    Info,
    /// 成功
    Success,
    /// 失败
    Error,
}

/// 一条 toast。
#[derive(Clone)]
struct Toast {
    /// 自增 id（到期按 id 移除）
    id: u64,
    /// 文本
    text: String,
    /// 级别
    kind: ToastKind,
}

/// 侧栏宽度拖拽状态。
#[derive(Clone, Copy)]
struct ResizeDrag {
    /// 首次鼠标移动时的 x（与施加拖拽的位置同源，`None` = 还没定位）
    anchor_x: Option<Pixels>,
    /// 按下时的侧栏宽度
    width: f32,
}

/// 按鼠标位移算新宽度（夹到上下限）：左边框往左拖变宽，所以位移取反。
fn resized_panel_width(press_width: f32, dx: f32) -> f32 {
    (press_width - dx).clamp(PANEL_MIN_WIDTH, PANEL_MAX_WIDTH)
}

/// 按鼠标位移算新宽度（夹到上下限）。
fn resized_width(press_width: f32, dx: f32) -> f32 {
    (press_width + dx).clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH)
}

/// GUI 根视图。
pub struct GuiApp {
    /// 设计令牌（明/暗两套）
    theme: Theme,
    /// 键盘焦点（根视图持有）
    focus: FocusHandle,
    /// 命令下发端
    cmd_tx: CommandTx,
    /// 事件排空任务（持有即运行）
    _events: Task<()>,
    /// 最近一次会话状态快照
    snapshot: Option<Box<Snapshot>>,
    /// 对话流
    transcript: Transcript,
    /// 输入框
    editor: Editor,
    /// 是否有一轮进行中
    busy: bool,
    /// 排队等待发送的消息数（busy 时提交进入队列）
    queued: usize,
    /// 对话流滚动句柄
    scroll: ScrollHandle,
    /// 侧栏会话列表滚动句柄
    sessions_scroll: ScrollHandle,
    /// 侧栏会话列表
    sessions: Vec<SessionRow>,
    /// 会话元数据的后台加载器（逐条回填；`None` 表示没在加载）
    sessions_loader: Option<ProgressiveMetas>,
    /// 侧栏是否还有会话元数据在读
    sessions_loading: bool,
    /// 侧栏轮询任务的持有者（丢弃即取消）
    _session_poll: Option<Task<()>>,
    /// 侧栏是否可见
    show_sidebar: bool,
    /// 当前覆盖层
    overlay: Option<Overlay>,
    /// 右侧面板（文件 / 变更）
    pub(super) panel: RightPanel,
    /// 是否展开全部工具输出
    expand_tools: bool,
    /// IME 正在组字的区间（UTF-16，`None` 表示没有在组字）
    marked_range: Option<Range<usize>>,
    /// 对话区实测宽度（上一帧）：滚动内容按它定宽，换行宽度才和实测宽度一致
    pane_width: Rc<Cell<Pixels>>,
    /// 上次钉底用的偏移（内容高度变了才重钉，用户自己滚开就不打扰）
    pinned_bottom: Rc<Cell<Option<Pixels>>>,
    /// 对话内容实测高度（上一帧）：滚动区里内容盒的实测高度偏小，用它补 min-height
    flow_height: Rc<Cell<Option<Pixels>>>,
    /// 输入框补全面板（None = 关闭）
    suggest: Option<Suggest>,
    /// 被手动关掉的补全触发位置（Esc / 退格删空）：同一次触发不再自动弹回
    suggest_dismissed: Option<usize>,
    /// `@` 补全用的文件路径缓存
    files_cache: FilesCache,
    /// 当前工作目录的 git 分支（非仓库为 None）
    branch: Option<String>,
    /// 上一次切换的条目与时间（面板重建后鼠标抬起会再触发一次 click，挡掉重复）
    last_toggle: Option<(String, std::time::Instant)>,
    /// 扩展面板的草稿：`(名称, 期望启用)`，None = 面板没开
    extensions_draft: Option<Vec<(String, bool)>>,
    /// `/skills` 面板的草稿规则（None = 面板没开，改动丢弃）
    skills_draft: Option<Vec<String>>,
    /// 屏幕右上角的 toast 队列
    toasts: Vec<Toast>,
    /// toast 自增 id
    toast_seq: u64,
    /// 右侧面板宽度（逻辑像素）
    panel_width: f32,
    /// 侧栏宽度（逻辑像素）
    sidebar_width: f32,
    /// 侧栏宽度拖拽状态
    resize_drag: Option<ResizeDrag>,
    /// 右侧面板宽度拖拽状态
    panel_resize: Option<ResizeDrag>,
    /// 补全面板自己的输入框：面板打开时键入进这里，主输入框保持触发词原样
    suggest_input: Editor,
    /// 补全面板候选区的滚动句柄
    suggest_scroll: ScrollHandle,
    /// 对话流的文本选区（按下/拖动建立）
    text_sel: Option<transcript::TextSel>,
    /// 对话流每行的元素边界（行序号 → 窗口坐标），点选时反查行
    line_bounds: Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>,
    /// 对话流每行文字元素的边界（行序号 → 窗口坐标），列定位按它算
    line_text_bounds: Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>,
    /// 对话流每行的文本与字号，复制取文本、点选定位都按它
    line_rows: Rc<RefCell<Vec<transcript::LineInfo>>>,
    /// 附件按钮的补全过滤：true 只列图片
    attach_images: bool,
    /// 标题栏/输入区的下拉菜单
    dropdown: Option<Dropdown>,
    /// 输入卡片在窗口里的实测位置（补全面板贴它上沿）
    composer_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// 输入框多行内容的滚动状态（超出显示行数时靠它滚）
    composer_scroll: ScrollHandle,
    /// 上次把光标滚进视口时的 (行, 列)：光标没动就不抢用户的滚动
    composer_cursor: Rc<Cell<Option<(usize, usize)>>>,
    /// 拖动滚动条的状态
    scroll_drag: Option<ScrollDrag>,
    /// 鼠标是否在对话流上（滚动条只在悬停时显示）
    transcript_hovered: bool,
    /// 鼠标是否在侧栏列表上
    sidebar_hovered: bool,
    /// 会话行右键菜单
    session_menu: Option<SessionMenu>,
    /// 会话对话框（重命名 / 删除确认）
    dialog: Option<SessionDialog>,
}

impl GuiApp {
    /// 构造根视图：接上事件通道，并在有初始消息时立即开跑第一轮。
    pub fn new(
        theme: Theme,
        cmd_tx: CommandTx,
        event_rx: UnboundedReceiver<GuiEvent>,
        initial_messages: Vec<AgentMessage>,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let events = cx.spawn(async move |this, cx| {
            let mut event_rx = event_rx;
            while let Some(event) = event_rx.recv().await {
                if this
                    .update(cx, |app: &mut Self, cx| {
                        app.apply_event(event, cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        if !initial_messages.is_empty() {
            _ = cmd_tx.send(AgentCommand::PromptBatch(initial_messages));
        }

        let mut app = Self {
            theme,
            focus,
            cmd_tx,
            _events: events,
            snapshot: None,
            transcript: Transcript::default(),
            editor: Editor::new(),
            busy: false,
            queued: 0,
            scroll: ScrollHandle::new(),
            sessions_scroll: ScrollHandle::new(),
            sessions: Vec::new(),
            sessions_loader: None,
            sessions_loading: false,
            _session_poll: None,
            show_sidebar: true,
            overlay: None,
            panel: RightPanel::default(),
            expand_tools: false,
            marked_range: None,
            pane_width: Rc::new(Cell::new(px(0.))),
            pinned_bottom: Rc::new(Cell::new(None)),
            flow_height: Rc::new(Cell::new(None)),
            suggest: None,
            files_cache: FilesCache::default(),
            branch: None,
            last_toggle: None,
            extensions_draft: None,
            skills_draft: None,
            toasts: Vec::new(),
            toast_seq: 0,
            panel_width: PANEL_DEFAULT_WIDTH,
            sidebar_width: SIDEBAR_DEFAULT_WIDTH,
            resize_drag: None,
            panel_resize: None,
            suggest_dismissed: None,
            suggest_input: Editor::new(),
            suggest_scroll: ScrollHandle::new(),
            text_sel: None,
            line_bounds: Rc::new(RefCell::new(HashMap::new())),
            line_text_bounds: Rc::new(RefCell::new(HashMap::new())),
            line_rows: Rc::new(RefCell::new(Vec::new())),
            attach_images: false,
            dropdown: None,
            composer_bounds: Rc::new(Cell::new(None)),
            composer_scroll: ScrollHandle::new(),
            composer_cursor: Rc::new(Cell::new(None)),
            scroll_drag: None,
            transcript_hovered: false,
            sidebar_hovered: false,
            session_menu: None,
            dialog: None,
        };
        app.refresh_branch();
        // 当前目录也算一条「最近用过」，菜单里能直接看到
        remember_cwd(&app.cwd());
        app.reload_sessions(cx);
        app
    }

    /// 视图焦点句柄（打开窗口时用来把键盘交给它）。
    pub fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }

    /// 上行事件 → 状态。
    fn apply_event(&mut self, event: GuiEvent, cx: &mut Context<Self>) {
        match event {
            GuiEvent::Agent(event) => {
                let kind = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match kind {
                    "agent_start" => self.busy = true,
                    "agent_settled" => {
                        self.busy = false;
                        if self.queued > 0 {
                            self.queued -= 1;
                        }
                    }
                    _ => {}
                }
                self.transcript.apply(&event);
                self.follow_bottom();
            }
            GuiEvent::History(messages) => {
                self.transcript = Transcript::from_messages(&messages);
                // 上一份内容的实测高度对新内容无效，置空后按新内容重新量
                self.flow_height.set(None);
                // 换会话/重载历史总是回到最新内容，重新接管钉底
                self.pinned_bottom.set(None);
                self.scroll_to_bottom();
            }
            GuiEvent::Notice(message) => {
                // 命令/压缩/重载的结果常驻对话区（与 TUI 一致），而不是一闪而过的 toast
                self.transcript.push_note(Note::Info(message));
                self.follow_bottom();
            }
            GuiEvent::Error(message) => {
                self.busy = false;
                self.toast(message.clone(), ToastKind::Error, cx);
                self.transcript.push_note(Note::Error(message));
                self.follow_bottom();
            }
            GuiEvent::Snapshot(snapshot) => {
                let cwd_changed = self
                    .snapshot
                    .as_ref()
                    .is_some_and(|old| old.cwd != snapshot.cwd);
                if cwd_changed {
                    self.reload_sessions(cx);
                    self.refresh_branch();
                    remember_cwd(&snapshot.cwd);
                }
                self.snapshot = Some(snapshot);
            }
        }
    }

    /// 让 transcript 跟随最新内容。
    fn scroll_to_bottom(&mut self) {
        self.scroll.scroll_to_bottom();
    }

    /// 新内容来了才跟随到底：用户滚上去看历史时不能每来一条事件就把他拽回底部。
    ///
    /// `pinned_bottom` 为 `None` 表示用户自己滚走了（见 `pin_bottom`），此时不抢。
    fn follow_bottom(&mut self) {
        if self.pinned_bottom.get().is_some() {
            self.scroll_to_bottom();
        }
    }

    /// 按输入框当前内容重算补全面板：触发词变了或没有触发词就开关面板。
    fn refresh_suggest(&mut self, cx: &mut Context<Self>) {
        // 面板开着时键入的是面板输入框，主输入框不会变，别覆盖面板状态
        if self.suggest.is_some() {
            return;
        }
        let before = self.editor.text_before_cursor();
        let Some((trigger, start, query)) = suggest::detect(&before) else {
            self.suggest = None;
            self.suggest_dismissed = None;
            self.attach_images = false;
            return;
        };
        // 同一次触发被 Esc / 退格手动关掉后不再自动弹回；触发位置变了才重新生效
        if !suggest_allowed(self.suggest_dismissed, start) {
            self.suggest = None;
            return;
        }
        self.suggest_dismissed = None;
        // 面板输入框显示已输入的前缀，之后继续敲的字符都进它
        self.suggest_input = Editor::new();
        if !query.is_empty() {
            self.suggest_input.insert(&query);
        }
        self.rebuild_suggest(trigger, start, query, cx);
    }

    /// 面板开着时按面板输入框重算候选。
    fn refresh_suggest_from_input(&mut self, cx: &mut Context<Self>) {
        let Some((trigger, start)) = self.suggest.as_ref().map(|s| (s.trigger, s.start)) else {
            return;
        };
        let query = self.suggest_input.text();
        self.rebuild_suggest(trigger, start, query, cx);
    }

    /// 文件列表到位/输入变化后重算：面板开着按面板输入，否则按主输入框。
    fn refresh_suggest_any(&mut self, cx: &mut Context<Self>) {
        if self.suggest.is_some() {
            self.refresh_suggest_from_input(cx);
        } else {
            self.refresh_suggest(cx);
        }
    }

    /// 重算候选并写回面板状态（`start`/`trigger` 由触发时定下，之后不变）。
    fn rebuild_suggest(
        &mut self,
        trigger: Trigger,
        start: usize,
        query: String,
        cx: &mut Context<Self>,
    ) {
        let items = match trigger {
            Trigger::Command => {
                // 命令 + 本会话技能（`/skill:名称`，与 TUI 的技能命令同形）
                let mut items = suggest::commands(&query);
                let prefix = query.trim();
                for (name, description, _source) in self
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.skills.clone())
                    .unwrap_or_default()
                {
                    let label = format!("/skill:{name}");
                    if !prefix.is_empty() && !label.trim_start_matches('/').starts_with(prefix) {
                        continue;
                    }
                    items.push(suggest::Item {
                        label,
                        detail: description,
                    });
                }
                // 已启用扩展声明的命令（执行入口还挂在 TUI 层，这里先按现状列出）
                for command in crate::core::extensions::registered_commands() {
                    let label = format!("/{}", command.name);
                    if !prefix.is_empty() && !label.trim_start_matches('/').starts_with(prefix) {
                        continue;
                    }
                    items.push(suggest::Item {
                        label,
                        detail: format!("{} (extension)", command.description),
                    });
                }
                items
            }
            Trigger::File => {
                self.ensure_files(cx);
                suggest::files(&self.files_cache.paths, &query, self.attach_images)
            }
            // shell 命令直接执行，不列候选；面板只留一句说明
            Trigger::Shell => Vec::new(),
        };
        // 同一触发词 + 同一前缀：保留选中项，避免每敲一个字符跳回第一行
        let keep = self
            .suggest
            .as_ref()
            .filter(|old| {
                old.trigger == trigger && old.query == query && old.items.len() == items.len()
            })
            .map(|old| old.selected)
            .unwrap_or(0);
        let selected = keep.min(items.len().saturating_sub(1));
        self.suggest = Some(Suggest {
            trigger,
            start,
            query,
            items,
            selected,
        });
    }

    /// 确保 `@` 补全的文件列表已按当前目录加载（第一次用时后台列一次）。
    fn ensure_files(&mut self, cx: &mut Context<Self>) {
        let cwd = self.cwd();
        if self.files_cache.loading
            || (self.files_cache.cwd == cwd && !self.files_cache.paths.is_empty())
        {
            return;
        }
        self.files_cache = FilesCache {
            cwd: cwd.clone(),
            loading: true,
            paths: Vec::new(),
        };
        cx.spawn(async move |this, cx| {
            let paths = cx
                .background_executor()
                .spawn(async move { panel::file_paths(&cwd) })
                .await;
            _ = this.update(cx, |this, cx| {
                this.files_cache.loading = false;
                this.files_cache.paths = paths;
                this.refresh_suggest_any(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// 鼠标点选输入框：光标落到 (行, 列)，`extend` 为真时扩展选区（拖选/Shift）。
    fn set_cursor_at(&mut self, row: usize, col: usize, extend: bool, cx: &mut Context<Self>) {
        // 行尾后的点击（占位方块之后）夹到行尾
        self.editor.set_cursor(row, col, extend);
        cx.notify();
    }

    /// 按窗口坐标找出落在哪一行文字上，返回（行序号, 行内字符下标）。
    ///
    fn line_hit(
        &self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
    ) -> Option<(usize, usize)> {
        let hit = self
            .line_bounds
            .borrow()
            .iter()
            .find(|(_, rect)| rect.contains(&position))
            .map(|(index, rect)| (*index, *rect))?;
        let (index, rect) = hit;
        let line = self
            .line_rows
            .borrow()
            .iter()
            .find(|line| line.index == index)
            .cloned()?;
        // 列号按文字元素自己的盒子算：外层行盒含 padding-left / 列表缩进，
        // 用它的左边界换算会让点选落点整体偏右
        let text = self
            .line_text_bounds
            .borrow()
            .get(&index)
            .copied()
            .unwrap_or(rect);
        // 用 gpui 自己的行布局定位：它会按实际宽度折行，y 落在哪一折行也算进去
        let local = text_local(position, text);
        Some((
            index,
            self.char_index(&line, text.size.width, local, window),
        ))
    }

    /// 行内某个位置落在第几个字符（字符下标，不是字节）。
    ///
    /// 直接用 gpui 自己的排版结果：`shape_text` 按行宽折行后，每个视觉行带着
    /// 自己的字形位置，`index_for_position` 再把（含 y 的）鼠标位置换成字节下标，
    /// 折行、中文双宽、混合字体都由排版器算，不做任何估算。
    fn char_index(
        &self,
        line: &transcript::LineInfo,
        width: Pixels,
        local: gpui::Point<Pixels>,
        window: &mut Window,
    ) -> usize {
        let total = line.text.chars().count();
        if line.text.is_empty() || width <= px(0.) {
            return 0;
        }
        let system = window.text_system().clone();
        let style = gpui::TextStyle {
            font_family: theme::UI_FONT_FAMILY.into(),
            ..Default::default()
        };
        let size = px(line.font_size);
        let runs = [style.to_run(line.text.len())];
        let Ok(lines) = system.shape_text(line.text.clone().into(), size, &runs, Some(width), None)
        else {
            return 0;
        };
        let Some(wrapped) = lines.first() else {
            return 0;
        };
        let line_height = px((line.font_size * TRANSCRIPT_LINE_HEIGHT).round());
        let byte = match wrapped.index_for_position(local, line_height) {
            Ok(byte) | Err(byte) => byte,
        };
        line.text
            .get(..byte.min(line.text.len()))
            .map(|head| head.chars().count())
            .unwrap_or(total)
    }

    /// 鼠标按下：建立选区（`extend` = 按住 Shift 接着扩）。
    fn start_text_selection(
        &mut self,
        position: gpui::Point<Pixels>,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((line, col)) = self.line_hit(position, window) else {
            if self.text_sel.is_some() {
                self.text_sel = None;
                cx.notify();
            }
            return;
        };
        let anchor = match (self.text_sel, extend) {
            (Some(selection), true) => selection.anchor,
            _ => (line, col),
        };
        self.text_sel = Some(transcript::TextSel {
            anchor,
            focus: (line, col),
        });
        cx.notify();
    }

    /// 拖动：把选区终点挪到鼠标位置。
    fn extend_text_selection(
        &mut self,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(selection) = self.text_sel else {
            return;
        };
        let hit = self.line_hit(position, window);
        let Some((line, col)) = hit else {
            return;
        };
        let next = transcript::TextSel {
            anchor: selection.anchor,
            focus: (line, col),
        };
        if next != selection {
            self.text_sel = Some(next);
            cx.notify();
        }
    }

    /// 点用户气泡：展开/收起过长的那条消息（`None` 时跟随全局 Ctrl-L 开关）。
    pub(super) fn toggle_user(&mut self, index: usize, cx: &mut Context<Self>) {
        let expand_tools = self.expand_tools;
        if let Some(transcript::Block::User(message)) = self.transcript.blocks_mut().get_mut(index)
        {
            let expanded = message.expanded.unwrap_or(expand_tools);
            message.expanded = Some(!expanded);
        }
        cx.notify();
    }

    /// 点工具卡片：展开/收起这一块的输出（`None` 时跟随全局 Ctrl-L 开关）。
    pub(super) fn toggle_tool(&mut self, index: usize, cx: &mut Context<Self>) {
        let expand_tools = self.expand_tools;
        if let Some(transcript::Block::Tool(tool)) = self.transcript.blocks_mut().get_mut(index) {
            let expanded = tool.expanded.unwrap_or(expand_tools);
            tool.expanded = Some(!expanded);
        }
        cx.notify();
    }

    /// 上下移动补全选中项（越界回绕）。
    fn move_suggestion(&mut self, delta: i32) {
        let Some(suggest) = self.suggest.as_mut() else {
            return;
        };
        let len = suggest.items.len();
        if len == 0 {
            return;
        }
        let next = suggest.selected as i32 + delta;
        suggest.selected = next.clamp(0, len as i32 - 1) as usize;
    }

    /// 把选中候选写进输入框（命令/文件补全）。
    fn apply_suggestion(&mut self, cx: &mut Context<Self>) {
        let Some(suggest) = self.suggest.clone() else {
            return;
        };
        let Some(item) = suggest.items.get(suggest.selected) else {
            return;
        };
        let end = self.editor.cursor_byte();
        // `/` 命令是动作不是文本：清掉触发文本直接执行
        if suggest.trigger == Trigger::Command {
            let name = item.label.trim_start_matches('/').to_string();
            self.editor.replace_byte_range(suggest.start..end, "");
            self.suggest = None;
            self.suggest_dismissed = None;
            self.run_slash(&name, cx);
            return;
        }
        let insert = format!("{} ", item.label);
        self.editor.replace_byte_range(suggest.start..end, &insert);
        self.suggest = None;
        self.suggest_dismissed = None;
        self.refresh_suggest(cx);
    }

    /// 点击候选：选中它并写入输入框。
    fn pick_suggestion(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(suggest) = self.suggest.as_mut() {
            suggest.selected = index;
        }
        self.apply_suggestion(cx);
        cx.notify();
    }

    /// 补全面板：贴在输入卡片上沿的覆盖层（不占布局空间）。
    fn suggest_panel(
        &self,
        suggest: &Suggest,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut column = div()
            .w_full()
            .rounded(px(10.))
            .bg(theme.composer)
            .border_1()
            .border_color(theme.border_strong)
            .p(px(4.))
            .flex()
            .flex_col()
            .text_size(px(12.5));
        column = column.child(
            div()
                .w_full()
                .rounded(px(6.))
                .bg(theme.canvas)
                .border_1()
                .border_color(theme.border)
                .px(px(8.))
                .py(px(5.))
                .text_size(px(12.5))
                .text_color(theme.text)
                .child(composer::render(&self.suggest_input, theme)),
        );
        if suggest.items.is_empty() {
            column = column.child(
                div()
                    .px(px(8.))
                    .py(px(6.))
                    .text_size(px(11.5))
                    .text_color(theme.text_ghost)
                    .child(SharedString::from(if suggest.trigger == Trigger::Shell {
                        "Runs locally; output goes into the context"
                    } else {
                        "No matches"
                    })),
            );
        }
        // 候选区与 Ctrl-K 面板同一套行渲染 + 可滚动容器：条目多时滚轮能翻到
        let mut rows = overlay::rows_container(
            "suggest-rows",
            &self.suggest_scroll,
            SUGGEST_ROW_HEIGHT * SUGGEST_ROWS as f32,
        );
        for (index, item) in suggest.items.iter().enumerate() {
            rows = rows.child(overlay::list_row(
                theme,
                overlay::RowSpec {
                    label: &item.label,
                    detail: &item.detail,
                    checked: None,
                    selected: index == suggest.selected,
                    height: SUGGEST_ROW_HEIGHT,
                },
                ("suggest-row", index),
                cx.listener(move |this, _, _, cx| this.pick_suggestion(index, cx)),
            ));
        }
        column = column.child(rows);
        column.into_any_element()
    }

    /// 模型下拉菜单：只作用于当前会话，不写配置。
    ///
    /// 同步取模型列表：异步任务要等前台执行器被唤醒，菜单可能在用户下次交互前
    /// 都不出现（曾经就因此完全看不到菜单）。模型表是本地 JSON，直接读足够快。
    fn open_model_menu(&mut self, anchor: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let mut items: Vec<MenuItem> = Vec::new();
        for provider in crate::core::auth::list_configured_providers() {
            for (id, name) in crate::core::model_resolver::list_models(&provider) {
                items.push(MenuItem {
                    label: format!("{provider} / {id}"),
                    detail: name,
                    action: Action::Command(Box::new(AgentCommand::SwitchModel {
                        provider: provider.clone(),
                        model_id: id,
                        persist: false,
                    })),
                });
            }
        }
        if items.is_empty() {
            items.push(MenuItem {
                label: "No models configured".into(),
                detail: String::new(),
                action: Action::None,
            });
        }
        self.dropdown = Some(Dropdown {
            items,
            anchor,
            width: 280.,
            scroll: true,
        });
        cx.notify();
    }

    /// 思考级别下拉菜单：同样只影响当前会话。
    fn open_thinking_menu(&mut self, anchor: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let items: Vec<MenuItem> = self
            .snapshot
            .as_ref()
            .map(|s| s.thinking_levels.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|level| MenuItem {
                label: format!("thinking {level}"),
                detail: String::new(),
                action: Action::Command(Box::new(AgentCommand::SetThinking {
                    level,
                    persist: false,
                })),
            })
            .collect();
        let items = if items.is_empty() {
            vec![MenuItem {
                label: "This model has no thinking levels".into(),
                detail: String::new(),
                action: Action::None,
            }]
        } else {
            items
        };
        self.dropdown = Some(Dropdown {
            items,
            anchor,
            width: 180.,
            scroll: false,
        });
        cx.notify();
    }

    /// 附件下拉菜单：文件 / 图片，选完插 `@` 让补全接着列路径。
    fn open_attach_menu(&mut self, anchor: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let items = vec![
            MenuItem {
                label: "File".into(),
                detail: "@ reference".into(),
                action: Action::Attach { images: false },
            },
            MenuItem {
                label: "Image".into(),
                detail: "@ reference".into(),
                action: Action::Attach { images: true },
            },
        ];
        self.dropdown = Some(Dropdown {
            items,
            anchor,
            width: 160.,
            scroll: false,
        });
        cx.notify();
    }

    /// 斜杠命令：与命令面板共用同一批动作。
    fn run_slash(&mut self, name: &str, cx: &mut Context<Self>) {
        match name {
            "new" => {
                _ = self.cmd_tx.send(AgentCommand::NewSession);
                self.transcript.clear();
                self.flow_height.set(None);
                self.reload_sessions(cx);
            }
            "resume" | "sessions" => self.open_session_picker(cx),
            "model" => self.open_model_picker(cx),
            "thinking" => self.open_thinking_picker(cx),
            "login" => self.open_login_panel(cx),
            "logout" => self.open_logout_panel(cx),
            "changelog" => self.open_changelog(cx),
            "giff" => {
                // 桌面端把 giff 的「看未提交改动」落成右侧变更面板：已经开着就关掉
                if self.panel.visible && self.panel.tab == PanelTab::Changes {
                    self.panel.visible = false;
                    cx.notify();
                } else {
                    self.toggle_panel(Some(PanelTab::Changes), cx);
                }
            }
            "tree" => {
                _ = self.cmd_tx.send(AgentCommand::Tree);
            }
            "debug" => {
                _ = self.cmd_tx.send(AgentCommand::DebugInfo);
            }
            "bug" => {
                let editor = Editor::new();
                self.dialog = Some(SessionDialog::BugHint { editor });
                cx.notify();
            }
            "reload" => {
                _ = self.cmd_tx.send(AgentCommand::Reload {
                    context_files: Vec::new(),
                    skills: Vec::new(),
                    system_prompt: None,
                    append_system_prompt: None,
                });
            }
            "label" => {
                let editor = Editor::new();
                self.dialog = Some(SessionDialog::Label { editor });
                cx.notify();
            }
            "import" => self.open_import_dialog(cx),
            "history" => self.open_history_panel(cx),
            "clone" => {
                _ = self.cmd_tx.send(AgentCommand::ForkClone);
            }
            "fork" => {
                if let Some(path) = self
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.session_path.clone())
                {
                    _ = self.cmd_tx.send(AgentCommand::ForkFrom { path, dir: None });
                } else {
                    self.transcript
                        .push_note(Note::Info("In-memory session cannot be forked".into()));
                }
            }
            "share" => {
                _ = self.cmd_tx.send(AgentCommand::Share { theme: None });
            }
            "export" => {
                _ = self.cmd_tx.send(AgentCommand::Export {
                    out: None,
                    theme: None,
                });
            }
            "trust" => {
                let cwd = self.cwd();
                crate::core::project_trust::set_project_trust(
                    std::path::Path::new(&cwd),
                    &settings_manager::agent_dir(),
                    true,
                );
                _ = self.cmd_tx.send(AgentCommand::RebuildTools);
                self.toast(format!("Project trusted: {cwd}"), ToastKind::Success, cx);
            }
            "name" => self.rename_current_session(cx),
            "copy" => {
                let text = self.transcript.last_assistant_text();
                if text.is_empty() {
                    self.toast("No reply to copy", ToastKind::Info, cx);
                } else {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                    self.toast("Copied the last reply", ToastKind::Success, cx);
                }
            }
            "compact" => {
                _ = self
                    .cmd_tx
                    .send(AgentCommand::Compact { instructions: None });
            }
            "rewind" => {
                _ = self.cmd_tx.send(AgentCommand::Rewind);
            }
            "appearance" => self.open_appearance_picker(cx),
            "panel" => self.toggle_panel(None, cx),
            "usage" => self.open_info(InfoKind::Usage, cx),
            "skills" => self.open_skills_panel(cx),
            "extension" => self.open_extension_panel(cx),
            "scoped-models" => self.open_scoped_models_panel(cx),
            "settings" => self.open_settings_panel(cx),
            "session" => self.open_info(InfoKind::Session, cx),
            "help" => self.open_info(InfoKind::Help, cx),
            "quit" => cx.quit(),
            // plan-mode 是内置扩展里唯一声明斜杠命令的：开关本身是 core 级 API，GUI 直接可调
            "plan" => {
                let enabled = crate::extensions::plan_mode::toggle_enabled();
                _ = self.cmd_tx.send(AgentCommand::RebuildTools);
                self.toast(
                    format!("Plan mode: {}", if enabled { "on" } else { "off" }),
                    ToastKind::Info,
                    cx,
                );
            }
            "todos" => {
                let items = crate::extensions::plan_mode::extract_todo_items("");
                self.transcript.push_note(Note::Info(if items.is_empty() {
                    "No todos right now".to_string()
                } else {
                    format!("{} todos (dock panel in the terminal UI)", items.len())
                }));
            }
            other
                if crate::core::extensions::registered_commands()
                    .iter()
                    .any(|command| command.name == other) =>
            {
                self.transcript.push_note(Note::Info(format!(
                    "Extension command /{other} is not wired into the desktop UI yet; use the terminal UI"
                )));
            }
            other if other.starts_with("skill:") => {
                // 技能命令：把 `/skill:名称` 交给 worker 展开成提示词并发送
                _ = self.cmd_tx.send(AgentCommand::Prompt(format!("/{other}")));
            }
            other => {
                // 不认识的命令：原样还给输入框，别悄悄吞掉
                self.editor.insert(&format!("/{other}"));
            }
        }
    }

    /// IME（中文/日文组字）只在绘制阶段能挂到窗口：这里放一层透明画布，
    /// 命中区域就是当前输入框，候选窗会贴着它弹。
    fn ime_layer(&self, cx: &mut Context<Self>) -> AnyElement {
        let focus = self.focus.clone();
        let view = cx.entity();
        canvas(
            |_, _, _| (),
            move |bounds, _, window, cx| {
                window.handle_input(&focus, ElementInputHandler::new(bounds, view.clone()), cx);
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .into_any_element()
    }

    /// 悬浮探测：按滚动区实测 bounds 与鼠标位置判定，指到谁就显谁的滚动条。
    ///
    /// 不用元素 `on_hover`：滚动容器的 hitbox 判定偏宽（鼠标在整列甚至整屏范围内都算悬停），
    /// 滚动条会常显。这里在绘制阶段挂一个全局鼠标监听，用 `ScrollHandle::bounds()` 比位置。
    fn hover_probe(&self, transcript_live: bool, cx: &mut Context<Self>) -> AnyElement {
        let transcript = self.scroll.clone();
        let sidebar = self.sessions_scroll.clone();
        let app = cx.entity();
        canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                let (transcript, sidebar) = (transcript.clone(), sidebar.clone());
                let move_app = app.clone();
                window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, window, cx| {
                    if phase != gpui::DispatchPhase::Bubble {
                        return;
                    }
                    let position = event.position;
                    let inside = |handle: &ScrollHandle| {
                        let bounds = handle.bounds();
                        bounds.size.width > px(0.) && bounds.contains(&position)
                    };
                    let in_transcript = transcript_live && inside(&transcript);
                    let in_sidebar = inside(&sidebar);
                    move_app.update(cx, |this, cx| {
                        this.drag_scrollbar(position, cx);
                        this.drag_sidebar_width(position, event.pressed_button, cx);
                        this.drag_panel_width(position, event.pressed_button, cx);
                        this.set_hover(in_transcript, in_sidebar, event.pressed_button, cx);
                        // 按住左键在对话流里拖动 = 扩选（选区在按下时建立）
                        if in_transcript
                            && event.pressed_button == Some(MouseButton::Left)
                            && this.text_sel.is_some()
                        {
                            this.extend_text_selection(position, window, cx);
                        }
                    });
                });

                // 按下：在对话流的文本行上建立选区；点空白处清掉
                let down_app = app.clone();
                window.on_mouse_event(move |event: &gpui::MouseDownEvent, phase, window, cx| {
                    if phase != gpui::DispatchPhase::Bubble || event.button != MouseButton::Left {
                        return;
                    }
                    let extend = event.modifiers.shift;
                    let position = event.position;
                    down_app.update(cx, |this, cx| {
                        this.start_text_selection(position, extend, window, cx);
                    });
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .into_any_element()
    }

    /// 拖右侧面板左边框：按住左键时按位移改宽度，松手即结束。
    fn drag_panel_width(
        &mut self,
        position: gpui::Point<Pixels>,
        pressed: Option<MouseButton>,
        cx: &mut Context<Self>,
    ) {
        let Some(mut drag) = self.panel_resize else {
            return;
        };
        if pressed != Some(MouseButton::Left) {
            self.panel_resize = None;
            cx.notify();
            return;
        }
        // 锚点与拖动位置必须同源，否则宽度变化和鼠标位移对不上
        let Some(anchor_x) = drag.anchor_x else {
            drag.anchor_x = Some(position.x);
            self.panel_resize = Some(drag);
            return;
        };
        let width = resized_panel_width(drag.width, f32::from(position.x - anchor_x));
        if width != self.panel_width {
            self.panel_width = width;
            cx.notify();
        }
    }

    /// 应用滚动条拖拽：鼠标在哪都按「按下点 + 位移」算偏移，跑出滚动条也不中断。
    fn drag_scrollbar(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(mut drag) = self.scroll_drag else {
            return;
        };
        // 锚点取全局鼠标事件里的位置：和按下时的坐标系可能不一致（缩放/元素局部），
        // 混用会让滑块比鼠标跑得快
        let Some(anchor_y) = drag.anchor_y else {
            drag.anchor_y = Some(position.y);
            self.scroll_drag = Some(drag);
            return;
        };
        let area = drag.area;
        let scroll = self.scroll_of(area);
        let viewport = f32::from(scroll.bounds().size.height);
        let max = f32::from(scroll.max_offset().height);
        if max <= 0. || viewport <= 0. {
            return;
        }
        // 滑块要跟着鼠标走：轨道上的位移按「内容高 / 轨道高」放大成偏移
        let travel = thumb_travel(viewport, max);
        let dy = f32::from(position.y - anchor_y);
        let target = drag_offset(f32::from(drag.offset), dy, max, travel);
        self.scroll_of_mut(area)
            .set_offset(gpui::point(px(0.), gpui::px(target)));
        cx.notify();
    }

    /// 拖侧栏右边框：按住左键时按位移改宽度，松手即结束。
    fn drag_sidebar_width(
        &mut self,
        position: gpui::Point<Pixels>,
        pressed: Option<MouseButton>,
        cx: &mut Context<Self>,
    ) {
        let Some(mut drag) = self.resize_drag else {
            return;
        };
        if pressed != Some(MouseButton::Left) {
            self.resize_drag = None;
            cx.notify();
            return;
        }
        // 锚点与拖动位置必须同源，否则宽度变化和鼠标位移对不上
        let Some(anchor_x) = drag.anchor_x else {
            drag.anchor_x = Some(position.x);
            self.resize_drag = Some(drag);
            return;
        };
        let width = resized_width(drag.width, f32::from(position.x - anchor_x));
        if width != self.sidebar_width {
            self.sidebar_width = width;
            cx.notify();
        }
    }

    /// 记录悬浮状态：有变化才重绘，顺手收掉鼠标已离开那个区的拖拽状态。
    fn set_hover(
        &mut self,
        transcript: bool,
        sidebar: bool,
        pressed: Option<MouseButton>,
        cx: &mut Context<Self>,
    ) {
        // 鼠标按着拖滚动条时即使离开该区域也继续拖，只有松手才清拖拽状态
        if pressed != Some(MouseButton::Left) {
            self.scroll_drag = self.scroll_drag.filter(|drag| match drag.area {
                ScrollArea::Transcript => transcript,
                ScrollArea::Sidebar => sidebar,
            });
        }
        if self.transcript_hovered == transcript && self.sidebar_hovered == sidebar {
            return;
        }
        self.transcript_hovered = transcript;
        self.sidebar_hovered = sidebar;
        cx.notify();
    }

    /// 打开会话行右键菜单。
    fn open_session_menu(
        &mut self,
        path: &str,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.session_menu = Some(SessionMenu {
            path: path.to_string(),
            position,
        });
        cx.notify();
    }

    /// 关掉右键菜单。
    fn close_session_menu(&mut self, cx: &mut Context<Self>) {
        self.session_menu = None;
        cx.notify();
    }

    /// 菜单「重命名」：弹对话框，输入框预填当前标题。
    fn start_rename(&mut self, path: &str, cx: &mut Context<Self>) {
        let title = self
            .sessions
            .iter()
            .find(|row| row.path == path)
            .map(|row| row.title.clone())
            .unwrap_or_default();
        let mut editor = Editor::new();
        editor.insert(&title);
        editor.select_all();
        self.session_menu = None;
        self.dialog = Some(SessionDialog::Rename {
            path: path.to_string(),
            editor,
        });
        cx.notify();
    }

    /// 菜单「删除」：先弹二次确认。
    fn ask_delete(&mut self, path: &str, cx: &mut Context<Self>) {
        let title = self
            .sessions
            .iter()
            .find(|row| row.path == path)
            .map(|row| row.title.clone())
            .unwrap_or_else(|| basename(path));
        self.session_menu = None;
        self.dialog = Some(SessionDialog::Delete {
            path: path.to_string(),
            title,
        });
        cx.notify();
    }

    /// `/import`：弹路径输入框，确认后作为会话载入。
    fn open_import_dialog(&mut self, cx: &mut Context<Self>) {
        let editor = Editor::new();
        self.dialog = Some(SessionDialog::Import { editor });
        cx.notify();
    }

    /// `/name`：给当前会话改名（预填现有会话名）。
    fn rename_current_session(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.session_path.clone())
        else {
            self.transcript
                .push_note(Note::Info("In-memory session cannot be renamed".into()));
            cx.notify();
            return;
        };
        let title = self
            .sessions
            .iter()
            .find(|row| row.path == path)
            .map(|row| row.title.clone())
            .unwrap_or_default();
        let mut editor = Editor::new();
        editor.insert(&title);
        editor.select_all();
        self.dialog = Some(SessionDialog::Rename { path, editor });
        cx.notify();
    }

    /// 底部工作目录：弹输入框，确认后切目录（会顺带在新目录下开会话）。
    /// 目录菜单：最近用过的目录 + 「Browse…」。
    fn open_cwd_menu(&mut self, _cx: &mut Context<Self>) {
        let mut entries: Vec<Entry> = recent_cwds()
            .into_iter()
            .map(|path| Entry {
                label: path.clone(),
                detail: String::new(),
                action: Action::SwitchCwdTo(path),
                ..Entry::default()
            })
            .collect();
        if entries.is_empty() {
            entries.push(Entry {
                label: "No recent directories".into(),
                ..Entry::default()
            });
        }
        entries.push(Entry {
            label: "Browse…".into(),
            detail: "Open the system picker".into(),
            action: Action::BrowseCwd,
            ..Entry::default()
        });
        self.overlay = Some(Overlay::List(List::new("Working directory", entries)));
    }

    fn pick_working_directory(&mut self, cx: &mut Context<Self>) {
        // 原生对话框是阻塞调用：丢到后台线程，选完再把结果送回视图
        let start = self.cwd();
        cx.spawn(async move |this, cx| {
            let picked = cx
                .background_executor()
                .spawn(async move {
                    native_dialog::FileDialogBuilder::default()
                        .set_title("Select working directory")
                        .set_location(&start)
                        .open_single_dir()
                        .show()
                        .ok()
                        .flatten()
                })
                .await;
            if let Some(path) = picked {
                _ = this.update(cx, |app, cx| {
                    _ = app.cmd_tx.send(AgentCommand::SwitchCwd {
                        path: path.to_string_lossy().to_string(),
                    });
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 对话框确认：重命名落盘 / 删除文件 / 切工作目录。
    fn confirm_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        match dialog {
            SessionDialog::Rename { path, editor } => {
                if let Some(name) = renamed_name(&editor.text()) {
                    rename_session(&path, name);
                }
            }
            SessionDialog::BugHint { editor } => {
                _ = self.cmd_tx.send(AgentCommand::BugReport {
                    hint: editor.text(),
                    include_session: true,
                    include_summary: false,
                });
                self.cx_notify(cx);
                return;
            }
            SessionDialog::Label { editor } => {
                let text = editor.text().trim().to_string();
                match self
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.leaf_entry_id.clone())
                {
                    Some(entry_id) => {
                        _ = self.cmd_tx.send(AgentCommand::Label {
                            entry_id,
                            label: (!text.is_empty()).then_some(text),
                        });
                    }
                    None => self
                        .transcript
                        .push_note(Note::Info("This branch has no entry to label".into())),
                }
                self.cx_notify(cx);
                return;
            }
            SessionDialog::Import { editor } => {
                let raw = editor.text().trim().to_string();
                let path = crate::utils::paths::resolve_path(&raw, &self.cwd());
                if !path.is_file() {
                    self.transcript.push_note(Note::Error(format!(
                        "Import failed: no such file {}",
                        path.display()
                    )));
                } else {
                    _ = self.cmd_tx.send(AgentCommand::ResumeSession {
                        path: path.to_string_lossy().to_string(),
                    });
                }
                self.cx_notify(cx);
                return;
            }
            SessionDialog::ApiKey { provider, editor } => {
                let key = editor.text().trim().to_string();
                if !key.is_empty() {
                    match crate::core::auth::write_auth_key(&provider, &key) {
                        Ok(()) => {
                            _ = self.cmd_tx.send(AgentCommand::ApplyKey {
                                provider: provider.clone(),
                                key,
                            });
                            self.transcript
                                .push_note(Note::Info(format!("Saved API key for {provider}")));
                        }
                        Err(error) => self
                            .transcript
                            .push_note(Note::Error(format!("Save failed: {error}"))),
                    }
                }
                self.cx_notify(cx);
                return;
            }
            SessionDialog::Number { key, editor } => {
                let (key, text) = (key, editor.text());
                self.confirm_number_setting(key, &text, cx);
                return;
            }
            SessionDialog::Delete { path, .. } => {
                let current = self
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.session_path.clone())
                    .is_some_and(|current| current == path);
                _ = std::fs::remove_file(&path);
                if current {
                    _ = self.cmd_tx.send(AgentCommand::NewSession);
                    self.transcript.clear();
                    self.flow_height.set(None);
                }
            }
        }
        self.reload_sessions(cx);
        cx.notify();
    }

    /// 取某个滚动区的滚动句柄。
    fn scroll_of(&self, area: ScrollArea) -> &ScrollHandle {
        match area {
            ScrollArea::Transcript => &self.scroll,
            ScrollArea::Sidebar => &self.sessions_scroll,
        }
    }

    /// [`Self::scroll_of`] 的可变版。
    fn scroll_of_mut(&mut self, area: ScrollArea) -> &mut ScrollHandle {
        match area {
            ScrollArea::Transcript => &mut self.scroll,
            ScrollArea::Sidebar => &mut self.sessions_scroll,
        }
    }

    /// 滚动条是否该出现：鼠标悬停在该区上，或正在拖它。
    fn scrollbar_visible(&self, area: ScrollArea) -> bool {
        let hovered = match area {
            ScrollArea::Transcript => self.transcript_hovered,
            ScrollArea::Sidebar => self.sidebar_hovered,
        };
        scrollbar_visible(self.scroll_drag.map(|drag| drag.area), area, hovered)
    }

    /// 滚动条：滑块位置按上一帧的滚动状态算，拖动时把鼠标位移换算成内容位移。
    fn scrollbar(&self, area: ScrollArea, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        if !self.scrollbar_visible(area) {
            return div().into_any_element();
        }
        let scroll = self.scroll_of(area);
        let viewport = f32::from(scroll.bounds().size.height);
        let max = f32::from(scroll.max_offset().height);
        if max <= 0. || viewport <= 0. {
            return div().into_any_element();
        }
        let height = thumb_height(viewport, max);
        let top = thumb_top(
            f32::from(scroll.offset().y),
            max,
            thumb_travel(viewport, max),
        );
        div()
            .id(("scrollbar", area.index()))
            .absolute()
            .top_0()
            .right_0()
            .h_full()
            .w(px(SCROLLBAR_HIT_WIDTH))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    let offset = this.scroll_of(area).offset().y;
                    let _ = event;
                    this.scroll_drag = Some(ScrollDrag {
                        area,
                        anchor_y: None,
                        offset,
                    });
                    cx.notify();
                }),
            )
            .on_mouse_move(
                cx.listener(move |this, event: &gpui::MouseMoveEvent, _, cx| {
                    if this.scroll_drag.is_some_and(|drag| drag.area == area) {
                        this.drag_scrollbar(event.position, cx);
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.scroll_drag = None;
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.scroll_drag = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .absolute()
                    // 尺寸来自上一帧实测的像素值，不能再过一次界面缩放
                    .top(gpui::px(top))
                    .right(px(1.))
                    .w(px(SCROLLBAR_WIDTH))
                    .h(gpui::px(height))
                    .rounded(px(4.))
                    .bg(theme.text_ghost),
            )
            .into_any_element()
    }

    /// 重读会话列表：目录列举留在主线程（只看 mtime），
    /// 逐条整文件解析交给后台线程，这里起一个轮询任务把就绪的条目填回来。
    fn reload_sessions(&mut self, cx: &mut Context<Self>) {
        let cwd = self.cwd();
        if let Some(mut loader) = self.sessions_loader.take() {
            loader.cancel();
        }
        self.sessions.clear();
        self.sessions_loading = true;
        self.sessions_loader = Some(ProgressiveMetas::start(sidebar::paths(&cwd)));

        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(120))
                    .await;
                let more = this.update(cx, |app, cx| {
                    if app.sessions_loader.as_mut().map(|l| l.drain()).unwrap_or(0) > 0 {
                        app.refresh_session_rows();
                        cx.notify();
                    }
                    app.sessions_loader.as_ref().is_some_and(|l| l.is_loading())
                });
                if !more.unwrap_or(false) {
                    _ = this.update(cx, |app, cx| {
                        app.sessions_loading = false;
                        app.refresh_session_rows();
                        cx.notify();
                    });
                    break;
                }
            }
        });
        self._session_poll = Some(poll);
    }

    /// 把加载器里已就绪的元数据投影成侧栏行。
    fn refresh_session_rows(&mut self) {
        self.sessions = self
            .sessions_loader
            .as_ref()
            .map(sidebar::rows)
            .unwrap_or_default();
    }

    /// 键盘入口：分发完统一重绘一次。
    ///
    /// 打开覆盖层 / 切面板的分支原本各自 `notify`，漏一处就表现为「按了没反应、
    /// 要再敲一个字符才出来」，所以重绘收在这一层。
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.dispatch_key(event, cx);
        self.refresh_suggest(cx);
        cx.notify();
    }

    /// 键盘分发：覆盖层打开时只喂覆盖层，否则走输入框。
    fn dispatch_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = event.keystroke.clone();
        let primary = keystroke.modifiers.control || keystroke.modifiers.platform;
        let shift = keystroke.modifiers.shift;

        if primary && keystroke.key == "q" {
            cx.quit();
            return;
        }

        if self.dialog.is_some() {
            if keystroke.key == "enter" {
                self.confirm_dialog(cx);
            } else {
                self.handle_dialog_key(&keystroke.key, shift);
            }
            cx.notify();
            return;
        }

        if self.session_menu.is_some() {
            // 菜单只认 Esc，其余键先收菜单再照常处理
            self.session_menu = None;
            if keystroke.key == "escape" {
                cx.notify();
                return;
            }
        }

        // Shift+Tab：循环 thinking 级别（同 TUI 的 `app.thinking.cycle`）；
        // 放在补全面板之前，面板开着时也能切
        if shift && keystroke.key == "tab" {
            self.cycle_thinking(cx);
            cx.notify();
            return;
        }

        if let Some(suggest) = self.suggest.clone() {
            match keystroke.key.as_str() {
                "escape" => {
                    self.suggest_dismissed = Some(suggest.start);
                    self.suggest = None;
                    cx.notify();
                    return;
                }
                "up" => {
                    self.move_suggestion(-1);
                    cx.notify();
                    return;
                }
                "down" => {
                    self.move_suggestion(1);
                    cx.notify();
                    return;
                }
                "tab" if !suggest.items.is_empty() => {
                    self.apply_suggestion(cx);
                    cx.notify();
                    return;
                }
                // 有候选时 Enter 先采纳；shell 面板没有候选，Enter 直接执行
                "enter" if suggest.trigger != Trigger::Shell && !suggest.items.is_empty() => {
                    self.apply_suggestion(cx);
                    cx.notify();
                    return;
                }
                // 退格/方向键改的是面板输入框，不动主输入框
                "backspace" if suggest.trigger != Trigger::Shell => {
                    if self.suggest_input.text().is_empty() {
                        // 面板输入已空：收起面板，触发符留在输入框里由用户自己删
                        self.suggest_dismissed = Some(suggest.start);
                        self.suggest = None;
                    } else {
                        self.suggest_input.backspace();
                        self.refresh_suggest_from_input(cx);
                    }
                    cx.notify();
                    return;
                }
                "delete" if suggest.trigger != Trigger::Shell => {
                    self.suggest_input.delete_forward();
                    self.refresh_suggest_from_input(cx);
                    cx.notify();
                    return;
                }
                "left" | "right" | "home" | "end" if suggest.trigger != Trigger::Shell => {
                    match keystroke.key.as_str() {
                        "left" => self.suggest_input.move_cursor(-1, 0, shift),
                        "right" => self.suggest_input.move_cursor(1, 0, shift),
                        "home" => self.suggest_input.home(shift),
                        _ => self.suggest_input.end(shift),
                    }
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }

        // Ctrl-S 先于覆盖层分发：技能/扩展面板是覆盖层，否则按键被它吃掉
        if primary && !shift && keystroke.key == "s" && self.save_panel_drafts(cx) {
            cx.notify();
            return;
        }

        if self.overlay.is_some() {
            self.handle_overlay_key(&keystroke.key, shift, cx);
            cx.notify();
            return;
        }

        // ── 编辑器键位：与 TUI 的 tui.editor.* 对齐 ─────────────
        let alt = keystroke.modifiers.alt;
        let key = keystroke.key.as_str();
        if (alt && (key == "left" || key == "b")) || (primary && key == "left") {
            self.editor.word_left(shift);
            cx.notify();
            return;
        }
        if (alt && (key == "right" || key == "f")) || (primary && key == "right") {
            self.editor.word_right(shift);
            cx.notify();
            return;
        }
        if primary && !shift && key == "b" {
            self.editor.move_cursor(-1, 0, false);
            cx.notify();
            return;
        }
        if primary && !shift && key == "a" {
            self.editor.home(false);
            cx.notify();
            return;
        }
        if primary && !shift && key == "e" {
            self.editor.end(false);
            cx.notify();
            return;
        }
        if primary && !shift && key == "u" {
            self.editor.delete_to_line_start();
            cx.notify();
            return;
        }
        if primary && shift && key == "k" {
            self.editor.delete_to_line_end();
            cx.notify();
            return;
        }
        if (primary && (key == "w" || key == "backspace")) || (alt && key == "backspace") {
            self.editor.delete_word_backward();
            cx.notify();
            return;
        }
        if alt && key == "d" {
            self.editor.delete_word_forward();
            cx.notify();
            return;
        }
        if primary && key == "j" {
            self.editor.insert("\n");
            cx.notify();
            return;
        }
        if primary && shift && key == "c" {
            // 对话流里有选区就先复制它，否则复制输入框里的选中文字
            let picked = self
                .text_sel
                .map(|selection| transcript::selected_text(selection, &self.line_rows.borrow()))
                .filter(|text| !text.is_empty());
            let text = picked.unwrap_or_else(|| self.editor.selected_text());
            if !text.is_empty() {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                self.toast("Copied selection", ToastKind::Info, cx);
            }
            return;
        }
        if primary && !shift && key == "c" {
            // 对话流里有选区时 Ctrl-C 复制它，否则沿用「清空输入框」
            if let Some(text) = self
                .text_sel
                .map(|selection| transcript::selected_text(selection, &self.line_rows.borrow()))
                .filter(|text| !text.is_empty())
            {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                self.toast("Copied selection", ToastKind::Info, cx);
                return;
            }
            self.editor.clear();
            cx.notify();
            return;
        }
        if primary && !shift && key == "d" && self.editor.is_empty() {
            cx.quit();
            return;
        }

        // 面板快捷键
        if primary {
            match keystroke.key.as_str() {
                "k" => {
                    self.open_command_palette(cx);
                    return;
                }
                // Ctrl+P / Ctrl+Shift+P 直接切上一个/下一个模型（同 TUI 的模型循环）
                "p" => {
                    self.cycle_model(if shift { -1 } else { 1 }, cx);
                    return;
                }
                "/" => {
                    self.open_model_picker(cx);
                    return;
                }
                "t" => {
                    self.open_thinking_picker(cx);
                    return;
                }
                "o" => {
                    self.open_session_picker(cx);
                    return;
                }
                "g" => {
                    self.open_appearance_picker(cx);
                    return;
                }
                "u" if shift => {
                    self.open_info(InfoKind::Usage, cx);
                    return;
                }
                "i" => {
                    self.open_skills_panel(cx);
                    return;
                }
                "," => {
                    self.open_settings_panel(cx);
                    return;
                }
                "b" if shift => {
                    self.show_sidebar = !self.show_sidebar;
                    cx.notify();
                    return;
                }
                "e" if shift => {
                    self.toggle_panel(None, cx);
                    cx.notify();
                    return;
                }
                "l" => {
                    self.expand_tools = !self.expand_tools;
                    cx.notify();
                    return;
                }
                "n" => {
                    _ = self.cmd_tx.send(AgentCommand::NewSession);
                    self.transcript.clear();
                    self.flow_height.set(None);
                    self.reload_sessions(cx);
                    cx.notify();
                    return;
                }
                "=" | "+" | "-" | "0" => {
                    let current = theme::ui_scale();
                    let scale = match keystroke.key.as_str() {
                        "0" => 1.0,
                        "-" => current - theme::UI_SCALE_STEP,
                        _ => current + theme::UI_SCALE_STEP,
                    };
                    _ = settings_manager::write_settings_ui_scale(Some(theme::set_ui_scale(scale)));
                    cx.notify();
                    return;
                }
                "s" if !shift => {
                    self.save_panel_drafts(cx);
                    return;
                }
                "a" if shift => {
                    self.editor.select_all();
                    cx.notify();
                    return;
                }
                "z" => {
                    _ = if shift {
                        self.editor.redo()
                    } else {
                        self.editor.undo()
                    };
                    cx.notify();
                    return;
                }
                "y" => {
                    self.editor.redo();
                    cx.notify();
                    return;
                }
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        self.editor.insert(&text);
                    }
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }

        match keystroke.key.as_str() {
            "enter" if shift => {
                self.editor.insert("\n");
                cx.notify();
            }
            "enter" => {
                self.submit(cx);
                cx.notify();
            }
            "escape" => {
                if self.busy {
                    // worker 直接 drop 回合 future，agent 不会再发 `agent_settled`，
                    // 忙状态只能在本地复位，中断提示也只能自己补（同 TUI 的硬中止）
                    _ = self.cmd_tx.send(AgentCommand::Abort);
                    self.queued = 0;
                    self.busy = false;
                    self.transcript.abort();
                    self.scroll_to_bottom();
                } else if self.panel.visible {
                    // 空闲时 Esc 先收右侧面板（giff 等）——之前面板没有关闭途径
                    self.panel.visible = false;
                }
                cx.notify();
            }
            "backspace" => {
                self.editor.backspace();
                cx.notify();
            }
            "delete" => {
                self.editor.delete_forward();
                cx.notify();
            }
            "left" => {
                self.editor.move_cursor(-1, 0, shift);
                cx.notify();
            }
            "right" => {
                self.editor.move_cursor(1, 0, shift);
                cx.notify();
            }
            "up" => {
                self.editor.move_cursor(0, -1, shift);
                cx.notify();
            }
            "down" => {
                self.editor.move_cursor(0, 1, shift);
                cx.notify();
            }
            "home" => {
                self.editor.home(shift);
                cx.notify();
            }
            "end" => {
                self.editor.end(shift);
                cx.notify();
            }
            "tab" => {
                self.editor.insert("  ");
                cx.notify();
            }
            // 可打印字符由平台经 IME 句柄（`replace_text_in_range`）送进来，
            // 这里再插一次就会变成「敲一个 a 出现 aa」。
            _ => {}
        }
    }

    /// 点击覆盖层列表的第 `visible` 行：先把该行设为选中（后续重建不跳回第一行），再执行。
    pub(super) fn pick_entry(&mut self, visible: usize, cx: &mut Context<Self>) {
        let action = match &mut self.overlay {
            Some(Overlay::List(list)) => {
                list.selected = visible;
                let filtered = list.filtered();
                filtered
                    .get(visible)
                    .map(|index| clone_action(&list.entries[*index].action))
            }
            _ => None,
        };
        if let Some(action) = action {
            // 勾选类动作会重建同一个面板，保留它才能沿用过滤与选中行
            if !keeps_overlay(&action) {
                self.overlay = None;
            }
            self.run_action(action, cx);
        }
        cx.notify();
    }

    /// 对话框按键：重命名时打字进输入框，Enter 确认、Esc 取消。
    fn handle_dialog_key(&mut self, key: &str, shift: bool) {
        if key == "escape" {
            self.dialog = None;
            return;
        }
        let Some(dialog) = self.dialog.as_mut() else {
            return;
        };
        // 只有带输入框的对话框（重命名 / 切目录）吃按键
        let editor = match dialog {
            SessionDialog::Rename { editor, .. }
            | SessionDialog::ApiKey { editor, .. }
            | SessionDialog::Import { editor }
            | SessionDialog::Label { editor }
            | SessionDialog::BugHint { editor }
            | SessionDialog::Number { editor, .. } => editor,
            SessionDialog::Delete { .. } => return,
        };
        match key {
            "backspace" => editor.backspace(),
            "delete" => editor.delete_forward(),
            "left" | "right" | "home" | "end" => match key {
                "left" => editor.move_cursor(-1, 0, shift),
                "right" => editor.move_cursor(1, 0, shift),
                "home" => editor.home(shift),
                _ => editor.end(shift),
            },
            _ => {}
        }
    }

    /// 覆盖层按键。
    fn handle_overlay_key(&mut self, key: &str, _shift: bool, cx: &mut Context<Self>) {
        if key == "escape" {
            // 详情页先退回列表（草稿还在），再按一次才关面板丢草稿
            if let Some(Overlay::List(list)) = self.overlay.as_mut()
                && list.detail.take().is_some()
            {
                return;
            }
            self.overlay = None;
            // 扩展/技能面板的改动是草稿：Esc 直接丢弃
            self.skills_draft = None;
            self.extensions_draft = None;
            return;
        }
        // 详情页只吃 Tab（回列表）与滚动键
        if let Some(Overlay::List(list)) = self.overlay.as_mut()
            && list.detail.is_some()
        {
            if key == "tab" || key == "enter" {
                list.detail = None;
            } else if let Some(detail) = list.detail.as_mut() {
                match key {
                    "up" => detail.scroll_by(-1, 18),
                    "down" => detail.scroll_by(1, 18),
                    "pageup" => detail.scroll_by(-12, 18),
                    "pagedown" => detail.scroll_by(12, 18),
                    _ => {}
                }
            }
            cx.notify();
            return;
        }
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        match overlay {
            Overlay::List(list) => match key {
                "enter" => {
                    let action = list.take_action().map(clone_action);
                    if let Some(action) = action {
                        // 同上：勾选类动作要保留面板
                        if !keeps_overlay(&action) {
                            self.overlay = None;
                        }
                        self.run_action(action, cx);
                    }
                }
                "tab" => self.open_list_detail(),
                "up" => list.move_selection(-1),
                "down" => list.move_selection(1),
                "backspace" => {
                    list.query.backspace();
                    list.selected = 0;
                }
                // 可打印字符由 IME 句柄写进过滤框（见 `ime_target`），
                // 这里再插一次就会「敲一个 a 出现 aa」。
                _ => {}
            },
            Overlay::Info(info) => match key {
                "up" => info.scroll_by(-1, 18),
                "down" => info.scroll_by(1, 18),
                "pageup" => info.scroll_by(-12, 18),
                "pagedown" => info.scroll_by(12, 18),
                _ => {}
            },
        }
    }

    /// 执行覆盖层动作。
    fn run_action(&mut self, action: Action, cx: &mut Context<Self>) {
        match action {
            Action::None => {}
            Action::Command(command) => {
                _ = self.cmd_tx.send(*command);
            }
            Action::LoadSession(path) => {
                _ = self.cmd_tx.send(AgentCommand::ResumeSession { path });
            }
            Action::SetDark(is_dark) => {
                self.theme = Theme::new(is_dark);
            }
            Action::NewSession => {
                _ = self.cmd_tx.send(AgentCommand::NewSession);
                self.transcript.clear();
                self.flow_height.set(None);
            }
            Action::Info(kind) => self.open_info(kind, cx),
            Action::Open(kind) => match kind {
                ListKind::Model => self.open_model_picker(cx),
                ListKind::Session => self.open_session_picker(cx),
                ListKind::Thinking => self.open_thinking_picker(cx),
                ListKind::Appearance => self.open_appearance_picker(cx),
                ListKind::PanelChanges => self.toggle_panel(Some(PanelTab::Changes), cx),
                ListKind::PanelFiles => self.toggle_panel(Some(PanelTab::Files), cx),
                ListKind::History => self.open_history_panel(cx),
            },
            Action::RefreshSessions => self.reload_sessions(cx),
            Action::Scale(scale) => {
                _ = settings_manager::write_settings_ui_scale(Some(theme::set_ui_scale(scale)));
            }
            Action::Quit => cx.quit(),
            Action::ConfigCommand(name) => self.run_slash(&name, cx),
            Action::SwitchCwdTo(path) => {
                _ = self.cmd_tx.send(AgentCommand::SwitchCwd { path });
            }
            Action::BrowseCwd => self.pick_working_directory(cx),
            Action::Settings => self.open_settings_panel(cx),
            Action::ToggleSkill { name, enable } => {
                let now = std::time::Instant::now();
                if ignore_repeat_toggle(
                    self.last_toggle
                        .as_ref()
                        .map(|(last, at)| (last.as_str(), *at)),
                    &name,
                    now,
                    TOGGLE_DEBOUNCE,
                ) {
                    // 长按鼠标：切换后面板重建，鼠标抬起又命中同一行，忽略这一次
                    cx.notify();
                    return;
                }
                self.last_toggle = Some((name.clone(), now));
                // 草稿里把该技能切到目标态：开 → 去掉 -name、加 +name（压过 !glob），
                // 关 → 去掉 +name、加 -name。Ctrl-S 才落盘。
                let rules = self.skills_draft.clone().unwrap_or_default();
                self.skills_draft = Some(skill_rules_with(&rules, &name, enable));
                self.rebuild_skills_panel(cx);
            }
            Action::InsertText(text) => {
                self.editor.insert(&text);
                self.refresh_suggest(cx);
                cx.notify();
            }
            Action::ToggleSidebar => {
                self.show_sidebar = !self.show_sidebar;
                cx.notify();
            }
            Action::Login => self.open_login_panel(cx),
            Action::Logout => self.open_logout_panel(cx),
            Action::Changelog => self.open_changelog(cx),
            Action::LoginProvider(provider) => {
                let editor = Editor::new();
                self.dialog = Some(SessionDialog::ApiKey { provider, editor });
                cx.notify();
            }
            Action::LogoutProvider(provider) => {
                match crate::core::auth::remove_auth(&provider) {
                    Ok(()) => self
                        .transcript
                        .push_note(Note::Info(format!("Removed API key for {provider}"))),
                    Err(error) => self
                        .transcript
                        .push_note(Note::Error(format!("Clear failed: {error}"))),
                }
                self.open_logout_panel(cx);
            }
            Action::Extensions => self.open_extension_panel(cx),
            Action::Skills => self.open_skills_panel(cx),
            Action::ScopedModels => self.open_scoped_models_panel(cx),
            Action::ToggleExtension(name) => {
                let now = std::time::Instant::now();
                if ignore_repeat_toggle(
                    self.last_toggle
                        .as_ref()
                        .map(|(last, at)| (last.as_str(), *at)),
                    &name,
                    now,
                    TOGGLE_DEBOUNCE,
                ) {
                    cx.notify();
                    return;
                }
                self.last_toggle = Some((name.clone(), now));
                // 与 TUI 面板一致：只改未保存的草稿，Ctrl-S 才应用并写盘
                if let Some(draft) = self.extensions_draft.as_mut()
                    && let Some(item) = draft.iter_mut().find(|(item, _)| item == &name)
                {
                    item.1 = !item.1;
                }
                self.rebuild_extension_panel(cx);
            }
            Action::ToggleScopedModel(pattern) => {
                let mut patterns = settings_manager::read_enabled_models();
                match patterns.iter().position(|item| item == &pattern) {
                    Some(index) => {
                        patterns.remove(index);
                    }
                    None => patterns.push(pattern),
                }
                if let Err(error) = settings_manager::write_enabled_models(Some(&patterns)) {
                    self.transcript
                        .push_note(Note::Error(format!("Cannot write settings: {error}")));
                }
                self.open_scoped_models_panel(cx);
            }
            Action::ToggleSetting(key) if key.is_number() => self.start_number_setting(key, cx),
            Action::ToggleSetting(key) => {
                let result = match key {
                    SettingKey::AutoCompact => settings_manager::write_settings_auto_compact(
                        !settings_manager::read_settings_auto_compact(),
                    ),
                    SettingKey::Retry => settings_manager::write_settings_retry_enabled(
                        !settings_manager::read_settings_retry_enabled(),
                    ),
                    SettingKey::HideThinking => settings_manager::write_settings_hide_thinking(
                        !settings_manager::read_settings_hide_thinking(),
                    ),
                    SettingKey::SyntaxHighlight => {
                        settings_manager::write_settings_syntax_highlight(
                            !settings_manager::read_settings_syntax_highlight(),
                        )
                    }
                    SettingKey::ImagesAutoResize => {
                        settings_manager::write_settings_images_auto_resize(
                            !settings_manager::read_settings_images_auto_resize(),
                        )
                    }
                    SettingKey::ImagesBlockImages => {
                        settings_manager::write_settings_images_block_images(
                            !settings_manager::read_settings_images_block_images(),
                        )
                    }
                    SettingKey::ExposeSessionEnvironment => {
                        settings_manager::write_settings_expose_session_environment(
                            !settings_manager::read_settings_expose_session_environment(),
                        )
                    }
                    SettingKey::ShowCacheMissNotices => {
                        settings_manager::write_settings_show_cache_miss_notices(
                            !settings_manager::read_settings_show_cache_miss_notices(),
                        )
                    }
                    SettingKey::SteeringMode => {
                        let next = match settings_manager::read_settings_steering_mode().as_str() {
                            "all" => "one-at-a-time",
                            "one-at-a-time" => "immediateInterrupt",
                            _ => "all",
                        };
                        settings_manager::write_settings_steering_mode(next)
                    }
                    SettingKey::FollowUpMode => {
                        let next = match settings_manager::read_settings_follow_up_mode().as_str() {
                            "all" => "one-at-a-time",
                            _ => "all",
                        };
                        settings_manager::write_settings_follow_up_mode(next)
                    }
                    SettingKey::CacheWarming => {
                        let next = match settings_manager::read_settings_cache_warming().as_str() {
                            "off" => "streaming",
                            "streaming" => "idle",
                            _ => "off",
                        };
                        settings_manager::write_settings_cache_warming(next).map(|_| ())
                    }
                    SettingKey::QuietStartup => {
                        use crate::core::settings_manager::QuietStartup;
                        let next = match settings_manager::read_settings_quiet_startup() {
                            QuietStartup::Full => QuietStartup::Header,
                            QuietStartup::Header => QuietStartup::Silent,
                            QuietStartup::Silent => QuietStartup::Full,
                        };
                        settings_manager::write_settings_quiet_startup(next)
                    }
                    // 数值项走输入框（上面带 guard 的分支），这里不会命中
                    _ => Ok(()),
                };
                if let Err(error) = result {
                    self.transcript
                        .push_note(Note::Error(format!("Cannot write settings: {error}")));
                }
                // 重开面板，让标签显示新值
                self.open_settings_panel(cx);
            }
            Action::Attach { images } => {
                // 附件就是 `@路径` 引用：插入触发符后让补全面板接着列文件
                self.attach_images = images;
                self.editor.insert("@");
                self.refresh_suggest(cx);
            }
        }
        cx.notify();
    }

    /// 提交输入框内容。
    fn submit(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.editor.trimmed() else {
            return;
        };
        self.editor.clear();
        self.suggest = None;

        // `!!cmd`：本地执行，输出只显示、不进对话上下文
        if let Some(command) = text.strip_prefix("!!") {
            let output = run_shell(command.trim());
            self.transcript
                .push_note(Note::Info(format!("$ {}\n{output}", command.trim())));
            self.scroll_to_bottom();
            cx.notify();
            return;
        }
        // `!cmd`：本地执行，输出作为用户消息进入上下文
        if let Some(command) = text.strip_prefix('!') {
            let output = run_shell(command.trim());
            let message = format!("$ {}\n{output}", command.trim());
            self.transcript.push_user(message.clone());
            _ = self.cmd_tx.send(AgentCommand::Prompt(message));
            self.scroll_to_bottom();
            cx.notify();
            return;
        }
        // `/cmd`：GUI 命令
        if let Some(command) = text.strip_prefix('/') {
            self.run_slash(command.trim(), cx);
            self.scroll_to_bottom();
            return;
        }

        // 回合中提交的输入由 worker 排队，这里同步计数给状态栏
        if self.busy {
            self.queued += 1;
        } else {
            self.busy = true;
        }
        self.transcript.push_user(text.clone());
        // 和 TUI 一样记进项目级 prompt 历史，`/history` 才翻得到
        crate::core::prompt_history::append(
            &self.cwd(),
            &text,
            settings_manager::read_settings_history_max_entries(),
        );
        _ = self.cmd_tx.send(AgentCommand::Prompt(text));
        self.scroll_to_bottom();
    }

    /// 打开/切换右侧面板；`tab` 为 None 时保持当前标签。
    fn toggle_panel(&mut self, tab: Option<PanelTab>, cx: &mut Context<Self>) {
        match tab {
            Some(tab) => {
                self.panel.tab = tab;
                self.panel.visible = true;
            }
            None => self.panel.visible = !self.panel.visible,
        }
        if self.panel.visible {
            self.load_panel(cx);
        }
    }

    /// 后台读面板内容（列文件 / git status / git diff 都不在主线程跑）。
    fn load_panel(&mut self, cx: &mut Context<Self>) {
        let cwd = self.cwd();
        let tab = self.panel.tab;
        let selected = self.panel.selected;
        self.panel.loading = true;
        cx.spawn(async move |this, cx| {
            let data = cx
                .background_executor()
                .spawn(async move { panel::RightPanel::load(&cwd, tab, selected) })
                .await;
            _ = this.update(cx, |app, cx| {
                app.panel.apply(data);
                cx.notify();
            });
        })
        .detach();
    }

    /// 选中某条变更：切下标并在后台重读它的 diff。
    pub(super) fn select_change(&mut self, index: usize, cx: &mut Context<Self>) {
        self.panel.selected = index;
        self.load_panel(cx);
        cx.notify();
    }

    /// 当前工作目录（快照优先，回落到进程 cwd）。
    fn cwd(&self) -> String {
        self.snapshot
            .as_ref()
            .map(|s| s.cwd.clone())
            .unwrap_or_else(|| crate::utils::paths::cwd().to_string_lossy().to_string())
    }

    /// 命令面板。
    fn open_command_palette(&mut self, _cx: &mut Context<Self>) {
        let entries = vec![
            Entry {
                label: "New session".into(),
                detail: "Ctrl-N".into(),
                action: Action::NewSession,
                ..Entry::default()
            },
            Entry {
                label: "Switch session…".into(),
                detail: "Ctrl-O".into(),
                action: Action::Open(ListKind::Session),
                ..Entry::default()
            },
            Entry {
                label: "Select model…".into(),
                detail: "Ctrl-/".into(),
                action: Action::Open(ListKind::Model),
                ..Entry::default()
            },
            Entry {
                label: "Thinking level…".into(),
                detail: "Ctrl-T".into(),
                action: Action::Open(ListKind::Thinking),
                ..Entry::default()
            },
            Entry {
                label: "Switch appearance…".into(),
                detail: "Ctrl-G".into(),
                action: Action::Open(ListKind::Appearance),
                ..Entry::default()
            },
            Entry {
                label: "Zoom in".into(),
                detail: "Ctrl+=".into(),
                action: Action::Scale(theme::ui_scale() + theme::UI_SCALE_STEP),
                ..Entry::default()
            },
            Entry {
                label: "Zoom out".into(),
                detail: "Ctrl+-".into(),
                action: Action::Scale(theme::ui_scale() - theme::UI_SCALE_STEP),
                ..Entry::default()
            },
            Entry {
                label: "Reset scale to 100%".into(),
                detail: "Ctrl+0".into(),
                action: Action::Scale(1.0),
                ..Entry::default()
            },
            Entry {
                label: "Usage & cost".into(),
                detail: "Ctrl-U".into(),
                action: Action::Info(InfoKind::Usage),
                ..Entry::default()
            },
            Entry {
                label: "Skills".into(),
                detail: "Ctrl-I".into(),
                action: Action::Skills,
                ..Entry::default()
            },
            Entry {
                label: "Settings".into(),
                detail: "Ctrl-,".into(),
                action: Action::Settings,
                ..Entry::default()
            },
            Entry {
                label: "Prompt history…".into(),
                detail: "/history".into(),
                action: Action::Open(ListKind::History),
                ..Entry::default()
            },
            Entry {
                label: "Toggle session list".into(),
                detail: "Ctrl-Shift-B".into(),
                action: Action::ToggleSidebar,
                ..Entry::default()
            },
            Entry {
                label: "Share session (gist)…".into(),
                detail: "/share".into(),
                action: Action::Command(Box::new(AgentCommand::Share { theme: None })),
                ..Entry::default()
            },
            Entry {
                label: "Login (API key)…".into(),
                detail: "/login".into(),
                action: Action::Login,
                ..Entry::default()
            },
            Entry {
                label: "Logout (clear key)…".into(),
                detail: "/logout".into(),
                action: Action::Logout,
                ..Entry::default()
            },
            Entry {
                label: "Changelog…".into(),
                detail: "/changelog".into(),
                action: Action::Changelog,
                ..Entry::default()
            },
            Entry {
                label: "Trust this project".into(),
                detail: "/trust".into(),
                action: Action::ConfigCommand("trust".into()),
                ..Entry::default()
            },
            Entry {
                label: "Reload skills / context".into(),
                detail: "/reload".into(),
                action: Action::ConfigCommand("reload".into()),
                ..Entry::default()
            },
            Entry {
                label: "Report a bug (zip)".into(),
                detail: "/bug".into(),
                action: Action::ConfigCommand("bug".into()),
                ..Entry::default()
            },
            Entry {
                label: "Debug info".into(),
                detail: "/debug".into(),
                action: Action::ConfigCommand("debug".into()),
                ..Entry::default()
            },
            Entry {
                label: "Extensions…".into(),
                detail: "/extension".into(),
                action: Action::Extensions,
                ..Entry::default()
            },
            Entry {
                label: "Scoped models…".into(),
                detail: "/scoped-models".into(),
                action: Action::ScopedModels,
                ..Entry::default()
            },
            Entry {
                label: "Keybindings".into(),
                detail: "?".into(),
                action: Action::Info(InfoKind::Help),
                ..Entry::default()
            },
            Entry {
                label: "Compact context".into(),
                detail: "/compact".into(),
                action: Action::Command(Box::new(AgentCommand::Compact { instructions: None })),
                ..Entry::default()
            },
            Entry {
                label: "Rewind last input".into(),
                detail: "/rewind".into(),
                action: Action::Command(Box::new(AgentCommand::Rewind)),
                ..Entry::default()
            },
            Entry {
                label: "Workspace changes…".into(),
                detail: "Ctrl-E".into(),
                action: Action::Open(ListKind::PanelChanges),
                ..Entry::default()
            },
            Entry {
                label: "Files…".into(),
                detail: String::new(),
                action: Action::Open(ListKind::PanelFiles),
                ..Entry::default()
            },
            Entry {
                label: "Refresh sessions".into(),
                detail: String::new(),
                action: Action::RefreshSessions,
                ..Entry::default()
            },
            Entry {
                label: "Quit".into(),
                detail: "Ctrl-Q".into(),
                action: Action::Quit,
                ..Entry::default()
            },
        ];
        let list = List::new("Commands", entries);
        self.overlay = Some(Overlay::List(list));
    }

    /// `/history`：本会话发过的用户消息，Enter 写回输入框（不发出去）。
    fn open_history_panel(&mut self, _cx: &mut Context<Self>) {
        let cwd = self.cwd();
        let max = settings_manager::read_settings_history_max_entries();
        // 与 TUI 一致：读项目级 prompt 历史，而不是当前会话上下文
        // （压缩/摘要之后上下文里只剩一两条用户消息，历史记录不该跟着丢）
        let entries = history_entries(crate::core::prompt_history::load(&cwd, max));
        self.overlay = Some(Overlay::List(List::new("History", entries)));
    }

    /// 登录面板：列出全部支持的 provider，Enter 填 key。
    fn open_login_panel(&mut self, _cx: &mut Context<Self>) {
        let entries: Vec<Entry> = crate::core::model_resolver::SUPPORTED_PROVIDERS
            .iter()
            .map(|provider| Entry {
                label: provider.to_string(),
                detail: if crate::core::auth::has_auth(provider) {
                    "configured".into()
                } else {
                    String::new()
                },
                action: Action::LoginProvider(provider.to_string()),
                ..Entry::default()
            })
            .collect();
        self.overlay = Some(Overlay::List(List::new("Login", entries)));
    }

    /// 登出面板：只列已存 key 的 provider。
    fn open_logout_panel(&mut self, _cx: &mut Context<Self>) {
        let mut entries: Vec<Entry> = crate::core::auth::list_auth_providers()
            .into_iter()
            .map(|provider| Entry {
                label: provider.clone(),
                detail: "Enter to clear key".into(),
                action: Action::LogoutProvider(provider),
                ..Entry::default()
            })
            .collect();
        if entries.is_empty() {
            entries.push(Entry {
                label: "No saved keys".into(),
                detail: String::new(),
                action: Action::None,
                ..Entry::default()
            });
        }
        self.overlay = Some(Overlay::List(List::new("Logout", entries)));
    }

    /// 更新日志：当前版本的内嵌 changelog 前若干行。
    fn open_changelog(&mut self, _cx: &mut Context<Self>) {
        let lines: Vec<String> = crate::core::changelog::CHANGELOG
            .lines()
            .take(120)
            .map(|line| line.to_string())
            .collect();
        self.overlay = Some(Overlay::Info(Info::new(
            format!("Changelog v{}", crate::core::changelog::VERSION),
            lines,
        )));
    }

    /// 数值型设置：弹输入框，Enter 解析后写入（temperature 留空 = 清除）。
    fn start_number_setting(&mut self, key: SettingKey, cx: &mut Context<Self>) {
        let current = match key {
            SettingKey::RetryMaxRetries => {
                settings_manager::read_settings_retry_max_retries().to_string()
            }
            SettingKey::Temperature => settings_manager::read_settings_temperature()
                .map(|value| value.to_string())
                .unwrap_or_default(),
            SettingKey::CompactReserve => {
                settings_manager::read_settings_compact_reserve().to_string()
            }
            SettingKey::CompactKeepRecent => {
                settings_manager::read_settings_compact_keep_recent().to_string()
            }
            _ => String::new(),
        };
        let mut editor = Editor::new();
        editor.insert(&current);
        editor.select_all();
        self.dialog = Some(SessionDialog::Number { key, editor });
        cx.notify();
    }

    /// 弹一条 toast（TUI 状态栏那类瞬时提示），几秒后自动消失。
    fn toast(&mut self, text: impl Into<String>, kind: ToastKind, cx: &mut Context<Self>) {
        let text: String = text.into();
        self.toast_seq += 1;
        let id = self.toast_seq;
        self.toasts.push(Toast { id, text, kind });
        while self.toasts.len() > TOAST_MAX {
            self.toasts.remove(0);
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TOAST_DURATION).await;
            _ = this.update(cx, |app, cx| {
                app.toasts.retain(|toast| toast.id != id);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// toast 的堆叠渲染（右上角，不挡输入卡片）。
    fn toast_layer(&self, theme: &Theme) -> AnyElement {
        let mut column = div()
            .absolute()
            .top(px(48.))
            .right(px(20.))
            .w(px(320.))
            .flex()
            .flex_col()
            .items_end()
            .gap(px(6.));
        for toast in &self.toasts {
            let (base, text_color) = match toast.kind {
                ToastKind::Info => (theme.raised, theme.text),
                ToastKind::Success => (theme.raised, theme.success),
                ToastKind::Error => (theme.raised, theme.danger),
            };
            column = column.child(
                div()
                    .w_full()
                    .px(px(12.))
                    .py(px(8.))
                    .rounded(px(8.))
                    .bg(base)
                    .border_1()
                    .border_color(theme.border_strong)
                    .text_size(px(12.))
                    .text_color(text_color)
                    .child(SharedString::from(toast.text.clone())),
            );
        }
        column.into_any_element()
    }

    /// 读当前工作目录的 git 分支（侧栏底部展示）。
    fn refresh_branch(&mut self) {
        self.branch = crate::core::extensions::git_branch(&self.cwd());
    }

    /// Ctrl-S：把扩展/技能面板的草稿落盘（面板打开时按键先被覆盖层分支吃掉，
    /// 所以要在覆盖层分发之前调用）。
    fn save_panel_drafts(&mut self, cx: &mut Context<Self>) -> bool {
        if let Some(draft) = self.extensions_draft.take() {
            let mut changed = 0usize;
            for (name, enabled) in draft {
                if crate::core::extensions::is_extension_enabled(&name) != enabled {
                    crate::core::extensions::set_extension_enabled(&name, enabled);
                    changed += 1;
                }
            }
            if changed > 0 {
                if let Err(error) = crate::core::extensions::persist_extension_states() {
                    self.toast(
                        format!("Cannot save extension state: {error}"),
                        ToastKind::Error,
                        cx,
                    );
                }
                _ = self.cmd_tx.send(AgentCommand::RebuildTools);
            }
            self.overlay = None;
            self.toast(
                format!("Applied {changed} extension change(s)"),
                ToastKind::Success,
                cx,
            );
            cx.notify();
            return true;
        }
        if let Some(rules) = self.skills_draft.take() {
            match settings_manager::write_settings_skills(&rules) {
                Ok(()) => {
                    _ = self.cmd_tx.send(AgentCommand::Reload {
                        context_files: Vec::new(),
                        skills: Vec::new(),
                        system_prompt: None,
                        append_system_prompt: None,
                    });
                    self.overlay = None;
                    self.toast("Skill rules saved", ToastKind::Success, cx);
                }
                Err(error) => self.toast(
                    format!("Cannot write settings: {error}"),
                    ToastKind::Error,
                    cx,
                ),
            }
        }
        cx.notify();
        true
    }

    /// 简单重绘（`confirm_dialog` 里提前返回时用）。
    fn cx_notify(&self, cx: &mut Context<Self>) {
        cx.notify();
    }

    /// 数值设置落盘：解析失败只报错，对话框保留以便改回来。
    fn confirm_number_setting(&mut self, key: SettingKey, text: &str, cx: &mut Context<Self>) {
        let value = text.trim();
        let result: Result<(), String> = match key {
            SettingKey::RetryMaxRetries => value
                .parse::<u32>()
                .map_err(|_| "needs a non-negative integer".to_string())
                .and_then(|v| {
                    settings_manager::write_settings_retry_max_retries(v).map_err(|e| e.to_string())
                }),
            SettingKey::Temperature => {
                if value.is_empty() {
                    settings_manager::write_settings_temperature(None).map_err(|e| e.to_string())
                } else {
                    value
                        .parse::<f64>()
                        .map_err(|_| "needs a decimal number".to_string())
                        .and_then(|v| {
                            settings_manager::write_settings_temperature(Some(v))
                                .map_err(|e| e.to_string())
                        })
                }
            }
            SettingKey::CompactReserve => value
                .parse::<u32>()
                .map_err(|_| "needs a non-negative integer".to_string())
                .and_then(|v| {
                    settings_manager::write_settings_compact_reserve(v).map_err(|e| e.to_string())
                }),
            SettingKey::CompactKeepRecent => value
                .parse::<u32>()
                .map_err(|_| "needs a non-negative integer".to_string())
                .and_then(|v| {
                    settings_manager::write_settings_compact_keep_recent(v)
                        .map_err(|e| e.to_string())
                }),
            _ => Ok(()),
        };
        match result {
            Ok(()) => {
                self.dialog = None;
                self.open_settings_panel(cx);
            }
            Err(error) => {
                self.transcript
                    .push_note(Note::Error(format!("Setting not saved: {error}")));
            }
        }
    }

    /// 设置面板：列出常用开关与外观，Enter 直接改并落盘到 settings.json。
    fn open_settings_panel(&mut self, _cx: &mut Context<Self>) {
        let on_off = |on: bool| if on { "on" } else { "off" };
        let entries = vec![
            Entry {
                label: format!(
                    "Theme: {}",
                    if self.theme.is_dark { "dark" } else { "light" }
                ),
                detail: "Enter to toggle".into(),
                action: Action::SetDark(!self.theme.is_dark),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Auto-compact: {}",
                    on_off(settings_manager::read_settings_auto_compact())
                ),
                detail: "Compact when near the limit".into(),
                action: Action::ToggleSetting(SettingKey::AutoCompact),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Auto-retry: {}",
                    on_off(settings_manager::read_settings_retry_enabled())
                ),
                detail: "Retry failed requests".into(),
                action: Action::ToggleSetting(SettingKey::Retry),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Hide thinking: {}",
                    on_off(settings_manager::read_settings_hide_thinking())
                ),
                detail: "Hide model reasoning".into(),
                action: Action::ToggleSetting(SettingKey::HideThinking),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Syntax highlighting: {}",
                    on_off(settings_manager::read_settings_syntax_highlight())
                ),
                detail: "Highlight code blocks".into(),
                action: Action::ToggleSetting(SettingKey::SyntaxHighlight),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Auto-resize images: {}",
                    on_off(settings_manager::read_settings_images_auto_resize())
                ),
                detail: "Downscale oversized images".into(),
                action: Action::ToggleSetting(SettingKey::ImagesAutoResize),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Block images: {}",
                    on_off(settings_manager::read_settings_images_block_images())
                ),
                detail: "Refuse image input".into(),
                action: Action::ToggleSetting(SettingKey::ImagesBlockImages),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Expose session env: {}",
                    on_off(settings_manager::read_settings_expose_session_environment())
                ),
                detail: "Pass session info to tools as env vars".into(),
                action: Action::ToggleSetting(SettingKey::ExposeSessionEnvironment),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Cache miss notices: {}",
                    on_off(settings_manager::read_settings_show_cache_miss_notices())
                ),
                detail: String::new(),
                action: Action::ToggleSetting(SettingKey::ShowCacheMissNotices),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Steering mode: {}",
                    settings_manager::read_settings_steering_mode()
                ),
                detail: "Enter to cycle".into(),
                action: Action::ToggleSetting(SettingKey::SteeringMode),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Follow-up mode: {}",
                    settings_manager::read_settings_follow_up_mode()
                ),
                detail: "Enter to cycle".into(),
                action: Action::ToggleSetting(SettingKey::FollowUpMode),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Cache warming: {}",
                    settings_manager::read_settings_cache_warming()
                ),
                detail: "Enter to cycle".into(),
                action: Action::ToggleSetting(SettingKey::CacheWarming),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Quiet startup: {:?}",
                    settings_manager::read_settings_quiet_startup()
                ),
                detail: "Enter to cycle".into(),
                action: Action::ToggleSetting(SettingKey::QuietStartup),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Max retries: {}",
                    settings_manager::read_settings_retry_max_retries()
                ),
                detail: "Enter to edit".into(),
                action: Action::ToggleSetting(SettingKey::RetryMaxRetries),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "temperature: {}",
                    settings_manager::read_settings_temperature()
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "(model default)".into())
                ),
                detail: "Enter to edit, empty restores default".into(),
                action: Action::ToggleSetting(SettingKey::Temperature),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Compact reserve tokens: {}",
                    settings_manager::read_settings_compact_reserve()
                ),
                detail: "Enter to edit".into(),
                action: Action::ToggleSetting(SettingKey::CompactReserve),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Compact keep recent tokens: {}",
                    settings_manager::read_settings_compact_keep_recent()
                ),
                detail: "Enter to edit".into(),
                action: Action::ToggleSetting(SettingKey::CompactKeepRecent),
                ..Entry::default()
            },
            Entry {
                label: format!("UI scale: {}%", (theme::ui_scale() * 100.0).round()),
                detail: "Enter to reset 100%".into(),
                action: Action::Scale(1.0),
                ..Entry::default()
            },
            Entry {
                label: "Default model…".into(),
                detail: String::new(),
                action: Action::Open(ListKind::Model),
                ..Entry::default()
            },
            Entry {
                label: "Thinking level…".into(),
                detail: String::new(),
                action: Action::Open(ListKind::Thinking),
                ..Entry::default()
            },
            Entry {
                label: format!(
                    "Config directory: {}",
                    settings_manager::agent_dir().display()
                ),
                detail: String::new(),
                action: Action::None,
                ..Entry::default()
            },
        ];
        self.overlay = Some(Overlay::List(List::new("Settings", entries)));
    }

    /// 扩展管理面板：`[x] 名称` 勾选态，Enter/点击切换草稿，Ctrl-S 才落盘，Tab 看详情。
    fn open_extension_panel(&mut self, cx: &mut Context<Self>) {
        if self.extensions_draft.is_none() {
            self.extensions_draft = Some(
                configurable_extensions()
                    .into_iter()
                    .map(|(name, enabled, _)| (name, enabled))
                    .collect(),
            );
        }
        self.rebuild_extension_panel(cx);
    }

    /// 用草稿重算扩展行：勾选态来自草稿，Ctrl-S 才落到注册表并写盘。
    fn rebuild_extension_panel(&mut self, cx: &mut Context<Self>) {
        let draft = self.extensions_draft.clone().unwrap_or_default();
        let modes: std::collections::HashMap<String, String> = configurable_extensions()
            .into_iter()
            .map(|(name, _, modes)| {
                let labels: Vec<String> = modes.iter().map(|m| m.as_str().to_string()).collect();
                (name, labels.join(" / "))
            })
            .collect();
        let mut entries: Vec<Entry> = draft
            .into_iter()
            .map(|(name, enabled)| Entry {
                label: name.clone(),
                detail: modes.get(&name).cloned().unwrap_or_default(),
                action: Action::ToggleExtension(name),
                checked: Some(enabled),
            })
            .collect();
        if entries.is_empty() {
            entries.push(Entry {
                label: "No extensions registered".into(),
                ..Entry::default()
            });
        }
        self.replace_list("Extensions", entries, DetailSource::Extension);
        cx.notify();
    }

    /// `/skills`：按 TUI 的技能管理面板实现——列出全部可发现的技能并显示启用态，
    /// Enter 在草稿里切换启停，Ctrl-S 落盘并重载，Tab 看详情，Esc 丢弃草稿。
    fn open_skills_panel(&mut self, cx: &mut Context<Self>) {
        if self.skills_draft.is_none() {
            self.skills_draft = Some(settings_manager::read_settings_skills());
        }
        self.rebuild_skills_panel(cx);
    }

    /// 用当前草稿重算技能行并重新打开面板。
    fn rebuild_skills_panel(&mut self, cx: &mut Context<Self>) {
        let rules = self.skills_draft.clone().unwrap_or_default();
        let (all, enabled) = crate::modes::gui::skills_snapshot(&rules);
        let mut entries: Vec<Entry> = all
            .into_iter()
            .map(|(name, description, source, model_off, _path)| {
                let on = enabled.contains(&name);
                let mut detail = description;
                if !source.is_empty() {
                    if !detail.is_empty() {
                        detail.push_str(" · ");
                    }
                    detail.push_str(&source);
                }
                if model_off {
                    detail.push_str(" · [model-off]");
                }
                Entry {
                    label: name.clone(),
                    detail,
                    action: Action::ToggleSkill { name, enable: !on },
                    checked: Some(on),
                }
            })
            .collect();
        if entries.is_empty() {
            entries.push(Entry {
                label: "No skills found".into(),
                detail: String::new(),
                action: Action::None,
                ..Entry::default()
            });
        }
        self.replace_list(
            format!("Skills (Ctrl-S saves {} rules)", rules.len()),
            entries,
            DetailSource::Skill,
        );
        cx.notify();
    }

    /// 重建当前列表面板：沿用旧的过滤框与选中行（切换勾选后不跳回第一行）。
    fn replace_list(
        &mut self,
        title: impl Into<String>,
        entries: Vec<Entry>,
        detail_source: DetailSource,
    ) {
        match self.overlay.as_mut() {
            Some(Overlay::List(list)) => {
                list.detail = None;
                list.detail_source = detail_source;
                list.refresh(title, entries);
            }
            _ => {
                let mut list = List::new(title, entries);
                list.detail_source = detail_source;
                self.overlay = Some(Overlay::List(list));
            }
        }
    }

    /// Tab：把当前选中项展开成只读详情页（扩展 / 技能），再按 Tab 或 Esc 回列表。
    fn open_list_detail(&mut self) {
        let Some(Overlay::List(list)) = &self.overlay else {
            return;
        };
        let Some(name) = list.selected_label().map(|label| label.to_string()) else {
            return;
        };
        let lines = match list.detail_source {
            DetailSource::None => return,
            DetailSource::Extension => crate::core::extensions::extension_detail_lines(&name),
            DetailSource::Skill => crate::modes::gui::skill_detail_lines(
                self.skills_draft.as_deref().unwrap_or_default(),
                &name,
            ),
        };
        if lines.is_empty() {
            return;
        }
        if let Some(Overlay::List(list)) = &mut self.overlay {
            list.detail = Some(Info::new(name, lines));
        }
    }

    /// `/scoped-models`：勾选哪些模型进入 Ctrl-P 循环名单（写 settings 的 enabledModels）。
    fn open_scoped_models_panel(&mut self, cx: &mut Context<Self>) {
        self.overlay = Some(Overlay::List(List::new(
            "Scoped models",
            vec![Entry {
                label: "Reading models…".into(),
                detail: String::new(),
                action: Action::None,
                ..Entry::default()
            }],
        )));
        cx.spawn(async move |this, cx| {
            let models = cx
                .background_executor()
                .spawn(async {
                    let mut models = Vec::new();
                    for provider in crate::core::auth::list_configured_providers() {
                        for (id, name) in crate::core::model_resolver::list_models(&provider) {
                            models.push((provider.clone(), id, name));
                        }
                    }
                    models
                })
                .await;
            _ = this.update(cx, |app, cx| {
                let enabled = settings_manager::read_enabled_models();
                let entries: Vec<Entry> = models
                    .into_iter()
                    .map(|(provider, id, name)| {
                        let pattern = format!("{provider}/{id}");
                        let on = enabled.is_empty()
                            || enabled.iter().any(|rule| rule == &pattern || rule == &id);
                        Entry {
                            label: format!("[{}] {pattern}", if on { "x" } else { " " }),
                            detail: name,
                            action: Action::ToggleScopedModel(pattern),
                            ..Entry::default()
                        }
                    })
                    .collect();
                app.overlay = Some(Overlay::List(List::new("Scoped models", entries)));
                cx.notify();
            });
        })
        .detach();
    }

    /// 模型选择器。
    fn open_model_picker(&mut self, cx: &mut Context<Self>) {
        self.overlay = Some(Overlay::List(List::new(
            "Model",
            vec![Entry {
                label: "Reading models…".into(),
                detail: String::new(),
                action: Action::None,
                ..Entry::default()
            }],
        )));
        cx.spawn(async move |this, cx| {
            let models = cx
                .background_executor()
                .spawn(async {
                    let mut models = Vec::new();
                    for provider in crate::core::auth::list_configured_providers() {
                        for (id, name) in crate::core::model_resolver::list_models(&provider) {
                            models.push((provider.clone(), id, name));
                        }
                    }
                    models
                })
                .await;
            _ = this.update(cx, |app, cx| {
                let mut entries: Vec<Entry> = models
                    .into_iter()
                    .map(|(provider, id, name)| Entry {
                        label: format!("{provider} / {id}"),
                        detail: name,
                        action: Action::Command(Box::new(AgentCommand::SwitchModel {
                            provider,
                            model_id: id,
                            persist: true,
                        })),
                        ..Entry::default()
                    })
                    .collect();
                if entries.is_empty() {
                    entries.push(Entry {
                        label: "No configured models".into(),
                        detail: "Run `prux auth` first".into(),
                        action: Action::None,
                        ..Entry::default()
                    });
                }
                app.overlay = Some(Overlay::List(List::new("Model", entries)));
                cx.notify();
            });
        })
        .detach();
    }

    /// 思考级别选择器。
    fn open_thinking_picker(&mut self, _cx: &mut Context<Self>) {
        let levels = self
            .snapshot
            .as_ref()
            .map(|s| s.thinking_levels.clone())
            .unwrap_or_default();
        let mut entries: Vec<Entry> = levels
            .into_iter()
            .map(|level| Entry {
                label: level.clone(),
                detail: String::new(),
                action: Action::Command(Box::new(AgentCommand::SetThinking {
                    level,
                    persist: true,
                })),
                ..Entry::default()
            })
            .collect();
        if entries.is_empty() {
            entries.push(Entry {
                label: "This model has no thinking levels".into(),
                detail: String::new(),
                action: Action::None,
                ..Entry::default()
            });
        }
        self.overlay = Some(Overlay::List(List::new("Thinking level", entries)));
    }

    /// Shift+Tab：在当前模型支持的 thinking 级别里循环（同 TUI 的 `app.thinking.cycle`）。
    ///
    /// 当前级别不在列表里（或还不知道）时从头算，保证总能切到下一个。
    fn cycle_thinking(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let levels = snapshot.thinking_levels.clone();
        if levels.is_empty() {
            self.toast("This model has no thinking levels", ToastKind::Info, cx);
            return;
        }
        let current = snapshot.thinking.clone().unwrap_or_else(|| "off".into());
        let index = levels.iter().position(|level| *level == current);
        let next = levels[cycle_index(index, levels.len(), 1)].clone();
        _ = self.cmd_tx.send(AgentCommand::SetThinking {
            level: next,
            persist: false,
        });
    }

    /// Ctrl+P / Ctrl+Shift+P：在模型 scope 里前后循环（同 TUI 的 `app.model.cycleForward`）。
    ///
    /// scope 来自 `--models` / `settings.enabledModels`（`agent.model_cycle`）；为空时才退回全部已配置模型。
    fn cycle_model(&mut self, dir: i32, cx: &mut Context<Self>) {
        let all: Vec<(String, String)> = crate::core::auth::list_configured_providers()
            .iter()
            .flat_map(|provider| {
                crate::core::model_resolver::list_models(provider)
                    .into_iter()
                    .map(|(id, _)| (provider.clone(), id))
            })
            .collect();
        let scoped = self
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.model_cycle.clone())
            .unwrap_or_default();
        let models = cycle_models(&scoped, &all);
        if models.is_empty() {
            self.toast("No models to cycle", ToastKind::Info, cx);
            return;
        }
        if models.len() == 1 {
            let hint = if scoped.is_empty() {
                "Only one model available"
            } else {
                "Only one model in scope"
            };
            self.toast(hint, ToastKind::Info, cx);
            return;
        }
        let current = self
            .snapshot
            .as_ref()
            .map(|snapshot| (snapshot.provider.clone(), snapshot.model.clone()));
        let index = current
            .as_ref()
            .and_then(|(provider, model)| {
                models
                    .iter()
                    .position(|(p, id)| p == provider && id == model)
            })
            .unwrap_or(0);
        let (provider, model_id) = models[cycle_index(Some(index), models.len(), dir)].clone();
        // scope 条目带了 thinking 时先同步思考级别再切模型（同 TUI）
        if let Some(level) = scoped
            .iter()
            .find(|scoped| scoped.provider == provider && scoped.id == model_id)
            .and_then(|scoped| scoped.thinking.clone())
        {
            _ = self.cmd_tx.send(AgentCommand::SetThinking {
                level,
                persist: false,
            });
        }
        _ = self.cmd_tx.send(AgentCommand::SwitchModel {
            provider,
            model_id,
            persist: false,
        });
    }

    /// 会话选择器。
    fn open_session_picker(&mut self, cx: &mut Context<Self>) {
        if self.sessions.is_empty() && !self.sessions_loading {
            self.reload_sessions(cx);
        }
        let entries: Vec<Entry> = self
            .sessions
            .iter()
            .map(|row| Entry {
                label: row.title.clone(),
                detail: format!("{} · {} messages", row.age, row.messages),
                action: Action::LoadSession(row.path.clone()),
                ..Entry::default()
            })
            .collect();
        let mut entries = entries;
        if entries.is_empty() {
            entries.push(Entry {
                label: if self.sessions_loading {
                    "Loading sessions…".into()
                } else {
                    "No sessions yet".to_string()
                },
                detail: String::new(),
                action: Action::None,
                ..Entry::default()
            });
        }
        self.overlay = Some(Overlay::List(List::new("Sessions", entries)));
    }

    /// 外观选择器（明/暗）。
    fn open_appearance_picker(&mut self, _cx: &mut Context<Self>) {
        let entries = vec![
            Entry {
                label: "dark".into(),
                detail: if self.theme.is_dark { "current" } else { "" }.into(),
                action: Action::SetDark(true),
                ..Entry::default()
            },
            Entry {
                label: "light".into(),
                detail: if self.theme.is_dark { "" } else { "current" }.into(),
                action: Action::SetDark(false),
                ..Entry::default()
            },
        ];
        self.overlay = Some(Overlay::List(List::new("Appearance", entries)));
    }

    /// 信息页。
    fn open_info(&mut self, kind: InfoKind, _cx: &mut Context<Self>) {
        let info = match kind {
            InfoKind::Usage => {
                let usage = self.snapshot.as_ref().map(|s| &s.usage);
                let mut lines = Vec::new();
                if let Some(usage) = usage {
                    lines.push(format!("input tokens: {}", usage.input));
                    lines.push(format!("output tokens: {}", usage.output));
                    lines.push(format!("cache read: {}", usage.cache_read));
                    lines.push(format!("cache write: {}", usage.cache_write));
                    lines.push(format!("Context usage: {}", usage.context_tokens));
                    lines.push(format!("Total cost: ${:.4}", usage.cost));
                } else {
                    lines.push("No usage yet".to_string());
                }
                Info::new("Usage", lines)
            }
            // `/session`：当前会话概要（对齐 TUI 的 /session）
            InfoKind::Session => {
                let mut lines = vec![format!("Working directory: {}", self.cwd())];
                if let Some(snapshot) = &self.snapshot {
                    lines.push(format!("Model: {}/{}", snapshot.provider, snapshot.model));
                    if let Some(level) = &snapshot.thinking {
                        lines.push(format!("Thinking level: {level}"));
                    }
                    lines.push(format!("Messages: {}", snapshot.messages));
                    lines.push(format!(
                        "Session file: {}",
                        snapshot.session_path.as_deref().unwrap_or("(in-memory session)")
                    ));
                    lines.push(format!("Active tools: {}", snapshot.tools.join(", ")));
                }
                Info::new("Sessions", lines)
            }
            InfoKind::Help => Info::new(
                "Keybindings",
                vec![
                    "Enter to send · Shift+Enter / Ctrl-J new line".to_string(),
                    "Esc abort turn · Ctrl-C clear input · Ctrl-D quit when empty".to_string(),
                    // 编辑器移动/删除与 TUI 的 tui.editor.* 一一对应
                    "Arrows move · Ctrl-B / Ctrl-F by char · Alt-←/→ or Alt-B/F by word".to_string(),
                    "Home / Ctrl-A line start · End / Ctrl-E line end".to_string(),
                    "Ctrl-W / Alt-Backspace delete word back · Alt-D delete word forward".to_string(),
                    "Ctrl-U delete to line start · Ctrl-Shift-K to line end · Ctrl-- undo".to_string(),
                    "Ctrl-Shift-C copy · Ctrl-V paste".to_string(),
                    "Ctrl-K commands · Ctrl-P / Ctrl-/ model · Shift-Tab / Ctrl-T thinking level".to_string(),
                    "Ctrl-O sessions · Ctrl-Shift-B sidebar · Ctrl-Shift-E right panel".to_string(),
                    "Ctrl-Shift-U usage · Ctrl-I skills · Ctrl-, settings · Ctrl-Shift-A select all".to_string(),
                    "Ctrl-N new session · Ctrl-Z undo · Ctrl-Y redo".to_string(),
                    "Ctrl+= zoom in · Ctrl-Shift+- zoom out · Ctrl+0 reset to 100%".to_string(),
                    "Ctrl-Q quit".to_string(),
                ],
            ),
        };
        self.overlay = Some(Overlay::Info(info));
    }

    /// 状态栏文案。
    fn status_text(&self) -> String {
        let messages = self.snapshot.as_ref().map(|s| s.messages).unwrap_or(0);
        if self.busy {
            return format!(
                "● Working… (Esc to abort, {} queued) · {messages} messages",
                self.queued
            );
        }
        format!(
            "{messages} messages · zoom {}%",
            (theme::ui_scale() * 100.0).round()
        )
    }
}

/// 最近工作目录最多记多少条。
const RECENT_CWD_LIMIT: usize = 20;

/// 最近工作目录清单文件（放在 agent 目录，不动用户的 settings.json）。
fn recent_cwds_path() -> std::path::PathBuf {
    settings_manager::agent_dir().join("recent-cwds.json")
}

/// 读最近工作目录（最新的在前，坏文件当空列表）。
fn recent_cwds() -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(recent_cwds_path()) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(&text)
        .map(|list| list.into_iter().take(RECENT_CWD_LIMIT).collect())
        .unwrap_or_default()
}

/// 记一个新工作目录：去重提到最前，超过上限截断。
fn remember_cwd(path: &str) {
    let mut list = recent_cwds();
    list.retain(|item| item != path);
    list.insert(0, path.to_string());
    list.truncate(RECENT_CWD_LIMIT);
    if let Some(dir) = recent_cwds_path().parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(&list) {
        let _ = std::fs::write(recent_cwds_path(), text);
    }
}

/// prompt 历史 → 面板条目：最新的在前，标签取首行并夹到 [`HISTORY_LABEL_WIDTH`]。
fn history_entries(history: Vec<crate::core::prompt_history::HistoryEntry>) -> Vec<Entry> {
    let mut entries: Vec<Entry> = history
        .into_iter()
        .rev()
        .map(|item| {
            // 首行可能是空行（多行提示常以换行开头），取第一行有内容的
            let first = item
                .text
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default();
            let label = crate::utils::display::fit_display(first, HISTORY_LABEL_WIDTH);
            Entry {
                label: if label.is_empty() {
                    item.text.clone()
                } else {
                    label
                },
                detail: String::new(),
                action: Action::InsertText(item.text),
                ..Entry::default()
            }
        })
        .collect();
    if entries.is_empty() {
        entries.push(Entry {
            label: "No prompts in history yet".into(),
            detail: String::new(),
            action: Action::None,
            ..Entry::default()
        });
    }
    entries
}

/// 勾选类动作会重建同一个面板：执行前别关覆盖层，否则过滤与选中行都丢了。
fn keeps_overlay(action: &Action) -> bool {
    matches!(
        action,
        Action::ToggleSkill { .. } | Action::ToggleExtension(_)
    )
}

/// GUI 能配置的扩展：底栏与横幅扩展只影响终端界面渲染，桌面端不列出来（也没法生效）。
fn configurable_extensions() -> Vec<(String, bool, Vec<crate::core::extensions::ExtensionMode>)> {
    let hidden: Vec<String> = crate::core::extensions::footer_names()
        .into_iter()
        .chain(crate::core::extensions::banner_names())
        .collect();
    crate::core::extensions::panel_entries()
        .into_iter()
        .filter(|(name, _, _)| !hidden.contains(name))
        .collect()
}

/// 技能草稿里把某个技能切到目标态：先清掉同名规则，开 → `+名称`，关 → `-名称`。
///
/// `+名称` 是 ForceInclude（压过 `!glob` 这类通配排除），`-名称` 是 ForceExclude，
/// 两者都是 `core::skills` 认识的规则形状。
fn skill_rules_with(rules: &[String], name: &str, enable: bool) -> Vec<String> {
    let exclude = format!("-{name}");
    let include = format!("+{name}");
    let mut next: Vec<String> = rules
        .iter()
        .filter(|rule| rule.as_str() != exclude && rule.as_str() != include)
        .cloned()
        .collect();
    next.push(if enable { include } else { exclude });
    next
}

/// 同一行两次切换要间隔多久才算两次操作（长按重建后会再弹一次 click）。
const TOGGLE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

/// 该不该忽略这次切换：同一条目、且距上次不到 `window` 就丢弃。
fn ignore_repeat_toggle(
    last: Option<(&str, std::time::Instant)>,
    name: &str,
    now: std::time::Instant,
    window: std::time::Duration,
) -> bool {
    last.is_some_and(|(last_name, at)| last_name == name && now.duration_since(at) < window)
}

/// 补全面板该不该自动弹出：手动关掉（Esc / 退格删空）过的同一次触发不再弹回。
fn suggest_allowed(dismissed: Option<usize>, start: usize) -> bool {
    dismissed != Some(start)
}

/// 遍历动作需要所有权：把借来的动作深拷一份（只有 String/命令两种载荷）。
fn clone_action(action: &Action) -> Action {
    match action {
        Action::None => Action::None,
        Action::Command(command) => Action::Command(Box::new(clone_command(command))),
        Action::LoadSession(path) => Action::LoadSession(path.clone()),
        Action::SetDark(is_dark) => Action::SetDark(*is_dark),
        Action::NewSession => Action::NewSession,
        Action::Open(kind) => Action::Open(*kind),
        Action::Info(kind) => Action::Info(*kind),
        Action::RefreshSessions => Action::RefreshSessions,
        Action::Scale(scale) => Action::Scale(*scale),
        Action::Quit => Action::Quit,
        Action::Attach { images } => Action::Attach { images: *images },
        Action::ToggleSetting(key) => Action::ToggleSetting(*key),
        Action::Settings => Action::Settings,
        Action::SwitchCwdTo(path) => Action::SwitchCwdTo(path.clone()),
        Action::BrowseCwd => Action::BrowseCwd,
        Action::ToggleSkill { name, enable } => Action::ToggleSkill {
            name: name.clone(),
            enable: *enable,
        },
        Action::ConfigCommand(name) => Action::ConfigCommand(name.clone()),
        Action::InsertText(text) => Action::InsertText(text.clone()),
        Action::ToggleSidebar => Action::ToggleSidebar,
        Action::Login => Action::Login,
        Action::Logout => Action::Logout,
        Action::LoginProvider(provider) => Action::LoginProvider(provider.clone()),
        Action::LogoutProvider(provider) => Action::LogoutProvider(provider.clone()),
        Action::Changelog => Action::Changelog,
        Action::Extensions => Action::Extensions,
        Action::Skills => Action::Skills,
        Action::ScopedModels => Action::ScopedModels,
        Action::ToggleExtension(name) => Action::ToggleExtension(name.clone()),
        Action::ToggleScopedModel(pattern) => Action::ToggleScopedModel(pattern.clone()),
    }
}

/// 复制一条命令（AgentCommand 未实现 Clone）。
fn clone_command(command: &AgentCommand) -> AgentCommand {
    match command {
        AgentCommand::SwitchModel {
            provider,
            model_id,
            persist,
        } => AgentCommand::SwitchModel {
            provider: provider.clone(),
            model_id: model_id.clone(),
            persist: *persist,
        },
        AgentCommand::SetThinking { level, persist } => AgentCommand::SetThinking {
            level: level.clone(),
            persist: *persist,
        },
        AgentCommand::Compact { instructions } => AgentCommand::Compact {
            instructions: instructions.clone(),
        },
        AgentCommand::Rewind => AgentCommand::Rewind,
        AgentCommand::NewSession => AgentCommand::NewSession,
        AgentCommand::SessionStats => AgentCommand::SessionStats,
        _ => AgentCommand::SessionStats,
    }
}

impl Render for GuiApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme.clone();

        // ── 标题栏：项目名 › 会话名，右侧模型/思考标签 ──────────────
        let cwd = self
            .snapshot
            .as_ref()
            .map(|s| s.cwd.clone())
            .unwrap_or_default();
        let project = basename(&cwd);
        let session_title = self
            .snapshot
            .as_ref()
            .and_then(|s| s.session_name.clone())
            .unwrap_or_else(|| "New session".to_string());
        let model = self
            .snapshot
            .as_ref()
            .map(|s| format!("{}/{}", s.provider, s.model))
            .unwrap_or_else(|| "No model selected".to_string());
        let thinking = self
            .snapshot
            .as_ref()
            .and_then(|s| s.thinking.clone())
            .filter(|t| t != "off");

        let mut chrome_right = div().flex().items_center().gap(px(6.));
        chrome_right = chrome_right.child(
            chip(&theme, model.clone(), false)
                .id("model-chip")
                .cursor_pointer()
                .hover(|style| style.bg(theme.overlay).text_color(theme.text))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                        this.open_model_menu(event.position, cx);
                    }),
                ),
        );
        chrome_right = chrome_right.child(
            chip(
                &theme,
                match &thinking {
                    Some(level) => format!("thinking {level}"),
                    None => "thinking off".to_string(),
                },
                false,
            )
            .id("thinking-chip")
            .cursor_pointer()
            .hover(|style| style.bg(theme.overlay).text_color(theme.text))
            .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, cx| {
                this.open_thinking_menu(event.position(), cx);
            })),
        );
        chrome_right = chrome_right.child(
            div()
                .id("command-palette-button")
                .h(px(20.))
                .px(px(8.))
                .rounded(px(5.))
                .flex()
                .items_center()
                .bg(theme.overlay)
                .border_1()
                .border_color(theme.border)
                .text_size(px(10.5))
                .text_color(theme.text_secondary)
                .cursor_pointer()
                .hover(|style| style.bg(theme.overlay_strong).text_color(theme.text))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.open_command_palette(cx);
                    cx.notify();
                }))
                .child(SharedString::from("Ctrl-K commands")),
        );

        let chrome = div()
            .flex_none()
            .h(px(CHROME_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(14.))
            .bg(theme.canvas)
            .border_b_1()
            .border_color(theme.border)
            .child(dot(theme.accent, 7.))
            .child(
                div()
                    .text_size(px(12.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from(project)),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.text_ghost)
                    .child(SharedString::from("›")),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(theme.text_secondary)
                    .child(SharedString::from(session_title)),
            )
            .child(div().flex_1())
            .child(chrome_right);

        // ── 对话流 ────────────────────────────────────────────────
        let pane_width = self.pane_width.get();
        // 内容的可用内宽：对话区宽度减去左右内边距（首帧还没量到时按最大宽度估）
        let flow_width = if pane_width > px(0.) {
            (pane_width - px(56.)).min(px(CONTENT_MAX_WIDTH))
        } else {
            px(CONTENT_MAX_WIDTH)
        };
        let empty = self.transcript.blocks().is_empty();
        let expand_tools = self.expand_tools;
        // 行登记每帧重建：行序号/边界/文本必须与本帧渲染结果一致
        self.line_bounds.borrow_mut().clear();
        self.line_text_bounds.borrow_mut().clear();
        self.line_rows.borrow_mut().clear();
        let mut lines = transcript::TextLines {
            next: 0,
            selection: self.text_sel,
            bounds: self.line_bounds.clone(),
            text_bounds: self.line_text_bounds.clone(),
            rows: self.line_rows.clone(),
        };
        let mut flow_ctx = transcript::FlowCtx {
            width: flow_width,
            lines: &mut lines,
        };
        // 默认行高是 φ（1.618em），行内代码/高亮背景会又高又连在一起，
        // 文字也不在背景里居中；正文统一用更紧的行高
        let mut flow = div().w_full().flex().flex_col().gap(px(14.));
        for (index, block) in self.transcript.blocks_mut().iter_mut().enumerate() {
            flow = flow.child(transcript::render_block(
                block,
                &theme,
                expand_tools,
                index,
                &cwd,
                &mut flow_ctx,
                cx,
            ));
        }

        let measured_height = self.flow_height.clone();
        let mut content = flow
            .flex_shrink_0()
            .max_w(px(CONTENT_MAX_WIDTH))
            .mx_auto()
            .px(px(28.))
            .pt(px(FLOW_PADDING_TOP))
            .pb(px(FLOW_PADDING_BOTTOM));
        // 滚动容器里的内容会被按「无限宽」测量，之后又按实际宽度换行画出来：
        // 长文本的实测高度偏小，往下压到后一个块上、展开时底部还会被截。
        // 用上一帧实测到的对话区宽度把内容定宽，测量和绘制就用同一个换行宽度。
        content = if pane_width > px(0.) {
            content.w(pane_width)
        } else {
            content.w_full()
        };
        // 同一处测量误差也在高度上：内容盒的实测高度会比子块实际占据的还小，
        // 滚到底仍有最后几行被裁掉。按上一帧子块实际的跨度补 min-height。
        if let Some(height) = measured_height.get() {
            content = content.min_h(height);
        }
        content =
            content.on_children_prepainted(move |children: Vec<Bounds<Pixels>>, window, _| {
                let (Some(first), Some(last)) = (children.first(), children.last()) else {
                    return;
                };
                let needed =
                    last.bottom() - first.top() + px(FLOW_PADDING_TOP + FLOW_PADDING_BOTTOM);
                if measured_height.get() != Some(needed) {
                    measured_height.set(Some(needed));
                    window.request_animation_frame();
                }
            });

        // 钉底：事件里那次 `scroll_to_bottom` 会停在旧的底（内容高度还会被二次测量改掉），
        // 这里在布局结束后按实际内容底边对一次；只在意内容变高变矮，用户自己滚开就不管了。
        let scroll = self.scroll.clone();
        let pinned = self.pinned_bottom.clone();
        let pane_width = self.pane_width.clone();
        let pin_bottom = move |_: Vec<Bounds<Pixels>>, window: &mut Window, _: &mut App| {
            let viewport_bounds = scroll.bounds();
            if (pane_width.get() - viewport_bounds.size.width).abs() > px(0.5) {
                pane_width.set(viewport_bounds.size.width);
                window.request_animation_frame();
            }
            let (Some(content), viewport) = (scroll.bounds_for_item(0), scroll.bounds()) else {
                return;
            };
            let target = viewport.bottom() - content.bottom();
            let offset = scroll.offset().y;
            // 「上次钉底的偏移」还在原位 = 这仍是我们的地盘；否则是用户滚轮/拖滚动条动过了
            let owned = pinned.get().filter(|last| (*last - offset).abs() <= px(1.));
            // 用户自己滚回底部时重新接管，这样新内容来了还会继续跟
            let following = owned.is_some() || (offset - target).abs() <= px(1.);
            if following && owned != Some(target) {
                scroll.set_offset(gpui::point(px(0.), target));
                window.request_animation_frame();
            }
            let next = following.then_some(target);
            if next != pinned.get() {
                pinned.set(next);
            }
        };

        let transcript_area = div()
            .on_children_prepainted(pin_bottom)
            .id("transcript")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            // 滚轮滚动不会通知视图，这里补一次重绘，滚动条滑块才跟得上
            .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
            // 悬浮探测走全局鼠标监听（见 `hover_probe`）：滚动容器的 hitbox 判定偏宽
            .child(content);
        let transcript = div()
            .relative()
            .flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(transcript_area)
            .child(self.scrollbar(ScrollArea::Transcript, &theme, cx));

        // ── 输入卡片：文本框 + 一行提示与动作 ──────────────────────
        let tokens = self
            .snapshot
            .as_ref()
            .map(|s| s.usage.context_tokens)
            .unwrap_or(0);
        let mut hint = div().flex().items_center().gap(px(8.));
        hint = hint.child(
            div()
                .id("attach")
                .h(px(20.))
                .px(px(8.))
                .rounded(px(5.))
                .flex()
                .items_center()
                .border_1()
                .border_color(theme.border)
                .text_size(px(11.))
                .text_color(theme.text_secondary)
                .cursor_pointer()
                .hover(|style| style.bg(theme.overlay).text_color(theme.text))
                .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, cx| {
                    this.open_attach_menu(event.position(), cx);
                }))
                .child(SharedString::from("+ Attach")),
        );
        if tokens > 0 {
            hint = hint.child(
                div()
                    .text_size(px(10.5))
                    .text_color(theme.text_ghost)
                    .child(SharedString::from(format!("context {tokens} tok"))),
            );
        }
        hint = hint.child(div().flex_1());
        let busy = self.busy;
        hint = hint.child(
            div()
                .id("send")
                .h(px(26.))
                .px(px(14.))
                .rounded(px(7.))
                .flex()
                .items_center()
                .text_size(px(11.5))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .bg(theme.inverse)
                .text_color(theme.on_inverse)
                .cursor_pointer()
                .hover(|style| style.opacity(0.86))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.busy {
                        _ = this.cmd_tx.send(AgentCommand::Abort);
                        this.queued = 0;
                    } else {
                        this.submit(cx);
                    }
                    cx.notify();
                }))
                .child(SharedString::from(if busy { "Stop" } else { "Send" })),
        );

        let ime_layer = self.ime_layer(cx);
        // 输入框可用内宽：上一帧实测的卡片宽减左右内边距（首帧还没量到时按窗口估算）
        let composer_wrap_width = self
            .composer_bounds
            .get()
            .map(|bounds| bounds.size.width - px(COMPOSER_H_PADDING * 2.))
            .filter(|width| *width > px(0.))
            .unwrap_or_else(|| px(600.));

        let composer_card = div()
            .relative()
            .w_full()
            .max_w(px(CONTENT_MAX_WIDTH))
            .mx_auto()
            .rounded(px(12.))
            .bg(theme.composer)
            .border_1()
            .border_color(theme.border_strong)
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(12.))
            .pt(px(10.))
            .pb(px(8.))
            .child(composer::render_wrapped(
                &self.editor,
                &theme,
                {
                    let app = cx.entity();
                    std::rc::Rc::new(move |event, row, col, _, cx| {
                        app.update(cx, |this, cx| {
                            this.set_cursor_at(row, col, event.modifiers.shift, cx);
                        });
                    })
                },
                {
                    let app = cx.entity();
                    std::rc::Rc::new(move |_, row, col, _, cx| {
                        // 拖动选区：锚点留在按下处，光标跟着走
                        app.update(cx, |this, cx| this.set_cursor_at(row, col, true, cx));
                    })
                },
                composer_wrap_width,
                &self.composer_scroll,
                &self.composer_cursor,
                window,
            ))
            .child(hint)
            .child(ime_layer);

        let footer_status = self.status_text();

        // ── 主区 ──────────────────────────────────────────────────
        let main = if empty {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .bg(theme.canvas)
                .justify_center()
                .child(
                    div()
                        .w_full()
                        .max_w(px(CONTENT_MAX_WIDTH))
                        .mx_auto()
                        .px(px(28.))
                        .flex()
                        .flex_col()
                        .gap(px(16.))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(4.))
                                .child(
                                    div()
                                        .text_size(px(22.))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(theme.text)
                                        .child(SharedString::from("What should we build today?")),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.5))
                                        .text_color(theme.text_tertiary)
                                        .child(SharedString::from(
                                            "Enter to send · Ctrl-K for commands · Ctrl-O to switch sessions",
                                        )),
                                ),
                        )
                        .child(measured_composer(composer_card, self.composer_bounds.clone()))
                        .child(
                            div()
                                .px(px(13.))
                                .text_size(px(10.5))
                                .text_color(theme.text_ghost)
                                .child(SharedString::from(
                                    "Shift+Enter new line · Ctrl-G appearance · Ctrl-Q quit",
                                )),
                        ),
                )
        } else {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .bg(theme.canvas)
                .child(transcript)
                .child(measured_composer(
                    div()
                        .flex_none()
                        .w_full()
                        .px(px(28.))
                        .pb(px(6.))
                        .child(composer_card),
                    self.composer_bounds.clone(),
                ))
                .child(
                    div().flex_none().w_full().px(px(28.)).pb(px(6.)).child(
                        div()
                            .max_w(px(CONTENT_MAX_WIDTH))
                            .mx_auto()
                            .px(px(13.))
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(10.5))
                            .text_color(theme.text_ghost)
                            .child(SharedString::from(footer_status))
                            .child(SharedString::from(
                                "Enter to send · Shift+Enter for a new line · Ctrl-K commands · Ctrl-Q quit",
                            )),
                    ),
                )
        };

        let mut body = div().flex().flex_1().min_h_0().w_full();
        if self.show_sidebar {
            body = body.child(
                div()
                    .relative()
                    .h_full()
                    .flex_none()
                    .child(sidebar::render(sidebar::SidebarCtx {
                        theme: &theme,
                        cwd: &cwd,
                        branch: self.branch.as_deref(),
                        width: self.sidebar_width,
                        rows: &self.sessions,
                        current: self
                            .snapshot
                            .as_ref()
                            .and_then(|s| s.session_path.as_deref()),
                        loading: self.sessions_loading,
                        scroll: &self.sessions_scroll,
                        scrollbar: self.scrollbar(ScrollArea::Sidebar, &theme, cx),
                        cmd_tx: self.cmd_tx.clone(),
                        new_session: Box::new(cx.listener(|this, _, _, cx| {
                            _ = this.cmd_tx.send(AgentCommand::NewSession);
                            this.transcript.clear();
                            this.flow_height.set(None);
                            this.reload_sessions(cx);
                            cx.notify();
                        })),
                        change_cwd: Box::new(cx.listener(|this, _, _, cx| this.open_cwd_menu(cx))),
                        context_menu: {
                            let app = cx.entity();
                            std::rc::Rc::new(
                                move |path: &str, event: &gpui::MouseDownEvent, _, cx| {
                                    let path = path.to_string();
                                    let position = event.position;
                                    app.update(cx, |this, cx| {
                                        this.open_session_menu(&path, position, cx)
                                    });
                                },
                            )
                        },
                    }))
                    .child(
                        // 右边框拖拽热区：骑在边框上，指针变左右缩放
                        div()
                            .id("sidebar-resize")
                            .absolute()
                            .top_0()
                            .right(px(-SIDEBAR_HANDLE_WIDTH / 2.))
                            .h_full()
                            .w(px(SIDEBAR_HANDLE_WIDTH))
                            .cursor(gpui::CursorStyle::ResizeLeftRight)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                                    this.resize_drag = Some(ResizeDrag {
                                        anchor_x: None,
                                        width: this.sidebar_width,
                                    });
                                    cx.stop_propagation();
                                    cx.notify();
                                }),
                            ),
                    ),
            );
        }
        body = body.child(main);
        if self.panel.visible {
            body = body.child(
                div()
                    .relative()
                    .h_full()
                    .flex_none()
                    .child(panel::render(&theme, &self.panel, self.panel_width, cx))
                    .child(
                        // 左边框拖拽热区：往左拉变宽，指针变左右缩放
                        div()
                            .id("panel-resize")
                            .absolute()
                            .top_0()
                            .left(px(-SIDEBAR_HANDLE_WIDTH / 2.))
                            .h_full()
                            .w(px(SIDEBAR_HANDLE_WIDTH))
                            .cursor(gpui::CursorStyle::ResizeLeftRight)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                                    this.panel_resize = Some(ResizeDrag {
                                        anchor_x: None,
                                        width: this.panel_width,
                                    });
                                    cx.stop_propagation();
                                    cx.notify();
                                }),
                            ),
                    ),
            );
        }

        let mut root = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.canvas)
            .text_color(theme.text)
            .text_size(px(13.))
            .font_family(theme::UI_FONT_FAMILY)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .child(chrome)
            .child(body);

        if let Some(overlay) = &self.overlay {
            root = root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w_full()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(gpui::rgba(0x00000066))
                    .occlude()
                    // 点遮罩（卡片外）关闭；卡片自己拦下事件，点里面不关
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.overlay = None;
                            this.skills_draft = None;
                            this.extensions_draft = None;
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|_, _, _, cx| cx.stop_propagation()),
                            )
                            .child(overlay::render(overlay, &theme, cx)),
                    ),
            );
        }
        root = root.child(self.toast_layer(&theme));
        root = root.child(self.hover_probe(!empty, cx));

        // ── 下拉菜单（模型 / 思考级别 / 附件）────────────────────
        if let Some(dropdown) = &self.dropdown {
            let (anchor, width, scroll) = (dropdown.anchor, dropdown.width, dropdown.scroll);
            // 锚点来自鼠标事件，是窗口的物理像素；尺寸按 px() 走的是「逻辑像素 ×ui_scale」。
            // 统一换到物理像素再算位置，否则菜单会被缩放顶到窗口外（曾整块看不见）。
            let scale = theme::ui_scale();
            let width_px = width * scale;
            let left = clamp_menu_left(
                f32::from(anchor.x),
                width_px,
                f32::from(window.viewport_size().width),
            );
            // 菜单比窗口还高时也别顶出去：按行数估个高度，底部同样让出边距
            let estimate = ((dropdown.items.len() as f32 * DROPDOWN_ROW_HEIGHT + 8.)
                .min(DROPDOWN_MAX_HEIGHT))
                * scale;
            let top = clamp_menu_top(
                f32::from(anchor.y) + 4. * scale,
                estimate,
                f32::from(window.viewport_size().height),
            );
            let mut card = div()
                .id("dropdown-menu")
                .absolute()
                .left(gpui::px(left))
                .top(gpui::px(top))
                .w(gpui::px(width_px))
                .p(px(4.))
                .rounded(px(8.))
                .bg(theme.composer)
                .border_1()
                .border_color(theme.border_strong)
                .flex()
                .flex_col()
                .occlude()
                // 同会话菜单：按下菜单不能冒泡给遮罩，否则菜单先关、click 不触发
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_, _, _, cx| cx.stop_propagation()),
                );
            if scroll {
                card = card.max_h(px(DROPDOWN_MAX_HEIGHT)).overflow_y_scroll();
            }
            for (index, item) in dropdown.items.iter().enumerate() {
                let action = clone_action(&item.action);
                let mut row = div()
                    .id(("dropdown", index))
                    .min_h(px(24.))
                    .px(px(8.))
                    .rounded(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .text_size(px(12.))
                    .text_color(theme.text_secondary)
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.overlay).text_color(theme.text))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .child(SharedString::from(item.label.clone())),
                    );
                if !item.detail.is_empty() {
                    row = row.child(
                        div()
                            .flex_none()
                            .text_size(px(10.5))
                            .text_color(theme.text_ghost)
                            .child(SharedString::from(item.detail.clone())),
                    );
                }
                card = card.child(row.on_click(cx.listener(move |this, _, _, cx| {
                    this.dropdown = None;
                    let action = clone_action(&action);
                    this.run_action(action, cx);
                    cx.notify();
                })));
            }
            root = root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.dropdown = None;
                            cx.notify();
                        }),
                    )
                    .child(card),
            );
        }

        // ── 输入补全面板：贴在输入卡片上沿的覆盖层 ────────────────
        if let (Some(suggest), Some(card)) = (self.suggest.clone(), self.composer_bounds.get()) {
            let rows = suggest.items.len().clamp(1, SUGGEST_ROWS) as f32;
            // 面板高度 = 输入框 + 行区（行数超上限时只按上限算，其余靠滚动）
            let height = px(SUGGEST_ROW_HEIGHT * rows) + px(SUGGEST_CHROME_HEIGHT);
            let top = (card.origin.y - height - px(SUGGEST_GAP)).max(px(0.));
            root = root.child(
                div()
                    .absolute()
                    .left(card.origin.x)
                    .w(card.size.width)
                    .top(top)
                    // 挡住下层：否则点在面板上的鼠标事件会穿到对话流/输入框
                    .occlude()
                    .child(self.suggest_panel(&suggest, &theme, cx)),
            );
        }

        // ── 会话行右键菜单 ────────────────────────────────────────
        if let Some(menu) = &self.session_menu {
            let anchor = menu.position;
            let path = menu.path.clone();
            let rename_path = path.clone();
            root = root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.close_session_menu(cx)),
                    )
                    .child(
                        div()
                            .absolute()
                            .left(anchor.x)
                            .top(anchor.y)
                            .w(px(132.))
                            .p(px(4.))
                            .rounded(px(8.))
                            .bg(theme.composer)
                            .border_1()
                            .border_color(theme.border_strong)
                            .flex()
                            .flex_col()
                            // 按下菜单本身不能冒泡到遮罩：遮罩会先关菜单，随后的 click 就落不到条目上
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|_, _, _, cx| cx.stop_propagation()),
                            )
                            .child(
                                div()
                                    .id("session-menu-rename")
                                    .h(px(26.))
                                    .px(px(8.))
                                    .rounded(px(6.))
                                    .flex()
                                    .items_center()
                                    .text_size(px(12.))
                                    .text_color(theme.text_secondary)
                                    .cursor_pointer()
                                    .hover(|style| style.bg(theme.overlay).text_color(theme.text))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.start_rename(&rename_path, cx);
                                    }))
                                    .child(SharedString::from("Rename")),
                            )
                            .child(
                                div()
                                    .id("session-menu-delete")
                                    .h(px(26.))
                                    .px(px(8.))
                                    .rounded(px(6.))
                                    .flex()
                                    .items_center()
                                    .text_size(px(12.))
                                    .text_color(theme.text_secondary)
                                    .cursor_pointer()
                                    .hover(|style| style.bg(theme.overlay).text_color(theme.danger))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.ask_delete(&path, cx);
                                    }))
                                    .child(SharedString::from("Delete")),
                            ),
                    ),
            );
        }

        // ── 重命名 / 删除确认 ─────────────────────────────────────
        if self.dialog.is_some() {
            let (title, body, field) = match &self.dialog {
                Some(SessionDialog::Rename { editor, .. }) => (
                    "Rename session",
                    String::new(),
                    Some(composer::render(editor, &theme)),
                ),
                Some(SessionDialog::BugHint { editor }) => (
                    "Report a bug",
                    "Describe what happened (bundled with redacted metadata)".to_string(),
                    Some(composer::render(editor, &theme)),
                ),
                Some(SessionDialog::Label { editor }) => (
                    "Set label",
                    "Leave empty to clear the label on this branch".to_string(),
                    Some(composer::render(editor, &theme)),
                ),
                Some(SessionDialog::Import { editor }) => (
                    "Import session",
                    "Path to a jsonl session (~ and relative paths work)".to_string(),
                    Some(composer::render(editor, &theme)),
                ),
                Some(SessionDialog::ApiKey { provider, editor }) => (
                    "Set API key",
                    format!("{provider} key is written to the auth file in the config directory"),
                    Some(composer::render(editor, &theme)),
                ),
                Some(SessionDialog::Number { editor, .. }) => (
                    "Edit setting",
                    String::new(),
                    Some(composer::render(editor, &theme)),
                ),
                Some(SessionDialog::Delete { title, .. }) => (
                    "Delete session",
                    format!("Delete \"{title}\"? This cannot be undone."),
                    None,
                ),
                None => ("", String::new(), None),
            };
            let ime = self.ime_layer(cx);
            let danger = matches!(self.dialog, Some(SessionDialog::Delete { .. }));
            let mut card = div()
                .relative()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_, _, _, cx| cx.stop_propagation()),
                )
                .w(px(380.))
                .rounded(px(12.))
                .bg(theme.composer)
                .border_1()
                .border_color(theme.border_strong)
                .p(px(16.))
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(SharedString::from(title)),
                );
            if let Some(field) = field {
                card = card.child(
                    div()
                        .w_full()
                        .rounded(px(8.))
                        .bg(theme.canvas)
                        .border_1()
                        .border_color(theme.border)
                        .px(px(10.))
                        .py(px(8.))
                        .child(field),
                );
            } else {
                card = card.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.text_secondary)
                        .child(SharedString::from(body)),
                );
            }
            card = card
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(theme.text_ghost)
                        .child(SharedString::from(if danger {
                            "Enter to delete · Esc cancel"
                        } else {
                            "Enter confirm · Esc cancel"
                        })),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_end()
                        .gap(px(8.))
                        .child(
                            div()
                                .id("dialog-cancel")
                                .h(px(26.))
                                .px(px(12.))
                                .rounded(px(7.))
                                .flex()
                                .items_center()
                                .text_size(px(11.5))
                                .bg(theme.overlay)
                                .text_color(theme.text_secondary)
                                .cursor_pointer()
                                .hover(|style| style.bg(theme.overlay_strong))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.dialog = None;
                                    cx.notify();
                                }))
                                .child(SharedString::from("Cancel")),
                        )
                        .child(
                            div()
                                .id("dialog-confirm")
                                .h(px(26.))
                                .px(px(12.))
                                .rounded(px(7.))
                                .flex()
                                .items_center()
                                .text_size(px(11.5))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .bg(theme.inverse)
                                .text_color(theme.on_inverse)
                                .cursor_pointer()
                                .hover(|style| style.opacity(0.86))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.confirm_dialog(cx);
                                }))
                                .child(SharedString::from(if danger {
                                    "Delete"
                                } else {
                                    "Confirm"
                                })),
                        ),
                )
                .child(ime);
            root = root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(gpui::rgba(0x00000066))
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.dialog = None;
                            cx.notify();
                        }),
                    )
                    .child(card),
            );
        }

        root
    }
}

impl GuiApp {
    /// IME 写入的目标：覆盖层列表打开时是它的过滤框，否则是输入卡片。
    fn ime_target(&self) -> &Editor {
        if let Some(
            SessionDialog::Rename { editor, .. }
            | SessionDialog::ApiKey { editor, .. }
            | SessionDialog::Import { editor }
            | SessionDialog::Label { editor }
            | SessionDialog::BugHint { editor }
            | SessionDialog::Number { editor, .. },
        ) = &self.dialog
        {
            return editor;
        }
        if let Some(suggest) = &self.suggest
            && suggest.trigger != Trigger::Shell
        {
            return &self.suggest_input;
        }
        match &self.overlay {
            Some(Overlay::List(list)) => &list.query,
            _ => &self.editor,
        }
    }

    /// [`Self::ime_target`] 的可变版：写过滤框时把选中项收回第一行。
    fn ime_target_mut(&mut self) -> &mut Editor {
        if let Some(
            SessionDialog::Rename { editor, .. }
            | SessionDialog::ApiKey { editor, .. }
            | SessionDialog::Import { editor }
            | SessionDialog::Label { editor }
            | SessionDialog::BugHint { editor }
            | SessionDialog::Number { editor, .. },
        ) = &mut self.dialog
        {
            return editor;
        }
        if let Some(suggest) = &self.suggest
            && suggest.trigger != Trigger::Shell
        {
            return &mut self.suggest_input;
        }
        match self.overlay.as_mut() {
            Some(Overlay::List(list)) => {
                list.selected = 0;
                &mut list.query
            }
            _ => &mut self.editor,
        }
    }

    /// IME 报上来的 UTF-16 区间 → 当前目标的字节区间（没给区间就用手上的组字区间/选区）。
    fn ime_range_bytes(&self, range: Option<Range<usize>>) -> Range<usize> {
        let target = self.ime_target();
        let text = target.text();
        let utf16 = range
            .or_else(|| self.marked_range.clone())
            .unwrap_or_else(|| target.selection_utf16().0);
        text_edit::utf16_range_to_bytes(&text, utf16)
    }
}

impl EntityInputHandler for GuiApp {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.ime_target().text();
        let bytes = text_edit::utf16_range_to_bytes(&text, range.clone());
        let from = text_edit::byte_to_utf16(&text, bytes.start);
        let to = text_edit::byte_to_utf16(&text, bytes.end);
        *adjusted_range = Some(from..to);
        Some(text[bytes].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let (range, reversed) = self.ime_target().selection_utf16();
        Some(UTF16Selection { range, reversed })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range.clone()
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 行首的 `/` 只是命令面板的触发符，不进输入框
        if text == "/" && self.suggest.is_none() && self.dialog.is_none() && self.overlay.is_none()
        {
            let before = self.editor.text_before_cursor();
            let probe = format!("{before}/");
            if suggest::detect(&probe).is_some_and(|(trigger, _, _)| trigger == Trigger::Command) {
                self.suggest_input = Editor::new();
                let start = self.editor.cursor_byte();
                self.rebuild_suggest(Trigger::Command, start, String::new(), cx);
                cx.notify();
                return;
            }
        }
        let bytes = self.ime_range_bytes(range);
        self.ime_target_mut().replace_byte_range(bytes, text);
        self.marked_range = None;
        self.refresh_suggest_any(cx);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 组字阶段（拼音/假名）不写进输入框：只记住插入点，等提交时 `replace_text_in_range`
        // 才落到文本里。否则输入框会显示一串「nihao」。
        if new_text.is_empty() {
            self.marked_range = None;
        } else if self.marked_range.is_none() {
            let cursor = self.ime_target().selection_utf16().0.start;
            self.marked_range = Some(cursor..cursor);
        }
        let _ = range;
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // 不做逐字定位：候选窗贴着输入卡片左下角即可
        Some(element_bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

/// 滑块高度：可视高在内容高里的占比 × 可视高，不低于 [`SCROLLBAR_MIN_THUMB`]。
fn thumb_height(viewport: f32, max: f32) -> f32 {
    let min_thumb = f32::from(px(SCROLLBAR_MIN_THUMB));
    (viewport * viewport / (viewport + max)).max(min_thumb)
}

/// 滑块能移动的像素长度（轨道高减滑块高）。
fn thumb_travel(viewport: f32, max: f32) -> f32 {
    (viewport - thumb_height(viewport, max)).max(1.)
}

/// 滑块顶端位置：`offset` 取 `[-max, 0]`。
fn thumb_top(offset: f32, max: f32, travel: f32) -> f32 {
    (-offset / max).clamp(0., 1.) * travel
}

/// 给输入卡片的外层挂位置实测：补全面板要贴着它上沿弹出来。
fn measured_composer(wrapper: gpui::Div, cell: Rc<Cell<Option<Bounds<Pixels>>>>) -> gpui::Div {
    wrapper.on_children_prepainted(move |bounds: Vec<Bounds<Pixels>>, window, _| {
        if let Some(card) = bounds.first()
            && cell.get() != Some(*card)
        {
            cell.set(Some(*card));
            window.request_animation_frame();
        }
    })
}

/// 跑一条 shell 命令并把输出（已截断）当文本返回；GUI 里 `!` `!!` 用。
fn run_shell(command: &str) -> String {
    crate::modes::interactive::handlers::tools::run_shell_command(command)
}

/// 重命名输入 → 最终名字：去掉首尾空白，空输入当作取消。
fn renamed_name(input: &str) -> Option<&str> {
    let name = input.trim();
    (!name.is_empty()).then_some(name)
}

/// 把会话名写进会话文件（`name` fact），失败时静默——侧栏下次加载自然反映结果。
fn rename_session(path: &str, name: &str) {
    if let Ok(mut session) = crate::core::session_manager::Session::open(path) {
        session.append_session_info(name);
    }
}

/// 滚动条显隐：正在拖它，或鼠标正悬停在它所在的滚动区。
fn scrollbar_visible(drag: Option<ScrollArea>, area: ScrollArea, hovered: bool) -> bool {
    drag == Some(area) || hovered
}

/// 拖动换算：抓取点位移 `dy`（鼠标向下为正）→ 新滚动偏移，保证滑块跟着鼠标走。
fn drag_offset(start: f32, dy: f32, max: f32, travel: f32) -> f32 {
    start - dy * max / travel
}

/// 鼠标窗口坐标 → 文本元素内的局部坐标（列定位用）。
///
/// 必须用文字元素自己的原点：外层行盒含 `padding-left` 与列表缩进，
/// 拿它的左边界换算，点选落点会整体偏右。
fn text_local(position: gpui::Point<Pixels>, text: Bounds<Pixels>) -> gpui::Point<Pixels> {
    gpui::point(position.x - text.left(), position.y - text.top())
}

/// 路径最后一段（项目名）。
fn basename(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(path)
        .to_string()
}

#[cfg(test)]
mod tests {
    /// 模型循环按 scope 走：顺序跟 scope，不可用的跳过；scope 为空才用全部模型。
    #[test]
    fn cycle_models_follows_scope_order() {
        use crate::core::model_scope::ScopedModel;
        let all = vec![
            ("p1".to_string(), "a".to_string()),
            ("p1".to_string(), "b".to_string()),
            ("p2".to_string(), "c".to_string()),
        ];
        let scoped = vec![
            ScopedModel {
                provider: "p2".into(),
                id: "c".into(),
                thinking: Some("high".into()),
            },
            ScopedModel {
                provider: "p1".into(),
                id: "a".into(),
                thinking: None,
            },
            ScopedModel {
                provider: "p1".into(),
                id: "missing".into(),
                thinking: None,
            },
        ];
        assert_eq!(
            cycle_models(&scoped, &all),
            vec![
                ("p2".to_string(), "c".to_string()),
                ("p1".to_string(), "a".to_string()),
            ]
        );
        assert_eq!(cycle_models(&[], &all), all);
    }

    /// 循环下标：尾接到头、头回接到尾，未知时取第一个。
    #[test]
    fn cycle_index_wraps_both_ways() {
        assert_eq!(cycle_index(Some(2), 3, 1), 0);
        assert_eq!(cycle_index(Some(0), 3, -1), 2);
        assert_eq!(cycle_index(None, 3, 1), 0);
        assert_eq!(cycle_index(None, 3, -1), 0);
    }

    /// 菜单高度超窗口时也要收进来，别顶出底部。
    #[test]
    fn menu_top_stays_inside_the_window() {
        assert_eq!(clamp_menu_top(100., 300., 1150.), 100.);
        assert_eq!(clamp_menu_top(1000., 300., 1150.), 1150. - 300. - 12.);
        assert_eq!(clamp_menu_top(0., 300., 200.), 12.);
    }

    /// 右侧按钮的菜单要往左让，不能顶出窗口；窄窗口下也不越过左边距。
    #[test]
    fn menu_left_stays_inside_the_window() {
        // 按钮在右边：让到只剩右边距
        assert_eq!(clamp_menu_left(1800., 280., 1940.), 1940. - 280. - 12.);
        // 正常位置：贴着按钮
        assert_eq!(clamp_menu_left(300., 280., 1940.), 300.);
        // 窗口比菜单还窄：左边界兜底
        assert_eq!(clamp_menu_left(10., 280., 200.), 12.);
    }

    /// 列定位的原点是文字盒自己的左上角，缩进行的 padding 不参与换算。
    #[test]
    fn text_local_is_relative_to_the_text_box() {
        // 外层行盒从 x=100 起，缩进 14 后文字盒从 x=114 起
        let text = Bounds::new(
            gpui::point(px(114.), px(40.)),
            gpui::size(px(300.), px(18.)),
        );
        let local = text_local(gpui::point(px(114.), px(40.)), text);
        assert_eq!(local.x, px(0.));
        assert_eq!(local.y, px(0.));
        let local = text_local(gpui::point(px(214.), px(49.)), text);
        assert_eq!(local.x, px(100.));
        assert_eq!(local.y, px(9.));
    }

    /// 长按同一行：短时间内重复的切换要被忽略，换一行或过窗口期才生效。
    #[test]
    fn repeat_toggle_is_debounced() {
        let now = std::time::Instant::now();
        let window = std::time::Duration::from_millis(250);
        assert!(ignore_repeat_toggle(
            Some(("alpha", now)),
            "alpha",
            now + std::time::Duration::from_millis(50),
            window,
        ));
        assert!(!ignore_repeat_toggle(
            Some(("alpha", now)),
            "alpha",
            now + std::time::Duration::from_millis(400),
            window,
        ));
        assert!(!ignore_repeat_toggle(
            Some(("alpha", now)),
            "beta",
            now + std::time::Duration::from_millis(10),
            window,
        ));
        assert!(!ignore_repeat_toggle(None, "alpha", now, window));
    }

    /// 技能开关：开 → `+名称`，关 → `-名称`，同名规则只留一条。
    #[test]
    fn skill_toggle_rewrites_exact_rule() {
        let rules = vec!["!old-*".to_string(), "-alpha".to_string()];
        let enabled = skill_rules_with(&rules, "alpha", true);
        assert!(enabled.contains(&"+alpha".to_string()));
        assert!(!enabled.contains(&"-alpha".to_string()));
        // 通配规则保留
        assert!(enabled.contains(&"!old-*".to_string()));

        let disabled = skill_rules_with(&enabled, "alpha", false);
        assert!(disabled.contains(&"-alpha".to_string()));
        assert!(!disabled.contains(&"+alpha".to_string()));
        assert_eq!(disabled.len(), 2);
    }

    /// 最近目录：去重提到最前，超过上限截断。
    #[test]
    fn remember_cwd_dedupes_and_caps() {
        let _guard = crate::test_support::AgentDirGuard::temp();
        for index in 0..(RECENT_CWD_LIMIT + 3) {
            remember_cwd(&format!("/work/{index}"));
        }
        remember_cwd("/work/5");
        let list = recent_cwds();
        assert_eq!(list.len(), RECENT_CWD_LIMIT);
        assert_eq!(list[0], "/work/5");
        assert_eq!(list.iter().filter(|item| *item == "/work/5").count(), 1);
    }

    /// 右侧面板：左边框往左拖变宽，宽度夹在上下限。
    #[test]
    fn resized_panel_width_grows_when_dragging_left() {
        assert_eq!(resized_panel_width(PANEL_DEFAULT_WIDTH, -40.), 360.);
        assert_eq!(
            resized_panel_width(PANEL_DEFAULT_WIDTH, 999.),
            PANEL_MIN_WIDTH
        );
        assert_eq!(
            resized_panel_width(PANEL_DEFAULT_WIDTH, -999.),
            PANEL_MAX_WIDTH
        );
    }

    /// toast 只留在最近 TOAST_MAX 条里，级别决定颜色（这里只测裁剪逻辑）。
    #[test]
    fn toast_queue_keeps_latest_only() {
        let mut toasts: Vec<u64> = Vec::new();
        for id in 0..(TOAST_MAX as u64 + 3) {
            toasts.push(id);
            while toasts.len() > TOAST_MAX {
                toasts.remove(0);
            }
        }
        assert_eq!(toasts.len(), TOAST_MAX);
        assert_eq!(*toasts.last().unwrap(), TOAST_MAX as u64 + 2);
    }

    /// 手动关掉的补全不会自动弹回；换了触发位置才重新生效。
    #[test]
    fn dismissed_suggest_stays_closed_until_trigger_moves() {
        assert!(!suggest_allowed(Some(0), 0));
        assert!(suggest_allowed(None, 0));
        assert!(suggest_allowed(Some(0), 3));
    }

    /// 拖侧栏：按位移加宽/收窄，越界夹到上下限。
    #[test]
    fn resized_width_clamps_to_limits() {
        assert_eq!(resized_width(SIDEBAR_DEFAULT_WIDTH, 40.), 292.);
        assert_eq!(
            resized_width(SIDEBAR_DEFAULT_WIDTH, -999.),
            SIDEBAR_MIN_WIDTH
        );
        assert_eq!(
            resized_width(SIDEBAR_DEFAULT_WIDTH, 999.),
            SIDEBAR_MAX_WIDTH
        );
    }

    use super::*;

    /// 拖满整条轨道 = 滚到另一端，且滑块位移与内容偏移严格成比例。
    #[test]
    fn drag_offset_follows_the_thumb() {
        let (viewport, max) = (1000., 9000.);
        let travel = thumb_travel(viewport, max);
        assert_eq!(thumb_height(viewport, max), 100.);

        // 抓在顶部往下拖满轨道 → 滚到底
        assert_eq!(drag_offset(0., travel, max, travel), -max);
        // 拖一半 → 滚一半
        assert_eq!(drag_offset(0., travel / 2., max, travel), -max / 2.);
        // 往上拖回一半 → 偏移跟着回来
        assert_eq!(drag_offset(-max, -travel / 2., max, travel), -max / 2.);
    }

    /// 内容越长滑块越短，但不会短过最小高度。
    #[test]
    fn thumb_height_shrinks_with_content() {
        assert_eq!(thumb_height(1000., 9000.), 100.);
        assert!(thumb_height(1000., 90_000.) < 100.);
        assert_eq!(
            thumb_height(1000., 90_000.),
            f32::from(px(SCROLLBAR_MIN_THUMB))
        );
    }

    /// 偏移区间线性映射到轨道区间。
    #[test]
    fn thumb_top_maps_offset_range() {
        let (max, travel) = (9000., 900.);
        assert_eq!(thumb_top(0., max, travel), 0.);
        assert_eq!(thumb_top(-max, max, travel), travel);
        assert_eq!(thumb_top(-max / 2., max, travel), travel / 2.);
        // 越界偏移不会把滑块甩出轨道
        assert_eq!(thumb_top(-max * 2., max, travel), travel);
        assert_eq!(thumb_top(max, max, travel), 0.);
    }

    /// 重命名输入：首尾空白去掉，全空白等于取消。
    #[test]
    fn renamed_name_trims_and_rejects_blank() {
        assert_eq!(renamed_name("  新名字 "), Some("新名字"));
        assert_eq!(renamed_name("\t\n"), None);
        assert_eq!(renamed_name(""), None);
    }

    /// 重命名把名字写进会话文件，再读回来就是新名字。
    #[test]
    fn rename_writes_session_name() {
        use crate::core::session_manager::{Session, session_meta};

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        let mut session =
            Session::create(tmp.path().to_str().unwrap(), Some(dir.clone()), true).unwrap();
        session.append_message(&AgentMessage::user_text("hi"));
        let path = session.get_session_file().unwrap().to_path_buf();

        rename_session(path.to_str().unwrap(), "重构侧栏");

        assert_eq!(
            session_meta(&path).unwrap().name.as_deref(),
            Some("重构侧栏")
        );
    }

    /// 悬停任意一处都显示；拖动时即使鼠标离开滚动区也保持显示。
    #[test]
    fn scrollbar_shows_on_hover_or_drag() {
        assert!(!scrollbar_visible(None, ScrollArea::Transcript, false));
        assert!(scrollbar_visible(None, ScrollArea::Transcript, true));
        assert!(scrollbar_visible(
            Some(ScrollArea::Transcript),
            ScrollArea::Transcript,
            false
        ));
        // 拖对话流时侧栏滚动条不应跟着冒出来
        assert!(!scrollbar_visible(
            Some(ScrollArea::Transcript),
            ScrollArea::Sidebar,
            false
        ));
    }
}
