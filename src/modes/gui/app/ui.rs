//! 扩展 UI 请求：`Select` 选择弹框、扩展覆盖层与其余请求的去处。
//!
//! 扩展只能往进程级队列里放请求（`core::extensions::request_ui`），TUI 每轮取一个；
//! GUI 没有那个事件循环，改为定时轮询 [`drain_requests`]，把请求落到覆盖层 / 对话流 / 命令上。

use super::*;
use crate::{
    core::{
        self,
        extensions::{
            CommandHost, ExtensionUiRequest, OverlayEvent, OverlayKey, SelectOption, UiNotifyLevel,
            overlay_event, render_custom_message, request_ui, submit_ui_choice, take_pending_ui,
            wants_redraw,
        },
        project_trust, settings_manager,
    },
    modes::gui::transcript::Block,
    utils::truncate::truncate_chars,
};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

/// 扩展 UI 请求的轮询间隔（毫秒）：队列没有通知机制，只能定时取。
const UI_POLL_MS: u64 = 100;

/// 扩展覆盖层「滚动一行」的位移（逻辑像素）：内容行高不固定，用它把方向键换算成位移。
const OVERLAY_SCROLL_STEP: f32 = 20.;

impl CommandHost for GuiApp {
    /// 往对话流追加一条提示（GUI 没有富文本系统消息流，级别只区分"错误"与"普通"）。
    fn push_message(&mut self, text: &str, level: UiNotifyLevel) {
        let note = match level {
            UiNotifyLevel::Error | UiNotifyLevel::Warning => Note::Error(text.to_string()),
            UiNotifyLevel::Info | UiNotifyLevel::Success => Note::Info(text.to_string()),
        };
        self.transcript.push_note(note);
    }

    /// 追加一条 Markdown 消息（GUI 没有富文本系统消息流，按普通消息呈现）。
    fn push_markdown(&mut self, text: &str, level: UiNotifyLevel) {
        self.push_message(text, level);
    }

    /// 显示停靠面板（重绘由分发点负责）。
    fn show_dock(&mut self) {
        self.dock_visible = true;
    }

    /// 停靠面板是否可见。
    fn dock_visible(&self) -> bool {
        self.dock_visible
    }

    /// 请求打开某个扩展的设置面板（走 UI 请求队列，由轮询落到覆盖层）。
    fn open_extension_settings(&mut self, ext: &str) {
        request_ui(ExtensionUiRequest::ShowSettings {
            ext: ext.to_string(),
        });
    }

    /// 把文本作为用户消息发出去：忙碌时由 worker 并入当前回合（与输入框提交同路径）。
    fn send_prompt(&mut self, text: &str) {
        _ = self.cmd_tx.send(AgentCommand::Prompt(text.to_string()));
    }

    /// 当前选中的模型（来自会话快照）。
    fn current_model(&self) -> Option<(String, String)> {
        let snapshot = self.snapshot.as_ref()?;
        Some((snapshot.provider.clone(), snapshot.model.clone()))
    }

    /// 切换模型（不落盘，与 `/model` 的切换一致）。
    fn switch_model(&mut self, provider: &str, model_id: &str) {
        _ = self.cmd_tx.send(AgentCommand::SwitchModel {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            persist: false,
        });
    }

    /// 当前工作目录（会话快照优先）。
    fn cwd(&self) -> PathBuf {
        PathBuf::from(self.cwd())
    }

    /// 是否正在流式输出。
    fn is_busy(&self) -> bool {
        self.busy
    }

    /// 状态栏临时提示：写入状态栏左侧，几秒后自行过期。
    fn set_status(&mut self, text: &str) {
        self.status_note = Some((text.to_string(), Instant::now() + Duration::from_secs(6)));
    }

    /// 最近一条用户消息正文（从对话流倒着找）。
    fn last_user_message(&self) -> Option<String> {
        self.transcript
            .blocks()
            .iter()
            .rev()
            .find_map(|block| match block {
                Block::User(message) => Some(message.text.clone()),
                _ => None,
            })
    }

    /// 请 worker 回执 agent 诊断信息。
    fn debug_info(&mut self) {
        _ = self.cmd_tx.send(AgentCommand::DebugInfo);
    }

