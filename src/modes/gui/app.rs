//! GUI 根视图：侧栏 + 对话流 + 输入卡片 + 面板。
//!
//! 与 agent 的一切交互都经 [`CommandTx`] 下发、经 [`GuiEvent`] 上行，视图不持有 agent。
//! 状态与装配留在本模块，其余按职责拆开：
//!
//! - [`icon`]：代码绘制的图标（停止方块等）；
//! - [`render`]：渲染，`render()` 只负责把各段拼起来；
//! - [`actions`]：动作与斜杠命令；
//! - [`keys`]：键盘分发；
//! - [`input`]：光标、选区与 IME；
//! - [`menus`]：下拉菜单、会话菜单与拖拽；
//! - [`panels`]：右侧面板与列表选择页；
//! - [`suggest`]：输入补全面板；
//! - [`ui`]：扩展 UI 请求（选择弹框 / 扩展覆盖层 / 通知）；
//! - [`status`]：状态栏文案与驱动它的计时/速率采样。

mod actions;
mod icon;
mod input;
mod keys;
mod menus;
mod panels;
mod render;
mod status;
mod suggest;
mod ui;

#[cfg(test)]
mod tests;

use super::{
    bridge::{GuiEvent, Snapshot},
    components::chip,
    composer,
    giff::{self, PanelTab, RightPanel},
    overlay::{
        self, Action, Column, DetailSource, Entry, Info, InfoKind, InfoRow, List, ListKind,
        Overlay, SettingKey, TABLE_MODES_W, TABLE_NAME_W, TABLE_PROVIDER_W, TABLE_SETTING_W,
        TABLE_SOURCE_W, TABLE_VALUE_W,
    },
    sidebar::{self, SessionRow},
    suggest::{self as suggest_engine, Suggest, Trigger},
    text_edit::{self, Editor},
    theme::{
        self, COMPOSER_H_PADDING, CONTENT_MAX_WIDTH, DROPDOWN_MAX_HEIGHT, DROPDOWN_ROW_HEIGHT,
        FLOW_PADDING_BOTTOM, FLOW_PADDING_TOP, MENU_EDGE_MARGIN, PANEL_DEFAULT_WIDTH,
        PANEL_MAX_WIDTH, PANEL_MIN_WIDTH, SCROLLBAR_HIT_WIDTH, SCROLLBAR_WIDTH,
        SIDEBAR_DEFAULT_WIDTH, SIDEBAR_HANDLE_WIDTH, SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH,
        SUGGEST_CHROME_HEIGHT, SUGGEST_GAP, SUGGEST_ROW_HEIGHT, SUGGEST_ROWS, TOAST_DURATION,
        TOAST_MAX, TRANSCRIPT_LINE_HEIGHT, Theme, min_thumb, px,
    },
    transcript::{self, Note, Transcript},
};
use crate::{
    cli::session_picker::ProgressiveMetas,
    core::{
        extensions::{self, ExtensionMode, OverlayKey, dispatch_agent_event, git_branch},
        model_scope::ScopedModel,
        provider::AgentMessage,
        session_manager::Session,
        settings_manager,
    },
    extensions::extra_themes,
    modes::interactive::{
        agent_actor::{AgentCommand, CommandTx},
        handlers::tools::run_shell_command,
    },
    utils::{
        geometry,
        paths::cwd,
        wheel::{WheelScrollAccelerator, WheelScrollLines},
    },
};
use gpui::{
    AnyElement, App, Bounds, Context, ElementInputHandler, EntityInputHandler, FocusHandle,
    IntoElement, KeyDownEvent, MouseButton, Pixels, Render, ScrollHandle, SharedString, Task,
    UTF16Selection, Window, canvas, div, prelude::*,
};
use status::UsageMeter;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{Duration, Instant},
};
use tokio::sync::mpsc::UnboundedReceiver;

/// 最近工作目录最多记多少条。
const RECENT_CWD_LIMIT: usize = 30;

/// 同一行两次切换要间隔多久才算两次操作（长按重建后会再弹一次 click）。
const TOGGLE_DEBOUNCE: Duration = Duration::from_millis(250);

/// Alt+滚轮加速倍数：按住 Alt 滚消息区时步长 ×5，用于快速跳过长回复。
const ALT_WHEEL_MULTIPLIER: f32 = 5.;

