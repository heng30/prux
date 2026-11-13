//! 动作与斜杠命令：把面板/菜单的 [`Action`] 与 `/` 命令落到 agent 命令或状态变更上。

use super::*;
use crate::{
    cli,
    core::{self, keybindings},
    modes::{gui::bridge::GuiEvent, slash},
    utils::paths,
};

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

    /// `/cwd <路径>`：校验目录存在、记入最近目录清单，再在新目录下开会话。
    ///
    /// 回合进行中不允许切目录（排队的新会话会把追问带进新目录），由
    /// [`Self::cwd_switch_blocked`] 拦下并提示。
    fn cmd_cwd_arg(&mut self, arg: &str, cx: &mut Context<Self>) {
        if self.cwd_switch_blocked(cx) {
            return;
        }

        // `@` 是输入框的引用前缀（`/cwd @../other`），与 TUI 一样先剥掉
        let arg = arg.trim();
        let arg = arg.strip_prefix('@').unwrap_or(arg).trim();
        let path = paths::resolve_path(arg, &self.cwd());
        if !path.is_dir() {
            self.transcript.push_note(Note::Error(format!(
                "/cwd: {}: No such directory",
                path.display()
            )));
            return;
        }

        // 规范路径：底栏与后续工具看到的都是干净路径（顺手消掉 `..` 与符号链接差异）
        let path = std::fs::canonicalize(&path)
            .map(|canonical| canonical.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string_lossy().to_string());
        cwd_store::remember_cwd(&path);
        _ = self.cmd_tx.send(AgentCommand::SwitchCwd { path });
    }

    /// `/session <参数>`：先认子命令（`statistics` / `rm-current` / `rm-project` /
    /// `rm-all`），其余按 1 起的序号切会话。
    ///
    /// 序号用会话目录「最近修改倒序」，直接列目录而不是读侧栏行——侧栏是逐条回填的，
    /// 刚启动时行数不全，用它会报出错误的可用范围（同 TUI 的 `cmd_session_switch`）。
    fn cmd_session_arg(&mut self, arg: &str, cx: &mut Context<Self>) {
        match arg.trim().to_ascii_lowercase().as_str() {
            "statistics" => self.open_info(InfoKind::Session, cx),
            "rm-current" | "rm-project" | "rm-all" => self.ask_delete_sessions(arg.trim(), cx),
            _ => {
                let paths = sidebar::paths(&self.cwd());
                match arg.parse::<usize>() {
                    Ok(index) if index >= 1 && index <= paths.len() => {
                        let path = paths[index - 1].to_string_lossy().to_string();
                        self.open_session_with_cwd_check(&path, cx);
                    }
                    _ => self.transcript.push_note(Note::Error(format!(
                        "invalid index: {arg} (available 1-{})",
                        paths.len()
                    ))),
                }
            }
        }
    }

    /// `/session rm-current|rm-project|rm-all`：弹删除确认（与侧栏单条删除同一个对话框）。
    fn ask_delete_sessions(&mut self, scope: &str, cx: &mut Context<Self>) {
        let current = self.snapshot.as_ref().and_then(|s| s.session_path.clone());
        let (paths, detail) = match scope {
            "rm-current" => match current {
                // 确认框正文是单行文本（GUI 对话框不做换行排版），路径在侧栏行里已可见
                Some(path) => (
                    vec![path],
                    "Delete the current session and start a new one?".to_string(),
                ),
                None => {
                    self.transcript
                        .push_note(Note::Error("No current session to delete".into()));
                    cx.notify();
                    return;
                }
            },
            "rm-project" => {
                let paths: Vec<String> = sidebar::paths(&self.cwd())
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect();
                let count = paths.len();
                let dir = core::session_manager::default_session_dir(
                    &self.cwd(),
                    &core::settings_manager::agent_dir(),
                );
                (
                    paths,
                    format!("Delete all {count} sessions in {}?", dir.display()),
                )
            }
            _ => {
                let agent_dir = core::settings_manager::agent_dir();
                let paths: Vec<String> = core::session_manager::list_all_sessions(&agent_dir)
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect();
                let count = paths.len();
                (
                    paths,
                    format!(
                        "Delete all {count} sessions in {}?",
                        agent_dir.join("sessions").display()
                    ),
                )
            }
        };
        self.ask_delete_many(paths, detail, cx);
    }

    /// 斜杠命令本体：`arg` 是命令名之后的原文（不吃参数的命令收到空串）。
    fn run_slash_with(&mut self, name: &str, arg: &str, cx: &mut Context<Self>) {
        match name {
            "name" if !arg.is_empty() => self.slash_set_name(arg),
            "thinking" if !arg.is_empty() => self.slash_switch_thinking(arg),
            "history" => self.slash_history(arg, cx),
            "export" if !arg.is_empty() => self.slash_export(arg),
            "import" if !arg.is_empty() => self.slash_import(arg, cx),
            "model" if !arg.is_empty() => self.slash_switch_model(arg),
            "session" if !arg.is_empty() => self.slash_session(arg, cx),
            "new" => self.slash_new(cx),
            "resume" => self.slash_open_sessions(cx),
            "model" => self.slash_open_models(cx),
            "thinking" => self.slash_open_thinking(cx),
            "login" => self.slash_login(cx),
            "logout" => self.slash_logout(cx),
            "changelog" => self.slash_changelog(cx),
            "giff" => self.slash_giff(cx),
            "tree" => self.slash_tree(),
            "bug" => self.slash_bug(arg, cx),
            "reload" => self.slash_reload(cx),
            "label" if !arg.is_empty() => self.slash_set_label(arg),
            "clone" => self.slash_clone(),
            "fork" => self.slash_fork(),
            "share" => self.slash_share(),
            "export" => self.slash_export_default(),
            "trust" => self.slash_trust(cx),
            "cwd" if !arg.is_empty() => self.slash_switch_cwd(arg, cx),
            "cwd" => self.slash_cwd_menu(cx),
            "copy" => self.slash_copy(cx),
            "compact" => self.slash_compact(arg),
            "theme" if !arg.is_empty() => self.slash_theme_named(arg, cx),
            "theme" => self.slash_appearance(cx),
            "panel" => self.slash_panel(cx),
            "dock" => self.slash_dock(cx),
            "sidebar" => self.slash_sidebar(cx),
            "skills" => self.slash_skills(arg, cx),
            "extension" => self.slash_extensions(cx),
            "scoped-models" => self.slash_scoped_models(cx),
            "settings" if !arg.is_empty() => self.slash_extension_settings(arg, cx),
            "settings" => self.slash_settings(cx),
            "session" => self.slash_session_info(cx),
            "hotkeys" => self.slash_help(cx),
            "quit" => self.slash_quit(cx),

            // 与 UI 无关的扩展命令（proxy / notify 等）：扩展自己处理并回通知
            other => self.slash_extension_command(other, cx),
        }
    }

    /// `/name <名>`：直接下发改名。
    fn slash_set_name(&mut self, arg: &str) {
        _ = self.cmd_tx.send(AgentCommand::SetSessionName {
            name: arg.to_string(),
        });
    }

    /// `/thinking <级别>`：级别合法就下发（并落盘），否则报可用取值。
    fn slash_switch_thinking(&mut self, arg: &str) {
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

    /// `/history [关键词]`：控制词直接弹确认；其余把整行写回输入框，
    /// 让补全面板重新接管搜索（否则关键词会被当成未知命令丢掉）。
    fn slash_history(&mut self, arg: &str, cx: &mut Context<Self>) {
        match arg {
            "@clear" => self.open_history_clear_confirm(false, cx),
            "@clear-all" => self.open_history_clear_confirm(true, cx),
            _ => {
                let line = format!("/history {arg}");
                self.editor.insert(line.trim_end());
                self.refresh_suggest(cx);
            }
        }
    }

    /// `/export <路径>`：导出到指定文件。
    fn slash_export(&mut self, arg: &str) {
        // 路径参数与 TUI 同一套解析（引号包裹的含空格路径 / 只取第一个片段）
        _ = self.cmd_tx.send(AgentCommand::Export {
            out: paths::parse_path_arg(arg),
            theme: self.export_theme(),
        });
    }

    /// `/import <路径>`：按路径导入会话文件。
    fn slash_import(&mut self, arg: &str, cx: &mut Context<Self>) {
        self.start_import(arg, cx);
    }

    /// `/model <id>`：有 scope 时只在 scope 里匹配，否则交给 worker 按当前 provider 校验。
    fn slash_switch_model(&mut self, arg: &str) {
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

    /// `/session <子命令|序号>`：见 [`Self::cmd_session_arg`]。
    fn slash_session(&mut self, arg: &str, cx: &mut Context<Self>) {
        self.cmd_session_arg(arg, cx);
    }

    /// `/new`：开新会话并清空当前展示。
    fn slash_new(&mut self, cx: &mut Context<Self>) {
        _ = self.cmd_tx.send(AgentCommand::NewSession);
        self.transcript.clear();
        self.flow_height.set(None);
        self.reload_sessions(cx);
    }

    /// `/resume`、`/sessions`：打开会话选择面板。
    fn slash_open_sessions(&mut self, cx: &mut Context<Self>) {
        self.open_session_picker(cx);
    }

    /// `/model`：打开模型选择面板。
    fn slash_open_models(&mut self, cx: &mut Context<Self>) {
        self.open_model_picker(cx);
    }

    /// `/thinking`：打开思考级别选择面板。
    fn slash_open_thinking(&mut self, cx: &mut Context<Self>) {
        self.open_thinking_picker(cx);
    }

    /// `/login`：打开登录面板。
    fn slash_login(&mut self, cx: &mut Context<Self>) {
        self.open_login_panel(cx);
    }

    /// `/logout`：打开登出面板。
    fn slash_logout(&mut self, cx: &mut Context<Self>) {
        self.open_logout_panel(cx);
    }

    /// `/changelog`：打开当前版本的内嵌更新日志。
    fn slash_changelog(&mut self, cx: &mut Context<Self>) {
        self.open_changelog(cx);
    }

    /// `/giff`：打开右侧变更面板。
    fn slash_giff(&mut self, cx: &mut Context<Self>) {
        self.toggle_panel_tab(PanelTab::Changes, cx);
    }

    /// `/tree`：显示会话树。
    fn slash_tree(&mut self) {
        _ = self.cmd_tx.send(AgentCommand::Tree);
    }

    /// `/bug [描述]`：先记下复现描述，再让用户选是否附带会话记录。
    fn slash_bug(&mut self, arg: &str, cx: &mut Context<Self>) {
        let mut editor = Editor::new();
        editor.insert(arg);
        self.dialog = Some(SessionDialog::BugHint { editor });
        cx.notify();
    }

    /// `/reload`：重读上下文文件、技能与系统提示词。
    fn slash_reload(&mut self, cx: &mut Context<Self>) {
        // 键位表也重读（同 TUI 的 /reload）：改完 keybindings.json 不用重启
        let kb = keybindings::set_global(keybindings::KeybindingsManager::load(
            &settings_manager::agent_dir(),
        ));

        for conflict in kb.conflicts() {
            self.transcript.push_note(Note::Error(format!(
                "keybindings.json conflict: {} is bound to {} ",
                conflict.key,
                conflict.keybindings.join(", ")
            )));
        }

        _ = self.cmd_tx.send(AgentCommand::Reload {
            context_files: Vec::new(),
            skills: Vec::new(),
            system_prompt: None,
            append_system_prompt: None,
        });
        cx.notify();
    }

    /// `/label <entryId> [标签]`：与 TUI 一样吃显式 id（标签为空 = 清除该节点标签）。
    fn slash_set_label(&mut self, arg: &str) {
        let (entry_id, label) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
        let label = label.trim();
        _ = self.cmd_tx.send(AgentCommand::Label {
            entry_id: entry_id.to_string(),
            label: (!label.is_empty()).then(|| label.to_string()),
        });
    }

    /// `/clone`：克隆当前会话。
    fn slash_clone(&mut self) {
        _ = self.cmd_tx.send(AgentCommand::ForkClone);
    }

    /// `/fork`：从当前会话文件分叉（内存会话没有文件可分叉，只提示）。
    fn slash_fork(&mut self) {
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

    /// `/share`：生成分享链接。
    fn slash_share(&mut self) {
        _ = self.cmd_tx.send(AgentCommand::Share {
            theme: self.export_theme(),
        });
    }

    /// 导出 / 分享用的主题名：取当前窗口明暗（桌面只有 dark / light 两套主题）。
    fn export_theme(&self) -> Option<String> {
        Some(if self.theme.is_dark { "dark" } else { "light" }.to_string())
    }

    /// `/export`（无参数）：导出到默认文件。
    fn slash_export_default(&mut self) {
        _ = self.cmd_tx.send(AgentCommand::Export {
            out: None,
            theme: self.export_theme(),
        });
    }

    /// `/trust`：打开项目信任面板。
    fn slash_trust(&mut self, cx: &mut Context<Self>) {
        self.open_trust_panel(cx);
    }

    /// `/cwd <路径>`：见 [`Self::cmd_cwd_arg`]。
    fn slash_switch_cwd(&mut self, arg: &str, cx: &mut Context<Self>) {
        self.cmd_cwd_arg(arg, cx);
    }

    /// `/cwd`（无参数）：打开最近目录菜单。
    fn slash_cwd_menu(&mut self, cx: &mut Context<Self>) {
        self.open_cwd_menu(cx);
    }

    /// `/copy`：复制最后一条助手回复。
    fn slash_copy(&mut self, cx: &mut Context<Self>) {
        self.copy_last_reply(cx);
    }

    /// `/compact [指示]`：压缩上下文，指示可空。
    fn slash_compact(&mut self, arg: &str) {
        _ = self.cmd_tx.send(AgentCommand::Compact {
            instructions: (!arg.is_empty()).then(|| arg.to_string()),
        });
    }

    /// `/appearance`、`/theme`：打开外观选择面板。
    fn slash_appearance(&mut self, cx: &mut Context<Self>) {
        self.open_appearance_picker(cx);
    }

    /// `/theme <名称>`：桌面只有 dark / light 两套主题，名称不认时列出可用值（同 TUI）。
    fn slash_theme_named(&mut self, arg: &str, cx: &mut Context<Self>) {
        let name = arg.trim().to_lowercase();
        match name.as_str() {
            "dark" => self.set_dark(true),
            "light" => self.set_dark(false),
            _ => {
                self.transcript.push_note(Note::Error(format!(
                    "unknown theme: {name} (available: dark, light)"
                )));
                cx.notify();
                return;
            }
        }
        self.toast(format!("switched theme: {name}"), ToastKind::Success, cx);
        cx.notify();
    }

    /// `/panel`：开关右侧面板。
    fn slash_panel(&mut self, cx: &mut Context<Self>) {
        self.toggle_panel(None, cx);
    }

    /// `/dock`：开关停靠区。
    fn slash_dock(&mut self, cx: &mut Context<Self>) {
        self.dock_visible = !self.dock_visible;
        cx.notify();
    }

    /// `/sidebar`：开关左侧会话列表。
    fn slash_sidebar(&mut self, cx: &mut Context<Self>) {
        self.set_sidebar_visible(!self.show_sidebar);
        cx.notify();
    }

    /// `/skills`：打开技能面板；`/skills check` 先跑运行前预检。
    fn slash_skills(&mut self, arg: &str, cx: &mut Context<Self>) {
        if arg.trim() == "check" {
            self.skills_check(cx);
            return;
        }
        self.open_skills_panel(cx);
    }

    /// `/extension`：打开扩展管理面板。
    fn slash_extensions(&mut self, cx: &mut Context<Self>) {
        self.open_extension_panel(cx);
    }

    /// `/scoped-models`：打开 Ctrl-P 循环名单面板。
    fn slash_scoped_models(&mut self, cx: &mut Context<Self>) {
        self.open_scoped_models_panel(cx);
    }

    /// `/settings <扩展名>`：只显示该扩展的设置项（与 TUI 的扩展设置面板同语义）。
    fn slash_extension_settings(&mut self, arg: &str, cx: &mut Context<Self>) {
        self.open_extension_settings(arg, cx);
    }

    /// `/settings`：打开设置面板。
    fn slash_settings(&mut self, cx: &mut Context<Self>) {
        self.open_settings_panel(cx);
    }

    /// `/session`（无参数）：显示当前会话信息。
    fn slash_session_info(&mut self, cx: &mut Context<Self>) {
        self.open_info(InfoKind::Session, cx);
    }

    /// `/help`、`/hotkeys`：显示快捷键表。
    fn slash_help(&mut self, cx: &mut Context<Self>) {
        self.open_info(InfoKind::Help, cx);
    }

    /// `/quit`：退出。
    fn slash_quit(&mut self, cx: &mut Context<Self>) {
        cx.quit();
    }

    /// 未知命令：先试扩展命令 handler，再试「仅终端可用」提示，最后当普通输入发出去。
    fn slash_extension_command(&mut self, other: &str, cx: &mut Context<Self>) {
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

    /// 兜底：不认识的斜杠命令按普通输入发送，绝不当成 UI 命令吞掉。
    ///
    /// `/skill:名称` 与 `/模板名 参数` 都是由 worker 展开成提示词的，所以两者走同一条路；
    /// 展开结果才是发给模型的内容（对话区显示的是原文）。
    fn run_unknown_slash(&mut self, other: &str, cx: &mut Context<Self>) {
        self.send_prompt(format!("/{other}"), false, cx);
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

    /// 鼠标移到覆盖层列表某一行：高亮跟着走（与键盘共用 `selected`，只有一束高亮）。
    pub(crate) fn hover_entry(&mut self, visible: usize, cx: &mut Context<Self>) {
        if let Some(Overlay::List(list)) = self.overlay.as_mut() {
            if list.selected == visible {
                return;
            }
            list.selected = visible;
            cx.notify();
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
            Action::None => self.act_none(),
            Action::Command(command) => self.act_command(command),
            Action::LoadSession(path) => self.act_load_session(path, cx),
            Action::SetDark(is_dark) => self.act_set_dark(is_dark),
            Action::Open(kind) => self.act_open(kind, cx),
            Action::UiChoice { id, text } => self.act_ui_choice(id, text),
            Action::TrustDecision(choice) => self.act_trust_decision(&choice, cx),
            Action::SettingsOverwrite(overwrite) => self.act_settings_overwrite(overwrite, cx),
            Action::ToggleBuiltinTool(name) => self.act_toggle_builtin_tool(name, cx),
            Action::CycleExtensionSetting { ext, key } => {
                self.act_cycle_extension_setting(&ext, &key, cx)
            }
            Action::RefreshSessions => self.act_refresh_sessions(cx),
            Action::Scale(scale) => self.act_scale(scale),
            Action::SwitchCwdTo(path) => self.act_switch_cwd_to(path, cx),
            Action::BrowseCwd => self.act_browse_cwd(cx),
            Action::Settings => self.act_settings(cx),
            Action::ToggleSkill { name, enable } => self.act_toggle_skill(name, enable, cx),
            Action::HistoryClear(all) => self.act_history_clear(all, cx),
            Action::ImportConfirm => self.act_import_confirm(cx),
            Action::ImportCwdContinue => self.act_import_cwd_continue(),
            Action::ImportCwdFork => self.act_import_cwd_fork(),
            Action::SessionRepair(salvage) => self.act_session_repair(salvage, cx),
            Action::BugReport {
                hint,
                include_session,
            } => self.act_bug_report(hint, include_session, cx),
            Action::BugReportDelivery {
                hint,
                include_session,
                include_summary,
            } => self.act_bug_report_delivery(hint, include_session, include_summary, cx),
            Action::BugReportExport {
                hint,
                include_session,
                include_summary,
            } => self.act_bug_report_export(hint, include_session, include_summary),
            Action::ResumeZombie { path, rewrite } => self.act_resume_zombie(path, rewrite),
            Action::ZombieCancel => self.act_zombie_cancel(),
            Action::SessionCancel => self.act_session_cancel(cx),
            Action::LoginAuthType(kind) => self.act_login_auth_type(&kind, cx),
            Action::LoginProvider(provider) => self.act_login_provider(provider, cx),
            Action::LoginOauth(provider) => self.act_login_oauth(provider, cx),
            Action::LoginMethod {
                provider,
                copy_code,
            } => self.act_login_method(provider, copy_code, cx),
            Action::LogoutProvider(provider) => self.act_logout_provider(provider, cx),
            Action::Extensions => self.act_extensions(cx),
            Action::Skills => self.act_skills(cx),
            Action::ToggleExtension(name) => self.act_toggle_extension(name, cx),
            Action::ToggleScopedModel(pattern) => self.act_toggle_scoped_model(pattern, cx),
            Action::ToggleSetting(key) if key.is_number() => {
                self.act_toggle_number_setting(key, cx)
            }
            Action::ToggleSetting(key) => self.act_toggle_setting(key, cx),
            Action::ToggleSidebar => self.act_toggle_sidebar(),
        }
        cx.notify();
    }

    /// `Action::None`：纯展示行被回车，什么也不做。
    fn act_none(&mut self) {}

    /// `Action::Command`：把命令原样下发给 worker。
    fn act_command(&mut self, command: Box<AgentCommand>) {
        _ = self.cmd_tx.send(*command);
    }

    /// `Action::LoadSession`：切到某个会话文件（cwd 已消失时先问 Continue / Fork / Cancel）。
    fn act_load_session(&mut self, path: String, cx: &mut Context<Self>) {
        self.open_session_with_cwd_check(&path, cx);
    }

    /// `Action::SetDark`：切明暗主题。
    fn act_set_dark(&mut self, is_dark: bool) {
        self.set_dark(is_dark);
    }

    /// `Action::Open`：打开另一个列表面板。
    fn act_open(&mut self, kind: ListKind, cx: &mut Context<Self>) {
        match kind {
            ListKind::Model => self.open_model_picker(cx),
            ListKind::Thinking => self.open_thinking_picker(cx),
            ListKind::Appearance => self.open_appearance_picker(cx),
            ListKind::PanelFiles => self.toggle_panel_tab(PanelTab::Files, cx),
            ListKind::WorkingDirectory => self.open_cwd_menu(cx),
        }
    }

    /// `Action::UiChoice`：把扩展选择面板的结果回传给发起方。
    fn act_ui_choice(&mut self, id: u64, text: Option<String>) {
        if let Some(prompt) = core::extensions::submit_ui_choice(id, text) {
            _ = self.cmd_tx.send(AgentCommand::Prompt(prompt));
        }
    }

    /// `Action::TrustDecision`：落实某个项目信任决策。
    fn act_trust_decision(&mut self, choice: &str, cx: &mut Context<Self>) {
        self.resolve_trust(choice, cx);
    }

    /// `Action::ToggleBuiltinTool`：切换某个内置工具的启用态并重开勾选面板。
    fn act_toggle_builtin_tool(&mut self, name: String, cx: &mut Context<Self>) {
        let enabled_now = panels::builtin_tools_enabled().contains(&name);
        match settings_manager::toggle_settings_default_tool(&name, enabled_now) {
            Ok(_) => self.open_builtin_tools_panel(cx),
            Err(error) => {
                self.transcript
                    .push_note(Note::Error(format!("Cannot write settings: {error}")));
                cx.notify();
            }
        }
    }

    /// `Action::SettingsOverwrite`：落实 settings.json 的覆写确认。
    fn act_settings_overwrite(&mut self, overwrite: bool, cx: &mut Context<Self>) {
        self.settings_overwrite_asked = None;
        if overwrite {
            match settings_manager::confirm_settings_overwrite() {
                Ok(()) => self.toast("Overwrote invalid settings file", ToastKind::Success, cx),
                Err(error) => self.toast(
                    format!("failed to save settings: {error}"),
                    ToastKind::Info,
                    cx,
                ),
            }
        } else {
            settings_manager::cancel_settings_overwrite();
            self.toast(
                "Settings not saved: file is not valid JSON",
                ToastKind::Info,
                cx,
            );
        }
        cx.notify();
    }

    /// `Action::CycleExtensionSetting`：把扩展的某个设置循环到下一个候选值（面板留着继续改）。
    fn act_cycle_extension_setting(&mut self, ext: &str, key: &str, cx: &mut Context<Self>) {
        self.cycle_extension_setting(ext, key, cx);
    }

    /// `Action::RefreshSessions`：重读会话列表。
    fn act_refresh_sessions(&mut self, cx: &mut Context<Self>) {
        self.reload_sessions(cx);
    }

    /// `Action::Scale`：改界面缩放系数并落盘。
    fn act_scale(&mut self, scale: f32) {
        _ = settings_manager::write_settings_ui_scale(Some(theme::set_ui_scale(scale)));
    }

    /// `Action::SwitchCwdTo`：切到最近目录清单里的某个目录。
    fn act_switch_cwd_to(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.cwd_switch_blocked(cx) {
            cwd_store::remember_cwd(&path);
            _ = self.cmd_tx.send(AgentCommand::SwitchCwd { path });
        }
    }

    /// `Action::BrowseCwd`：打开系统目录选择器。
    fn act_browse_cwd(&mut self, cx: &mut Context<Self>) {
        if !self.cwd_switch_blocked(cx) {
            self.pick_working_directory(cx);
        }
    }

    /// `Action::Settings`：打开设置面板。
    fn act_settings(&mut self, cx: &mut Context<Self>) {
        self.open_settings_panel(cx);
    }

    /// `Action::ToggleSkill`：在草稿里把某个技能切到目标启停态（Ctrl-S 才落盘）。
    fn act_toggle_skill(&mut self, name: String, enable: bool, cx: &mut Context<Self>) {
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

    /// `Action::HistoryClear`：确认后的历史清理（`all` = 连别的项目一起清）。
    fn act_history_clear(&mut self, all: bool, cx: &mut Context<Self>) {
        self.clear_history_files(all, cx);
    }

    /// `Action::ImportConfirm`：用户确认后真正开始导入。
    fn act_import_confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(PendingSession::Confirm { source }) = self.pending_session.take() {
            self.run_import(source, cx);
        }
    }

    /// `Action::ImportCwdContinue`：会话记录的目录与当前不同，仍就在当前目录恢复（下次仍会问）。
    fn act_import_cwd_continue(&mut self) {
        if let Some(PendingSession::Cwd { destination }) = self.pending_session.take() {
            self.resume_session_file(destination);
        }
    }

    /// `Action::ImportCwdFork`：把会话复制成当前目录下的新会话，原会话保留。
    /// 与 TUI 的 `Fork` 选项、启动参数 `-s <别处>` 修好后的去向同一条路（`ForkFrom`）。
    fn act_import_cwd_fork(&mut self) {
        if let Some(PendingSession::Cwd { destination }) = self.pending_session.take() {
            _ = self.cmd_tx.send(AgentCommand::ForkFrom {
                path: destination.to_string_lossy().to_string(),
                dir: None,
            });
        }
    }

    /// `Action::SessionRepair`：损坏会话的抢救（`true`）或截断（`false`）。
    fn act_session_repair(&mut self, salvage: bool, cx: &mut Context<Self>) {
        self.repair_session(salvage, cx);
    }

    /// `Action::BugReport`：答完「是否附带会话记录」后决定下一步。
    ///
    /// 与 TUI 一致：附带会话记录就直接问导出方式（摘要多余），否则再问一句要不要模型摘要。
    fn act_bug_report(&mut self, hint: String, include_session: bool, cx: &mut Context<Self>) {
        if include_session {
            self.act_bug_report_delivery(hint, true, false, cx);
            return;
        }

        self.ask_choice(
            "Attach a summary written by the current model instead?",
            vec![
                (
                    "Yes, generate a summary".to_string(),
                    Action::BugReportDelivery {
                        hint: hint.clone(),
                        include_session: false,
                        include_summary: true,
                    },
                ),
                (
                    "No".to_string(),
                    Action::BugReportDelivery {
                        hint,
                        include_session: false,
                        include_summary: false,
                    },
                ),
                ("Cancel".to_string(), Action::None),
            ],
            cx,
        );
    }

    /// `Action::BugReportDelivery`：最后一步——选导出方式（zip / 取消）。
    fn act_bug_report_delivery(
        &mut self,
        hint: String,
        include_session: bool,
        include_summary: bool,
        cx: &mut Context<Self>,
    ) {
        let title = format!(
            "Bug report — transcript: {}, summary: {}",
            if include_session {
                "included"
            } else {
                "not included"
            },
            if include_summary {
                "written by the current model"
            } else {
                "none"
            },
        );
        self.ask_choice(
            title,
            vec![
                (
                    "Export as Zip".to_string(),
                    Action::BugReportExport {
                        hint: hint.clone(),
                        include_session,
                        include_summary,
                    },
                ),
                ("Cancel".to_string(), Action::None),
            ],
            cx,
        );
    }

    /// `Action::BugReportExport`：真的打包诊断信息。
    fn act_bug_report_export(
        &mut self,
        hint: String,
        include_session: bool,
        include_summary: bool,
    ) {
        _ = self.cmd_tx.send(AgentCommand::BugReport {
            hint,
            include_session,
            include_summary,
        });
    }

    /// `Action::ResumeZombie`：打开未完成 operation 的会话（`rewrite` = 先把旧 run 标记结束）。
    fn act_resume_zombie(&mut self, path: String, rewrite: bool) {
        _ = self.cmd_tx.send(AgentCommand::ResumeZombie {
            path,
            action: if rewrite { "rewrite" } else { "continue" }.to_string(),
        });
    }

    /// `Action::ZombieCancel`：不打开未完成 operation 的会话。
    fn act_zombie_cancel(&mut self) {
        self.transcript
            .push_note(Note::Info("Session left unopened".to_string()));
    }

    /// `Action::SessionCancel`：取消当下的会话操作。
    fn act_session_cancel(&mut self, cx: &mut Context<Self>) {
        self.cancel_session_op(cx);
    }

    /// `Action::LoginAuthType`：按认证方式列出 provider。
    fn act_login_auth_type(&mut self, kind: &str, cx: &mut Context<Self>) {
        self.open_login_provider_panel(kind == "oauth", cx);
    }

    /// `Action::LoginProvider`：打开 API key 输入框。
    fn act_login_provider(&mut self, provider: String, cx: &mut Context<Self>) {
        let editor = Editor::new();
        self.dialog = Some(SessionDialog::ApiKey { provider, editor });
        cx.notify();
    }

    /// `Action::LoginOauth`：开始 OAuth 登录（能复制授权码的 provider 先问用哪种方式）。
    fn act_login_oauth(&mut self, provider: String, cx: &mut Context<Self>) {
        // 需要先选方式的 provider 先问一句，其余直接开流程
        if core::oauth::supports_copy_code_login(&provider) {
            let name = core::login_registry::provider_name(&provider);
            self.ask_choice(
                format!("Login to {name}"),
                vec![
                    (
                        "Browser login · opens a local callback page".to_string(),
                        Action::LoginMethod {
                            provider: provider.clone(),
                            copy_code: false,
                        },
                    ),
                    (
                        "Copy code login · paste the code shown on the page".to_string(),
                        Action::LoginMethod {
                            provider,
                            copy_code: true,
                        },
                    ),
                ],
                cx,
            );
        } else {
            self.start_oauth_login(provider, false, cx);
        }
    }

    /// `Action::LoginMethod`：按选定的方式启动 OAuth 登录。
    fn act_login_method(&mut self, provider: String, copy_code: bool, cx: &mut Context<Self>) {
        self.start_oauth_login(provider, copy_code, cx);
    }

    /// `Action::LogoutProvider`：清掉某个 provider 的凭据后重开登出面板。
    fn act_logout_provider(&mut self, provider: String, cx: &mut Context<Self>) {
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

    /// `Action::Extensions`：打开扩展管理面板。
    fn act_extensions(&mut self, cx: &mut Context<Self>) {
        self.open_extension_panel(cx);
    }

    /// `Action::Skills`：打开技能面板。
    fn act_skills(&mut self, cx: &mut Context<Self>) {
        self.open_skills_panel(cx);
    }

    /// `Action::ToggleExtension`：在草稿里切换某个扩展的启用状态（Ctrl-S 才落盘）。
    fn act_toggle_extension(&mut self, name: String, cx: &mut Context<Self>) {
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

    /// `Action::ToggleScopedModel`：在草稿里增删某个模型的 Ctrl-P 循环名单资格。
    fn act_toggle_scoped_model(&mut self, pattern: String, cx: &mut Context<Self>) {
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

    /// `Action::ToggleSetting`（数值项）：打开数字输入框。
    fn act_toggle_number_setting(&mut self, key: SettingKey, cx: &mut Context<Self>) {
        self.start_number_setting(key, cx);
    }

    /// `Action::ToggleSetting`（开关项）：取反落盘后重开面板，让标签显示新值。
    fn act_toggle_setting(&mut self, key: SettingKey, cx: &mut Context<Self>) {
        let result = match key {
            SettingKey::AutoCompact => settings_manager::write_settings_auto_compact(
                !settings_manager::read_settings_auto_compact(),
            ),
            SettingKey::Retry => settings_manager::write_settings_retry_enabled(
                !settings_manager::read_settings_retry_enabled(),
            ),
            SettingKey::HideThinking => {
                let hide = !settings_manager::read_settings_hide_thinking();
                self.show_thinking = !hide;
                settings_manager::write_settings_hide_thinking(hide)
            }
            SettingKey::ImagesAutoResize => settings_manager::write_settings_images_auto_resize(
                !settings_manager::read_settings_images_auto_resize(),
            ),
            SettingKey::ImagesBlockImages => settings_manager::write_settings_images_block_images(
                !settings_manager::read_settings_images_block_images(),
            ),
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
            SettingKey::GuiTheme => {
                self.set_dark(!self.theme.is_dark);
                Ok(())
            }
            SettingKey::Mermaid => {
                self.transcript.invalidate_rendering();
                settings_manager::write_settings_mermaid(!settings_manager::read_settings_mermaid())
            }
            SettingKey::LaTeX => {
                self.transcript.invalidate_rendering();
                settings_manager::write_settings_latex(!settings_manager::read_settings_latex())
            }
            SettingKey::AutocompleteMaxVisible => {
                const TIERS: [usize; 6] = [3, 5, 7, 10, 15, 20];
                let current = settings_manager::read_settings_autocomplete_max_visible();
                let next = TIERS
                    .iter()
                    .find(|tier| **tier > current)
                    .copied()
                    .unwrap_or(TIERS[0]);
                settings_manager::write_settings_autocomplete_max_visible(next)
            }
            SettingKey::HistoryMaxEntries => {
                let current = settings_manager::read_settings_history_max_entries();
                let next = settings_manager::HISTORY_MAX_ENTRIES_TIERS
                    .iter()
                    .find(|tier| **tier > current)
                    .copied()
                    .unwrap_or(settings_manager::HISTORY_MAX_ENTRIES_TIERS[0]);
                settings_manager::write_settings_history_max_entries(next)
            }
            SettingKey::WheelScrollLines => {
                use crate::utils::wheel::WheelScrollLines;
                let current = settings_manager::read_settings_fullscreen_wheel_scroll_lines();
                let next = match current {
                    WheelScrollLines::Auto => WheelScrollLines::Lines(1),
                    WheelScrollLines::Lines(1) => WheelScrollLines::Lines(2),
                    WheelScrollLines::Lines(2) => WheelScrollLines::Lines(3),
                    WheelScrollLines::Lines(3) => WheelScrollLines::Lines(5),
                    WheelScrollLines::Lines(5) => WheelScrollLines::Lines(10),
                    WheelScrollLines::Lines(_) => WheelScrollLines::Auto,
                };
                self.wheel_accel.set_lines(next);
                settings_manager::write_settings_fullscreen_wheel_scroll_lines(next)
            }
            SettingKey::ShareDestination => {
                use crate::core::settings_manager::SessionShareDestination;
                let next = match settings_manager::read_settings_session_share_destination() {
                    SessionShareDestination::Auto => SessionShareDestination::Gist,
                    SessionShareDestination::Gist => SessionShareDestination::Radius,
                    SessionShareDestination::Radius => SessionShareDestination::Auto,
                };
                settings_manager::write_settings_session_share_destination(next)
            }
            SettingKey::DefaultProjectTrust => {
                let next = match settings_manager::read_settings()
                    .default_project_trust
                    .as_deref()
                {
                    Some("always") => "never",
                    Some("never") => "ask",
                    _ => "always",
                };
                settings_manager::write_settings_default_project_trust(next)
            }
            SettingKey::BuiltinTools => {
                self.open_builtin_tools_panel(cx);
                return;
            }
            SettingKey::GuiFrame => {
                self.set_frame_gui(!settings_manager::read_settings_frame_gui(), cx);
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

    /// `Action::ToggleSidebar`：开关左侧会话列表。
    fn act_toggle_sidebar(&mut self) {
        self.set_sidebar_visible(!self.show_sidebar);
    }

    /// 回合进行中不允许切工作目录：worker 会把命令排到回合结束，届时新会话清空上下文，
    /// 排队中的追问会落到新目录里。返回 `true` 表示已被拦下并给了提示。
    pub(super) fn cwd_switch_blocked(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.busy {
            return false;
        }

        self.toast(
            "Wait for the current turn to finish before switching directory",
            ToastKind::Info,
            cx,
        );
        true
    }

    /// 中止当前回合（Esc 与停止按钮共用）。
    ///
    /// worker 直接 drop 回合 future，agent 不会再发 `agent_settled`，忙状态只能
    /// 本地复位，中断提示也只能自己补（同 TUI 的硬中止）。
    pub(super) fn abort_turn(&mut self, cx: &mut Context<Self>) {
        // 回合 future 被 drop 后不会再有 agent_end，补一条走与正常事件相同的分发路径：
        // 靠事件断电的扩展（hang-detect 等）才不会以为回合还在跑、静默超时后把已停下的
        // 会话又拉起来；本地的状态栏计量（⌛ 耗时 / 速率 / 在途流式计数）也只有收到它才会定格。
        self.apply_event(
            GuiEvent::Agent(serde_json::json!({
                "type": "agent_end",
                "reason": "aborted",
            })),
            cx,
        );
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
            // 空输入按 Enter：把对话流拉回最新的输出（重新接管钉底）
            self.pinned_bottom.set(Some(gpui::px(0.)));
            self.scroll_to_bottom();
            cx.notify();
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
            let cwd = self.cwd();
            self.transcript.push_user(message.clone(), &cwd);
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

        self.send_prompt(text, follow_up, cx);
    }

    /// 把一段文本作为用户消息发出：展开、记历史、回显、忙碌时排队（同 TUI 的提交路径）。
    ///
    /// `/skill:名` 与 `/模板名 参数` 在这里就展开成真正的提示词：忙碌时排队的消息由
    /// agent 直接注入、不经过 worker 的 `Prompt` 分支，到那里再展开就晚了
    /// （模板会原样发给模型）。worker 侧仍保留同一次展开，兜住扩展自己发起的 Prompt。
    fn send_prompt(&mut self, text: String, follow_up: bool, cx: &mut Context<Self>) {
        // 历史里留用户敲的原文（同 TUI）：展开后的提示词又长又只有这一次有意义
        core::prompt_history::append(
            &self.cwd(),
            &text,
            settings_manager::read_settings_history_max_entries(),
        );
        let text = slash::expand_user_text(&text, &self.skills, &self.prompt_templates);

        // 与 TUI 的提交路径同源：扩展靠这条广播收素材（auto-title 的首条输入）。
        core::extensions::dispatch_user_submit(&text, self.busy);

        // `@图片` 展开成图片附件（同 TUI 的提交路径）；follow-up 只带文本，与 TUI 的排队路径一致
        let (text, images) = if follow_up {
            (text, Vec::new())
        } else {
            cli::file_processor::expand_at_image_refs(&text, &self.cwd(), self.image_limits())
        };

        if self.busy {
            // 忙碌：只排队，由 agent 在回合内注入 / 结算后取走，不在本地开新回合
            if follow_up {
                self.follow_ups.push_back(text);
            } else if let Ok(mut inbox) = self.steer_inbox.lock() {
                inbox.push_back(message_with_images(&text, &images));
            }

            cx.notify();
            return;
        }

        self.busy = true;
        let cwd = self.cwd();
        self.transcript.push_user(text.clone(), &cwd);
        _ = self.cmd_tx.send(if images.is_empty() {
            AgentCommand::Prompt(text)
        } else {
            AgentCommand::PromptMessage(message_with_images(&text, &images))
        });
        self.scroll_to_bottom();
    }

    /// 当前模型的内联图片限制（与 TUI 同源：模型条目的 `inputLimits.images.resize`）。
    fn image_limits(&self) -> ImageResizeLimits {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| {
                core::model_resolver::find_model(&snapshot.provider, &snapshot.model).ok()
            })
            .map(|entry| entry.input_limits.images.resize)
            .unwrap_or_default()
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

    /// 把排队的消息全部取回编辑器（steer 在前、follow-up 在后，空行分隔，
    /// 拼在当前草稿之前），队列清空；返回取回的条数，两个队列都空时返回 None（不动编辑器）。
    pub(super) fn restore_queued(&mut self) -> Option<usize> {
        let (combined, count) =
            restore_text(&self.steer_inbox, &mut self.follow_ups, &self.editor.text())?;
        self.editor.clear();
        self.editor.insert(&combined);
        Some(count)
    }

    /// Alt+Up：把排队的消息全部取回编辑器（见 [`Self::restore_queued`]）；没有排队时提示一句。
    pub(super) fn dequeue_queued(&mut self, cx: &mut Context<Self>) {
        let Some(count) = self.restore_queued() else {
            self.toast("No queued messages to restore", ToastKind::Info, cx);
            return;
        };

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

    /// 点侧栏搜索框 / `Ctrl-Shift-H`：之后敲的字进会话过滤框（侧栏收起时一并展开，
    /// Files 面板的搜索框同时让出焦点）。
    pub(crate) fn focus_session_search(&mut self, cx: &mut Context<Self>) {
        self.set_sidebar_visible(true);
        self.session_search_focus = true;
        self.panel.search_focus = false;
        cx.notify();
    }

    /// 点 Files 页的搜索框：之后敲的字进过滤框。
    pub(crate) fn focus_panel_search(&mut self, cx: &mut Context<Self>) {
        self.panel.search_focus = true;
        self.session_search_focus = false;
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

    /// `/history @clear` / `@clear-all`（历史补全面板里的控制词）：先弹 Yes/No 确认面板。
    ///
    /// 无文件可删时不弹窗，只给一句提示（否则用户会对着一个空确认框发楞）。
    pub(super) fn open_history_clear_confirm(&mut self, all: bool, cx: &mut Context<Self>) {
        let cwd = self.cwd();
        let (title, detail) = if all {
            let (files, entries) = core::prompt_history::all_summary();
            if files == 0 {
                self.toast("No prompt history files to clear", ToastKind::Info, cx);
                return;
            }

            (
                "Clear ALL prompt history".to_string(),
                format!(
                    "{files} files · {entries} entries (every project) · {}",
                    core::prompt_history::history_dir().display()
                ),
            )
        } else {
            match core::prompt_history::project_summary(&cwd) {
                Some((name, entries)) => (
                    "Clear prompt history for this project".to_string(),
                    format!("{name} · {entries} entries"),
                ),
                None => {
                    self.toast("No prompt history for this project", ToastKind::Info, cx);
                    return;
                }
            }
        };

        let entries = vec![
            Entry {
                label: "Yes, delete".to_string(),
                detail,
                action: Action::HistoryClear(all),
                ..Entry::default()
            },
            Entry {
                label: "Cancel".to_string(),
                detail: String::new(),
                action: Action::None,
                ..Entry::default()
            },
        ];
        self.overlay = Some(Overlay::List(List::new(title, entries)));
        cx.notify();
    }

    /// 确认后的删除：连内存里的输入历史一起清，否则删除后 ↑ 还能翻出已不存在的条目。
    fn clear_history_files(&mut self, all: bool, cx: &mut Context<Self>) {
        let done = if all {
            core::prompt_history::clear_all()
        } else {
            core::prompt_history::clear_project(&self.cwd())
        };
        self.editor.clear_history();

        let mut msg = if all {
            format!(
                "Cleared all prompt history ({} files, {} entries)",
                done.files, done.entries
            )
        } else {
            format!(
                "Cleared prompt history for this project ({} entries)",
                done.entries
            )
        };

        if done.removed_dir {
            msg.push_str(" · removed empty history directory");
        }

        self.toast(msg, ToastKind::Success, cx);
    }

    /// `/login` 第一步：选认证方式（账号 OAuth / API key）。
    pub(super) fn open_login_panel(&mut self, cx: &mut Context<Self>) {
        self.ask_choice(
            "Select authentication method",
            vec![
                (
                    "Sign in with an account".to_string(),
                    Action::LoginAuthType("oauth".to_string()),
                ),
                (
                    "Sign in with an API key".to_string(),
                    Action::LoginAuthType("api_key".to_string()),
                ),
                ("Cancel".to_string(), Action::None),
            ],
            cx,
        );
    }

    /// `/login` 第二步：按认证方式列出 provider。
    ///
    /// 状态列与 TUI 一致：已存凭据 `✓ stored`、环境变量 `✓ env (VAR)`、都没有
    /// `• not configured`；models.json 里显式声明的 apiKey 会压过环境变量（同 TUI）。
    pub(super) fn open_login_provider_panel(&mut self, oauth: bool, cx: &mut Context<Self>) {
        let ids = if oauth {
            core::login_registry::oauth_provider_ids()
        } else {
            core::login_registry::api_key_provider_ids()
        };
        let declared = core::model_config::read_models_config();

        let entries: Vec<Entry> = ids
            .iter()
            .map(|id| {
                let stored = core::auth::has_auth(id);
                let env = if declared.get(*id).is_some_and(|c| c.api_key.is_some()) {
                    None
                } else {
                    core::auth::env_key_source(id)
                };
                let status = match (stored, env) {
                    (true, Some(var)) => format!("✓ stored · env ({var})"),
                    (true, None) => "✓ stored".to_string(),
                    (false, Some(var)) => format!("✓ env ({var})"),
                    (false, None) => "• not configured".to_string(),
                };
                Entry {
                    label: core::login_registry::provider_name(id),
                    cells: vec![status],
                    action: if oauth {
                        Action::LoginOauth((*id).to_string())
                    } else {
                        Action::LoginProvider((*id).to_string())
                    },
                    ..Entry::default()
                }
            })
            .collect();

        let mut list = List::new(
            if oauth {
                "Login — select account provider"
            } else {
                "Login — select provider"
            },
            entries,
        );
        list.columns = vec![
            Column::flex("Provider"),
            Column::fixed("Status", TABLE_STATUS_W),
        ];
        self.overlay = Some(Overlay::List(list));
        cx.notify();
    }

    /// 启动 OAuth 登录：流程跑在后台任务里，进度由 [`Self::poll_oauth`] 送回面板。
    pub(super) fn start_oauth_login(
        &mut self,
        provider: String,
        copy_code: bool,
        cx: &mut Context<Self>,
    ) {
        let started = if copy_code {
            core::oauth::start_copy_code_login(&provider)
        } else {
            core::oauth::start_login(&provider)
        };
        let login = match started {
            Ok(login) => login,
            Err(error) => {
                self.transcript
                    .push_note(Note::Error(format!("OAuth setup failed: {error}")));
                cx.notify();
                return;
            }
        };

        let title = format!("Login to {}", login.provider_name);
        let (input_tx, input_rx) = std::sync::mpsc::channel();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));

        if !crate::utils::async_task::spawn(crate::modes::gui::oauth::run(
            login,
            input_rx,
            cancel.clone(),
            event_tx,
        )) {
            self.transcript.push_note(Note::Error(
                "OAuth login needs an async runtime, which is unavailable here".to_string(),
            ));
            cx.notify();
            return;
        }

        // 轮询任务只负责把通道里的进度搬进面板；关掉面板（丢弃本结构）就停
        let poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(120))
                    .await;
                let done = this.update(cx, |app, cx| {
                    let done = app.poll_oauth(cx);
                    cx.notify();
                    done
                });
                if done.unwrap_or(true) {
                    break;
                }
            }
        });

        self.dialog = Some(SessionDialog::Oauth {
            title,
            panel: OauthPanel {
                note: Some("Starting...".to_string()),
                ..OauthPanel::default()
            },
            editor: Editor::new(),
        });
        self.oauth = Some(OauthFlow {
            input: input_tx,
            cancel,
            events: event_rx,
            _poll: poll,
        });
        cx.notify();
    }

    /// 把 OAuth 流程的进度应用到登录面板。返回 true 表示流程已结束（轮询任务可停）。
    pub(super) fn poll_oauth(&mut self, cx: &mut Context<Self>) -> bool {
        let events: Vec<OauthEvent> = {
            let Some(flow) = self.oauth.as_ref() else {
                return true;
            };
            let mut events = Vec::new();
            while let Ok(event) = flow.events.try_recv() {
                events.push(event);
            }
            events
        };

        for event in events {
            match event {
                OauthEvent::Show(panel) => {
                    // 授权地址 / 验证 URI 落面板即复制到系统剪贴板（TUI 同款提示进消息流）
                    if let Some(url) = panel.url.as_ref() {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(url.clone()));
                        self.transcript
                            .push_note(Note::Info(format!("Login URL copied to clipboard: {url}")));
                    }
                    if let Some(SessionDialog::Oauth { panel: current, .. }) = &mut self.dialog {
                        *current = panel;
                    }
                }
                OauthEvent::Note(text) => self.transcript.push_note(Note::Info(text)),
                OauthEvent::Done(result) => {
                    self.dialog = None;
                    self.finish_oauth(result);
                    return true;
                }
            }
        }
        false
    }

    /// OAuth 结束：成功则写凭据并让 worker 即时用上，失败只提示。
    fn finish_oauth(&mut self, result: Result<(String, core::oauth::OAuthCredential), String>) {
        let (provider, cred) = match result {
            Ok(done) => done,
            Err(message) => {
                self.transcript.push_note(Note::Error(message));
                return;
            }
        };

        match core::auth::write_oauth_credential(&provider, &cred) {
            Ok(()) => {
                let name = core::login_registry::provider_name(&provider);
                self.transcript.push_note(Note::Info(format!(
                    "Saved OAuth account for {name}. Credentials saved to {}",
                    core::auth::auth_path().display()
                )));
                _ = self.cmd_tx.send(AgentCommand::ApplyKey {
                    provider,
                    key: cred.access,
                });
            }
            Err(error) => self.transcript.push_note(Note::Error(format!(
                "Failed to save credentials for {provider}: {error}"
            ))),
        }
    }

    /// 关掉 OAuth 登录：通知流程退出并释放回调端口。
    pub(super) fn cancel_oauth(&mut self) {
        if let Some(flow) = self.oauth.take() {
            flow.cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
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
        if let Some(Overlay::List(list)) = self.overlay.as_mut() {
            list.hint = Some("↑↓ select · Enter clear key · Esc close".to_string());
        }
    }

    /// 更新日志：当前版本的内嵌 changelog，按 Markdown 渲染在信息面板里。
    pub(super) fn open_changelog(&mut self, _cx: &mut Context<Self>) {
        let lines: Vec<String> = core::changelog::CHANGELOG
            .lines()
            .map(|line| line.to_string())
            .collect();
        self.overlay = Some(Overlay::Info(
            Info::new(format!("Changelog v{}", core::changelog::VERSION), lines).into_markdown(),
        ));
    }
}

/// 吃参数的斜杠命令：`/name foo` 里的 foo 是值，不能当命令名的一部分。
fn takes_argument(command: &str) -> bool {
    matches!(
        command,
        "name"
            | "thinking"
            | "export"
            | "import"
            | "label"
            | "model"
            | "session"
            | "settings"
            | "history"
            | "compact"
            | "bug"
            | "cwd"
            | "theme"
            | "skills"
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
            label: "Sidebar".into(),
            detail: "Ctrl-Shift-B".into(),
            action: Action::ToggleSidebar,
            ..Entry::default()
        },
        Entry {
            label: "Working directory…".into(),
            detail: "Ctrl-Shift-W".into(),
            action: Action::Open(ListKind::WorkingDirectory),
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