    /// 请 worker 回执当前系统提示词。
    fn debug_system_prompt(&mut self) {
        _ = self.cmd_tx.send(AgentCommand::SystemPrompt);
    }

    /// 当前会话文件路径（会话快照里没有则为内存会话）。
    fn session_path(&self) -> Option<String> {
        self.snapshot.as_ref()?.session_path.clone()
    }

    /// 最近若干条对话的纯文本摘录：取对话流里用户/助手/系统块的正文。
    fn transcript_excerpt(&self, max_messages: usize, max_chars: usize) -> String {
        let mut lines: Vec<String> = Vec::new();
        for block in self.transcript.blocks() {
            let line = match block {
                Block::User(message) => format!("user: {}", message.text.trim()),
                Block::Assistant(assistant) => format!("assistant: {}", assistant.text.trim()),
                Block::System(text) => format!("system: {}", text.trim()),
                Block::Tool(_) | Block::Note(_) => continue,
            };

            if line.ends_with(": ") {
                continue;
            }
            lines.push(line);
        }

        let start = lines.len().saturating_sub(max_messages);
        let joined = lines[start..].join("\n");
        truncate_chars(&joined, max_chars, None, false)
    }

    /// 上转型：GUI 侧没有 TUI 面板类命令，取回 `App` 永远失败（命令据此给提示）。
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl GuiApp {
    /// 起一个定时任务轮询扩展 UI 请求（TUI 是每轮取一个，GUI 没有那个循环）。
    pub(super) fn start_request_poll(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(UI_POLL_MS))
                    .await;
                let Ok(_) = this.update(cx, |app, cx| app.drain_requests(cx)) else {
                    break;
                };
            }
        })
        .detach();
    }

    /// 轮询并处理全部待办的扩展 UI 请求；返回是否处理了任何一条（调用方据此重绘）。
    pub(super) fn drain_requests(&mut self, cx: &mut Context<Self>) -> bool {
        let mut handled = false;
        while let Some(request) = take_pending_ui() {
            self.apply_request(request, cx);
            handled = true;
        }

        // 扩展覆盖层可能正在等后台结果（`/btw` 的答案、`/doctor` 的探针）：每轮重拉一帧，
        // 与 TUI 每帧重绘同一个效果；扩展不再认领时 `refresh` 会把它收起来
        if matches!(self.overlay, Some(Overlay::Extension(_))) {
            self.refresh_extension_overlay();
            cx.notify();
        }

        // 停靠面板里的扩展可以声明「需要持续重绘」（网关计数、进度），空闲时也要跟着刷
        if self.dock_visible && wants_redraw() {
            cx.notify();
        }
        handled
    }

    /// 处理一条扩展 UI 请求。
    fn apply_request(&mut self, request: ExtensionUiRequest, cx: &mut Context<Self>) {
        match request {
            ExtensionUiRequest::Select { id, title, options } => {
                // 一次只显示一个覆盖层：先把占着位置的那个按「取消」收掉，
                // 否则它的发起方（`ask_prompt` 等待者 / 覆盖层扩展）永远等不到回音
                self.dismiss_overlay();
                let mut list = List::new(title, select_entries(id, options));
                // Esc 收起面板时按「取消」回传，发起方的等待者才不会一直挂着
                list.choice = Some(id);
                self.overlay = Some(Overlay::List(list));
            }
            ExtensionUiRequest::ShowOverlay { id } => {
                // 同上：被顶掉的覆盖层要收到 `Closed`，扩展才能清状态（问卷会当作取消收尾）
                self.dismiss_overlay();
                if let Some(overlay) = overlay::ExtOverlay::open(id) {
                    self.overlay = Some(Overlay::Extension(overlay));
                } else {
                    // 扩展已经不认领这个 id：回一个 Closed 让它清掉状态
                    overlay_event(id, OverlayEvent::Closed);
                }
            }
            ExtensionUiRequest::HideOverlay { id } => {
                if let Some(Overlay::Extension(ext)) = self.overlay.as_ref()
                    && ext.id == id
                {
                    self.dismiss_overlay();
                }
            }
            ExtensionUiRequest::Notify { text, level } => {
                let note = match level {
                    UiNotifyLevel::Error => Note::Error(text),
                    _ => Note::Info(text),
                };
                self.transcript.push_note(note);
                self.follow_bottom();
            }
            ExtensionUiRequest::NotifyRich { spans, level } => {
                let text: String = spans.into_iter().map(|span| span.text).collect();
                let note = match level {
                    UiNotifyLevel::Error => Note::Error(text),
                    _ => Note::Info(text),
                };
                self.transcript.push_note(note);
                self.follow_bottom();
            }
            ExtensionUiRequest::CustomMessage {
                label,
                custom_type,
                data,
            } => {
                let body = render_custom_message(&custom_type, &data)
                    .map(|spans| spans.into_iter().map(|span| span.text).collect::<String>())
                    .unwrap_or_else(|| data.to_string());
                self.transcript
                    .push_note(Note::Info(format!("{label} {body}")));
                self.follow_bottom();
            }
            ExtensionUiRequest::Continuation { message } => {
                _ = self.cmd_tx.send(AgentCommand::PromptMessage(*message));
            }
            ExtensionUiRequest::AbortRun => self.abort_turn(cx),
            ExtensionUiRequest::RestartRun { message } => {
                self.abort_turn(cx);
                _ = self.cmd_tx.send(AgentCommand::PromptMessage(*message));
            }
            ExtensionUiRequest::RewindLastUserInput { .. } => {
                _ = self.cmd_tx.send(AgentCommand::Rewind);
            }
            ExtensionUiRequest::RebuildTools => {
                _ = self.cmd_tx.send(AgentCommand::RebuildTools);
            }
            ExtensionUiRequest::PersistSessionEntry { custom_type, data } => {
                _ = self.cmd_tx.send(AgentCommand::AppendCustomEntry {
                    custom_type,
                    data: Some(data),
                });
            }
            // 扩展请求展示停靠内容（subagent / tasks 等的实时进度）
            ExtensionUiRequest::ShowDock => self.dock_visible = true,
            // 只显示该扩展自己的设置项（TUI 的扩展设置面板同语义）
            ExtensionUiRequest::ShowSettings { ext } => self.open_extension_settings(&ext, cx),
        }
        cx.notify();
    }

    /// 启动时的项目信任询问（同 TUI 的 `maybe_ask_project_trust`）：项目里有需要信任的
    /// 资源、没存过决策、默认策略是 ask 且命令行没显式给 `-a/--no-approve` 时才弹；
    /// 不弹时若项目确实未被信任，留一条提示（项目资源被忽略，`/trust` 可保存决策）。
    pub(super) fn maybe_ask_trust(&mut self, cx: &mut Context<Self>) {
        let cwd = PathBuf::from(self.cwd());
        if !project_trust::has_trust_requiring_project_resources(&cwd) {
            return;
        }

        let agent_dir = settings_manager::agent_dir();
        let ask = self.trust_override.is_none()
            && project_trust::trust_decision(&cwd, &agent_dir).is_none()
            && project_trust::default_project_trust(&agent_dir) == project_trust::ProjectTrust::Ask;

        if ask {
            self.open_trust_panel(cx);
            return;
        }

        if !project_trust::is_project_trusted(&cwd, &agent_dir) {
            self.transcript.push_note(Note::Info(format!(
                "This project is not trusted. Project {} resources and packages are ignored.                  Use /trust to save a trust decision, then restart {}.",
                crate::PROJECT_SCOPE_NAME,
                crate::APP_NAME
            )));
        }
    }

    /// 单个扩展的设置面板：只列该扩展声明的设置项，`Space` 在预定义选项里循环。
    ///
    /// 设置项来自 [`crate::core::extensions::settings_for`]，写回走
    /// `apply_extension_setting`（扩展自己决定是否落盘）。`select` 是重建时要保持的高亮行。
    pub(super) fn open_extension_settings(&mut self, ext: &str, cx: &mut Context<Self>) {
        self.show_extension_settings(ext, None, cx);
    }

    /// [`Self::open_extension_settings`] 的实现：`select` 用于循环取值后停在原行。
    fn show_extension_settings(
        &mut self,
        ext: &str,
        select: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let items = core::extensions::settings_for(ext);
        let entries: Vec<Entry> = items
            .iter()
            .map(|item| Entry {
                label: item.label.clone(),
                // 可循环的项把当前值裹上箭头，提示 Space 会改它
                detail: if item.choices.is_empty() {
                    item.value.clone()
                } else {
                    format!("◂ {} ▸", item.value)
                },
                action: if item.choices.is_empty() {
                    Action::None
                } else {
                    Action::CycleExtensionSetting {
                        ext: ext.to_string(),
                        key: item.key.clone(),
                    }
                },
                ..Entry::default()
            })
            .collect();

        let title = if items.is_empty() {
            format!("{ext} · no settings")
        } else {
            format!("Settings · {ext}")
        };

        // Space 在这里是「换一个值」而不是「确认」，提示行照实写
        let hint = "↑↓ select · Space cycle value · Esc close";

        // 同一个面板重建（循环取值后）：沿用过滤框与高亮行，否则搜索内容一按就丢、列表又变回完整条目
        if let Some(Overlay::List(list)) = self.overlay.as_mut()
            && list.title == title
        {
            list.refresh(title, entries);
            list.hint = Some(hint.to_string());
            list.space_activates = true;

            if let Some(index) = select {
                list.select_index(index);
            }

            cx.notify();
            return;
        }

        let mut list = List::new(title, entries);
        list.hint = Some(hint.to_string());
        list.space_activates = true;

        if let Some(index) = select {
            list.select_index(index);
        }

        self.overlay = Some(Overlay::List(list));
        cx.notify();
    }

    /// 扩展设置项循环到下一个候选值（末项回到首项），成功后按原行重建面板。
    pub(super) fn cycle_extension_setting(&mut self, ext: &str, key: &str, cx: &mut Context<Self>) {
        let items = core::extensions::settings_for(ext);
        let Some(item) = items.iter().find(|item| item.key == key) else {
            return;
        };

        if item.choices.is_empty() {
            return;
        }

        let position = item
            .choices
            .iter()
            .position(|choice| choice == &item.value)
            .map(|index| (index + 1) % item.choices.len())
            .unwrap_or(0);
        let next = item.choices[position].clone();
        match core::extensions::apply_extension_setting(ext, key, &next) {
            Ok(()) => {}
            Err(error) => self.toast(
                format!("Cannot change {ext} setting: {error}"),
                ToastKind::Error,
                cx,
            ),
        }

        // 保持当前高亮行（重建后条目顺序不变）
        let selected = self.overlay.as_ref().and_then(|overlay| match overlay {
            Overlay::List(list) => Some(list.selected),
            _ => None,
        });
        self.show_extension_settings(ext, selected, cx);
    }

    /// `/trust` 面板：展示当前目录与已保存决策。选项是 TUI 对话框那几项
    /// （Trust / Trust parent folder / Do not trust / Remove all saved trust decisions）
    /// 加上启动选择器才有的两个「仅本次」选项——桌面端两处共用同一个面板。
    pub(super) fn open_trust_panel(&mut self, cx: &mut Context<Self>) {
        let cwd = self.cwd();
        let agent_dir = settings_manager::agent_dir();
        let cwd_path = std::path::Path::new(&cwd);
        let saved = project_trust::trust_decision(cwd_path, &agent_dir);
        let current = saved.as_ref().map(|(_, trusted)| *trusted);
        let note = match &saved {
            None => "no saved decision".to_string(),
            Some((path, trusted)) => format!(
                "{} ({path})",
                if *trusted { "trusted" } else { "untrusted" }
            ),
        };

        let entry = |label: String, detail: String, choice: &str| Entry {
            label,
            detail,
            action: Action::TrustDecision(choice.to_string()),
            ..Entry::default()
        };
        let mut entries = vec![entry(
            "Trust".to_string(),
            if current == Some(true) {
                "current".to_string()
            } else {
                String::new()
            },
            "trust",
        )];

        if let Some(parent) = cwd_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            entries.push(entry(
                format!("Trust parent folder ({})", parent.display()),
                String::new(),
                "trust_parent",
            ));
        }

        entries.push(entry(
            "Do not trust".to_string(),
            if current == Some(false) {
                "current".to_string()
            } else {
                String::new()
            },
            "distrust",
        ));
        entries.push(entry(
            "Trust (this session only)".to_string(),
            String::new(),
            "trust_session",
        ));
        entries.push(entry(
            "Do not trust (this session only)".to_string(),
            String::new(),
            "distrust_session",
        ));
        entries.push(entry(
            "Remove all saved trust decisions".to_string(),
            String::new(),
            "clear",
        ));

        let mut list = List::new(format!("Project trust · {cwd} · {note}"), entries);
        if let Some(index) = list
            .entries
            .iter()
            .position(|item| item.detail == "current")
        {
            list.select_index(index);
        }
        self.overlay = Some(Overlay::List(list));
        cx.notify();
    }

    /// 应用 `/trust` 面板里的选择；`clear` 会先弹一次二次确认。
    pub(super) fn resolve_trust(&mut self, choice: &str, cx: &mut Context<Self>) {
        let cwd = PathBuf::from(self.cwd());
        let agent_dir = settings_manager::agent_dir();
        match choice {
            "clear" => {
                let entries = vec![
                    Entry {
                        label: "Yes, remove all saved trust decisions".to_string(),
                        detail: String::new(),
                        action: Action::TrustDecision("clear-confirm".to_string()),
                        ..Entry::default()
                    },
                    Entry {
                        label: "Cancel".to_string(),
                        detail: String::new(),
                        action: Action::None,
                        ..Entry::default()
                    },
                ];
                self.overlay = Some(Overlay::List(List::new(
                    "Remove all saved trust decisions?",
                    entries,
                )));
            }
            "clear-confirm" => {
                project_trust::clear_all_trust(&agent_dir);
                self.toast("Removed all saved trust decisions", ToastKind::Success, cx);
            }
            "trust_parent" => {
                let parent = cwd.parent().unwrap_or(&cwd).to_path_buf();
                project_trust::set_project_trust(&parent, &agent_dir, true);
                project_trust::clear_project_trust(&cwd, &agent_dir);
                self.toast(
                    format!(
                        "Saved: trust parent folder {} (restart to apply)",
                        parent.display()
                    ),
                    ToastKind::Success,
                    cx,
                );
            }
            // 仅本次运行生效（不写盘），与 TUI 的启动选择器一致
            "trust_session" | "distrust_session" => {
                let trusted = choice == "trust_session";
                project_trust::set_session_trust(&cwd, trusted);
                _ = self.cmd_tx.send(AgentCommand::Reload {
                    context_files: Vec::new(),
                    skills: Vec::new(),
                    system_prompt: None,
                    append_system_prompt: None,
                });
                self.toast(
                    format!(
                        "Project {} for this session only",
                        if trusted { "trusted" } else { "untrusted" }
                    ),
                    ToastKind::Info,
                    cx,
                );
            }
            other => {
                let trusted = other == "trust";
                project_trust::set_project_trust(&cwd, &agent_dir, trusted);
                self.toast(
                    format!(
                        "Saved: project {} (restart to apply)",
                        if trusted { "trusted" } else { "untrusted" }
                    ),
                    if trusted {
                        ToastKind::Success
                    } else {
                        ToastKind::Info
                    },
                    cx,
                );
            }
        }
        cx.notify();
    }

    /// 扩展覆盖层把按键翻译后回传；返回是否被扩展消费。
    pub(super) fn send_overlay_key(&mut self, key: OverlayKey) -> bool {
        let Some(Overlay::Extension(ext)) = self.overlay.as_ref() else {
            return false;
        };

        let input = ext.view.input.as_ref().map(|_| ext.input.text());
        let consumed = core::extensions::overlay_event(ext.id, OverlayEvent::Key { key, input });
        if consumed {
            self.refresh_extension_overlay();
        }
        consumed
    }

    /// 扩展覆盖层重新拉一帧；扩展不再认领时收起面板。
    pub(super) fn refresh_extension_overlay(&mut self) {
        let Some(Overlay::Extension(ext)) = self.overlay.as_mut() else {
            return;
        };

        if !ext.refresh() {
            self.dismiss_overlay();
        }
    }

    /// 扩展覆盖层内容滚动：`rows` 为正向下、负向上，`f32::MAX` / `f32::MIN` 直达两端。
    pub(super) fn scroll_extension_overlay(&mut self, rows: f32) {
        let Some(Overlay::Extension(ext)) = self.overlay.as_ref() else {
            return;
        };

        let max = f32::from(ext.scroll.max_offset().height);
        let current = f32::from(ext.scroll.offset().y);
        let next = if rows >= f32::MAX {
            0.
        } else if rows <= f32::MIN {
            -max
        } else {
            (current + rows * OVERLAY_SCROLL_STEP).clamp(-max, 0.)
        };

        ext.scroll
            .set_offset(gpui::point(gpui::px(0.), gpui::px(next)));
    }

    /// 扩展覆盖层的内联输入提交：文本交给扩展，缓冲清空。
    pub(super) fn submit_overlay_input(&mut self) {
        let Some(Overlay::Extension(ext)) = self.overlay.as_mut() else {
            return;
        };

        let text = ext.input.text();
        ext.input.clear();
        core::extensions::overlay_event(ext.id, OverlayEvent::Submit { text });
        self.refresh_extension_overlay();
    }

    /// 收起覆盖层：选择面板按「取消」回传，扩展覆盖层收到 `Closed`。
    pub(super) fn dismiss_overlay(&mut self) {
        match self.overlay.take() {
            Some(Overlay::List(list)) => {
                if let Some(id) = list.choice
                    && let Some(prompt) = submit_ui_choice(id, None)
                {
                    _ = self.cmd_tx.send(AgentCommand::Prompt(prompt));
                }
            }
            Some(Overlay::Extension(ext)) => {
                core::extensions::overlay_event(ext.id, OverlayEvent::Closed);
            }
            _ => {}
        }
    }
}

