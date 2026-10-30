//! TUI 宿主对扩展命令能力面（[`CommandHost`]）的实现与命令接线适配器。
//!
//! 命令正文只依赖 trait，因此同一个 handler 在 TUI 与桌面 GUI 上都能跑。仍需 TUI 面板/
//! 表单的命令用 [`tui_only`] 注册：GUI 侧取不到 `App`，改成一条"仅终端可用"的提示。

use super::super::app::App;
use crate::{
    core::extensions::{
        self, BoxedHandler, CommandHost, SlashCommandHandler, UiNotifyLevel, register_command,
    },
    extensions::util::send_prompt,
    modes::interactive::{agent_actor::AgentCommand, app::MsgLevel},
    utils::truncate::truncate_chars,
};
use std::{any::Any, path::PathBuf, sync::Arc};

/// 宿主消息级别 → TUI 系统消息级别（两套枚举语义一一对应）。
fn msg_level(level: UiNotifyLevel) -> MsgLevel {
    match level {
        UiNotifyLevel::Info => MsgLevel::Info,
        UiNotifyLevel::Success => MsgLevel::Success,
        UiNotifyLevel::Warning => MsgLevel::Warning,
        UiNotifyLevel::Error => MsgLevel::Error,
    }
}

impl CommandHost for App {
    /// 追加一条系统消息到聊天区（TUI 的 `push_msg` 已经置 `dirty`）。
    fn push_message(&mut self, text: &str, level: UiNotifyLevel) {
        self.push_msg(text.to_string(), msg_level(level));
    }

    /// 追加一条 Markdown 系统消息（走聊天区同一套渲染）。
    fn push_markdown(&mut self, text: &str, level: UiNotifyLevel) {
        self.push_msg_markdown(text.to_string(), msg_level(level));
    }

    /// 显示停靠面板。
    fn show_dock(&mut self) {
        self.dock_visible = true;
        self.dirty = true;
    }

    /// 停靠面板是否可见。
    fn dock_visible(&self) -> bool {
        self.dock_visible
    }

    /// 打开某个扩展的设置面板。
    fn open_extension_settings(&mut self, ext: &str) {
        if self.ext_settings.is_open_for(ext) {
            self.ext_settings.close();
        } else {
            self.ext_settings.open(ext);
        }
        self.dirty = true;
    }

    /// 空闲时开新回合，忙碌时并进当前回合的 steer 队列（不锁 agent）。
    fn send_prompt(&mut self, text: &str) {
        send_prompt(self, text.to_string());
    }

    /// 当前选中的模型。
    fn current_model(&self) -> Option<(String, String)> {
        let provider = self.current_provider.clone()?;
        let model = self.current_model.clone()?;
        Some((provider, model))
    }

    /// 切换模型（不落盘，与 `/model` 的命令行切换一致）。
    fn switch_model(&mut self, provider: &str, model_id: &str) {
        self.worker.send(AgentCommand::SwitchModel {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            persist: false,
        });
    }

    /// 当前工作目录（字符串字段的 `Path` 视图）。
    fn cwd(&self) -> PathBuf {
        PathBuf::from(&self.cwd)
    }

    /// 是否正在流式输出。
    fn is_busy(&self) -> bool {
        self.busy
    }

    /// 状态栏临时提示。
    fn set_status(&mut self, text: &str) {
        self.set_status_msg(text.to_string());
    }

    /// 最近一条用户消息正文。
    fn last_user_message(&self) -> Option<String> {
        self.messages
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map(|message| message.text())
    }

    /// 请 worker 回执 agent 诊断信息。
    fn debug_info(&mut self) {
        self.worker.send(AgentCommand::DebugInfo);
    }

    /// 请 worker 回执当前系统提示词。
    fn debug_system_prompt(&mut self) {
        self.worker.send(AgentCommand::SystemPrompt);
    }

    /// 当前会话文件路径。
    fn session_path(&self) -> Option<String> {
        self.current_session_path.clone()
    }

    /// 最近若干条对话的纯文本摘录（`role: 正文`，每条一行）。
    fn transcript_excerpt(&self, max_messages: usize, max_chars: usize) -> String {
        let mut out = String::new();
        let start = self.messages.len().saturating_sub(max_messages);
        for message in &self.messages[start..] {
            let text = message.text();
            let text = text.trim();
            if text.is_empty() {
                continue;
            }

            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(role_label(&message.role));
            out.push_str(": ");
            out.push_str(text);
        }

        truncate_chars(&out, max_chars, None, false)
    }

