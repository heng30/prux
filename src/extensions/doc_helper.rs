//! `doc-helper` 扩展：把程序帮助文档从系统提示词里挪出来，改成按需询问。
//!
//! **默认不启用**：注册后为禁用态，命令与抑制效果都不生效；需在 `/extension` 面板开启。
//!
//! - 精简系统提示词：启用后 [`Extension::suppress_documentation`] 返回 `true`，
//!   [`crate::core::system_prompt::build_system_prompt`] 跳过整段程序文档描述
//!   （README/docs/examples 指针 + 主题映射 bullet），让默认提示词更短；
//! - 按需提问：`/help <question>` 把**同一段**文档描述（
//!   [`crate::core::system_prompt::documentation_section`]，唯一真源）当作一条普通
//!   user 消息投给模型——**不改系统提示词**，因此不破坏前缀缓存命中率，缓存
//!   增益只发生在用户真正需要文档时；
//! - 无参 `/help` 只在 TUI 里提示用法 + 文档主题清单，不触发模型回合；
//! - handler 不锁 agent（`busy_safe = true`）：空闲走普通提问路径，忙碌时作为 steer 注入当前回合。

use crate::{
    APP_NAME,
    core::{
        extensions::{Extension, ExtensionCommand, ExtensionMode, ExtensionTool},
        provider::AgentMessage,
        system_prompt::documentation_section,
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_DOC_HELPER, command_arg},
    modes::interactive::{
        app::{App, MsgLevel},
        handlers::register_slash_command,
    },
};
use std::sync::Arc;

/// 扩展名
const EXT: &str = "doc-helper";

/// 命令名
const CMD: &str = "help";

/// 自声明工厂：linkme 分布式切片
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static DOC_HELPER_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_DOC_HELPER,
    make: || -> Arc<dyn Extension> { Arc::new(DocHelper::new()) },
};

/// `doc-helper` 扩展（无状态：抑制为静态声明，命令即用即答）。
#[derive(Default)]
pub struct DocHelper;

impl DocHelper {
    /// 构造扩展实例（无状态）。
    pub fn new() -> Self {
        Self
    }
}

impl Extension for DocHelper {
    /// 扩展标识名 `doc-helper`。
    fn name(&self) -> &str {
        EXT
    }

    /// 供 `/extension` 面板展示的英文简介。
    fn description(&self) -> &str {
        concat!(
            "Move ",
            env!("CARGO_PKG_NAME"),
            " help docs out of the system prompt; ask about ",
            env!("CARGO_PKG_NAME"),
            " docs on demand with /help <question>"
        )
    }

    /// 仅在 `Minimal` 模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Minimal]
    }

    /// 默认不启用，需用户在 `/extension` 面板手动开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 启用后让系统提示词跳过整段程序文档描述，改由 `/help` 按需提供。
    fn suppress_documentation(&self) -> bool {
        true
    }

    /// 本扩展不提供工具。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 注册 `/help <question>` 命令（busy_safe：忙碌时走 steer 注入，不锁 agent）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: format!(
                "/help <question> asks about {APP_NAME} using its bundled docs; no args lists doc topics"
            ),
            busy_safe: true, // handler 不锁 agent（只写 App 消息/steer 队列），忙碌时可立即执行
            subcommands: Vec::new(),
        }]
    }

    /// 注册时把 `command_help` 挂到斜杠命令表上。
    fn on_registered(&self) {
        register_slash_command(EXT, CMD, command_help);
    }
}

