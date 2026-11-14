//! 斜杠命令的 RPC 宿主：把 [`CommandHost`] 的动作接到 stdout 的 UI 记录上。
//!
//! 命令正文（`/compact`、`/goal` 之类）原本只认 TUI/GUI 的宿主对象。RPC 里没有面板，
//! 所以凡是「纯文本回执」都发成 `notify`，凡是「TUI 面板/表单」都明确回一句做不到——
//! 按 [`CommandHost`] 的约定，静默 no-op 是不允许的（用户点了没反应比报错更难查）。

use super::{protocol, server::State};
use crate::core::{
    agent_session::Agent,
    context_breakdown,
    extensions::{CommandHost, UiNotifyLevel, dispatch_exec_ctx},
};
use serde_json::json;
use std::{
    any::Any,
    path::{Path, PathBuf},
};

/// 一次斜杠命令执行期间的宿主句柄：借用 agent 与主循环状态，不持有任何锁。
pub(super) struct RpcHost<'a> {
    /// 已装配好的 agent（命令改模型、换目录、读上下文都靠它）。
    pub(super) agent: &'a mut Agent,
    /// 主循环状态（follow-up 队列、skills/templates 快照）。
    pub(super) state: &'a mut State,
    /// 下转型占位：RPC 没有具体宿主类型可给，面板类命令拿到 `()` 就会走「做不到」分支。
    pub(super) nothing: (),
}

/// 通知级别 → pi 的 `notifyType`；成功与信息合并成 `info`（协议只有三档）。
fn notify_type(level: UiNotifyLevel) -> &'static str {
    match level {
        UiNotifyLevel::Error => "error",
        UiNotifyLevel::Warning => "warning",
        UiNotifyLevel::Info | UiNotifyLevel::Success => "info",
    }
}

/// 发给客户端的一条纯文本回执。
fn notify(text: &str, level: UiNotifyLevel) {
    protocol::fire(
        "notify",
        json!({"message": text, "notifyType": notify_type(level)}),
    );
}

