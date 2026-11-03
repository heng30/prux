//! `hooks` 扩展：在生命周期事件上跑**用户配置的外部命令**，按它们的表态继续 / 拦截 / 改写。
//!
//! 与扩展系统里的 Rust 钩子不同，这里的钩子来自 `settings.json` 的 `hooks` 段（格式与
//! Claude Code 同形），改配置即可生效，不需要重新编译。支持四个事件：
//!
//! | 事件 | 时机 | 能做什么 |
//! |---|---|---|
//! | `PreToolUse` | 工具调用前 | 拒绝（`deny`）、要求确认（`ask`）、改写工具入参（`updatedInput`） |
//! | `PostToolUse` | 工具调用后 | 用 `reason` 覆盖工具结果交给模型，或追加一段上下文 |
//! | `UserPromptSubmit` | 用户提交输入时 | 拒绝这次提交（不启动回合，聊天区给理由）、追加一段上下文 |
//! | `Stop` | 一回合即将结算时 | `block` + `reason` 要求再跑一轮（`reason` 作为新一轮的用户消息） |
//!
//! 信封走 stdin（`hook_event_name` / `session_id` / `cwd` + 事件字段），结果按
//! 退出码与 stdout JSON 解释：`0` 看 JSON，`2` 视为阻止（理由取 stderr），其它退出码
//! 与超时都按「没有表态」处理，绝不让一条坏钩子挡住工具调用。
//!
//! 配置说明见 `docs/extensions/hooks.md`；`/hooks` 打印当前生效的钩子与配置错误。

mod config;
mod exec;

use crate::{
    core::{
        extensions::{
            AfterToolCallOutcome, BeforeToolCallOutcome, BoundaryOutcome, CommandHost, Extension,
            ExtensionCommand, ExtensionHook, ExtensionMode, ExtensionTool, ForkProjectInfo,
            SelectOption, UiNotifyLevel, UserPromptAction, ask_prompt,
        },
        provider::AgentMessage,
        session_manager::Session,
        tools::ToolError,
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_HOOKS, util::notify_plain},
    modes::tui::handlers::register_host_command,
    utils::{paths::cwd as current_cwd, process::blocking},
};
use config::{HookEvent, HooksConfig};
use exec::{Decision, Verdict};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
};

/// 扩展名（`/extension` 面板里的标识）。
const EXT: &str = "hooks";

/// 斜杠命令名（`/hooks`）。
const CMD: &str = "hooks";

/// `Stop` 钩子连续要求续跑的次数上限：超过就停下并提示，避免钩子里写错条件把会话焊死。
const MAX_STOP_CONTINUATIONS: u32 = 5;

/// 写进信封的 `tool_response` / 上下文长度上限（字符）：钩子不是数据通道。
const ENVELOPE_TEXT_LIMIT: usize = 32_000;

/// `Stop` 钩子已要求的续跑次数（每次真实用户提交归零）。
static STOP_CONTINUATIONS: AtomicU32 = AtomicU32::new(0);

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static HOOKS_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_HOOKS,
    make: || -> Arc<dyn Extension> { Arc::new(Hooks) },
};

/// 当前会话的 `(session_id, cwd)`，由会话钩子填充；缺失时 `session_id` 为空串。
fn session_ctx() -> &'static Mutex<(String, String)> {
    /// 信封里的会话标识；进程级单例（会话切换时整体覆盖）。
    static CTX: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));
    &CTX
}

/// 当前会话的 `(session_id, cwd)`；cwd 未初始化时回落到进程工作目录。
fn envelope_ctx() -> (String, String) {
    let (session_id, cwd) = session_ctx()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let cwd = if cwd.is_empty() {
        current_cwd().to_string_lossy().to_string()
    } else {
        cwd
    };
    (session_id, cwd)
}

/// 已经提示过的「钩子起不来」命令，避免每次工具调用都刷同一条警告。
fn warned() -> &'static Mutex<HashSet<String>> {
    /// 已提示过启动失败的命令串集合。
    static WARNED: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    WARNED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 钩子起不来 / 超时这类「配置有问题」的失败提示一次（不进模型上下文）。
fn warn_once(command: &str, reason: &str) {
    let first = warned()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(command.to_string());
    if first {
        notify_plain(format!("hook failed: {reason}"), UiNotifyLevel::Warning);
    }
}

/// 按截断上限裁一段文本（按字符边界，不切坏多字节）。
fn clip(text: &str) -> String {
    if text.chars().count() <= ENVELOPE_TEXT_LIMIT {
        return text.to_string();
    }
    let mut out: String = text.chars().take(ENVELOPE_TEXT_LIMIT).collect();
    out.push_str("\n… (truncated)");
    out
}