/// `/help [question]`：无参提示用法 + 文档主题；有参把文档描述段 + 问题作为
/// 一条普通用户消息投出（空闲触发新回合，忙碌作为 steer 注入当前回合）。
///
/// 注意：注入的是**用户消息**而非系统提示词，避免每轮改动前缀、保住缓存命中率。
fn command_help(st: &mut App, raw: &str) -> bool {
    let question = command_arg(raw).trim();

    if question.is_empty() {
        st.push_msg(
            format!(
                "/help <question> — ask a question about {APP_NAME}; the agent will read its bundled docs.\n\n{}",
                documentation_section()
            ),
            MsgLevel::Info,
        );
        return false;
    }

    let text = format!("{}\n\nQuestion: {}", documentation_section(), question);
    if st.busy {
        // 忙碌：作为 steer 立即注入当前回合（与输入框 steer 提交同路径，不锁 agent）
        if let Ok(mut inbox) = st.runtime_steer_inbox.lock() {
            inbox.push_back(AgentMessage::user_text(&text));
        }
        st.dirty = true;
    } else {
        // 空闲：按普通用户提问路径投递，渲染用户气泡并触发新回合
        st.messages.push(AgentMessage::user_text(&text));
        st.status_start = Some(text);
        st.dirty = true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::{
            extensions::{
                ExtensionMode, documentation_suppressed, register_extension, set_extension_enabled,
                set_extension_mode, unregister_extension,
            },
            system_prompt::{SystemPromptOptions, build_system_prompt},
        },
        test_support::{AUTH_TEST_LOCK, AgentDirGuard},
    };
    use std::collections::HashMap;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn prompt_options() -> SystemPromptOptions {
        SystemPromptOptions {
            cwd: "/tmp/proj".to_string(),
            selected_tools: Some(vec!["read".to_string()]),
            hidden_tools: Vec::new(),
            tool_snippets: HashMap::new(),
            prompt_guidelines: Vec::new(),
            append_system_prompt: None,
            context_files: Vec::new(),
            skills: Vec::new(),
            custom_prompt: None,
        }
    }

    /// 启用 doc-helper 时默认系统提示词不含文档段；默认关闭时保留。
    #[test]
    fn enabled_extension_suppresses_documentation_section() {
        let _g = lock();
        let _ad = AgentDirGuard::temp();
        set_extension_mode(ExtensionMode::All);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        register_extension(DocHelper::new());
        // 默认关闭：不抑制，文档段保留
        assert!(!documentation_suppressed());
        let with_docs = build_system_prompt(&prompt_options());
        assert!(with_docs.contains("documentation (read only"));
        assert!(with_docs.contains("Additional docs:"));
        assert!(with_docs.contains("docs/themes.md"));

        // 开启：抑制，整段消失
        set_extension_enabled(EXT, true);
        assert!(documentation_suppressed());
        let without = build_system_prompt(&prompt_options());
        assert!(!without.contains("documentation (read only"));
        assert!(!without.contains("Additional docs:"));
        assert!(!without.contains("docs/themes.md"));

        assert!(unregister_extension(EXT));
    }

    /// 命令声明：存在、busy_safe、无子命令。
    #[test]
    fn command_is_registered_busy_safe() {
        let ext = DocHelper::new();
        let cmds = ext.commands();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "help");
        assert!(cmds[0].busy_safe);
        assert!(cmds[0].subcommands.is_empty());
        assert!(ext.suppress_documentation());
        assert!(!ext.default_enabled());
    }

    /// 命令存在性随启用状态：默认关闭时不出现，开启后出现且 busy_safe。
    #[test]
    fn command_surfaces_only_when_enabled() {
        let _g = lock();
        let _ad = AgentDirGuard::temp();
        set_extension_mode(ExtensionMode::All);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        register_extension(DocHelper::new());
        // 默认关闭：help 命令由本扩展提供但不过滤后不可见
        assert_ne!(
            crate::core::extensions::command_provider("help").as_deref(),
            Some(EXT)
        );
        assert!(crate::core::extensions::command_provider("help").is_none());

        set_extension_enabled(EXT, true);
        assert_eq!(
            crate::core::extensions::command_provider("help").as_deref(),
            Some(EXT)
        );
        assert!(
            crate::core::extensions::registered_commands()
                .iter()
                .any(|c| c.name == "help")
        );
        assert!(crate::core::extensions::command_busy_safe("help"));

        assert!(unregister_extension(EXT));
    }

    /// `/help` 无参：只提示，不触发回合、不注入消息。
    #[test]
    fn help_without_question_only_notifies() {
        let _g = lock();
        let _ad = AgentDirGuard::temp();
        let mut st = App::new();
        assert!(!command_help(&mut st, "help"));
        assert!(st.status_start.is_none());
        assert!(st.messages.is_empty());
        assert!(!st.system_messages.is_empty(), "应 push 一条 info 提示");
    }

    /// `/help <question>`：注入文档段 + 问题，触发普通回合。
    #[test]
    fn help_with_question_injects_and_starts_turn() {
        let _g = lock();
        let _ad = AgentDirGuard::temp();
        let mut st = App::new();
        assert!(!command_help(&mut st, "help how do I add a model"));

        let sent = st.status_start.clone().expect("应触发新回合");
        assert!(sent.contains("documentation (read only"));
        assert!(sent.contains("Additional docs:"));
        assert!(
            sent.ends_with("Question: how do I add a model"),
            "注入文本应以 Question 结尾：{sent}"
        );
        assert_eq!(st.messages.len(), 1);
        assert_eq!(st.messages[0].text(), sent);
    }

    /// 忙碌：走 steer 注入，不触发新回合、不入 messages。
    #[test]
    fn help_while_busy_queues_steer() {
        let _g = lock();
        let _ad = AgentDirGuard::temp();
        let mut st = App::new();
        st.busy = true;
        assert!(!command_help(&mut st, "help what is prux"));

        assert!(st.status_start.is_none());
        assert!(st.messages.is_empty());
        let inbox = st.runtime_steer_inbox.lock().unwrap();
        assert_eq!(inbox.len(), 1);
        assert!(inbox[0].text().contains("documentation (read only"));
        assert!(inbox[0].text().ends_with("Question: what is prux"));
    }
}
