//! `/debug`：调试工具集
//!
//! 子命令按「调试对象」划分，一个子命令一个模块：
//! - `info`：agent 运行状态汇总（provider/model/会话叶子等），经 worker 回执；
//! - `provider`：供应商原始流事件查看器，订阅 [`ExtensionHook::ProviderStreamEvent`]，
//!   把**归一化之前**的 provider 流事件缓冲起来，每轮 assistant 消息结束时渲染一张卡片
//!   （provider/model/api + 捕获条数 + 事件摘要），并把整份事件载荷落成会话自定义条目
//!   （供导出与排查）；
//! - `system-prompt`：打印当前系统提示词原文，经 worker 回执。
//!
//! 默认**不启用**（声明 `default_enabled() == false`）：`provider` 声明了最高频的订阅面，
//! 不该默认给所有用户挂上。只在 `/extension` 面板开启（`Dev` 及以上模式可用）。

mod provider;

use super::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_DEBUG, command_arg};
use crate::{
    core::extensions::{
        CommandHost, Extension, ExtensionCommand, ExtensionHook, ExtensionMode, ExtensionTool,
        RichSpan, SubcommandDef, UiNotifyLevel,
    },
    modes::interactive::handlers::register_host_command,
};
use serde_json::Value;
use std::sync::Arc;

/// 扩展名（也是 `/extension` 面板中的标识）
const EXT: &str = "debug";
/// 斜杠命令名（与扩展名一致）
const CMD: &str = "debug";

/// `/debug` 子命令（顺序 = 输入框候选顺序）。
///
/// 候选面板只做一级：`/debug ` 之后列出本表，写了第二个词（`/debug provider on`）面板即关闭，
/// 故 `provider` 的 `[on|off|dump]` 只能写进说明里提示。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "info",
        description: "Show agent runtime status (provider, model, session)",
    },
    SubcommandDef {
        name: "provider",
        description: "Capture raw provider stream events ([on|off|dump])",
    },
    SubcommandDef {
        name: "system-prompt",
        description: "Print the current system prompt",
    },
];

/// 自声明工厂：linkme 分布式切片
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static DEBUG_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_DEBUG,
    make: || -> Arc<dyn Extension> { Arc::new(Debug) },
};

/// 调试工具集扩展（`/debug`）
pub struct Debug;

impl Extension for Debug {
    /// 返回固定扩展名 `debug`。
    fn name(&self) -> &str {
        EXT
    }

    /// 返回 `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Debug helpers: agent status, raw provider stream events, system prompt"
    }

    /// 默认关闭：高频的 provider 订阅按需开启（/extension 面板空格）
    fn default_enabled(&self) -> bool {
        false
    }

    /// 仅在 Dev 及以上模式可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev]
    }

    /// 不提供任何工具。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/debug` 斜杠命令（busy-safe，带 info/provider/system-prompt 子命令）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "/debug <info|provider|system-prompt> - debug helpers".to_string(),
            busy_safe: true, // handler 只碰扩展自身状态、App 消息与命令入队，不锁 agent
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 订阅 agent 事件（回合边界）与 provider 原始流事件（`provider` 子命令用）。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![
            ExtensionHook::AgentEvent,
            ExtensionHook::ProviderStreamEvent,
        ]
    }

    /// 注册 `/debug` 的命令处理器。
    fn on_registered(&self) {
        register_host_command(EXT, CMD, command_debug);
    }

    /// 转发给 `provider` 子命令的捕获缓冲。
    fn provider_stream_event(&self, event: &Value) {
        provider::on_stream_event(event);
    }

    /// 转发给 `provider` 子命令：turn_start / agent_end 重置缓冲，message_end 渲染卡片。
    fn on_agent_event(&self, event: &Value) {
        provider::on_agent_event(event);
    }

    /// 只认领 `provider` 子命令落盘的条目类型；其它类型返回 `None`。
    fn render_custom_message(&self, custom_type: &str, data: &Value) -> Option<Vec<RichSpan>> {
        provider::render_custom_message(custom_type, data)
    }
}

/// `/debug <info|provider|system-prompt> [args]`：按子命令分派，未知子命令打印用法。
///
/// 参数原文经 [`command_arg`] 去掉命令名后，再按首个空白切成「子命令 + 其余参数」。
fn command_debug(host: &mut dyn CommandHost, raw: &str) -> bool {
    let args = command_arg(raw);
    let (sub, rest) = match args.split_once(char::is_whitespace) {
        Some((sub, rest)) => (sub, rest.trim()),
        None => (args, ""),
    };

    match sub.to_lowercase().as_str() {
        "info" => host.debug_info(),
        "provider" => provider::command(host, rest),
        "system-prompt" => host.debug_system_prompt(),
        other => host.push_message(&usage(other), UiNotifyLevel::Warning),
    }
    false
}