/// 依次跑一个事件下的全部钩子：入参改写逐条串联，意向按最严合并。
fn dispatch(
    config: &HooksConfig,
    event: HookEvent,
    tool_name: &str,
    base: Value,
    args: Value,
) -> (Decision, Value) {
    let (session_id, cwd) = envelope_ctx();
    let mut merged = Decision::default();
    let mut args = args;

    for group in config.matching(event, tool_name) {
        let mut extra = base.clone();
        if let Some(obj) = extra.as_object_mut() {
            if !tool_name.is_empty() {
                obj.insert("tool_name".to_string(), json!(tool_name));
            }
            if !args.is_null() {
                obj.insert("tool_input".to_string(), args.clone());
            }
        }
        let envelope = exec::envelope(event.as_str(), &session_id, &cwd, extra);

        for hook in &group.commands {
            let decision = exec::run(hook, &cwd, &envelope);
            if decision.verdict == Verdict::None
                && let Some(reason) = decision.reason.as_deref()
                && !reason.is_empty()
            {
                warn_once(&hook.command, reason);
            }
            if let Some(next) = decision.updated_input.clone() {
                args = next;
            }
            merged.merge(decision);
            // 已经拒绝就别再跑后面的钩子：它们拿到的是「不会发生的调用」。
            if merged.verdict == Verdict::Deny {
                return (merged, args);
            }
        }
    }

    (merged, args)
}

/// `hooks` 扩展主体。
pub struct Hooks;

impl Extension for Hooks {
    /// 扩展名 `hooks`。
    fn name(&self) -> &str {
        EXT
    }

