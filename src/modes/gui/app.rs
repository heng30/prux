//! GUI 根视图：自绘标题栏 + 侧栏 + 对话流 + 输入卡片 + 面板。
//!
//! 本文件只做「状态 + 渲染 + 按键分发」，与 agent 的一切交互都经 [`CommandTx`] 下发、
//! 经 [`GuiEvent`] 上行，视图不持有 agent。

use super::bridge::{GuiEvent, Snapshot};
use super::components::{chip, dot};
use super::composer;
use super::overlay::{self, Action, Entry, Info, InfoKind, List, ListKind, Overlay};
use super::panel::{self, PanelTab, RightPanel};
use super::sidebar::{self, SessionRow};
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
    UTF16Selection, Window, canvas, div, point, prelude::*,
};
use std::cell::Cell;
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
/// 滚动条滑块最小高度。
const SCROLLBAR_MIN_THUMB: f32 = 24.0;

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
    /// 对话流实测高度（上一帧 prepaint 量到的，用来给滚动内容定高）
    content_height: Rc<Cell<Pixels>>,
    /// 上次钉底用的偏移（内容高度变了才重钉，用户自己滚开就不打扰）
    pinned_bottom: Rc<Cell<Option<Pixels>>>,
    /// 拖动滚动条时的抓取点（按下时的鼠标 y 与滚动偏移）
    scroll_drag: Option<(Pixels, Pixels)>,
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
            sessions: Vec::new(),
            sessions_loader: None,
            sessions_loading: false,
            _session_poll: None,
            show_sidebar: true,
            overlay: None,
            panel: RightPanel::default(),
            expand_tools: false,
            marked_range: None,
            content_height: Rc::new(Cell::new(px(0.))),
            pinned_bottom: Rc::new(Cell::new(None)),
            scroll_drag: None,
        };
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
                self.scroll_to_bottom();
            }
            GuiEvent::History(messages) => {
                self.transcript = Transcript::from_messages(&messages);
                self.scroll_to_bottom();
            }
            GuiEvent::Error(message) => {
                self.busy = false;
                self.transcript.push_note(Note::Error(message));
                self.scroll_to_bottom();
            }
            GuiEvent::Snapshot(snapshot) => {
                let cwd_changed = self
                    .snapshot
                    .as_ref()
                    .is_some_and(|old| old.cwd != snapshot.cwd);
                if cwd_changed {
                    self.reload_sessions(cx);
                }
                self.snapshot = Some(snapshot);
            }
        }
    }

    /// 让 transcript 跟随最新内容。
    fn scroll_to_bottom(&mut self) {
        self.scroll.scroll_to_bottom();
    }

    /// 对话流滚动条：滑块位置按上一帧的滚动状态算，拖动时按鼠标位移改偏移。
    fn scrollbar(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let viewport = f32::from(self.scroll.bounds().size.height);
        let max = f32::from(self.scroll.max_offset().height);
        if max <= 0. || viewport <= 0. {
            return div().into_any_element();
        }
        let offset = f32::from(self.scroll.offset().y);
        let min_thumb = f32::from(px(SCROLLBAR_MIN_THUMB));
        let thumb_height = (viewport * viewport / (viewport + max)).max(min_thumb);
        let thumb_top = (-offset / max).clamp(0., 1.) * (viewport - thumb_height);
        div()
            .id("transcript-scrollbar")
            .absolute()
            .top_0()
            .right_0()
            .h_full()
            .w(px(SCROLLBAR_HIT_WIDTH))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    this.scroll_drag = Some((event.position.y, this.scroll.offset().y));
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if let Some((press_y, press_offset)) = this.scroll_drag {
                    this.scroll
                        .set_offset(point(px(0.), press_offset + (press_y - event.position.y)));
                    cx.notify();
                }
            }))
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
                    .top(gpui::px(thumb_top))
                    .right(px(1.))
                    .w(px(SCROLLBAR_WIDTH))
                    .h(gpui::px(thumb_height))
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

        if self.overlay.is_some() {
            self.handle_overlay_key(&keystroke.key, shift, cx);
            cx.notify();
            return;
        }

        // 面板快捷键
        if primary {
            match keystroke.key.as_str() {
                "k" | "p" => {
                    self.open_command_palette(cx);
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
                "u" => {
                    self.open_info(InfoKind::Usage, cx);
                    return;
                }
                "i" => {
                    self.open_info(InfoKind::Skills, cx);
                    return;
                }
                "," => {
                    self.open_info(InfoKind::Settings, cx);
                    return;
                }
                "b" => {
                    self.show_sidebar = !self.show_sidebar;
                    cx.notify();
                    return;
                }
                "e" => {
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
                "a" => {
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
                "c" => {
                    let text = self.editor.selected_text();
                    if !text.is_empty() {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                    }
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
                self.submit();
                cx.notify();
            }
            "escape" => {
                if self.busy {
                    _ = self.cmd_tx.send(AgentCommand::Abort);
                    self.queued = 0;
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

    /// 点击覆盖层列表的第 `visible` 行：取过滤后的条目并执行。
    pub(super) fn pick_entry(&mut self, visible: usize, cx: &mut Context<Self>) {
        let action = match &self.overlay {
            Some(Overlay::List(list)) => {
                let filtered = list.filtered();
                filtered
                    .get(visible)
                    .map(|index| clone_action(&list.entries[*index].action))
            }
            _ => None,
        };
        if let Some(action) = action {
            self.overlay = None;
            self.run_action(action, cx);
        }
        cx.notify();
    }

    /// 覆盖层按键。
    fn handle_overlay_key(&mut self, key: &str, _shift: bool, cx: &mut Context<Self>) {
        if key == "escape" {
            self.overlay = None;
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
                        self.overlay = None;
                        self.run_action(action, cx);
                    }
                }
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
            }
            Action::Info(kind) => self.open_info(kind, cx),
            Action::Open(kind) => match kind {
                ListKind::Model => self.open_model_picker(cx),
                ListKind::Session => self.open_session_picker(cx),
                ListKind::Thinking => self.open_thinking_picker(cx),
                ListKind::Appearance => self.open_appearance_picker(cx),
                ListKind::PanelChanges => self.toggle_panel(Some(PanelTab::Changes), cx),
                ListKind::PanelFiles => self.toggle_panel(Some(PanelTab::Files), cx),
            },
            Action::RefreshSessions => self.reload_sessions(cx),
            Action::Scale(scale) => {
                _ = settings_manager::write_settings_ui_scale(Some(theme::set_ui_scale(scale)));
            }
            Action::Quit => cx.quit(),
        }
        cx.notify();
    }

    /// 提交输入框内容。
    fn submit(&mut self) {
        let Some(text) = self.editor.trimmed() else {
            return;
        };
        self.editor.clear();

        // 回合中提交的输入由 worker 排队，这里同步计数给状态栏
        if self.busy {
            self.queued += 1;
        } else {
            self.busy = true;
        }
        self.transcript.push_user(text.clone());
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
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().to_string())
            })
            .unwrap_or_default()
    }

    /// 命令面板。
    fn open_command_palette(&mut self, _cx: &mut Context<Self>) {
        let entries = vec![
            Entry {
                label: "新建会话".into(),
                detail: "Ctrl-N".into(),
                action: Action::NewSession,
            },
            Entry {
                label: "切换会话…".into(),
                detail: "Ctrl-O".into(),
                action: Action::Open(ListKind::Session),
            },
            Entry {
                label: "选择模型…".into(),
                detail: "Ctrl-/".into(),
                action: Action::Open(ListKind::Model),
            },
            Entry {
                label: "设置思考级别…".into(),
                detail: "Ctrl-T".into(),
                action: Action::Open(ListKind::Thinking),
            },
            Entry {
                label: "切换外观…".into(),
                detail: "Ctrl-G".into(),
                action: Action::Open(ListKind::Appearance),
            },
            Entry {
                label: "放大界面".into(),
                detail: "Ctrl+=".into(),
                action: Action::Scale(theme::ui_scale() + theme::UI_SCALE_STEP),
            },
            Entry {
                label: "缩小界面".into(),
                detail: "Ctrl+-".into(),
                action: Action::Scale(theme::ui_scale() - theme::UI_SCALE_STEP),
            },
            Entry {
                label: "界面缩放回到 100%".into(),
                detail: "Ctrl+0".into(),
                action: Action::Scale(1.0),
            },
            Entry {
                label: "用量与费用".into(),
                detail: "Ctrl-U".into(),
                action: Action::Info(InfoKind::Usage),
            },
            Entry {
                label: "技能".into(),
                detail: "Ctrl-I".into(),
                action: Action::Info(InfoKind::Skills),
            },
            Entry {
                label: "设置".into(),
                detail: "Ctrl-,".into(),
                action: Action::Info(InfoKind::Settings),
            },
            Entry {
                label: "快捷键".into(),
                detail: "?".into(),
                action: Action::Info(InfoKind::Help),
            },
            Entry {
                label: "压缩上下文".into(),
                detail: "/compact".into(),
                action: Action::Command(Box::new(AgentCommand::Compact { instructions: None })),
            },
            Entry {
                label: "回退上一条输入".into(),
                detail: "/rewind".into(),
                action: Action::Command(Box::new(AgentCommand::Rewind)),
            },
            Entry {
                label: "工作区变更…".into(),
                detail: "Ctrl-E".into(),
                action: Action::Open(ListKind::PanelChanges),
            },
            Entry {
                label: "文件列表…".into(),
                detail: String::new(),
                action: Action::Open(ListKind::PanelFiles),
            },
            Entry {
                label: "刷新会话列表".into(),
                detail: String::new(),
                action: Action::RefreshSessions,
            },
            Entry {
                label: "退出".into(),
                detail: "Ctrl-Q".into(),
                action: Action::Quit,
            },
        ];
        let list = List::new("命令", entries);
        self.overlay = Some(Overlay::List(list));
    }

    /// 模型选择器。
    fn open_model_picker(&mut self, cx: &mut Context<Self>) {
        self.overlay = Some(Overlay::List(List::new(
            "模型",
            vec![Entry {
                label: "正在读取模型列表…".into(),
                detail: String::new(),
                action: Action::None,
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
                    })
                    .collect();
                if entries.is_empty() {
                    entries.push(Entry {
                        label: "没有已配置的模型".into(),
                        detail: "先运行 prux auth".into(),
                        action: Action::None,
                    });
                }
                app.overlay = Some(Overlay::List(List::new("模型", entries)));
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
            })
            .collect();
        if entries.is_empty() {
            entries.push(Entry {
                label: "当前模型不支持思考级别".into(),
                detail: String::new(),
                action: Action::None,
            });
        }
        self.overlay = Some(Overlay::List(List::new("思考级别", entries)));
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
                detail: format!("{} · {} 条", row.age, row.messages),
                action: Action::LoadSession(row.path.clone()),
            })
            .collect();
        let mut entries = entries;
        if entries.is_empty() {
            entries.push(Entry {
                label: if self.sessions_loading {
                    "正在加载会话…".into()
                } else {
                    "还没有会话".to_string()
                },
                detail: String::new(),
                action: Action::None,
            });
        }
        self.overlay = Some(Overlay::List(List::new("会话", entries)));
    }

    /// 外观选择器（明/暗）。
    fn open_appearance_picker(&mut self, _cx: &mut Context<Self>) {
        let entries = vec![
            Entry {
                label: "暗色".into(),
                detail: if self.theme.is_dark { "当前" } else { "" }.into(),
                action: Action::SetDark(true),
            },
            Entry {
                label: "亮色".into(),
                detail: if self.theme.is_dark { "" } else { "当前" }.into(),
                action: Action::SetDark(false),
            },
        ];
        self.overlay = Some(Overlay::List(List::new("外观", entries)));
    }

    /// 信息页。
    fn open_info(&mut self, kind: InfoKind, _cx: &mut Context<Self>) {
        let info = match kind {
            InfoKind::Usage => {
                let usage = self.snapshot.as_ref().map(|s| &s.usage);
                let mut lines = Vec::new();
                if let Some(usage) = usage {
                    lines.push(format!("输入 token：{}", usage.input));
                    lines.push(format!("输出 token：{}", usage.output));
                    lines.push(format!("缓存读：{}", usage.cache_read));
                    lines.push(format!("缓存写：{}", usage.cache_write));
                    lines.push(format!("上下文占用：{}", usage.context_tokens));
                    lines.push(format!("费用合计：${:.4}", usage.cost));
                } else {
                    lines.push("尚无用量数据".to_string());
                }
                Info::new("用量", lines)
            }
            InfoKind::Skills => {
                let mut lines = Vec::new();
                if let Some(snapshot) = &self.snapshot {
                    for (name, description, source) in &snapshot.skills {
                        lines.push(format!("{name}  [{source}]"));
                        if !description.is_empty() {
                            lines.push(format!("    {description}"));
                        }
                    }
                }
                if lines.is_empty() {
                    lines.push("本会话没有可用技能".to_string());
                }
                Info::new("技能", lines)
            }
            InfoKind::Settings => {
                let settings = settings_manager::read_settings();
                let mut lines = vec![
                    format!("配置目录：{}", settings_manager::agent_dir().display()),
                    format!(
                        "外观：{}",
                        if self.theme.is_dark {
                            "暗色"
                        } else {
                            "亮色"
                        }
                    ),
                    format!(
                        "默认模型：{}",
                        settings
                            .default_model
                            .clone()
                            .unwrap_or_else(|| "（未设置）".into())
                    ),
                    format!(
                        "思考级别：{}",
                        settings
                            .default_thinking_level
                            .clone()
                            .unwrap_or_else(|| "（未设置）".into())
                    ),
                ];
                if let Some(snapshot) = &self.snapshot {
                    lines.push(format!(
                        "当前模型：{}/{}",
                        snapshot.provider, snapshot.model
                    ));
                    lines.push(format!(
                        "会话文件：{}",
                        snapshot.session_path.as_deref().unwrap_or("（内存会话）")
                    ));
                    lines.push(format!("激活工具：{}", snapshot.tools.join(", ")));
                }
                Info::new("设置", lines)
            }
            InfoKind::Help => Info::new(
                "快捷键",
                vec![
                    "Enter 发送 · Shift+Enter 换行".to_string(),
                    "Esc 中止当前回合".to_string(),
                    "Ctrl-K 命令面板 · Ctrl-/ 模型 · Ctrl-T 思考级别".to_string(),
                    "Ctrl-O 会话列表 · Ctrl-B 切换侧栏 · Ctrl-E 右侧面板".to_string(),
                    "Ctrl-U 用量 · Ctrl-I 技能 · Ctrl-, 设置".to_string(),
                    "Ctrl-N 新建会话 · Ctrl-Z 撤销 · Ctrl-V 粘贴".to_string(),
                    "Ctrl+= 放大界面 · Ctrl+- 缩小 · Ctrl+0 回到 100%".to_string(),
                    "Ctrl-Q 退出".to_string(),
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
                "● 生成中…（Esc 中止，排队 {}） · {messages} 条消息",
                self.queued
            );
        }
        format!(
            "{messages} 条消息 · 缩放 {}%",
            (theme::ui_scale() * 100.0).round()
        )
    }
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            .unwrap_or_else(|| "新会话".to_string());
        let model = self
            .snapshot
            .as_ref()
            .map(|s| format!("{}/{}", s.provider, s.model))
            .unwrap_or_else(|| "未选择模型".to_string());
        let thinking = self
            .snapshot
            .as_ref()
            .and_then(|s| s.thinking.clone())
            .filter(|t| t != "off");

        let mut chrome_right = div().flex().items_center().gap(px(6.));
        chrome_right = chrome_right.child(chip(&theme, model.clone(), false));
        if let Some(thinking) = thinking {
            chrome_right = chrome_right.child(chip(&theme, format!("思考 {thinking}"), false));
        }
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
                .child(SharedString::from("Ctrl-K 命令")),
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
        let empty = self.transcript.blocks().is_empty();
        let expand_tools = self.expand_tools;
        let mut flow = div().w_full().flex().flex_col().gap(px(14.));
        for block in self.transcript.blocks_mut() {
            flow = flow.child(transcript::render_block(block, &theme, expand_tools));
        }

        // 这棵子树按内容算出来的高度会比实际画出来的矮一截（块越多差得越多），
        // 滚到底时最后一行就被输入卡片切掉。用上一帧实测的流高把外层高度定死，
        // 高度对上了就不再要帧。
        let measured = self.content_height.clone();
        let flow = flow.on_children_prepainted(move |bounds, window, _| {
            let (Some(first), Some(last)) = (bounds.first(), bounds.last()) else {
                return;
            };
            let height = last.bottom() - first.top();
            if (measured.get() - height).abs() > px(0.5) {
                measured.set(height);
                window.request_animation_frame();
            }
        });
        let measured_height = self.content_height.get();
        let mut content = div()
            .w_full()
            .flex_shrink_0()
            .max_w(px(CONTENT_MAX_WIDTH))
            .mx_auto()
            .px(px(28.))
            .pt(px(FLOW_PADDING_TOP))
            .pb(px(FLOW_PADDING_BOTTOM))
            .child(flow);
        if measured_height > px(0.) {
            content = content.h(measured_height + px(FLOW_PADDING_TOP) + px(FLOW_PADDING_BOTTOM));
        }

        // 钉底：事件里那次 `scroll_to_bottom` 会停在旧的底（内容高度还会被二次测量改掉），
        // 这里在布局结束后按实际内容底边对一次；只在意内容变高变矮，用户自己滚开就不管了。
        let scroll = self.scroll.clone();
        let pinned = self.pinned_bottom.clone();
        let pin_bottom = move |_: Vec<Bounds<Pixels>>, window: &mut Window, _: &mut App| {
            let (Some(content), viewport) = (scroll.bounds_for_item(0), scroll.bounds()) else {
                return;
            };
            let target = viewport.bottom() - content.bottom();
            if pinned
                .get()
                .is_some_and(|last| (last - target).abs() <= px(0.5))
            {
                return;
            }
            pinned.set(Some(target));
            scroll.set_offset(gpui::point(px(0.), target));
            window.request_animation_frame();
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
            .child(content);
        let transcript = div()
            .relative()
            .flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(transcript_area)
            .child(self.scrollbar(&theme, cx));

        // ── 输入卡片：文本框 + 一行提示与动作 ──────────────────────
        let tokens = self
            .snapshot
            .as_ref()
            .map(|s| s.usage.context_tokens)
            .unwrap_or(0);
        let mut hint = div().flex().items_center().gap(px(8.));
        hint = hint.child(chip(&theme, model, false));
        if tokens > 0 {
            hint = hint.child(
                div()
                    .text_size(px(10.5))
                    .text_color(theme.text_ghost)
                    .child(SharedString::from(format!("上下文 {tokens} tok"))),
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
                        this.submit();
                    }
                    cx.notify();
                }))
                .child(SharedString::from(if busy { "停止" } else { "发送" })),
        );

        // IME（中文/日文组字）只在绘制阶段能挂到窗口：这里放一层透明画布，
        // 命中区域就是输入卡片，候选窗会贴着它弹。
        let ime_layer = {
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
        };

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
            .child(composer::render(&self.editor, &theme))
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
                                        .child(SharedString::from("今天想做点什么？")),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.5))
                                        .text_color(theme.text_tertiary)
                                        .child(SharedString::from(
                                            "描述任务后回车发送；Ctrl-K 打开命令面板，Ctrl-O 切换会话",
                                        )),
                                ),
                        )
                        .child(composer_card)
                        .child(
                            div()
                                .px(px(13.))
                                .text_size(px(10.5))
                                .text_color(theme.text_ghost)
                                .child(SharedString::from(
                                    "Shift+Enter 换行 · Ctrl-G 外观 · Ctrl-Q 退出",
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
                .child(
                    div()
                        .flex_none()
                        .w_full()
                        .px(px(28.))
                        .pb(px(6.))
                        .child(composer_card),
                )
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
                                "Enter 发送 · Shift+Enter 换行 · Ctrl-K 命令 · Ctrl-Q 退出",
                            )),
                    ),
                )
        };

        let mut body = div().flex().flex_1().min_h_0().w_full();
        if self.show_sidebar {
            body = body.child(sidebar::render(
                &theme,
                &cwd,
                &self.sessions,
                self.snapshot
                    .as_ref()
                    .and_then(|s| s.session_path.as_deref()),
                self.sessions_loading,
                self.cmd_tx.clone(),
                cx.listener(|this, _, _, cx| {
                    _ = this.cmd_tx.send(AgentCommand::NewSession);
                    this.transcript.clear();
                    this.reload_sessions(cx);
                    cx.notify();
                }),
            ));
        }
        body = body.child(main);
        if self.panel.visible {
            body = body.child(panel::render(&theme, &self.panel, cx));
        }

        let mut root = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.canvas)
            .text_color(theme.text)
            .text_size(px(13.))
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
                    .child(overlay::render(overlay, &theme, cx)),
            );
        }
        root
    }
}

impl GuiApp {
    /// IME 写入的目标：覆盖层列表打开时是它的过滤框，否则是输入卡片。
    fn ime_target(&self) -> &Editor {
        match &self.overlay {
            Some(Overlay::List(list)) => &list.query,
            _ => &self.editor,
        }
    }

    /// [`Self::ime_target`] 的可变版：写过滤框时把选中项收回第一行。
    fn ime_target_mut(&mut self) -> &mut Editor {
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
        let bytes = self.ime_range_bytes(range);
        self.ime_target_mut().replace_byte_range(bytes, text);
        self.marked_range = None;
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

/// 路径最后一段（项目名）。
fn basename(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(path)
        .to_string()
}