    /// 上转型为 `Any`，供 [`tui_host`] 下转型回 `App`。
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// 消息角色 → 摘录里的前缀（工具结果统一写成 `tool`，与聊天区展示一致）。
fn role_label(role: &str) -> &str {
    match role {
        "toolResult" | "tool_result" => "tool",
        other => other,
    }
}

/// 从扩展命令的宿主里取 TUI 的 `App`：非 TUI 宿主返回 `None`。
///
/// 面板/表单类命令必须先判空并给出提示（GUI 侧暂无对应界面），
/// 不能静默跳过——用户点了没反应比报错更难查。
pub fn tui_host(host: &mut dyn CommandHost) -> Option<&mut App> {
    host.as_any_mut().downcast_mut::<App>()
}

/// 把「需要 TUI `App`」的命令包成宿主无关入口：非 TUI 宿主给一条提示。
///
/// 提示文案统一在这里，避免每个面板类命令各写一份；命令本身无需感知宿主差异。
pub fn tui_only(handler: fn(&mut App, &str) -> bool) -> BoxedHandler {
    Arc::new(
        move |host: &mut dyn CommandHost, raw: &str| match tui_host(host) {
            Some(app) => handler(app, raw),
            None => {
                let command = raw
                    .trim()
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                host.push_message(
                    &format!("Extension command /{command} is terminal-only; use the terminal UI"),
                    UiNotifyLevel::Warning,
                );
                false
            }
        },
    )
}

/// 注册扩展命令的 TUI 执行入口：正文收 `&mut App`（面板/表单类命令）。
///
/// 命令的"存在性"由已启用扩展的 [`crate::core::extensions::Extension::commands`] 声明动态决定；
/// 本函数只把 handler 包成宿主无关入口并写入 [`register_command`] 的同一张表，
/// 于是桌面 GUI 也能查到它（执行时给出仅终端可用的提示）。
pub fn register_slash_command(ext: &str, cmd: &str, handler: fn(&mut App, &str) -> bool) {
    register_command(ext, cmd, tui_only(handler));
}

/// 注册「只依赖 [`CommandHost`]」的命令：TUI 与桌面 GUI 执行同一份正文。
///
/// 与 [`register_slash_command`] 写同一张表，区别只在正文是否需要 TUI `App`。
pub fn register_host_command(ext: &str, cmd: &str, handler: SlashCommandHandler) {
    extensions::register_host_command(ext, cmd, handler);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::AgentMessage;

    /// 一条指定角色的消息。
    fn message(role: &str, text: &str) -> AgentMessage {
        let mut message = AgentMessage::user_text(text);
        message.role = role.to_string();
        message
    }

    /// 摘录只取最近若干条、按 `role: 正文` 一行一条，超长从头截断。
    #[test]
    fn transcript_excerpt_keeps_recent_messages_and_caps_chars() {
        let mut app = App::new();
        app.messages = vec![
            message("user", "old"),
            message("assistant", "a1"),
            message("user", "u2"),
            message("assistant", "a3"),
        ];
        assert_eq!(
            app.transcript_excerpt(2, 1000),
            "user: u2\nassistant: a3",
            "只取最近两条"
        );
        let long = app.transcript_excerpt(4, 12);
        assert!(
            long.chars().count() <= 13,
            "总长受字符上限约束（省略号额外占 1 个字符）"
        );
        assert!(long.starts_with("user: old"), "超长时从头截断");

        app.messages = vec![message("toolResult", "out")];
        assert_eq!(
            app.transcript_excerpt(1, 100),
            "tool: out",
            "工具结果用 tool 前缀"
        );

        app.messages = vec![message("assistant", "   "), message("user", "hi")];
        assert_eq!(
            app.transcript_excerpt(4, 100),
            "user: hi",
            "正文为空的轮次不留空行"
        );

        app.messages.clear();
        assert_eq!(app.transcript_excerpt(4, 100), "");
    }

    /// 模型镜像：两个字段都就位时才返回。
    #[test]
    fn current_model_reads_app_mirror() {
        let mut app = App::new();
        assert_eq!(app.current_model(), None);
        app.current_provider = Some("openai".to_string());
        app.current_model = Some("gpt-4".to_string());
        assert_eq!(
            app.current_model(),
            Some(("openai".to_string(), "gpt-4".to_string()))
        );
    }
}
