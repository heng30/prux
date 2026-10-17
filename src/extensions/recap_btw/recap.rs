//! `recap` 扩展：`/recap` 生成一行会话小结，作为系统消息推到聊天区。
//!
//! 走一次独立的一次性请求（最近 [`EXCERPT_MESSAGES`] 条、[`EXCERPT_CHARS`] 字以内的摘录，
//! `max_tokens` 限 [`MAX_TOKENS`]，关不掉思考的模型会抬到 [`super::shared`] 的下限），
//! 结果规整到 [`MAX_CHARS`] 字以内（去前缀、去引号、硬截断）后作为一条系统消息输出。
//! 它**不进会话历史、不改上下文**——主模型看不到这条小结。
//!
//! 没有后台任务、没有开关、没有落盘：只在用户显式执行时才有动作（会花一次模型请求）。
//!
//! **默认不启用**，且只在 `Dev` / `Creator` 扩展模式下可用。

use super::shared;
use crate::{
    core::{
        extensions::{Extension, ExtensionCommand, ExtensionMode, ExtensionTool, UiNotifyLevel},
        provider::AgentMessage,
        session_manager::Session,
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_RECAP,
        util::{notify_text, session_id_of},
    },
    modes::interactive::{
        app::{App, MsgLevel},
        handlers::register_slash_command,
    },
    utils::{glyphs::DEF_RECAP, truncate::truncate_chars},
};
use std::sync::Arc;

/// 扩展名。
const EXT: &str = "recap";

/// 系统提示：压成一行、给"回来接着干"的人看。
const SYSTEM: &str = "Summarize this coding session in ONE line of at most 120 characters. \
                      Say what is being worked on and where it stands. No preamble, no \
                      markdown heading, no trailing period padding.";

/// 摘录的消息条数上限。
const EXCERPT_MESSAGES: usize = 12;

/// 摘录的字符上限。
const EXCERPT_CHARS: usize = 12000;

/// 回复的 token 上限。
const MAX_TOKENS: u32 = 160;

/// 一行小结的字符上限。
const MAX_CHARS: usize = 120;

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static RECAP_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_RECAP,
    make: || -> Arc<dyn Extension> { Arc::new(Recap) },
};

/// `recap` 扩展。
pub struct Recap;

impl Extension for Recap {
    /// 扩展名 `recap`。
    fn name(&self) -> &str {
        EXT
    }

    /// 一句话说明：一行会话小结（系统消息）。
    fn description(&self) -> &str {
        "One-line session recap as a system message (/recap)"
    }

    /// 默认**不启用**：每执行一次就花一次模型请求，不该由框架替用户决定。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 只在 `Dev` / `Creator` 扩展模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 不提供模型工具（小结由用户显式发起）。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/recap` 命令。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: "recap".to_string(),
            description: "/recap (one-line session summary as a system message)".to_string(),
            busy_safe: true, // 只起一次性请求，不锁 agent
            subcommands: Vec::new(),
        }]
    }

    /// 注册 `/recap` 的 handler。
    fn on_registered(&self) {
        register_slash_command(EXT, "recap", command_recap);
    }

    /// 记住当前会话 id（一次性请求会带上它做提示缓存路由）。
    fn on_session_start(&self, _cwd: &str, _messages: &[AgentMessage], session: Option<&Session>) {
        shared::remember_session(session.map(|s| s.session_id.clone()));
    }

    /// 会话切换：记住新会话 id。
    fn on_session_switched(&self, session_path: Option<&str>, _messages: &[AgentMessage]) {
        shared::remember_session(session_id_of(session_path));
    }
}

/// `/recap` handler：把最近摘录交给一次性请求，结果作为系统消息输出。
fn command_recap(st: &mut App, _raw: &str) -> bool {
    // 本会话中途启用时会话钩子没跑过，槽里没有 id（opencode 会报缺 x-opencode-session）
    shared::refresh_session_from_app(st);

    let Some((provider, model_id)) = shared::current_model(st) else {
        st.push_msg(
            "No model is selected yet. Pick one with /model, then try again.".to_string(),
            MsgLevel::Warning,
        );
        return false;
    };

    let excerpt = shared::build_excerpt(&st.messages, EXCERPT_MESSAGES, EXCERPT_CHARS);
    if excerpt.trim().is_empty() {
        st.push_msg(
            "Nothing to summarize yet — this session has no messages.".to_string(),
            MsgLevel::Info,
        );
        return false;
    }

    st.set_status_msg("creating recap…");

    if let Err(e) =
        shared::spawn_one_shot(provider, model_id, SYSTEM, excerpt, MAX_TOKENS, |result| {
            match result {
                Ok(text) => notify_text(
                    &format!("{DEF_RECAP} recap: {}", normalize_recap(&text)),
                    UiNotifyLevel::Info,
                ),
                Err(e) => notify_text(&format!("recap failed: {e}"), UiNotifyLevel::Warning),
            }
        })
    {
        notify_text(&format!("recap failed: {e}"), UiNotifyLevel::Warning);
    }
    false
}