/// 未知子命令（或未给子命令）时的用法提示；`arg` 非空时一并回显，便于定位敲错的词。
fn usage(arg: &str) -> String {
    let head = "Usage: /debug <info|provider|system-prompt>";
    if arg.is_empty() {
        head.to_string()
    } else {
        format!("{head} (got `{arg}`)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::extensions::{
            ExtensionMode, command_provider, register_extension, set_extension_enabled,
            set_extension_mode, unregister_extension,
        },
        modes::interactive::{
            agent_actor::{AgentCommand, WorkerHandle, channels},
            app::{App, sys_spans_text},
        },
        test_support::{AUTH_TEST_LOCK, AgentDirGuard},
    };

    /// 串行锁：扩展注册表与 UI 队列都是进程级全局态。
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 把 App 的命令通道换成可观测的测试通道（默认 `App::new()` 是丢弃命令的 null 句柄）。
    fn app_with_channel() -> (App, tokio::sync::mpsc::UnboundedReceiver<AgentCommand>) {
        let (tx, rx) = channels();
        let mut st = App::new();
        st.worker = WorkerHandle::new(tx);
        (st, rx)
    }

    /// 注册并启用扩展（默认关闭，需显式开启才会收到事件）
    pub(super) fn register_enabled() {
        let _ = unregister_extension(EXT);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        set_extension_mode(ExtensionMode::All);
        register_extension(Debug);
        set_extension_enabled(EXT, true);
        provider::reset_for_test();
    }

    /// 注销扩展并复位 provider 捕获状态
    pub(super) fn cleanup() {
        provider::reset_for_test();
        let _ = unregister_extension(EXT);
    }

    #[test]
    fn registering_extension_exposes_command() {
        let _g = lock();
        let _ad = AgentDirGuard::temp();
        register_enabled();
        assert_eq!(command_provider(CMD).as_deref(), Some(EXT));
        cleanup();
    }

    #[test]
    fn subcommands_match_parser() {
        // 候选元数据与 command_debug 的分支一一对应
        let names: Vec<&str> = SUBCOMMANDS.iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["info", "provider", "system-prompt"]);

        let cmds = Debug.commands();
        assert_eq!(cmds[0].subcommands, SUBCOMMANDS.to_vec());

        for name in names {
            let mut st = App::new();
            command_debug(&mut st, &format!("debug {name}"));
            assert!(
                !st.system_messages
                    .iter()
                    .any(|(_, m)| sys_spans_text(&m.spans).contains("Usage:")),
                "子命令 `{name}` 应被解析器识别"
            );
        }
    }

    #[test]
    fn info_and_system_prompt_ask_the_worker() {
        let _g = lock();

        let (mut st, mut rx) = app_with_channel();
        command_debug(&mut st, "debug info");
        assert!(
            matches!(rx.try_recv(), Ok(AgentCommand::DebugInfo)),
            "info 应下发 DebugInfo 给 worker"
        );

        let (mut st, mut rx) = app_with_channel();
        command_debug(&mut st, "debug system-prompt");
        assert!(
            matches!(rx.try_recv(), Ok(AgentCommand::SystemPrompt)),
            "system-prompt 应下发 SystemPrompt 给 worker"
        );
    }

    #[test]
    fn missing_or_unknown_subcommand_reports_usage() {
        let _g = lock();

        let mut st = App::new();
        assert!(!command_debug(&mut st, "debug"));
        assert!(st.system_messages.iter().any(|(_, m)| {
            let text = sys_spans_text(&m.spans);
            text.contains("Usage:") && !text.contains("got")
        }));

        let mut st = App::new();
        command_debug(&mut st, "debug bogus");
        assert!(
            st.system_messages
                .iter()
                .any(|(_, m)| { sys_spans_text(&m.spans).contains("(got `bogus`)") })
        );
    }

    #[test]
    fn provider_subcommand_arguments_reach_the_provider_module() {
        let _g = lock();
        provider::reset_for_test();

        // 无参切换开启
        let mut st = App::new();
        command_debug(&mut st, "debug provider");
        assert!(provider::enabled(), "无参 `provider` 应切换为开启");

        // 带参 `off` 关闭
        let mut st = App::new();
        command_debug(&mut st, "debug provider off");
        assert!(!provider::enabled(), "`provider off` 应关闭捕获");

        // 未知动作：提示用法且不改状态
        let mut st = App::new();
        command_debug(&mut st, "debug provider bogus");
        assert!(!provider::enabled());
        assert!(
            st.system_messages
                .iter()
                .any(|(_, m)| { sys_spans_text(&m.spans).contains("Usage: /debug provider") })
        );

        cleanup();
    }
}
