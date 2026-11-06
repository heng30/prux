//! 下拉菜单、侧栏会话菜单与拖拽：标题栏/输入区的浮层与宽度调整。

use super::*;
use crate::{core, modes::cwd_store, utils::paths};
use std::path::Path;

impl GuiApp {
    /// 模型下拉菜单：只作用于当前会话，不写配置。
    ///
    /// 同步取模型列表：异步任务要等前台执行器被唤醒，菜单可能在用户下次交互前
    /// 都不出现（曾经就因此完全看不到菜单）。模型表是本地 JSON，直接读足够快。
    pub(super) fn open_model_menu(&mut self, anchor: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let mut items: Vec<MenuItem> = Vec::new();
        for provider in core::auth::list_configured_providers() {
            for (id, name) in core::model_resolver::list_models(&provider) {
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
                label: "No models configured, use `/login` or `/model`".into(),
                detail: String::new(),
                action: Action::None,
            });
        }

        self.dropdown = Some(Dropdown {
            items,
            anchor,
            width: 280.,
            scroll: true,
            handle: ScrollHandle::new(),
            selected: 0,
        });
        cx.notify();
    }

    /// 思考级别下拉菜单：同样只影响当前会话。
    pub(super) fn open_thinking_menu(
        &mut self,
        anchor: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let items: Vec<MenuItem> = self
            .snapshot
            .as_ref()
            .map(|s| s.thinking_levels.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|level| MenuItem {
                label: level.clone(),
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
            handle: ScrollHandle::new(),
            selected: 0,
        });
        cx.notify();
    }

    /// 附件按钮：弹系统文件选择框，选中后把 `@路径` 插到光标处。
    ///
    /// 图片与文本同一个入口：发送时 `@路径` 指向图片就自动收集成图片附件。
    pub(super) fn pick_attachment(&mut self, cx: &mut Context<Self>) {
        // 原生对话框必须在主线程（= 前台执行器）打开：Windows 后端需要主线程的 OLE STA，
        // macOS 要求主线程，丢到背景线程会直接失败（且错误被 `ok()` 吞掉，表现为点了没反应）。
        // 代价是对话框期间界面不重绘——系统模态框本来就有自己的消息循环。
        let start = self.cwd();
        cx.spawn(async move |this, cx| {
            let picked = native_dialog::FileDialogBuilder::default()
                .set_title("Attach a file")
                .set_location(&start)
                .open_single_file()
                .show()
                .ok()
                .flatten();

            if let Some(path) = picked {
                _ = this.update(cx, |app, cx| {
                    let cwd = PathBuf::from(app.cwd());
                    let path = paths::relative_posix(&cwd, &path);
                    // 带空白的路径要加引号，`@路径` 的解析按空白分词
                    let insert = if path.contains(char::is_whitespace) {
                        format!("@\"{path}\" ")
                    } else {
                        format!("@{path} ")
                    };
                    app.editor.insert(&insert);
                    app.refresh_suggest(cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 悬浮探测：按滚动区实测 bounds 与鼠标位置判定，指到谁就显谁的滚动条。
    ///
    /// 不用元素 `on_hover`：滚动容器的 hitbox 判定偏宽（鼠标在整列甚至整屏范围内都算悬停），
    /// 滚动条会常显。这里在绘制阶段挂一个全局鼠标监听，用 `ScrollHandle::bounds()` 比位置。
    ///
    /// 对话区的滚轮加速也走这里：capture 阶段先于原生滚动拿到事件，算完步长并
    /// `stop_propagation`，原生滚动（bubble 阶段）就不会再按未加速的步长盖一次。
    pub(super) fn hover_probe(&self, transcript_live: bool, cx: &mut Context<Self>) -> AnyElement {
        let transcript = self.scroll.clone();
        let sidebar = self.sessions_scroll.clone();
        let app = cx.entity();
        canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                let (transcript, sidebar) = (transcript.clone(), sidebar.clone());
                let move_app = app.clone();

                let wheel = transcript.clone();
                let wheel_app = app.clone();
                window.on_mouse_event(move |event: &gpui::ScrollEvent, phase, window, cx| {
                    if phase != gpui::DispatchPhase::Capture
                        || !transcript_live
                        || !wheel.bounds().contains(&event.position)
                    {
                        return;
                    }

                    let applied = wheel_app.update(cx, |this, cx| {
                        let applied = this.wheel_transcript(event, window.line_height());
                        if applied {
                            cx.notify();
                        }
                        applied
                    });

                    if applied {
                        cx.stop_propagation();
                    }
                });

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

                        if this.panel.scroll_drag.is_some() {
                            if event.pressed_button == Some(MouseButton::Left) {
                                this.panel.drag_scroll(position);
                            } else {
                                this.panel.scroll_drag = None;
                            }
                            cx.notify();
                        }

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
    pub(super) fn drag_panel_width(
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

        let width = geometry::resized_width(
            drag.width,
            -f32::from(position.x - anchor_x),
            theme::ui_scale(),
            (PANEL_MIN_WIDTH, PANEL_MAX_WIDTH),
        );

        if width != self.panel_width {
            self.panel_width = width;
            cx.notify();
        }
    }

    /// 应用滚动条拖拽：鼠标在哪都按「按下点 + 位移」算偏移，跑出滚动条也不中断。
    pub(super) fn drag_scrollbar(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
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
        let Some(scroll) = self.scroll_of(area) else {
            return;
        };
        let viewport = f32::from(scroll.bounds().size.height);
        let max = f32::from(scroll.max_offset().y);
        if max <= 0. || viewport <= 0. {
            return;
        }

        // 滑块要跟着鼠标走：轨道上的位移按「内容高 / 轨道高」放大成偏移
        let travel = geometry::thumb_travel(viewport, max, min_thumb());
        let dy = f32::from(position.y - anchor_y);
        let target = geometry::drag_offset(f32::from(drag.offset), dy, max, travel);
        if let Some(scroll) = self.scroll_of_mut(area) {
            scroll.set_offset(gpui::point(px(0.), gpui::px(target)));
        }
        cx.notify();
    }

    /// 拖侧栏右边框：按住左键时按位移改宽度，松手即结束。
    pub(super) fn drag_sidebar_width(
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

        let width = geometry::resized_width(
            drag.width,
            f32::from(position.x - anchor_x),
            theme::ui_scale(),
            (SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH),
        );

        if width != self.sidebar_width {
            self.sidebar_width = width;
            cx.notify();
        }
    }

    /// 记录悬浮状态：有变化才重绘，顺手收掉鼠标已离开那个区的拖拽状态。
    pub(super) fn set_hover(
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
                ScrollArea::Overlay => false,
            });
        }

        if self.transcript_hovered == transcript && self.sidebar_hovered == sidebar {
            return;
        }

        self.transcript_hovered = transcript;
        self.sidebar_hovered = sidebar;
        cx.notify();
    }

    /// 鼠标移到菜单某一项：高亮跟着走。
    ///
    /// 菜单只有一束高亮（`selected`），鼠标悬停与键盘移动改的是同一项，否则键盘选中项
    /// 会一直亮着，看起来同时高亮两行。
    pub(super) fn hover_session_menu(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(menu) = self.session_menu.as_mut() {
            if menu.selected == index {
                return;
            }
            menu.selected = index;
            cx.notify();
        }
    }

    /// 鼠标移到下拉菜单某一行：高亮跟着走。
    ///
    /// 同 [`Self::hover_session_menu`]：菜单只有一束高亮，鼠标悬停与键盘移动改的是同一项。
    pub(super) fn hover_dropdown(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(dropdown) = self.dropdown.as_mut() {
            if dropdown.selected == index {
                return;
            }
            dropdown.selected = index;
            cx.notify();
        }
    }

    /// 打开会话行右键菜单。
    pub(super) fn open_session_menu(
        &mut self,
        path: &str,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.session_menu = Some(SessionMenu {
            path: path.to_string(),
            position,
            selected: 0,
        });
        cx.notify();
    }

    /// 关掉右键菜单。
    pub(super) fn close_session_menu(&mut self, cx: &mut Context<Self>) {
        self.session_menu = None;
        cx.notify();
    }

    /// 菜单「重命名」：弹对话框，输入框预填当前标题。
    pub(super) fn start_rename(&mut self, path: &str, cx: &mut Context<Self>) {
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

    /// 菜单「删除」：先弹二次确认。当前打开的会话不能从侧栏删（与终端 UI 的选择器一致）。
    pub(super) fn ask_delete(&mut self, path: &str, cx: &mut Context<Self>) {
        self.session_menu = None;
        if self
            .snapshot
            .as_ref()
            .and_then(|s| s.session_path.as_deref())
            == Some(path)
        {
            self.transcript
                .push_note(Note::Error("Cannot delete the current session".into()));
            cx.notify();
            return;
        }

        let title = self
            .sessions
            .iter()
            .find(|row| row.path == path)
            .map(|row| row.title.clone())
            .unwrap_or_else(|| paths::file_name_of(Path::new(path)).unwrap_or_default());
        self.ask_delete_many(vec![path.to_string()], format!("Delete \"{title}\"?"), cx);
    }

    /// 批量删除确认：`/session rm-*` 与侧栏单条共用同一个对话框。
    pub(super) fn ask_delete_many(
        &mut self,
        paths: Vec<String>,
        detail: String,
        cx: &mut Context<Self>,
    ) {
        self.session_menu = None;
        if paths.is_empty() {
            self.transcript
                .push_note(Note::Info("No sessions to delete".into()));
        } else {
            self.dialog = Some(SessionDialog::Delete { paths, detail });
        }
        cx.notify();
    }

    /// `/import`：弹路径输入框，确认后作为会话载入。
    pub(super) fn open_import_dialog(&mut self, cx: &mut Context<Self>) {
        let editor = Editor::new();
        self.dialog = Some(SessionDialog::Import { editor });
        cx.notify();
    }

    /// `/import <path.jsonl>`：校验路径 → 确认替换 → 复制进当前会话目录 → cwd 确认 → 切会话。
    ///
    /// 校验顺序与文案同 TUI：参数可带 `@` 前缀（输入框的引用前缀），相对路径按当前目录解析。
    pub(super) fn start_import(&mut self, raw: &str, cx: &mut Context<Self>) {
        let source = match import_source(raw, &self.cwd()) {
            Ok(source) => source,
            Err(error) => {
                self.transcript.push_note(Note::Error(error));
                cx.notify();
                return;
            }
        };

        self.pending_session = Some(PendingSession::Confirm { source });
        self.ask_choice(
            "Import session (replaces the current session)",
            vec![
                ("Yes, import".to_string(), Action::ImportConfirm),
                ("No, cancel".to_string(), Action::SessionCancel),
            ],
            cx,
        );
    }

    /// 确认后的实际导入：`open_checked` 校验完整性 → 复制进会话目录（同名递增，不覆盖）
    /// → 会话记录的 cwd 与当前不一致时弹确认 → 切会话；中间行损坏则弹修复选择。
    pub(super) fn run_import(&mut self, source: PathBuf, cx: &mut Context<Self>) {
        match Session::open_checked(&source.to_string_lossy()) {
            OpenSessionResult::Ok(session) => {
                let destination = match import_copy(&source, &self.cwd()) {
                    Ok(destination) => destination,
                    Err(error) => {
                        self.transcript.push_note(Note::Error(error));
                        cx.notify();
                        return;
                    }
                };

                let cwd = self.cwd();
                let cwd_missing = session.cwd.is_empty() || !Path::new(&session.cwd).exists();
                if cwd_missing || session.cwd != cwd {
                    let recorded = if session.cwd.is_empty() {
                        "<unknown>".to_string()
                    } else {
                        session.cwd.clone()
                    };
                    self.pending_session = Some(PendingSession::Cwd { destination });
                    let title = if cwd_missing {
                        "Session cwd not found"
                    } else {
                        "Session cwd mismatch"
                    };
                    self.ask_choice(
                        title,
                        vec![
                            (
                                format!("Yes, resume in {cwd} (recorded: {recorded})"),
                                Action::ImportCwdContinue,
                            ),
                            ("No, cancel".to_string(), Action::SessionCancel),
                        ],
                        cx,
                    );
                } else {
                    self.resume_session_file(destination);
                    cx.notify();
                }
            }
            OpenSessionResult::Invalid(e) => {
                self.transcript
                    .push_note(Note::Error(format!("import failed: {e}")));
                cx.notify();
            }
            OpenSessionResult::Repairable { path, plan } => {
                self.stage_session_repair(path, plan, RepairIntent::Resume, cx);
            }
        }
    }

    /// 弹「未完成 operation」面板：会话里有崩溃残留的未关闭 run 时，
    /// 让用户选 Continue（按最新 run 恢复，日志不动）/ Rewrite（把旧 run 标记为已结束）/ Cancel。
    pub(super) fn stage_zombie_operations(
        &mut self,
        path: PathBuf,
        count: usize,
        cx: &mut Context<Self>,
    ) {
        let path = path.to_string_lossy().to_string();
        let mut choices: Vec<(String, Action)> = zombie_choices(count)
            .into_iter()
            .map(|(label, rewrite)| {
                (
                    label,
                    Action::ResumeZombie {
                        path: path.clone(),
                        rewrite,
                    },
                )
            })
            .collect();
        choices.push((
            "Cancel: leave file unchanged".to_string(),
            Action::ZombieCancel,
        ));
        self.ask_choice(
            format!("Session has {count} unfinished operation(s) (from interrupted runs)"),
            choices,
            cx,
        );
    }

    /// 弹损坏会话的修复面板：抢救（跳过损坏行）/ 截断（只留前缀）/ 取消。
    ///
    /// `/import`、打开已有会话（启动、`/resume`、侧栏）都走这里；区别只在修好之后
    /// 是「打开」还是「fork 进当前项目」。
    pub(super) fn stage_session_repair(
        &mut self,
        path: PathBuf,
        plan: SessionRepairPlan,
        intent: RepairIntent,
        cx: &mut Context<Self>,
    ) {
        let mut choices: Vec<(String, Action)> = repair_choices(&plan)
            .into_iter()
            .map(|(label, salvage)| (label, Action::SessionRepair(salvage)))
            .collect();
        choices.push(("Cancel (no repair)".to_string(), Action::SessionCancel));
        self.pending_session = Some(PendingSession::Repair { path, plan, intent });
        self.ask_choice("Session file corrupted", choices, cx);
    }

    /// 修复选择后的执行：写回修好的文件，成功则按去向重新打开。
    pub(super) fn repair_session(&mut self, salvage: bool, cx: &mut Context<Self>) {
        let Some(PendingSession::Repair { path, plan, intent }) = self.pending_session.take()
        else {
            return;
        };

        if let Err(error) = Session::repair_write(&path.to_string_lossy(), &plan, salvage) {
            self.transcript
                .push_note(Note::Error(format!("Session repair failed: {error}")));
            cx.notify();
            return;
        }

        match intent {
            RepairIntent::Resume => self.resume_session_file(path),
            RepairIntent::Fork { dir } => {
                _ = self.cmd_tx.send(AgentCommand::ForkFrom {
                    path: path.to_string_lossy().to_string(),
                    dir,
                });
            }
        }
        cx.notify();
    }

    /// 确认框 / cwd 确认 / 修复选择共用的取消：丢掉中间状态，不动会话。
    pub(super) fn cancel_session_op(&mut self, cx: &mut Context<Self>) {
        let note = match self.pending_session.take() {
            // 修复被取消：文件保持原样（与 TUI 同文案）
            Some(PendingSession::Repair { .. }) => "Session repair cancelled, file left unchanged",
            _ => "Import cancelled",
        };
        self.transcript.push_note(Note::Info(note.to_string()));
        cx.notify();
    }

    /// 确认列表（导入确认 / cwd 确认 / 修复选择 / `/bug` 附带决定共用同一套外观）。
    fn ask_choice(
        &mut self,
        title: impl Into<String>,
        choices: Vec<(String, Action)>,
        cx: &mut Context<Self>,
    ) {
        let entries = choices
            .into_iter()
            .map(|(label, action)| Entry {
                label,
                action,
                ..Entry::default()
            })
            .collect();
        self.overlay = Some(Overlay::List(List::new(title.into(), entries)));
        cx.notify();
    }

    /// 切到某个会话文件（导入复制好的、修好的都用它）。
    pub(super) fn resume_session_file(&mut self, destination: PathBuf) {
        _ = self.cmd_tx.send(AgentCommand::ResumeSession {
            path: destination.to_string_lossy().to_string(),
        });
    }

    /// `/name`：给当前会话改名（预填现有会话名）。
    pub(super) fn rename_current_session(&mut self, cx: &mut Context<Self>) {
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

    /// 目录菜单：最近用过的目录 + 「Browse…」；确认后发 `SwitchCwd`（会顺带在新目录下开会话）。
    pub(super) fn open_cwd_menu(&mut self, _cx: &mut Context<Self>) {
        let current = self.cwd();
        let mut entries: Vec<Entry> = cwd_store::recent_cwds()
            .into_iter()
            .map(|path| Entry {
                detail: if path == current {
                    "✓ current".into()
                } else {
                    String::new()
                },
                label: path.clone(),
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

        let mut list = List::new("Working directory", entries);
        list.hint = Some("↑↓ select · Enter confirm · Esc close".to_string());
        self.overlay = Some(Overlay::List(list));
    }

    // 同 [`Self::pick_attachment`]：原生对话框必须在前台执行器（主线程）打开
    pub(super) fn pick_working_directory(&mut self, cx: &mut Context<Self>) {
        let start = self.cwd();
        cx.spawn(async move |this, cx| {
            let picked = native_dialog::FileDialogBuilder::default()
                .set_title("Select working directory")
                .set_location(&start)
                .open_single_dir()
                .show()
                .ok()
                .flatten();

            if let Some(path) = picked {
                _ = this.update(cx, |app, cx| {
                    if !app.cwd_switch_blocked(cx) {
                        _ = app.cmd_tx.send(AgentCommand::SwitchCwd {
                            path: path.to_string_lossy().to_string(),
                        });
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 对话框确认：重命名落盘 / 删除文件 / 切工作目录。
    pub(super) fn confirm_dialog(&mut self, cx: &mut Context<Self>) {
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
                // 与会话记录一起打包前先问一句（同 TUI 的第一个确认；默认不附带）
                let hint = editor.text();
                self.ask_choice(
                    "Include the session transcript?",
                    vec![
                        (
                            "Yes, include it (the zip is saved locally)".to_string(),
                            Action::BugReport {
                                hint: hint.clone(),
                                include_session: true,
                            },
                        ),
                        (
                            "No, diagnostics only".to_string(),
                            Action::BugReport {
                                hint,
                                include_session: false,
                            },
                        ),
                        ("Cancel".to_string(), Action::None),
                    ],
                    cx,
                );
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
                // 真正的校验/确认都在 start_import 里（与 `/import <path>` 同一条路）
                let raw = editor.text().trim().to_string();
                self.start_import(&raw, cx);
                return;
            }
            SessionDialog::ApiKey { provider, editor } => {
                let key = editor.text().trim().to_string();
                if !key.is_empty() {
                    match core::auth::write_auth_key(&provider, &key) {
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
            SessionDialog::Delete { paths, .. } => {
                let current = self.snapshot.as_ref().and_then(|s| s.session_path.clone());
                let deleting_current = current
                    .as_ref()
                    .is_some_and(|current| paths.iter().any(|p| p == current));
                let files: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
                let (deleted, failed) = core::session_manager::delete_sessions(&files);

                // 删的是当前会话：文件已经没了，接着往旧路径写会把它重建出来，必须换会话
                if deleting_current {
                    _ = self.cmd_tx.send(AgentCommand::NewSession);
                    self.transcript.clear();
                    self.flow_height.set(None);
                }

                match failed.first() {
                    None => self
                        .transcript
                        .push_note(Note::Info(format!("Deleted {deleted} session(s)"))),
                    Some((path, error)) => self.transcript.push_note(Note::Error(format!(
                        "Deleted {deleted} session(s) · {} failed: {} ({error})",
                        failed.len(),
                        path.display()
                    ))),
                }
            }
        }
        self.reload_sessions(cx);
        cx.notify();
    }

    /// 重读会话列表：目录列举留在主线程（只看 mtime），
    /// 逐条整文件解析交给后台线程，这里起一个轮询任务把就绪的条目填回来。
    pub(super) fn reload_sessions(&mut self, cx: &mut Context<Self>) {
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
    pub(super) fn refresh_session_rows(&mut self) {
        self.sessions = self
            .sessions_loader
            .as_ref()
            .map(sidebar::rows)
            .unwrap_or_default();
    }
}

/// `/import` 的参数 → 源文件路径：去掉 `@` 前缀，空参数与文件不存在各自给出错误文案。
///
/// 文案与 TUI 的 `cmd_import` 一致（同一句话在终端与桌面端都能对上搜索结果）。
pub(super) fn import_source(raw: &str, cwd: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    let arg = raw.strip_prefix('@').unwrap_or(raw).trim();
    if arg.is_empty() {
        return Err("Usage: /import <path.jsonl>".to_string());
    }

    let source = paths::resolve_path(arg, cwd);
    if !source.is_file() {
        return Err(format!(
            "import failed: Failed to read session file: {}: No such file or directory",
            source.display()
        ));
    }
    Ok(source)
}

/// 把源会话文件复制进当前会话目录，返回可直接打开的路径。
///
/// 同名目标已存在时递增编号（`name-1.jsonl`…），复制用 `O_EXCL` 作最后一道保险，
/// 绝不静默覆盖已有会话；源文件本来就在会话目录里则原样返回。
pub(super) fn import_copy(source: &Path, cwd: &str) -> Result<PathBuf, String> {
    let dir = core::session_manager::default_session_dir(cwd, &settings_manager::agent_dir());
    let Some(name) = source.file_name() else {
        return Ok(source.to_path_buf());
    };
    if dir.join(name) == source {
        return Ok(source.to_path_buf());
    }

    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("import failed: Failed to create session dir: {error}"))?;
    let destination = paths::uniquify_import_destination(&dir, name);
    paths::copy_no_overwrite(source, &destination)
        .map_err(|error| format!("import failed: Failed to copy session: {error}"))?;
    Ok(destination)
}

/// 修复面板的可选项（`(文案, 是否走抢救路径)`）：先抢救、后截断，与 TUI 面板同序。
///
/// 无法抢救（引用断裂等）时只给截断；调用方在后面补「取消」。
pub(super) fn repair_choices(plan: &SessionRepairPlan) -> Vec<(String, bool)> {
    let mut choices = Vec::new();
    if let Some(content) = plan.salvage_content.as_ref() {
        choices.push((
            format!(
                "Salvage: skip broken lines, keep {} lines ({} dropped)",
                content.lines().count(),
                plan.salvaged_skipped
            ),
            true,
        ));
    }
    choices.push((
        format!(
            "Truncate: keep prefix only ({} lines kept, {} dropped)",
            plan.valid_lines,
            plan.dropped_lines()
        ),
        false,
    ));
    choices
}

/// 未完成 operation 面板可选项（`(文案, 是否 rewrite)`）：Continue 在前、Rewrite 在后。
///
/// `count` 是检测到的未完成 operation 数（含最新一次），被标记为已结束的旧 run 数 = count - 1。
pub(super) fn zombie_choices(count: usize) -> Vec<(String, bool)> {
    vec![
        (
            "Continue: resume latest run, leave log as-is".to_string(),
            false,
        ),
        (
            format!(
                "Rewrite: mark {} older run(s) as finished (backup + append)",
                count.saturating_sub(1)
            ),
            true,
        ),
    ]
}