/// 可拖拽滚动条所在的滚动区。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ScrollArea {
    /// 对话流
    Transcript = 0,
    /// 侧栏会话列表
    Sidebar = 1,
    /// 覆盖层信息页（`/changelog` 这类，滚动句柄随覆盖层开关出现）
    Overlay = 2,
}

impl ScrollArea {
    /// 元素 id 用的编号（同一帧里两个滚动条不能撞 id）。
    fn index(self) -> usize {
        self as usize
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
    /// 菜单内容区的滚动句柄（键盘上下移动时跟着滚）
    handle: ScrollHandle,
    /// 键盘高亮的行（打开时从第一行算起）
    selected: usize,
}

/// 侧栏会话行的右键菜单。
struct SessionMenu {
    /// 会话文件路径
    path: String,
    /// 菜单锚点（右键位置，窗口坐标）
    position: gpui::Point<Pixels>,
    /// 键盘高亮项：0 = 重命名，1 = 删除
    selected: usize,
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
pub(super) struct ScrollDrag {
    /// 正在拖哪个滚动区
    pub(super) area: ScrollArea,
    /// 首次鼠标移动时的 y（与施加拖拽的位置同源，`None` = 还没定位）
    pub(super) anchor_y: Option<Pixels>,
    /// 按下时的滚动偏移
    pub(super) offset: Pixels,
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
    /// 运行中 steer 注入箱（与 agent 共享同一个队列）：忙碌时提交的文本排这里，
    /// run_loop 在回合内注入，不单独开一轮；未注入的条目显示在待发送区
    steer_inbox: Arc<Mutex<VecDeque<AgentMessage>>>,
    /// follow-up 队列：回合 settle 后才发送，同样显示在待发送区
    follow_ups: VecDeque<String>,
    /// 本轮用量计时与 token 速率（状态栏用）
    meter: UsageMeter,
    /// 忙碌期的重绘任务是否在跑（状态栏耗时走字与忙碌指示闪烁靠它）
    ticking: bool,
    /// 忙碌指示的闪烁相位：tick 每半秒翻转一次，乒乓出「正在忙」的脉冲
    blink: bool,
    /// 对话流滚动句柄
    scroll: ScrollHandle,
    /// 对话流滚轮的连滚加速（与 TUI 同源的速度换算）
    wheel_accel: WheelScrollAccelerator,
    /// 滚轮加速的时间基准（单调时钟，启动时固定）
    wheel_epoch: Instant,
    /// 侧栏会话列表滚动句柄
    sessions_scroll: ScrollHandle,
    /// 侧栏会话列表
    sessions: Vec<SessionRow>,
    /// 会话元数据的后台加载器（逐条回填；`None` 表示没在加载）
    sessions_loader: Option<ProgressiveMetas>,
    /// 侧栏是否还有会话元数据在读
    sessions_loading: bool,
    /// 侧栏顶部的会话搜索框内容（按标题过滤列表）
    session_search: Editor,
    /// 会话搜索框是否在吃键盘
    session_search_focus: bool,
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
    /// 是否显示思考块（Ctrl-T 切换，同 TUI 的 `app.thinking.toggle`）
    show_thinking: bool,
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
    /// 进行中的 `@` 补全扫描的中止开关（换查询时作废旧扫描）
    file_scan_cancel: Option<Arc<AtomicBool>>,
    /// 当前工作目录的 git 分支（非仓库为 None）
    branch: Option<String>,
    /// 上一次切换的条目与时间（面板重建后鼠标抬起会再触发一次 click，挡掉重复）
    last_toggle: Option<(String, Instant)>,
    /// 扩展面板的草稿：`(名称, 期望启用)`，None = 面板没开
    extensions_draft: Option<Vec<(String, bool)>>,
    /// `/skills` 面板的草稿规则（None = 面板没开，改动丢弃）
    skills_draft: Option<Vec<String>>,
    /// `/scoped-models` 面板的草稿启用名单（None = 面板没开，Ctrl-S 才落盘）
    scoped_draft: Option<Vec<String>>,
    /// 扩展命令写的状态栏临时提示（文案 + 过期时刻）；渲染时过期即忽略
    status_note: Option<(String, Instant)>,
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
    pub(super) scroll_drag: Option<ScrollDrag>,
    /// 鼠标是否在对话流上（滚动条只在悬停时显示）
    transcript_hovered: bool,
    /// 鼠标是否在侧栏列表上
    sidebar_hovered: bool,
    /// 会话行右键菜单
    session_menu: Option<SessionMenu>,
    /// 会话对话框（重命名 / 删除确认）
    dialog: Option<SessionDialog>,
    /// `-a/--approve` / `--no-approve` 的显式信任覆盖：Some 时启动不弹信任面板
    trust_override: Option<bool>,
    /// 停靠面板是否可见（`/dock` 切换或扩展 `ShowDock` 请求置位；无内容时不显示）
    dock_visible: bool,
    /// 停靠面板内部滚动（内容超过高度上限时）
    dock_scroll: ScrollHandle,
    /// 按下一个「刚把覆盖层输入行打开」的字符键时要吞掉的那个字符：
    /// 扩展收到 `Char(c)` 后才会把输入行露出来，而 IME 随后又会把同一个字符写进刚出现的输入框
    /// （表现为 note 输入框里凭空多出一个 `n`）。TUI 只有一条按键通路，没有这个问题。
    swallow_char: Option<char>,
}

impl GuiApp {
    /// 构造根视图：接上事件通道，并在有初始消息时立即开跑第一轮。
    pub fn new(
        theme: Theme,
        cmd_tx: CommandTx,
        event_rx: UnboundedReceiver<GuiEvent>,
        initial_messages: Vec<AgentMessage>,
        steer_inbox: Arc<Mutex<VecDeque<AgentMessage>>>,
        trust_override: Option<bool>,
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
            steer_inbox,
            follow_ups: VecDeque::new(),
            meter: UsageMeter::default(),
            ticking: false,
            blink: false,
            scroll: ScrollHandle::new(),
            wheel_accel: WheelScrollAccelerator::new(WheelScrollLines::Auto, true),
            wheel_epoch: Instant::now(),
            sessions_scroll: ScrollHandle::new(),
            sessions: Vec::new(),
            sessions_loader: None,
            sessions_loading: false,
            session_search: Editor::new(),
            session_search_focus: false,
            _session_poll: None,
            show_sidebar: true,
            overlay: None,
            panel: RightPanel::default(),
            expand_tools: false,
            show_thinking: true,
            marked_range: None,
            pane_width: Rc::new(Cell::new(px(0.))),
            // 初始就当「正在跟随」：空会话的第一批内容出现时也要滚到底，
            // 由 `pin_bottom` 在首帧后按实际内容底边纠正。
            pinned_bottom: Rc::new(Cell::new(Some(px(0.)))),
            flow_height: Rc::new(Cell::new(None)),
            suggest: None,
            file_scan_cancel: None,
            branch: None,
            last_toggle: None,
            extensions_draft: None,
            skills_draft: None,
            scoped_draft: None,
            status_note: None,
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
            trust_override,
            dock_visible: false,
            dock_scroll: ScrollHandle::new(),
            swallow_char: None,
        };
        app.start_request_poll(cx);
        app.maybe_ask_trust(cx);
        app.refresh_branch();