    /// 来源：移植自 zerostack 的外部命令钩子（事件集合与信封协议则对齐 Claude Code）。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "zerostack".to_string(),
            plugin_version: "1.8.5".to_string(),
            url: String::from("https://github.com/gi-dellav/zerostack"),
        })
    }

    /// 面板说明：跑用户在 `settings.json` 里配置的生命周期命令。
    fn description(&self) -> &str {
        "Run user-configured lifecycle hooks (settings.json `hooks`) around tool calls, \
         prompts and turn settlement: block, ask, rewrite tool input or output, or continue \
         the run. /hooks lists the active hooks and config errors"
    }

    /// 仅 `Dev` / `Creator` 模式可用：它会改变工具调用行为。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认关闭：钩子是用户自己写的命令，没配置就没必要跑。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供工具：全部行为由事件钩子驱动。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/hooks`（打印生效的钩子与配置错误）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "List the hooks configured in settings.json and their config errors"
                .to_string(),
            // 只读配置并打印，不锁 agent。
            busy_safe: true,
            subcommands: Vec::new(),
        }]
    }

    /// 接线 `/hooks` 执行入口。
    fn on_registered(&self) {
        register_host_command(EXT, CMD, command_hooks);
    }

    /// 关心的事件面：工具前后、用户提交、结算边界。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![
            ExtensionHook::BeforeToolCallAsync,
            ExtensionHook::AfterToolCall,
            ExtensionHook::UserPrompt,
            ExtensionHook::Boundary,
        ]
    }

    /// 记住会话 id 与工作目录（信封字段），并复位 `Stop` 续跑计数。
    fn on_session_start(&self, cwd: &str, _messages: &[AgentMessage], session: Option<&Session>) {
        let session_id = session.map(|s| s.session_id.clone()).unwrap_or_default();
        *session_ctx().lock().unwrap_or_else(|e| e.into_inner()) = (session_id, cwd.to_string());
        STOP_CONTINUATIONS.store(0, Ordering::Relaxed);
    }

    /// 换会话时同步新会话标识（按会话文件名，`/new` 走的分支没有 `Session` 引用）。
    fn on_session_switched(&self, session_path: Option<&str>, _messages: &[AgentMessage]) {
        let id = crate::extensions::util::session_id_of(session_path).unwrap_or_default();
        session_ctx().lock().unwrap_or_else(|e| e.into_inner()).0 = id;
        STOP_CONTINUATIONS.store(0, Ordering::Relaxed);
    }

    /// 真实用户提交：`Stop` 的续跑计数从头算。
    fn on_user_submit(&self, _text: &str, _busy: bool) {
        STOP_CONTINUATIONS.store(0, Ordering::Relaxed);
    }

    /// `PreToolUse`：拒绝 / 请求确认 / 改写入参。
    fn before_tool_call_async(
        &self,
        name: String,
        args: Value,
        _assistant_message: Option<AgentMessage>,
        _context: Vec<AgentMessage>,
        _all_args: Value,
    ) -> BoxFuture<'static, std::result::Result<BeforeToolCallOutcome, ToolError>> {
        Box::pin(async move {
            let config = config::load();
            if config.is_empty() {
                return Ok(BeforeToolCallOutcome::allow());
            }

            let tool = name.clone();
            let original = args.clone();
            let outcome = crate::utils::process::spawn_blocking(move || {
                dispatch(&config, HookEvent::PreToolUse, &tool, json!({}), args)
            })
            .await
            .map_err(|e| ToolError(format!("hooks: {e}")))?;
            let (decision, rewritten) = outcome;
            // 只在实际变了时才把参数交回去，免得每次调用都触发一次无意义的替换。
            let args = (rewritten != original).then_some(rewritten);

            let reason = decision
                .reason
                .clone()
                .unwrap_or_else(|| format!("`PreToolUse` hook blocked tool \"{name}\""));

            if decision.verdict == Verdict::Ask {
                // 钩子要确认：面板里问一次，拿不到答复（非交互 / Esc / 超时）一律按拒绝。
                let answer = ask_prompt(
                    format!("hooks: {reason}"),
                    vec![
                        SelectOption::new("allow  Run this call once"),
                        SelectOption::new("deny  Block this call"),
                    ],
                    Some(std::time::Duration::from_secs(120)),
                )
                .await;
                if !answer.as_deref().is_some_and(|a| a.starts_with("allow")) {
                    return Ok(BeforeToolCallOutcome {
                        block: true,
                        reason: Some(reason),
                        terminate: false,
                        args: None,
                    });
                }
                return Ok(BeforeToolCallOutcome {
                    block: false,
                    reason: None,
                    terminate: false,
                    args,
                });
            }

            if decision.verdict == Verdict::Deny {
                return Ok(BeforeToolCallOutcome {
                    block: true,
                    reason: Some(reason),
                    terminate: false,
                    args: None,
                });
            }

            Ok(BeforeToolCallOutcome {
                block: false,
                reason: None,
                terminate: false,
                args,
            })
        })
    }

    /// `PostToolUse`：`reason` 覆盖工具结果，`additionalContext` 追加在后。
    fn after_tool_call_ext(
        &self,
        name: &str,
        args: &Value,
        result_text: &str,
        _is_error: bool,
    ) -> std::result::Result<AfterToolCallOutcome, ToolError> {
        let config = config::load();
        if config.is_empty() {
            return Ok(AfterToolCallOutcome {
                text: result_text.to_string(),
                content: None,
                details: None,
                is_error: None,
                usage: None,
                terminate: None,
            });
        }

        let tool = name.to_string();
        let args = args.clone();
        let response = clip(result_text);
        let (decision, _) = blocking(move || {
            dispatch(
                &config,
                HookEvent::PostToolUse,
                &tool,
                json!({ "tool_response": response }),
                args,
            )
        });

        let text = match (decision.verdict, decision.reason, decision.context) {
            // 拒绝语义在这里等于「把理由当成工具结果交给模型」：调用已经发生，撤不回来了。
            (Verdict::Deny, reason, context) => {
                let mut text =
                    reason.unwrap_or_else(|| format!("`PostToolUse` hook blocked tool \"{name}\""));
                if let Some(context) = context {
                    text.push('\n');
                    text.push_str(&context);
                }
                text
            }
            (_, _, Some(context)) => format!("{result_text}\n\n{context}"),
            (_, _, None) => result_text.to_string(),
        };

        Ok(AfterToolCallOutcome {
            text,
            content: None,
            details: None,
            is_error: None,
            usage: None,
            terminate: None,
        })
    }

    /// `UserPromptSubmit`：拒绝这次提交并提示理由，或把追加的上下文并进提示词。
    fn on_user_prompt(&self, text: &str) -> Option<UserPromptAction> {
        let config = config::load();
        if config.is_empty() {
            return None;
        }

        let prompt = clip(text);
        let (decision, _) = blocking(move || {
            dispatch(
                &config,
                HookEvent::UserPromptSubmit,
                "",
                json!({ "prompt": prompt }),
                Value::Null,
            )
        });

        match decision.verdict {
            Verdict::Deny => {
                let reason = decision
                    .reason
                    .unwrap_or_else(|| "`UserPromptSubmit` hook blocked this prompt".to_string());
                notify_plain(reason, UiNotifyLevel::Warning);
                Some(UserPromptAction::Handled)
            }
            _ => decision
                .context
                .filter(|ctx| !ctx.trim().is_empty())
                .map(|ctx| UserPromptAction::Rewrite(format!("{text}\n\n{ctx}"))),
        }
    }

    /// `Stop`：结算前跑钩子，有 `block` 就把理由作为新一轮的用户消息续跑（自限 [`MAX_STOP_CONTINUATIONS`] 次）。
    fn on_boundary(&self, event: &Value) -> Option<BoundaryOutcome> {
        if event.get("type").and_then(|v| v.as_str()) != Some("agent_before_settle") {
            return None;
        }
        // 子代理结算与用户主动中断、以及已有排队输入时不续跑（与 goal 同一套护栏）。
        if event.get("agentScope").and_then(|v| v.as_str()) != Some("main") {
            return None;
        }
        if event.get("outcome").and_then(|v| v.as_str()) == Some("aborted") {
            return None;
        }
        if event
            .get("hasQueuedMessages")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return None;
        }

        let config = config::load();
        if config.is_empty() {
            return None;
        }

        let active = STOP_CONTINUATIONS.load(Ordering::Relaxed) > 0;
        let (decision, _) = blocking(move || {
            dispatch(
                &config,
                HookEvent::Stop,
                "",
                json!({ "stop_hook_active": active }),
                Value::Null,
            )
        });

        if decision.verdict != Verdict::Deny {
            return None;
        }
        let reason = decision
            .reason
            .unwrap_or_else(|| "`Stop` hook asked to keep going".to_string());

        let count = STOP_CONTINUATIONS.fetch_add(1, Ordering::Relaxed) + 1;
        if count > MAX_STOP_CONTINUATIONS {
            STOP_CONTINUATIONS.store(0, Ordering::Relaxed);
            notify_plain(
                format!(
                    "`Stop` hook asked to continue {} times in a row; stopping. Fix the hook's condition.",
                    MAX_STOP_CONTINUATIONS
                ),
                UiNotifyLevel::Warning,
            );
            return None;
        }

        Some(BoundaryOutcome::continue_with(vec![
            AgentMessage::user_text(&reason),
        ]))
    }
}