/// `Select` 请求的选项 → 选择面板条目：每条把选项文本作为答案回传给发起方。
fn select_entries(id: u64, options: Vec<SelectOption>) -> Vec<Entry> {
    options
        .into_iter()
        .map(|option| Entry {
            label: option.text.clone(),
            detail: String::new(),
            action: Action::UiChoice {
                id,
                text: Some(option.text),
            },
            ..Entry::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::{SelectOption, set_ui_ready};

    /// 选项各自带回传动作（同一 id + 自己的文本），面板行数与选项数一致。
    #[test]
    fn select_options_become_ui_choice_entries() {
        let entries = select_entries(9, vec![SelectOption::new("a"), SelectOption::new("b")]);
        assert_eq!(entries.len(), 2);
        for (entry, expected) in entries.iter().zip(["a", "b"]) {
            assert_eq!(entry.label, expected);
            match &entry.action {
                Action::UiChoice {
                    id,
                    text: Some(answer),
                } => {
                    assert_eq!(*id, 9);
                    assert_eq!(answer, expected);
                }
                _ => panic!("选择面板的动作应是 UiChoice"),
            }
        }
    }

    /// 面板答案按 id 送到 `ask_prompt` 的等待方（Select 的整条通路）。
    #[test]
    fn select_answer_reaches_the_waiter() {
        // UI 状态是进程级全局量，与其它扩展的 UI 测试串行。
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while crate::core::extensions::take_pending_ui().is_some() {}
        set_ui_ready(true);

        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let answer = runtime.block_on(async {
            let waiter = tokio::spawn(async {
                crate::core::extensions::ask_prompt(
                    "title",
                    vec![SelectOption::new("a"), SelectOption::new("b")],
                    Some(std::time::Duration::from_secs(5)),
                )
                .await
            });
            // 轮询取走 Select 请求（spawn 出去的任务在 worker 线程上入队）
            let id = loop {
                if let Some(ExtensionUiRequest::Select { id, .. }) =
                    crate::core::extensions::take_pending_ui()
                {
                    break id;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            };
            crate::core::extensions::submit_ui_choice(id, Some("b".to_string()));
            waiter.await.expect("waiter task")
        });
        set_ui_ready(false);
        assert_eq!(answer, Some("b".to_string()));
    }
}
