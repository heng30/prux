//! 动作与斜杠命令：把面板/菜单的 [`Action`] 与 `/` 命令落到 agent 命令或状态变更上。

use super::*;
use crate::{cli, core};

impl GuiApp {
    /// 斜杠命令：与命令面板共用同一批动作。
    pub(super) fn run_slash(&mut self, name: &str, cx: &mut Context<Self>) {
        // `/name rest`：只有少数命令吃参数，其余把整串当命令名（落到下面的兜底分支）
        match name.split_once(' ') {
            Some((command, arg)) if takes_argument(command) => {
                let (command, arg) = (command.to_string(), arg.trim().to_string());
                self.run_slash_with(&command, &arg, cx);
            }
            _ => self.run_slash_with(name, "", cx),
        }
    }

    /// 斜杠命令本体：`arg` 是命令名之后的原文（不吃参数的命令收到空串）。
    fn run_slash_with(&mut self, name: &str, arg: &str, cx: &mut Context<Self>) {
        match name {
            "name" if !arg.is_empty() => {
                _ = self.cmd_tx.send(AgentCommand::SetSessionName {
                    name: arg.to_string(),
                });
            }
            "thinking" if !arg.is_empty() => {
                if cli::args::VALID_THINKING_LEVELS.contains(&arg) {
                    _ = self.cmd_tx.send(AgentCommand::SetThinking {
                        level: arg.to_string(),
                        persist: true,
                    });
                } else {
                    self.transcript.push_note(Note::Error(format!(
                        "Unknown thinking level: {arg} (valid: {})",
                        cli::args::VALID_THINKING_LEVELS.join(", ")
                    )));
                }
            }
            "export" if !arg.is_empty() => {
                _ = self.cmd_tx.send(AgentCommand::Export {
                    out: Some(arg.to_string()),
                    theme: None,
                });
            }
            "import" if !arg.is_empty() => {
                _ = self.cmd_tx.send(AgentCommand::ResumeSession {
                    path: arg.to_string(),
                });
            }
            // `/model <id>`：有 scope 时只在 scope 里匹配，否则交给 worker 按当前 provider 校验
            "model" if !arg.is_empty() => {
                let scoped = self
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.model_cycle.clone())
                    .unwrap_or_default();
                let needle = arg.to_lowercase();

                if scoped.is_empty() {
                    let provider = self
                        .snapshot
                        .as_ref()
                        .map(|snapshot| snapshot.provider.clone())
                        .unwrap_or_default();
                    _ = self.cmd_tx.send(AgentCommand::SwitchModel {
                        provider,
                        model_id: arg.to_string(),
                        persist: false,
                    });
                } else if let Some(model) = scoped.iter().find(|model| {
                    model.canonical().to_lowercase() == needle || model.id.to_lowercase() == needle
                }) {
                    _ = self.cmd_tx.send(AgentCommand::SwitchModel {
                        provider: model.provider.clone(),
                        model_id: model.id.clone(),
                        persist: false,
                    });
                } else {
                    self.transcript
                        .push_note(Note::Error(format!("model not found in scope: {arg}")));
                }
            }
            // `/session <序号>`：按会话目录里「最近修改倒序」的 1 起序号切会话。
            // 直接列目录而不是读侧栏行——侧栏是逐条回填的，刚启动时行数不全，
            // 用它会报出错误的可用范围（同 TUI 的 `cmd_session_switch`）。
            "session" if !arg.is_empty() => {
                let paths = sidebar::paths(&self.cwd());
                match arg.parse::<usize>() {
                    Ok(index) if index >= 1 && index <= paths.len() => {
                        let path = paths[index - 1].to_string_lossy().to_string();
                        _ = self.cmd_tx.send(AgentCommand::ResumeSession { path });
                    }
                    _ => self.transcript.push_note(Note::Error(format!(
                        "invalid index: {arg} (available 1-{})",
                        paths.len()
                    ))),
                }
            }
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
            "giff" => self.toggle_panel_tab(PanelTab::Changes, cx),
            "tree" => _ = self.cmd_tx.send(AgentCommand::Tree),
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
            "trust" => self.open_trust_panel(cx),
            "name" => self.rename_current_session(cx),
            "copy" => self.copy_last_reply(cx),
            "compact" => {
                _ = self
                    .cmd_tx
                    .send(AgentCommand::Compact { instructions: None });
            }
            "appearance" | "theme" => self.open_appearance_picker(cx),
            "panel" => self.toggle_panel(None, cx),
            // 停靠面板：扩展提供的状态行（内容为空时整块不渲染）
            "dock" => {
                self.dock_visible = !self.dock_visible;
                cx.notify();
            }
            "sidebar" => {
                self.show_sidebar = !self.show_sidebar;
                cx.notify();
            }
            "skills" => self.open_skills_panel(cx),
            "extension" => self.open_extension_panel(cx),
            "scoped-models" => self.open_scoped_models_panel(cx),
            // `/settings <扩展名>`：只显示该扩展的设置项（与 TUI 的扩展设置面板同语义）
            "settings" if !arg.is_empty() => self.open_extension_settings(arg, cx),
            "settings" => self.open_settings_panel(cx),
            "session" => self.open_info(InfoKind::Session, cx),
            "help" | "hotkeys" => self.open_info(InfoKind::Help, cx),
            "quit" => cx.quit(),

            // 与 UI 无关的扩展命令（proxy / notify 等）：扩展自己处理并回通知
            other => {
                // 扩展命令吃参数（`/proxy listen 1.2.3.4:80`）：命令名是首个 token
                let (command, rest) = match other.split_once(char::is_whitespace) {
                    Some((name, rest)) => (name, rest.trim()),
                    None => (other, ""),
                };

                // 扩展命令：与 TUI 共用同一份 handler（参数是宿主能力面，不是 App）
                let raw = format!("/{command} {rest}");
                if let Some(handler) = core::extensions::command_handler(command) {
                    handler(self, raw.trim_end());
                    cx.notify();
                    return;
                }

                if core::extensions::registered_commands()
                    .iter()
                    .any(|registered| registered.name == command)
                {
                    self.transcript.push_note(Note::Info(format!(
                        "Extension command /{command} is terminal-only; use the terminal UI"
                    )));
                    return;
                }

                self.run_unknown_slash(other, cx);
            }
        }
    }

    /// 兜底：不认识的斜杠命令原样还给输入框（技能命令交给 worker 展开）。
    fn run_unknown_slash(&mut self, other: &str, _cx: &mut Context<Self>) {
        match other {
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

    /// 复制最后一条助手回复（`/copy` 与 TUI 的 `app.message.copy` = Ctrl-X 共用）。
    pub(super) fn copy_last_reply(&mut self, cx: &mut Context<Self>) {
        let text = self.transcript.last_assistant_text();
        if text.is_empty() {
            self.toast("No reply to copy", ToastKind::Info, cx);
        } else {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            self.toast("Copied the last reply", ToastKind::Success, cx);
        }
    }

    /// 点击覆盖层列表的第 `visible` 行：先把该行设为选中（后续重建不跳回第一行），再执行。
    pub(crate) fn pick_entry(&mut self, visible: usize, cx: &mut Context<Self>) {
        let action = match &mut self.overlay {
            Some(Overlay::List(list)) => {
                list.selected = visible;
                let filtered = list.filtered();
                filtered
                    .get(visible)
                    .map(|index| list.entries[*index].action.clone())
            }
            _ => None,
        };

        if let Some(action) = action {
            // 勾选类动作会重建同一个面板，保留它才能沿用过滤与选中行
            if !keeps_overlay(&action) {
                // 选择/信任面板的动作已经回传结果，收起时不要再按「取消」发一次
                if let Some(Overlay::List(list)) = self.overlay.as_mut() {
                    list.choice = None;
                }
                self.dismiss_overlay();
            }
            self.run_action(action, cx);
        }

        cx.notify();
    }

    /// 执行覆盖层动作。
    pub(super) fn run_action(&mut self, action: Action, cx: &mut Context<Self>) {
        match action {
            Action::None => {}
            Action::Command(command) => _ = self.cmd_tx.send(*command),
            Action::LoadSession(path) => {
                _ = self.cmd_tx.send(AgentCommand::ResumeSession { path });
            }
            Action::SetDark(is_dark) => self.set_dark(is_dark),
            Action::Open(kind) => match kind {
                ListKind::Model => self.open_model_picker(cx),
                ListKind::Thinking => self.open_thinking_picker(cx),
                ListKind::Appearance => self.open_appearance_picker(cx),
                ListKind::PanelFiles => self.toggle_panel_tab(PanelTab::Files, cx),
            },
            Action::UiChoice { id, text } => {
                if let Some(prompt) = core::extensions::submit_ui_choice(id, text) {
                    _ = self.cmd_tx.send(AgentCommand::Prompt(prompt));
                }
            }
            Action::TrustDecision(choice) => self.resolve_trust(&choice, cx),
            Action::CycleExtensionSetting { ext, key } => {
                self.cycle_extension_setting(&ext, &key, cx);
                // 面板要留着继续改：跳过下面的「收起覆盖层」
                return;
            }
            Action::RefreshSessions => self.reload_sessions(cx),
            Action::Scale(scale) => {
                _ = settings_manager::write_settings_ui_scale(Some(theme::set_ui_scale(scale)));
            }
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
            Action::LoginProvider(provider) => {
                let editor = Editor::new();
                self.dialog = Some(SessionDialog::ApiKey { provider, editor });
                cx.notify();
            }
            Action::LogoutProvider(provider) => {
                match core::auth::remove_auth(&provider) {
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
                // 与扩展/技能面板一致：只改草稿，Ctrl-S 才落盘
                if self.scoped_draft.is_none() {
                    self.scoped_draft = Some(settings_manager::read_enabled_models());
                }

                if let Some(draft) = self.scoped_draft.as_mut() {
                    match draft.iter().position(|item| item == &pattern) {
                        Some(index) => {
                            draft.remove(index);
                        }
                        None => draft.push(pattern),
                    }
                }
                self.rebuild_scoped_models_panel(cx);
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
                    SettingKey::GuiTheme => {
                        self.set_dark(!self.theme.is_dark);
                        Ok(())
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

    /// 中止当前回合（Esc 与停止按钮共用）。
    ///
    /// worker 直接 drop 回合 future，agent 不会再发 `agent_settled`，忙状态只能
    /// 本地复位，中断提示也只能自己补（同 TUI 的硬中止）。
    pub(super) fn abort_turn(&mut self, cx: &mut Context<Self>) {
        _ = self.cmd_tx.send(AgentCommand::Abort);
        self.busy = false;
        self.transcript.abort();
        self.scroll_to_bottom();
        cx.notify();
    }

    /// 取出编辑器内容（清空、记编辑器历史、收起补全）：空内容返回 None。
    fn take_input(&mut self) -> Option<String> {
        let text = self.editor.trimmed()?;
        self.editor.clear();
        self.editor
            .push_history(&text, settings_manager::read_settings_history_max_entries());
        self.suggest = None;
        Some(text)
    }

    /// 提交输入框内容。
    ///
    /// `follow_up` 为 true（Alt+Enter）时，忙碌下排进 follow-up 队列（settle 后发送），
    /// 否则排进 steer 注入箱（回合内注入）；两者都不立即发送，也不回显进对话流。
    pub(super) fn submit(&mut self, follow_up: bool, cx: &mut Context<Self>) {
        let Some(text) = self.take_input() else {
            return;
        };

        // `!!cmd`：本地执行，输出只显示、不进对话上下文
        if let Some(command) = text.strip_prefix("!!") {
            let output = run_shell_command(command.trim());
            self.transcript
                .push_note(Note::Info(format!("$ {}\n{output}", command.trim())));
            self.scroll_to_bottom();
            cx.notify();
            return;
        }

        // `!cmd`：本地执行，输出作为用户消息进入上下文
        if let Some(command) = text.strip_prefix('!') {
            let output = run_shell_command(command.trim());
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

        // 和 TUI 一样记进项目级 prompt 历史，`/history` 才翻得到
        core::prompt_history::append(
            &self.cwd(),
            &text,
            settings_manager::read_settings_history_max_entries(),
        );

        if self.busy {
            // 忙碌：只排队，由 agent 在回合内注入 / 结算后取走，不在本地开新回合
            if follow_up {
                self.follow_ups.push_back(text);
            } else if let Ok(mut inbox) = self.steer_inbox.lock() {
                inbox.push_back(AgentMessage::user_text(&text));
            }

            cx.notify();
            return;
        }

        self.busy = true;
        self.transcript.push_user(text.clone());
        _ = self.cmd_tx.send(AgentCommand::Prompt(text));
        self.scroll_to_bottom();
    }

    /// 待发送消息数（steer 注入箱 + follow-up 队列）：状态栏「N queued」用。
    pub(super) fn pending_count(&self) -> usize {
        pending_count(&self.steer_inbox, &self.follow_ups)
    }

    /// 待发送区行（见 [`pending_lines`]）。
    pub(super) fn pending_lines(&self) -> Vec<String> {
        pending_lines(&self.steer_inbox, &self.follow_ups)
    }

    /// settle 后取下一批待发送消息（见 [`take_next_batch`]）。
    pub(super) fn drain_next_batch(&mut self) -> Option<Vec<AgentMessage>> {
        take_next_batch(&self.steer_inbox, &mut self.follow_ups)
    }

    /// Alt+Up：把排队的消息全部取回编辑器（steer 在前、follow-up 在后，空行分隔，
    /// 拼在当前草稿之前），队列清空；没有排队时提示一句。
    pub(super) fn dequeue_queued(&mut self, cx: &mut Context<Self>) {
        let Some((combined, count)) =
            restore_text(&self.steer_inbox, &mut self.follow_ups, &self.editor.text())
        else {
            self.toast("No queued messages to restore", ToastKind::Info, cx);
            return;
        };

        self.editor.clear();
        self.editor.insert(&combined);
        self.toast(
            format!(
                "Restored {count} queued message{}",
                if count > 1 { "s" } else { "" }
            ),
            ToastKind::Info,
            cx,
        );
    }

    /// 打开/切换右侧面板；`tab` 为 None 时保持当前标签。
    pub(super) fn toggle_panel(&mut self, tab: Option<PanelTab>, cx: &mut Context<Self>) {
        match tab {
            Some(tab) => {
                self.panel.tab = tab;
                self.panel.visible = true;
                self.panel.search_focus = tab == PanelTab::Files;
            }
            None => self.panel.visible = !self.panel.visible,
        }

        if !self.panel.visible {
            self.panel.search_focus = false;
            self.panel.scroll_drag = None;
            cx.notify();
            return;
        }
        self.load_panel(cx);
    }

    /// 切到某个页签：已经开着且就在这页就收起，否则打开并 reload。
    pub(super) fn toggle_panel_tab(&mut self, tab: PanelTab, cx: &mut Context<Self>) {
        if self.panel.visible && self.panel.tab == tab {
            self.panel.visible = false;
            self.panel.search_focus = false;
            self.panel.scroll_drag = None;
            cx.notify();
            return;
        }
        self.toggle_panel(Some(tab), cx);
    }

    /// 点页签按钮：切页并重读（文件列表/变更计数也跟着刷新）。
    pub(crate) fn set_panel_tab(&mut self, tab: PanelTab, cx: &mut Context<Self>) {
        self.panel.tab = tab;
        self.panel.search_focus = tab == PanelTab::Files;
        self.panel.scroll_drag = None;
        self.load_panel(cx);
        cx.notify();
    }

    /// 点 Files 页的搜索框：之后敲的字进过滤框。
    pub(crate) fn focus_panel_search(&mut self, cx: &mut Context<Self>) {
        self.panel.search_focus = true;
        cx.notify();
    }

    /// 后台读面板内容（列文件 / git status / git diff / 文件预览都不在主线程跑）。
    pub(super) fn load_panel(&mut self, cx: &mut Context<Self>) {
        let cwd = self.cwd();
        let tab = self.panel.tab;
        let selected = self.panel.selected;
        let file = self.panel.selected_file.clone();
        let syntect = self.theme.syntect();
        self.panel.loading = true;
        cx.spawn(async move |this, cx| {
            let data = cx
                .background_executor()
                .spawn(async move {
                    giff::RightPanel::load(&cwd, tab, selected, file.as_deref(), &syntect)
                })
                .await;
            _ = this.update(cx, |app, cx| {
                app.panel.apply(data);
                cx.notify();
            });
        })
        .detach();
    }

    /// 选中某条变更：切下标并在后台重读它的 diff。
    pub(crate) fn select_change(&mut self, index: usize, cx: &mut Context<Self>) {
        self.panel.selected = index;
        self.load_panel(cx);
        cx.notify();
    }

    /// Files 页选中某个文件：后台读正文并高亮。
    pub(crate) fn select_file(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.panel.files.get(index) else {
            return;
        };
        self.panel.selected_file = Some(row.path.clone());
        self.panel.search_focus = false;
        self.load_panel(cx);
        cx.notify();
    }

    /// 命令面板。
    pub(super) fn open_command_palette(&mut self, _cx: &mut Context<Self>) {
        let list = List::new("Commands", command_palette_entries());
        self.overlay = Some(Overlay::List(list));
    }

    /// `/history`：本会话发过的用户消息，Enter 写回输入框（不发出去）。
    pub(super) fn open_history_panel(&mut self, _cx: &mut Context<Self>) {
        let cwd = self.cwd();
        let max = settings_manager::read_settings_history_max_entries();
        // 与 TUI 一致：读项目级 prompt 历史，而不是当前会话上下文
        // （压缩/摘要之后上下文里只剩一两条用户消息，历史记录不该跟着丢）
        let entries = history_entries(core::prompt_history::load(&cwd, max));
        self.overlay = Some(Overlay::List(List::new("History", entries)));
    }

    /// 登录面板：列出全部支持的 provider，Enter 填 key。
    pub(super) fn open_login_panel(&mut self, _cx: &mut Context<Self>) {
        let entries: Vec<Entry> = core::model_resolver::SUPPORTED_PROVIDERS
            .iter()
            .map(|provider| Entry {
                label: provider.to_string(),
                detail: if core::auth::has_auth(provider) {
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
    pub(super) fn open_logout_panel(&mut self, _cx: &mut Context<Self>) {
        let mut entries: Vec<Entry> = core::auth::list_auth_providers()
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
    pub(super) fn open_changelog(&mut self, _cx: &mut Context<Self>) {
        let lines: Vec<String> = core::changelog::CHANGELOG
            .lines()
            .map(|line| line.to_string())
            .collect();
        self.overlay = Some(Overlay::Info(Info::new(
            format!("Changelog v{}", core::changelog::VERSION),
            lines,
        )));
    }
}

/// 吃参数的斜杠命令：`/name foo` 里的 foo 是值，不能当命令名的一部分。
fn takes_argument(command: &str) -> bool {
    matches!(
        command,
        "name" | "thinking" | "export" | "import" | "model" | "session" | "settings"
    )
}

/// Ctrl-M 命令面板的条目：只放 `/` 面板里没有的东西 + 允许两边都在的三个
/// （`skills` / `extension` / `settings`，它们有独立面板）；其余斜杠命令只从 `/` 面板进。
pub(super) fn command_palette_entries() -> Vec<Entry> {
    vec![
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
            label: "Extensions…".into(),
            detail: "Ctrl-E".into(),
            action: Action::Extensions,
            ..Entry::default()
        },
        Entry {
            label: "Switch appearance…".into(),
            detail: "Ctrl-Shift-G".into(),
            action: Action::Open(ListKind::Appearance),
            ..Entry::default()
        },
        Entry {
            label: "Zoom in".into(),
            detail: "Ctrl-=".into(),
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
            detail: "Ctrl-0".into(),
            action: Action::Scale(1.0),
            ..Entry::default()
        },
        Entry {
            label: "Files…".into(),
            detail: "Ctrl-Shift-F".into(),
            action: Action::Open(ListKind::PanelFiles),
            ..Entry::default()
        },
        Entry {
            label: "Refresh sessions".into(),
            detail: String::new(),
            action: Action::RefreshSessions,
            ..Entry::default()
        },
    ]
}