/// `/hooks`：打印当前生效的钩子与配置错误。
fn command_hooks(host: &mut dyn CommandHost, _raw: &str) -> bool {
    let config = config::load();

    let mut lines: Vec<String> = Vec::new();
    if config.is_empty() {
        lines.push(format!(
            "No hooks configured. Add a `{}` section to settings.json (see docs/extensions/hooks.md).",
            config::SETTINGS_KEY
        ));
    } else {
        let total: usize = config.groups.iter().map(|g| g.commands.len()).sum();
        lines.push(format!(
            "{} hook command{} from settings.json `{}`:",
            total,
            if total == 1 { "" } else { "s" },
            config::SETTINGS_KEY
        ));
        for group in &config.groups {
            let matcher = if group.matcher_src.is_empty() {
                "*"
            } else {
                group.matcher_src.as_str()
            };
            for hook in &group.commands {
                lines.push(format!(
                    "  {}  {}  →  {}  ({}s)",
                    group.event.as_str(),
                    matcher,
                    hook.command,
                    hook.timeout.as_secs()
                ));
            }
        }
    }

    for error in &config.errors {
        lines.push(format!("config error: {error}"));
    }

    host.push_message(&lines.join("\n"), UiNotifyLevel::Info);
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/extension` 二级详情要展示「移植自 zerostack v1.8.5」及项目 URL。
    #[test]
    fn fork_project_reports_zerostack_source() {
        let info = Hooks.fork_project().expect("hooks 应声明 fork 来源");
        assert_eq!(info.plugin_name, "zerostack");
        assert_eq!(info.plugin_version, "1.8.5");
        assert_eq!(info.url, "https://github.com/gi-dellav/zerostack");
    }

    /// `settings.json` 的 `hooks` 段真的会被读到（走 `settings_section` 的真实路径）。
    #[test]
    fn loads_hooks_from_settings_json() {
        let _guard = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let agent = crate::test_support::AgentDirGuard::temp();
        std::fs::write(
            crate::core::settings_manager::agent_dir().join("settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"bash","hooks":[{"command":"true"}]}]}}"#,
        )
        .expect("write settings.json");

        let config = config::load();
        assert!(config.errors.is_empty(), "{:?}", config.errors);
        assert_eq!(config.groups.len(), 1);
        assert!(config.groups[0].matches(HookEvent::PreToolUse, "bash"));
        drop(agent);
    }

    /// 钩子命令按配置顺序串联改写，最严意向胜出。
    #[test]
    fn dispatch_chains_rewrites_and_keeps_the_strictest_verdict() {
        let config = config::parse(&json!({
            "PreToolUse": [{ "matcher": "bash", "hooks": [
                { "type": "command", "command": "echo '{\"hookSpecificOutput\":{\"updatedInput\":{\"command\":\"ls\"}}}'" },
                { "type": "command", "command": "echo '{\"decision\":\"approve\"}'" }
            ]}]
        }));
        let (decision, args) = dispatch(
            &config,
            HookEvent::PreToolUse,
            "bash",
            json!({}),
            json!({"command": "echo hi"}),
        );
        assert_eq!(decision.verdict, Verdict::Allow);
        assert_eq!(args["command"], "ls");
    }
}