impl CommandHost for RpcHost<'_> {
    /// 纯文本回执走 `notify`。
    fn push_message(&mut self, text: &str, level: UiNotifyLevel) {
        notify(text, level);
    }

    /// RPC 没有 markdown 渲染：原文回执，客户端自己决定怎么画。
    fn push_markdown(&mut self, text: &str, level: UiNotifyLevel) {
        notify(text, level);
    }

    /// RPC 没有停靠面板，也没人能让它可见。
    fn show_dock(&mut self) {}

    /// 停靠面板恒不可见。
    fn dock_visible(&self) -> bool {
        false
    }

    /// 扩展设置面板是 TUI/GUI 的界面，RPC 只能回一句做不到。
    fn open_extension_settings(&mut self, ext: &str) {
        notify(
            &format!("extension settings panel for `{ext}` is not available in rpc mode"),
            UiNotifyLevel::Info,
        );
    }

    /// 命令追加一条用户消息：进 follow-up 队列，本轮结算后自动起下一轮。
    fn send_prompt(&mut self, text: &str) {
        if !text.trim().is_empty() {
            self.state.push_follow_up(text);
        }
    }

    /// 当前选中的模型（provider, 模型 id）。
    fn current_model(&self) -> Option<(String, String)> {
        Some((
            self.agent.model.provider.clone(),
            self.agent.model.model_id.clone(),
        ))
    }

    /// 只改会话级模型，不写 settings.json（命令只是"切一下"，持久化由客户端决定）。
    fn switch_model(&mut self, provider: &str, model_id: &str) {
        if let Err(e) = self.agent.switch_model(provider, model_id, false) {
            notify(&format!("switch model failed: {e}"), UiNotifyLevel::Error);
        }
    }

    /// 换工作目录：按新目录重建工具与系统提示，消息与会话文件不动。
    fn set_cwd(&mut self, path: &str) {
        if !Path::new(path).is_dir() {
            notify(&format!("not a directory: {path}"), UiNotifyLevel::Error);
            return;
        }
        self.agent.cwd = path.to_string();
        self.agent.rebuild_tools();
        dispatch_exec_ctx(&self.agent.exec_ctx());
    }

    /// 当前工作目录。
    fn cwd(&self) -> PathBuf {
        PathBuf::from(&self.agent.cwd)
    }

    /// 斜杠命令只在空闲时执行，永远是 false。
    fn is_busy(&self) -> bool {
        false
    }

    /// 状态栏提示走 `setStatus`（客户端自己决定画多久）。
    fn set_status(&mut self, text: &str) {
        protocol::fire(
            "setStatus",
            json!({"statusKey": "status", "statusText": text}),
        );
    }

    /// 最近一条用户消息正文（`/rewind` 预览用）。
    fn last_user_message(&self) -> Option<String> {
        self.agent
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.text())
            .filter(|text| !text.trim().is_empty())
    }

    /// `/debug info`：会话、模型与规模的概要（终端回执的 RPC 版）。
    fn debug_info(&mut self) {
        let file = self
            .agent
            .session
            .as_ref()
            .and_then(|s| s.get_session_file())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "(in-memory)".to_string());
        notify(
            &format!(
                "session: {file}\nmodel: {}/{}\nthinking: {}\nmessages: {}\ntools: {}",
                self.agent.model.provider,
                self.agent.model.model_id,
                self.agent.thinking_level.as_deref().unwrap_or("off"),
                self.agent.messages.len(),
                self.agent.get_active_tools().len(),
            ),
            UiNotifyLevel::Info,
        );
    }

    /// `/debug system-prompt`：当前系统提示词原文。
    fn debug_system_prompt(&mut self) {
        notify(&self.agent.system_prompt, UiNotifyLevel::Info);
    }

    /// `/debug tools`：本次声明的工具清单（隐藏不声明的工具不算）。
    fn debug_tools(&mut self) {
        let names: Vec<String> = self
            .agent
            .tools
            .iter()
            .map(|(name, _, _)| name.clone())
            .filter(|name| !self.agent.hidden_declarations.contains(name))
            .collect();
        notify(&names.join("\n"), UiNotifyLevel::Info);
    }

    /// `/debug context`：上下文窗口的来源拆分。
    fn debug_context_breakdown(&mut self) {
        let tools: Vec<(String, String, serde_json::Value)> = self
            .agent
            .tools
            .iter()
            .filter(|(name, _, _)| !self.agent.hidden_declarations.contains(name))
            .cloned()
            .collect();
        let breakdown = context_breakdown::breakdown(
            &self.agent.system_prompt,
            &tools,
            &self.agent.messages,
            self.agent.model.context_window,
        );
        notify(&context_breakdown::render(&breakdown), UiNotifyLevel::Info);
    }

    /// 当前会话文件；内存会话（`--no-session`）为 `None`。
    fn session_path(&self) -> Option<String> {
        self.agent
            .session
            .as_ref()
            .and_then(|s| s.get_session_file())
            .map(|p| p.to_string_lossy().to_string())
    }

    /// 最近若干条对话的纯文本摘录（`/btw`、`/recap` 的「带上下文再问一次」）。
    fn transcript_excerpt(&self, max_messages: usize, max_chars: usize) -> String {
        let mut lines: Vec<String> = Vec::new();
        for message in &self.agent.messages {
            let text = message.text();
            if text.trim().is_empty() {
                continue;
            }
            let text: String = text.chars().take(max_chars).collect();
            lines.push(format!("{}: {}", message.role, text));
        }
        let start = lines.len().saturating_sub(max_messages);
        lines[start..].join("\n")
    }

    /// RPC 没有具体宿主类型可以下转型：面板类命令据此走「做不到」的分支。
    ///
    /// 面板类命令会 `downcast_mut::<T>()` 失败后回一句「这里做不到」，
    /// 所以退化成 `self`（而非某个具体宿主）就是正确语义。
    fn as_any_mut(&mut self) -> &mut dyn Any {
        &mut self.nothing
    }
}