        // 当前目录也算一条「最近用过」，菜单里能直接看到
        prune_recent_cwds();
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
                // 扩展也要看到 agent 事件（TUI 在事件循环里逐条转发）。不转发的话，
                // 靠事件驱动的扩展——plan-mode 抽计划步骤、goal 结算、loop/hang 检测、
                // notify、subagent 状态——在桌面端等于不存在。
                if kind == "agent_settled" {
                    // notify 用这个字段区分「真正空闲」与「还有排队输入」
                    let mut enriched = event.clone();
                    if let Some(obj) = enriched.as_object_mut() {
                        obj.insert(
                            "hasQueuedMessages".to_string(),
                            serde_json::Value::Bool(
                                !self.follow_ups.is_empty()
                                    || !self.steer_inbox.lock().unwrap().is_empty(),
                            ),
                        );
                    }
                    dispatch_agent_event(&enriched);
                } else {
                    dispatch_agent_event(&event);
                }

                match kind {
                    "agent_start" => {
                        self.busy = true;
                        self.start_status_tick(cx);
                    }
                    "agent_settled" => {
                        self.busy = false;
                        // 回合结束：取下一批排队消息（steer 先于 follow-up，
                        // 同类按 settings 的 mode 取整批或一条）继续跑，与 TUI 一致
                        if let Some(batch) = self.drain_next_batch() {
                            _ = self.cmd_tx.send(AgentCommand::PromptBatch(batch));
                            self.busy = true;
                        }
                    }
                    _ => {}
                }
                let model = self.snapshot.as_ref().map(|s| s.model.clone());
                self.meter.observe(&event, model.as_deref());
                self.sample_token_rate();
                self.transcript.apply(&event, &self.cwd());
                self.follow_bottom();
            }
            GuiEvent::History(messages) => {
                self.transcript = Transcript::from_messages(&messages, &self.cwd());
                // 换会话后速率/耗时基线作废（累计用量可能小很多）
                self.meter = UsageMeter::default();
                // 上一份内容的实测高度对新内容无效，置空后按新内容重新量
                self.flow_height.set(None);
                // 换会话/重载历史总是回到最新内容，重新接管钉底
                self.pinned_bottom.set(Some(px(0.)));
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
                let previous = self.cwd();
                self.snapshot = Some(snapshot);

                let cwd = self.cwd();
                if cwd != previous {
                    self.reload_sessions(cx);
                    self.refresh_branch();
                    remember_cwd(&cwd);
                }
            }
        }
    }

    /// 让 transcript 跟随最新内容。
    fn scroll_to_bottom(&mut self) {
        self.scroll.scroll_to_bottom();
    }

    /// 对话流翻页：`dir` 为 -1 向上、1 向下，步长取滚动区高度（同 TUI 的 shift+pageUp/Down）。
    fn page_transcript(&mut self, dir: f32) {
        let viewport = f32::from(self.scroll.bounds().size.height);
        let max = f32::from(self.scroll.max_offset().height);
        if viewport <= 0. || max <= 0. {
            return;
        }
        let current = f32::from(self.scroll.offset().y);
        let next = (current + dir * viewport).clamp(-max, 0.);
        // 偏移是物理像素，这里不能再走 `px()`（那是「设计像素 × 缩放」）
        self.scroll
            .set_offset(gpui::point(gpui::px(0.), gpui::px(next)));
    }

    /// 滚轮滚消息区：步长与 TUI 同源（[`WheelScrollAccelerator::next`]：孤立一格 1 份，
    /// 快滚最多 10 份），按住 Alt 再 ×[`ALT_WHEEL_MULTIPLIER`]。返回是否已自行滚过。
    ///
    /// 加速系数乘在系统给的增量上（触控板的小增量仍然平滑），不用「一格 = 固定行数」去
    /// 覆盖它。返回 true 时调用方要吞掉事件——原生滚动在 bubble 阶段，否则会再滚一遍。
    fn wheel_transcript(&mut self, event: &gpui::ScrollWheelEvent, line_height: Pixels) -> bool {
        // shortcut: 只按对话区 bounds 判定，盖住它的面板/菜单不单独算，
        // 面板变多或与对话区大面积重叠时改成按各自 bounds 判定
        if self.overlay.is_some()
            || self.dialog.is_some()
            || self.session_menu.is_some()
            || self.dropdown.is_some()
        {
            return false;
        }

        let dy = f32::from(event.delta.pixel_delta(line_height).y);
        if dy == 0. {
            return false;
        }

        let direction = if dy < 0. { 1 } else { -1 };
        let now_ms = self.wheel_epoch.elapsed().as_millis() as u64;
        let mut factor = self.wheel_accel.next(direction, now_ms) as f32;
        if event.modifiers.alt {
            factor *= ALT_WHEEL_MULTIPLIER;
        }

        let max = f32::from(self.scroll.max_offset().height);
        let next = (f32::from(self.scroll.offset().y) + dy * factor).clamp(-max, 0.);
        // 偏移是物理像素，不能再走 `px()`（那是「设计像素 × 缩放」）
        self.scroll
            .set_offset(gpui::point(gpui::px(0.), gpui::px(next)));
        true
    }

    /// 新内容来了才跟随到底：用户滚上去看历史时不能每来一条事件就把他拽回底部。
    ///
    /// `pinned_bottom` 为 `None` 表示用户自己滚走了（见 `pin_bottom`），此时不抢。
    fn follow_bottom(&mut self) {
        if self.pinned_bottom.get().is_some() {
            self.scroll_to_bottom();
        }
    }

    /// 展开/收起后内容高度大变：清掉上一帧的实测高度（那会把本帧布局压在旧高度上，
    /// 下一帧再涨/退一次，贴底时就是一次抖动）。
    ///
    /// 这里**不**再 `scroll_to_bottom()`：那会给 `ScrollHandle` 挂一个基于旧内容高度的
    /// 滚动请求，和下一帧 `pin_bottom` 按新内容算的偏移抢同一个值，一来一回就是抖。
    fn resized_bottom(&mut self) {
        self.flow_height.set(None);
        if self.pinned_bottom.get().is_some() {
            // 记成当前偏移，钉底逻辑才认得出这是自己的地盘（见 `pin_bottom`）
            self.pinned_bottom.set(Some(self.scroll.offset().y));
        }
    }

    /// 点用户气泡：展开/收起过长的那条消息（`None` 时跟随全局 Ctrl-L 开关）。
    pub(super) fn toggle_user(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.repeat_toggle(&format!("user:{index}")) {
            cx.notify();
            return;
        }

        let expand_tools = self.expand_tools;
        if let Some(transcript::Block::User(message)) = self.transcript.blocks_mut().get_mut(index)
        {
            let expanded = message.expanded.unwrap_or(expand_tools);
            message.expanded = Some(!expanded);
            self.resized_bottom();
        }
        cx.notify();
    }

    /// 点工具卡片：展开/收起这一块的输出（`None` 时跟随全局 Ctrl-L 开关）。
    pub(super) fn toggle_tool(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.repeat_toggle(&format!("tool:{index}")) {
            cx.notify();
            return;
        }

        let expand_tools = self.expand_tools;
        if let Some(transcript::Block::Tool(tool)) = self.transcript.blocks_mut().get_mut(index) {
            let expanded = tool.expanded.unwrap_or(expand_tools);
            tool.expanded = Some(!expanded);
            self.resized_bottom();
        }
        cx.notify();
    }

    /// 同一条目在 [`TOGGLE_DEBOUNCE`] 内只接受一次切换，返回 true 表示这次要丢掉。
    ///
    /// 卡片展开/收起会改高度，鼠标抬起时指针还落在它上面，gpui 会再命中同一个元素、
    /// 又发一次 click，于是展开与收起反复切换（画面就是抖的）。
    fn repeat_toggle(&mut self, key: &str) -> bool {
        let now = Instant::now();
        if ignore_repeat_toggle(
            self.last_toggle
                .as_ref()
                .map(|(last, at)| (last.as_str(), *at)),
            key,
            now,
            TOGGLE_DEBOUNCE,
        ) {
            return true;
        }

        self.last_toggle = Some((key.to_string(), now));
        false
    }

    /// 取某个滚动区的滚动句柄；覆盖层没打开信息页时为 `None`。
    pub(super) fn scroll_of(&self, area: ScrollArea) -> Option<&ScrollHandle> {
        match area {
            ScrollArea::Transcript => Some(&self.scroll),
            ScrollArea::Sidebar => Some(&self.sessions_scroll),
            ScrollArea::Overlay => match self.overlay.as_ref()? {
                Overlay::Info(info) => Some(&info.scroll),
                Overlay::List(list) => list.detail.as_ref().map(|info| &info.scroll),
                Overlay::Extension(_) => None,
            },
        }
    }

    /// [`Self::scroll_of`] 的可变版。
    fn scroll_of_mut(&mut self, area: ScrollArea) -> Option<&mut ScrollHandle> {
        match area {
            ScrollArea::Transcript => Some(&mut self.scroll),
            ScrollArea::Sidebar => Some(&mut self.sessions_scroll),
            ScrollArea::Overlay => match self.overlay.as_mut()? {
                Overlay::Info(info) => Some(&mut info.scroll),
                Overlay::List(list) => list.detail.as_mut().map(|info| &mut info.scroll),
                Overlay::Extension(_) => None,
            },
        }
    }

    /// 滚动条是否该出现：鼠标悬停在该区上，或正在拖它。
    fn scrollbar_visible(&self, area: ScrollArea) -> bool {
        let hovered = match area {
            ScrollArea::Transcript => self.transcript_hovered,
            ScrollArea::Sidebar => self.sidebar_hovered,
            ScrollArea::Overlay => false,
        };
        scrollbar_visible(self.scroll_drag.map(|drag| drag.area), area, hovered)
    }

    /// 当前工作目录（快照优先，回落到进程 cwd）。
    fn cwd(&self) -> String {
        self.snapshot
            .as_ref()
            .map(|s| s.cwd.clone())
            .unwrap_or_else(|| cwd().to_string_lossy().to_string())
    }

    /// 切换明暗主题并写回 `theme-gui`，下次启动恢复。
    fn set_dark(&mut self, is_dark: bool) {
        self.theme = Theme::new(is_dark);
        _ = settings_manager::write_settings_theme_gui(if is_dark { "dark" } else { "light" });
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

    /// 读当前工作目录的 git 分支（侧栏底部展示）。
    fn refresh_branch(&mut self) {
        self.branch = git_branch(&self.cwd());
    }

    /// 简单重绘（`confirm_dialog` 里提前返回时用）。
    fn cx_notify(&self, cx: &mut Context<Self>) {
        cx.notify();
    }
}

