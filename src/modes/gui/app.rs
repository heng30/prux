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
    /// 按下时的鼠标 y
    press_y: Pixels,
    /// 按下时的滚动偏移
    offset: Pixels,
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
    /// 对话流实测高度（上一帧 prepaint 量到的，用来给滚动内容定高）
    content_height: Rc<Cell<Pixels>>,
    /// 上次钉底用的偏移（内容高度变了才重钉，用户自己滚开就不打扰）
    pinned_bottom: Rc<Cell<Option<Pixels>>>,
    /// 输入框补全面板（None = 关闭）
    suggest: Option<Suggest>,
    /// `@` 补全用的文件路径缓存
    files_cache: FilesCache,
    /// 附件按钮的补全过滤：true 只列图片
    attach_images: bool,
    /// 标题栏/输入区的下拉菜单
    dropdown: Option<Dropdown>,
    /// 输入卡片在窗口里的实测位置（补全面板贴它上沿）
    composer_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
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
            content_height: Rc::new(Cell::new(px(0.))),
            pinned_bottom: Rc::new(Cell::new(None)),
            suggest: None,
            files_cache: FilesCache::default(),
            attach_images: false,
            dropdown: None,
            composer_bounds: Rc::new(Cell::new(None)),
            scroll_drag: None,
            transcript_hovered: false,
            sidebar_hovered: false,
            session_menu: None,
            dialog: None,
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

    /// 按输入框当前内容重算补全面板：触发词变了或没有触发词就开关面板。
    fn refresh_suggest(&mut self, cx: &mut Context<Self>) {
        let before = self.editor.text_before_cursor();
        let Some((trigger, start, query)) = suggest::detect(&before) else {
            self.suggest = None;
            self.attach_images = false;
            return;
        };
        let items = match trigger {
            Trigger::Command => suggest::commands(&query),
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
                this.refresh_suggest(cx);
                cx.notify();
            });
        })
        .detach();
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
        let insert = if suggest.trigger == Trigger::File {
            format!("{} ", item.label)
        } else {
            item.label.clone()
        };
        self.editor.replace_byte_range(suggest.start..end, &insert);
        self.suggest = None;
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
        if suggest.items.is_empty() {
            column = column.child(
                div()
                    .px(px(8.))
                    .py(px(6.))
                    .text_size(px(11.5))
                    .text_color(theme.text_ghost)
                    .child(SharedString::from(if suggest.trigger == Trigger::Shell {
                        "回车在本地执行，输出进入对话上下文"
                    } else {
                        "没有匹配项"
                    })),
            );
        }
        for (index, item) in suggest.items.iter().enumerate() {
            let selected = index == suggest.selected;
            let mut row = div()
                .h(px(SUGGEST_ROW_HEIGHT))
                .px(px(8.))
                .rounded(px(6.))
                .flex()
                .items_center()
                .gap(px(10.));
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
            column = column.child(
                row.id(("suggest", index))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.pick_suggestion(index, cx))),
            );
        }
        column.into_any_element()
    }

    /// 模型下拉菜单：只作用于当前会话，不写配置。
    fn open_model_menu(&mut self, anchor: gpui::Point<Pixels>, cx: &mut Context<Self>) {
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
                let items: Vec<MenuItem> = models
                    .into_iter()
                    .map(|(provider, id, name)| MenuItem {
                        label: format!("{provider} / {id}"),
                        detail: name,
                        action: Action::Command(Box::new(AgentCommand::SwitchModel {
                            provider,
                            model_id: id,
                            persist: false,
                        })),
                    })
                    .collect();
                app.dropdown = Some(Dropdown {
                    items,
                    anchor,
                    width: 280.,
                    scroll: true,
                });
                cx.notify();
            });
        })
        .detach();
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
                label: format!("思考 {level}"),
                detail: String::new(),
                action: Action::Command(Box::new(AgentCommand::SetThinking {
                    level,
                    persist: false,
                })),
            })
            .collect();
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
                label: "文件".into(),
                detail: "@ 引用".into(),
                action: Action::Attach { images: false },
            },
            MenuItem {
                label: "图片".into(),
                detail: "@ 引用".into(),
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
                self.reload_sessions(cx);
            }
            "sessions" => self.open_session_picker(cx),
            "model" => self.open_model_picker(cx),
            "thinking" => self.open_thinking_picker(cx),
            "appearance" => self.open_appearance_picker(cx),
            "panel" => self.toggle_panel(None, cx),
            "usage" => self.open_info(InfoKind::Usage, cx),
            "skills" => self.open_info(InfoKind::Skills, cx),
            "settings" => self.open_info(InfoKind::Settings, cx),
            "help" => self.open_info(InfoKind::Help, cx),
            "quit" => cx.quit(),
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
                let (transcript, sidebar, app) = (transcript.clone(), sidebar.clone(), app.clone());
                window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, _, cx| {
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
                    app.update(cx, |this, cx| this.set_hover(in_transcript, in_sidebar, cx));
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .into_any_element()
    }

    /// 记录悬浮状态：有变化才重绘，顺手收掉鼠标已离开那个区的拖拽状态。
    fn set_hover(&mut self, transcript: bool, sidebar: bool, cx: &mut Context<Self>) {
        if self.transcript_hovered == transcript && self.sidebar_hovered == sidebar {
            return;
        }
        self.transcript_hovered = transcript;
        self.sidebar_hovered = sidebar;
        self.scroll_drag = self.scroll_drag.filter(|drag| match drag.area {
            ScrollArea::Transcript => transcript,
            ScrollArea::Sidebar => sidebar,
        });
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

    /// 对话框确认：重命名落盘 / 删除文件。
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
                    this.scroll_drag = Some(ScrollDrag {
                        area,
                        press_y: event.position.y,
                        offset,
                    });
                    cx.notify();
                }),
            )
            .on_mouse_move(
                cx.listener(move |this, event: &gpui::MouseMoveEvent, _, cx| {
                    if !this.scroll_drag.is_some_and(|drag| drag.area == area) {
                        return;
                    }
                    let Some(drag) = this.scroll_drag else { return };
                    // 滑块要跟着鼠标走：轨道上的位移按「内容高 / 轨道高」放大成偏移
                    let scroll = this.scroll_of(area);
                    let viewport = f32::from(scroll.bounds().size.height);
                    let max = f32::from(scroll.max_offset().height);
                    let travel = thumb_travel(viewport, max);
                    let dy = f32::from(event.position.y - drag.press_y);
                    let target = drag_offset(f32::from(drag.offset), dy, max, travel);
                    this.scroll_of_mut(area)
                        .set_offset(gpui::point(px(0.), gpui::px(target)));
                    cx.notify();
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

        if let Some(suggest) = self.suggest.clone() {
            match keystroke.key.as_str() {
                "escape" => {
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
                _ => {}
            }
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
                self.submit(cx);
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

    /// 对话框按键：重命名时打字进输入框，Enter 确认、Esc 取消。
    fn handle_dialog_key(&mut self, key: &str, shift: bool) {
        let Some(dialog) = self.dialog.as_mut() else {
            return;
        };
        match key {
            "escape" => self.dialog = None,
            "backspace" => {
                if let SessionDialog::Rename { editor, .. } = dialog {
                    editor.backspace();
                }
            }
            "delete" => {
                if let SessionDialog::Rename { editor, .. } = dialog {
                    editor.delete_forward();
                }
            }
            "left" | "right" | "home" | "end" => {
                if let SessionDialog::Rename { editor, .. } = dialog {
                    match key {
                        "left" => editor.move_cursor(-1, 0, shift),
                        "right" => editor.move_cursor(1, 0, shift),
                        "home" => editor.home(shift),
                        _ => editor.end(shift),
                    }
                }
            }
            _ => {}
        }
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
        Action::Attach { images } => Action::Attach { images: *images },
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
        chrome_right = chrome_right.child(
            chip(&theme, model.clone(), false)
                .id("model-chip")
                .cursor_pointer()
                .hover(|style| style.bg(theme.overlay).text_color(theme.text))
                .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, cx| {
                    this.open_model_menu(event.position(), cx);
                })),
        );
        chrome_right = chrome_right.child(
            chip(
                &theme,
                match &thinking {
                    Some(level) => format!("思考 {level}"),
                    None => "思考 off".to_string(),
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
        for (index, block) in self.transcript.blocks_mut().iter_mut().enumerate() {
            flow = flow.child(transcript::render_block(
                block,
                &theme,
                expand_tools,
                index,
                cx,
            ));
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
                .child(SharedString::from("+ 附件")),
        );
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
                        this.submit(cx);
                    }
                    cx.notify();
                }))
                .child(SharedString::from(if busy { "停止" } else { "发送" })),
        );

        let ime_layer = self.ime_layer(cx);

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
                        .child(measured_composer(composer_card, self.composer_bounds.clone()))
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
                                "Enter 发送 · Shift+Enter 换行 · Ctrl-K 命令 · Ctrl-Q 退出",
                            )),
                    ),
                )
        };

        let mut body = div().flex().flex_1().min_h_0().w_full();
        if self.show_sidebar {
            body = body.child(sidebar::render(sidebar::SidebarCtx {
                theme: &theme,
                cwd: &cwd,
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
                    this.reload_sessions(cx);
                    cx.notify();
                })),
                context_menu: {
                    let app = cx.entity();
                    std::rc::Rc::new(move |path: &str, event: &gpui::MouseDownEvent, _, cx| {
                        let path = path.to_string();
                        let position = event.position;
                        app.update(cx, |this, cx| this.open_session_menu(&path, position, cx));
                    })
                },
            }));
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
        root = root.child(self.hover_probe(!empty, cx));

        // ── 下拉菜单（模型 / 思考级别 / 附件）────────────────────
        if let Some(dropdown) = &self.dropdown {
            let (anchor, width, scroll) = (dropdown.anchor, dropdown.width, dropdown.scroll);
            let mut card = div()
                .id("dropdown-menu")
                .absolute()
                .left(anchor.x)
                .top(anchor.y + px(4.))
                .w(px(width))
                .p(px(4.))
                .rounded(px(8.))
                .bg(theme.composer)
                .border_1()
                .border_color(theme.border_strong)
                .flex()
                .flex_col()
                .occlude();
            if scroll {
                card = card.max_h(px(360.)).overflow_y_scroll();
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
            let height = px(SUGGEST_ROW_HEIGHT * rows) + px(8.);
            let top = (card.origin.y - height - px(SUGGEST_GAP)).max(px(0.));
            root = root.child(
                div()
                    .absolute()
                    .left(card.origin.x)
                    .w(card.size.width)
                    .top(top)
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
                                    .child(SharedString::from("重命名")),
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
                                    .child(SharedString::from("删除")),
                            ),
                    ),
            );
        }

        // ── 重命名 / 删除确认 ─────────────────────────────────────
        if self.dialog.is_some() {
            let (title, body, field) = match &self.dialog {
                Some(SessionDialog::Rename { editor, .. }) => (
                    "重命名会话",
                    String::new(),
                    Some(composer::render(editor, &theme)),
                ),
                Some(SessionDialog::Delete { title, .. }) => (
                    "删除会话",
                    format!("删除「{title}」？此操作不可撤销。"),
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
                            "Enter 确认删除 · Esc 取消"
                        } else {
                            "Enter 确认 · Esc 取消"
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
                                .child(SharedString::from("取消")),
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
                                    "删除"
                                } else {
                                    "确认"
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
        if let Some(SessionDialog::Rename { editor, .. }) = &self.dialog {
            return editor;
        }
        match &self.overlay {
            Some(Overlay::List(list)) => &list.query,
            _ => &self.editor,
        }
    }

    /// [`Self::ime_target`] 的可变版：写过滤框时把选中项收回第一行。
    fn ime_target_mut(&mut self) -> &mut Editor {
        if let Some(SessionDialog::Rename { editor, .. }) = &mut self.dialog {
            return editor;
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
        let bytes = self.ime_range_bytes(range);
        self.ime_target_mut().replace_byte_range(bytes, text);
        self.marked_range = None;
        self.refresh_suggest(cx);
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

/// 路径最后一段（项目名）。
fn basename(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(path)
        .to_string()
}

#[cfg(test)]
mod tests {
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