/// 规整小结文本：去掉前缀（`recap:` / `※`）与引号、归一空白，再按字符上限硬截断
/// （契约是"≤ [`MAX_CHARS`] 字"，省略号不该占配额）。
fn normalize_recap(text: &str) -> String {
    let mut cleaned = text.trim().to_string();
    for prefix in [
        DEF_RECAP, "recap:", "Recap:", "recap :", "SUMMARY:", "Summary:",
    ] {
        if let Some(rest) = cleaned.strip_prefix(prefix) {
            cleaned = rest.trim().to_string();
        }
    }
    cleaned = cleaned
        .trim_matches(|c| c == '"' || c == '\'' || c == '“' || c == '”')
        .trim()
        .to_string();
    cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&cleaned, MAX_CHARS, None, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{core::extensions::take_pending_ui, test_support::AUTH_TEST_LOCK};
    use std::sync::MutexGuard;

    /// 全局状态锁（多个用例共享进程级会话 id 槽与 UI 队列）。
    fn lock_global() -> MutexGuard<'static, ()> {
        AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 一条指定角色的消息。
    fn message(role: &str, text: &str) -> AgentMessage {
        let mut message = AgentMessage::user_text(text);
        message.role = role.to_string();
        message
    }

    #[test]
    fn normalize_recap_strips_prefix_quotes_and_caps_length() {
        assert_eq!(
            normalize_recap("  Recap: working on the parser  "),
            "working on the parser"
        );
        assert_eq!(normalize_recap("※ \"done\" "), "done");
        assert_eq!(normalize_recap("a\n  b"), "a b", "换行与多空格归一");
        let long = normalize_recap(&"字".repeat(200));
        assert_eq!(long.chars().count(), MAX_CHARS, "硬截断到上限");
    }

    #[test]
    fn recap_without_model_hints() {
        let _g = lock_global();
        let mut st = App::new();
        st.current_provider = None;
        st.current_model = None;
        assert!(!command_recap(&mut st, "recap"));
        assert!(!st.system_messages.is_empty(), "没有型号应提示");
    }

    #[test]
    fn recap_without_history_hints() {
        let _g = lock_global();
        let mut st = App::new();
        st.current_provider = Some("anthropic".to_string());
        st.current_model = Some("claude-fable-5".to_string());
        assert!(!command_recap(&mut st, "recap"));
        assert!(!st.system_messages.is_empty(), "没有历史应提示");
    }

    /// 单测没有 tokio 运行时 → 请求发不出去，应作为系统消息报错而不是静默。
    #[test]
    fn recap_without_runtime_notifies_failure() {
        let _g = lock_global();
        while take_pending_ui().is_some() {}

        let mut st = App::new();
        st.current_provider = Some("anthropic".to_string());
        st.current_model = Some("claude-fable-5".to_string());
        st.messages.push(message("user", "hello world"));

        assert!(!command_recap(&mut st, "recap"));
        assert!(matches!(
            take_pending_ui(),
            Some(crate::core::extensions::ExtensionUiRequest::NotifyRich {
                level: UiNotifyLevel::Warning,
                ..
            })
        ));
        while take_pending_ui().is_some() {}
    }

    #[test]
    fn command_and_mode_surface() {
        let cmds = Recap.commands();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "recap");
        assert!(cmds[0].busy_safe);
        assert!(cmds[0].subcommands.is_empty(), "没有子命令");
        assert_eq!(Recap.name(), EXT);
        assert!(!Recap.default_enabled(), "默认不启用");
        assert_eq!(
            Recap.modes(),
            vec![ExtensionMode::Dev, ExtensionMode::Creator],
            "只在 Dev / Creator 模式可用"
        );
        assert!(Recap.tools().is_empty());
        assert!(Recap.hooks().is_empty(), "不再提供 dock 等钩子");
    }
}