/// 待发送消息数（steer 注入箱 + follow-up 队列）。
fn pending_count(inbox: &Mutex<VecDeque<AgentMessage>>, follow_ups: &VecDeque<String>) -> usize {
    let steers = inbox.lock().map(|inbox| inbox.len()).unwrap_or(0);
    steers + follow_ups.len()
}

/// 待发送区行：steer 在前（标 `Steering:`），follow-up 在后（标 `Follow-up:`），
/// 每条只取首行（行数不随内容行数变）。
fn pending_lines(
    inbox: &Mutex<VecDeque<AgentMessage>>,
    follow_ups: &VecDeque<String>,
) -> Vec<String> {
    let first = |text: String| text.split('\n').next().unwrap_or("").to_string();
    let mut lines: Vec<String> = inbox
        .lock()
        .map(|inbox| {
            inbox
                .iter()
                .map(|message| format!("Steering: {}", first(message.text())))
                .collect()
        })
        .unwrap_or_default();
    lines.extend(
        follow_ups
            .iter()
            .map(|text| format!("Follow-up: {}", first(text.clone()))),
    );
    lines
}

/// settle 后取下一批待发送消息：steer（回合内注入箱）优先于 follow-up（本地队列），
/// 同类内部 FIFO，按 settings 的 mode all=整批 / one-at-a-time=一条。
/// `None` 表示两个队列都已空（闸门已锁时也返回 None）。
fn take_next_batch(
    inbox: &Mutex<VecDeque<AgentMessage>>,
    follow_ups: &mut VecDeque<String>,
) -> Option<Vec<AgentMessage>> {
    let steer_mode = settings_manager::read_settings_steering_mode();
    let follow_up_mode = settings_manager::read_settings_follow_up_mode();

    let steer_batch: Vec<AgentMessage> = {
        let mut inbox = inbox.lock().ok()?;
        if steer_mode == "all" {
            inbox.drain(..).collect()
        } else {
            inbox.pop_front().into_iter().collect()
        }
    };

    if !steer_batch.is_empty() {
        return Some(steer_batch);
    }

    let mut batch = Vec::new();
    while let Some(text) = follow_ups.pop_front() {
        batch.push(AgentMessage::user_text(&text));
        if follow_up_mode != "all" {
            break;
        }
    }

    (!batch.is_empty()).then_some(batch)
}

