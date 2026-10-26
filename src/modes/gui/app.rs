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
use super::text_edit::Editor;
use super::theme::Theme;
use super::transcript::{self, Note, Transcript};
use crate::core::provider::AgentMessage;
use crate::core::settings_manager;
use crate::modes::interactive::agent_actor::{AgentCommand, CommandTx};
use gpui::{
    App, Context, FocusHandle, IntoElement, KeyDownEvent, Render, ScrollHandle, SharedString, Task,
    Window, div, prelude::*, px,
};
use tokio::sync::mpsc::UnboundedReceiver;

/// 正文列最大宽度：宽屏下不铺满整屏，阅读更稳。
const CONTENT_MAX_WIDTH: f32 = 760.0;
/// 标题栏高度。
const CHROME_HEIGHT: f32 = 40.0;

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
    /// 侧栏高亮下标
    session_selected: usize,
    /// 侧栏是否可见
    show_sidebar: bool,
    /// 当前覆盖层
    overlay: Option<Overlay>,
    /// 右侧面板（文件 / 变更）
    pub(super) panel: RightPanel,
    /// 是否展开全部工具输出
    expand_tools: bool,
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
                        app.apply_event(event);
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

        Self {
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
            session_selected: 0,
            show_sidebar: true,
            overlay: None,
            panel: RightPanel::default(),
            expand_tools: false,
        }
    }

    /// 视图焦点句柄（打开窗口时用来把键盘交给它）。
    pub fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }

    /// 上行事件 → 状态。
    fn apply_event(&mut self, event: GuiEvent) {
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
                if self.sessions.is_empty() {
                    self.reload_sessions(None);
                }
                self.snapshot = Some(snapshot);
            }
        }
    }

    /// 让 transcript 跟随最新内容。
    fn scroll_to_bottom(&mut self) {
        self.scroll.scroll_to_bottom();
    }

    /// 重读会话列表。
    fn reload_sessions(&mut self, cwd: Option<&str>) {
        let cwd = cwd
            .map(str::to_string)
            .or_else(|| self.snapshot.as_ref().map(|s| s.cwd.clone()))
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
            });
        self.sessions = sidebar::load(&cwd);
    }

    /// 键盘分发：覆盖层打开时只喂覆盖层，否则走输入框。
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = event.keystroke.clone();
        let primary = keystroke.modifiers.control || keystroke.modifiers.platform;
        let shift = keystroke.modifiers.shift;

        if primary && keystroke.key == "q" {
            cx.quit();
            return;
        }

        if self.overlay.is_some() {
            self.handle_overlay_key(&keystroke.key, primary, shift, cx);
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
                    self.toggle_panel(None);
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
                    self.reload_sessions(None);
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
            _ => {
                if let Some(text) = &keystroke.key_char {
                    self.editor.insert(text);
                    cx.notify();
                }
            }
        }
    }

    /// 覆盖层按键。
    fn handle_overlay_key(
        &mut self,
        key: &str,
        primary: bool,
        _shift: bool,
        cx: &mut Context<Self>,
    ) {
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
                _ => {
                    if !primary
                        && key.chars().count() == 1
                        && let Some(text) = keystroke_char(key)
                    {
                        list.query.insert(&text);
                        list.selected = 0;
                    }
                }
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
                ListKind::PanelChanges => self.toggle_panel(Some(PanelTab::Changes)),
                ListKind::PanelFiles => self.toggle_panel(Some(PanelTab::Files)),
            },
            Action::RefreshSessions => self.reload_sessions(None),
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
    fn toggle_panel(&mut self, tab: Option<PanelTab>) {
        let cwd = self.cwd();
        match tab {
            Some(tab) => {
                self.panel.tab = tab;
                self.panel.visible = true;
            }
            None => self.panel.visible = !self.panel.visible,
        }
        if self.panel.visible {
            self.panel.reload(&cwd);
        }
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
    fn open_model_picker(&mut self, _cx: &mut Context<Self>) {
        let mut entries = Vec::new();
        for provider in crate::core::auth::list_configured_providers() {
            for (id, name) in crate::core::model_resolver::list_models(&provider) {
                entries.push(Entry {
                    label: format!("{provider} / {id}"),
                    detail: name,
                    action: Action::Command(Box::new(AgentCommand::SwitchModel {
                        provider: provider.clone(),
                        model_id: id,
                        persist: true,
                    })),
                });
            }
        }
        if entries.is_empty() {
            entries.push(Entry {
                label: "没有已配置的模型".into(),
                detail: "先运行 prux auth".into(),
                action: Action::None,
            });
        }
        self.overlay = Some(Overlay::List(List::new("模型", entries)));
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
    fn open_session_picker(&mut self, _cx: &mut Context<Self>) {
        self.reload_sessions(None);
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
                label: "还没有会话".into(),
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
                    "Ctrl-O 会话列表 · Ctrl-B 切换侧栏".to_string(),
                    "Ctrl-U 用量 · Ctrl-I 技能 · Ctrl-, 设置".to_string(),
                    "Ctrl-N 新建会话 · Ctrl-Z 撤销 · Ctrl-V 粘贴".to_string(),
                    "Ctrl-Q 退出".to_string(),
                ],
            ),
        };
        self.overlay = Some(Overlay::Info(info));
    }

    /// 状态栏文案。
    fn status_text(&self) -> String {
        if self.busy {
            return format!(
                "● 生成中…（Esc 中止，排队 {}） · {} 条消息",
                self.queued,
                self.snapshot.as_ref().map(|s| s.messages).unwrap_or(0)
            );
        }
        "Enter 发送 · Shift+Enter 换行 · Ctrl-K 命令 · Ctrl-Q 退出".to_string()
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

/// 单字符按键 → 可插入文本（gpui 未提供 `key_char` 时按 key 兜底）。
fn keystroke_char(key: &str) -> Option<String> {
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) => Some(ch.to_string()),
        _ => None,
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
                .pl(px(2.))
                .text_size(px(10.5))
                .text_color(theme.text_ghost)
                .child(SharedString::from("Ctrl-K")),
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
        let mut flow = div().w_full().flex().flex_col().gap(px(14.));
        for block in self.transcript.blocks() {
            flow = flow.child(transcript::render_block(block, &theme, self.expand_tools));
        }

        let transcript_area = div()
            .id("transcript")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                div()
                    .w_full()
                    .max_w(px(CONTENT_MAX_WIDTH))
                    .mx_auto()
                    .px(px(28.))
                    .py(px(20.))
                    .child(flow),
            );

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

        let composer_card = div()
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
            .child(hint);

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
                        .child(composer_card),
                )
                .child(
                    div()
                        .flex_none()
                        .w_full()
                        .px(px(28.))
                        .pb(px(6.))
                        .child(
                            div()
                                .max_w(px(CONTENT_MAX_WIDTH))
                                .mx_auto()
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
                .child(transcript_area)
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
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(10.5))
                            .text_color(theme.text_ghost)
                            .child(SharedString::from(footer_status))
                            .child(SharedString::from(
                                "Shift+Enter 换行 · Ctrl-G 外观 · Ctrl-Q 退出",
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
                self.session_selected,
                self.cmd_tx.clone(),
                cx.listener(|this, _, _, cx| {
                    _ = this.cmd_tx.send(AgentCommand::NewSession);
                    this.transcript.clear();
                    this.reload_sessions(None);
                    cx.notify();
                }),
            ));
        }
        body = body.child(main);
        if self.panel.visible {
            body = body.child(panel::render(&theme, &cwd, &self.panel, cx));
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
                    .child(overlay::render(overlay, &theme)),
            );
        }
        root
    }
}

/// 路径最后一段（项目名）。
fn basename(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(path)
        .to_string()
}