/// 取回排队的消息（Alt+Up）：steer 在前、follow-up 在后，空行分隔并拼在草稿 `draft` 之前。
/// 返回合并后的编辑器文本与取回的条数；两个队列都空时返回 None（不动编辑器）。
fn restore_text(
    inbox: &Mutex<VecDeque<AgentMessage>>,
    follow_ups: &mut VecDeque<String>,
    draft: &str,
) -> Option<(String, usize)> {
    let mut texts: Vec<String> = inbox
        .lock()
        .map(|mut inbox| inbox.drain(..).map(|message| message.text()).collect())
        .unwrap_or_default();
    texts.extend(follow_ups.drain(..));

    if texts.is_empty() {
        return None;
    }

    let count = texts.len();
    let mut combined = texts.join("\n\n");
    if !draft.trim().is_empty() {
        combined.push_str("\n\n");
        combined.push_str(draft);
    }

    Some((combined, count))
}

/// 最近工作目录清单文件（放在 agent 目录，不动用户的 settings.json）。
fn recent_cwds_path() -> PathBuf {
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
    write_recent_cwds(&list);
}

/// 写回最近目录清单（清单目录不存在时先建）。
fn write_recent_cwds(list: &[String]) {
    if let Some(dir) = recent_cwds_path().parent() {
        _ = std::fs::create_dir_all(dir);
    }

    if let Ok(text) = serde_json::to_string_pretty(list) {
        _ = std::fs::write(recent_cwds_path(), text);
    }
}

/// 启动时清清单：已经不存在的目录移除并写回，免得菜单里留下点不开的条目。
fn prune_recent_cwds() {
    let list = recent_cwds();
    let live: Vec<String> = list
        .iter()
        .filter(|path| Path::new(path).is_dir())
        .cloned()
        .collect();

    if live.len() != list.len() {
        write_recent_cwds(&live);
    }
}

/// 勾选/切换类动作会重建同一个面板：执行前别关覆盖层，否则过滤与选中行都丢了。
fn keeps_overlay(action: &Action) -> bool {
    matches!(
        action,
        Action::ToggleSkill { .. }
            | Action::ToggleExtension(_)
            | Action::ToggleSetting(_)
            | Action::ToggleScopedModel(_)
            | Action::CycleExtensionSetting { .. }
    )
}

/// GUI 能配置的扩展：底栏与横幅扩展只影响终端界面渲染，桌面端不列出来（也没法生效）；
/// `extra-themes` 是纯资源同步（把内嵌主题写进 `agent_dir/themes`），也没有可配置项。
fn configurable_extensions() -> Vec<(String, bool, Vec<ExtensionMode>)> {
    let hidden: Vec<String> = extensions::footer_names()
        .into_iter()
        .chain(extensions::banner_names())
        .chain([extra_themes::EXT.to_string()])
        .collect();

    extensions::panel_entries()
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

/// 该不该忽略这次切换：同一条目、且距上次不到 `window` 就丢弃。
fn ignore_repeat_toggle(
    last: Option<(&str, Instant)>,
    name: &str,
    now: Instant,
    window: Duration,
) -> bool {
    last.is_some_and(|(last_name, at)| last_name == name && now.duration_since(at) < window)
}

/// 补全面板该不该自动弹出：手动关掉（Esc / 退格删空）过的同一次触发不再弹回。
fn suggest_allowed(dismissed: Option<usize>, start: usize) -> bool {
    dismissed != Some(start)
}

/// 重命名输入 → 最终名字：去掉首尾空白，空输入当作取消。
fn renamed_name(input: &str) -> Option<&str> {
    let name = input.trim();
    (!name.is_empty()).then_some(name)
}

/// 把会话名写进会话文件（`name` fact），失败时静默——侧栏下次加载自然反映结果。
fn rename_session(path: &str, name: &str) {
    if let Ok(mut session) = Session::open(path) {
        session.append_session_info(name);
    }
}

/// 滚动条显隐：正在拖它，或鼠标正悬停在它所在的滚动区。
fn scrollbar_visible(drag: Option<ScrollArea>, area: ScrollArea, hovered: bool) -> bool {
    drag == Some(area) || hovered
}

/// 鼠标窗口坐标 → 文本元素内的局部坐标（列定位用）。
///
/// 必须用文字元素自己的原点：外层行盒含 `padding-left` 与列表缩进，
/// 拿它的左边界换算，点选落点会整体偏右。
fn text_local(position: gpui::Point<Pixels>, text: Bounds<Pixels>) -> gpui::Point<Pixels> {
    gpui::point(position.x - text.left(), position.y - text.top())
}

/// 循环取下一个下标：`dir` 为 ±1，`current` 未知（还没对上号）时取第一个。
///
/// `len` 必须非空：调用方都先判过空列表。
fn cycle_index(current: Option<usize>, len: usize, dir: i32) -> usize {
    let Some(current) = current else {
        return 0;
    };
    ((current as i32 + dir).rem_euclid(len as i32)) as usize
}

/// `Ctrl-P` 循环的候选模型：`scoped` 非空时严格按 scope 顺序、跳过目录里不可用的；
/// 为空时才退回全部已配置模型（与 TUI 的模型循环一致）。
fn cycle_models(scoped: &[ScopedModel], all: &[(String, String)]) -> Vec<(String, String)> {
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
