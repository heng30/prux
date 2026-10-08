//! `notify` 扩展：agent 从忙碌转为空闲（整轮 settle）时执行一条外部通知命令。
//!
//! **触发时机**：`agent_settled` —— 整轮（含重试 / 溢出恢复的所有 `run_loop` 尝试）
//! 真正结束、UI 复位忙碌态的那一刻。不能用 `agent_end`：它每次 `run_loop` 尝试都会发，
//! 重试期间会误触发多次；`Esc`/`Ctrl+C` 硬中断会丢弃回合 future，不经过本事件，
//! 因此**用户主动中断不会通知**（符合直觉）。
//!
//! **只在真正空闲时通知**：`agent_settled` 时若仍有排队用户输入（steer / follow-up），
//! UI 会立刻起下一轮（见 `drain_next_batch`），此刻并非真正空闲——本扩展跳过通知，
//! 保留本轮状态，等到队列排空、最后一次结算才发。因此耗时统计的是**整段忙碌期**
//! （首个 `agent_start` 到最终空闲结算），而非中途某一小轮。
//!
//! **通知内容**（`${message}`）：程序生成的**固定文案**，由三段拼成——
//! `会话名 · 状态 · 耗时`（如 `my-session · completed · 1m12s`）：
//! - 会话名：`on_session_start` 记录的会话名（未命名时回落 `prux`），
//!   并随 `session_info_changed` 事件更新；
//! - 状态：本轮最后一条 assistant 的 `stop_reason` → `completed` / `error` / `aborted`；
//! - 耗时：整段忙碌期首个 `agent_start` 到最后一次空闲 `agent_settled` 的墙钟时长
//!   （紧凑格式 `9s` / `1m12s`）。
//!
//! **配置**：`agent_dir()/extensions/notify.json` 的 `settledCommand`，例如
//! `notify-send prux "${message}"`。命令按空白切词后直接 `spawn`（不经 shell），
//! 每个词里的 `${message}` 都会被替换——因此文案即使含空格也仍是**单个参数**。
//! 模板不含占位符时，文案作为末位参数追加。空模板 = 不执行。
//!
//! **命令**：`/notify show` 打印当前 `settledCommand`；
//! `/notify settled <command>` 写入配置（如 `/notify settled notify-send prux ${message}`）；
//! `/notify reset [all|settled]` 清空命令（`all` 清全部、`settled` 只清 settledCommand）。
//!
//! **默认不启用**，声明 `Dev` / `Creator` 两个模式（`Minimal` 下不可用）。

mod config;

use crate::{
    APP_NAME,
    core::{
        extensions::{
            Extension, ExtensionCommand, ExtensionHook, ExtensionMode, ExtensionTool,
            SubcommandDef, UiNotifyLevel,
        },
        provider::AgentMessage,
        session_manager::{self, Session},
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_NOTIFY, command_arg, util::notify_text,
    },
    modes::interactive::{
        app::{App, MsgLevel},
        handlers::register_slash_command,
    },
    utils::time::format_duration,
};
use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// 扩展名（注册名，也是 `/extension` 面板里的名字）
const EXT: &str = "notify";

/// 斜杠命令名（`/notify`）
const CMD: &str = "notify";

/// 会话名缺省时的占位（固定文案的第一段）。
const DEFAULT_SESSION: &str = APP_NAME;

/// 外部命令的等待上限：超时强杀，避免用户命令挂死留下僵尸进程。
const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// `/notify` 子命令（顺序 = 输入框候选顺序）。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "show",
        description: "Print the current settledCommand",
    },
    SubcommandDef {
        name: "settled",
        description: "Set settledCommand (e.g. /notify settled notify-send prux ${message})",
    },
    SubcommandDef {
        name: "reset",
        description: "Clear commands: /notify reset <all|settled>",
    },
];

/// 外部命令启动失败只提示一次（与 loop-detect 的 hookCmd 同口径）。
static HOOK_WARNED: AtomicBool = AtomicBool::new(false);

/// 自声明工厂：linkme 分布式切片
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static NOTIFY_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_NOTIFY,
    make: || -> Arc<dyn Extension> { Arc::new(Notify) },
};

/// 进程内状态：本轮结算通知所需的三个字段。
#[derive(Default)]
struct State {
    /// 本轮最后一条 assistant 消息（用于推导状态）。
    last_assistant: Option<AgentMessage>,
    /// 本轮首个 `agent_start` 的墙钟起点（`None` = 本轮尚未开始）。
    started_at: Option<Instant>,
    /// 当前会话名（未命名/未知时 `None`，文案回落 [`DEFAULT_SESSION`]）。
    session_name: Option<String>,
}

/// 进程内状态单例（首次访问惰性初始化）：会话名、本轮起点与末条 assistant 都存在这里。
fn state() -> &'static Mutex<State> {
    /// 进程级状态单例，首次访问时惰性初始化。
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// 取状态锁；锁被毒化（持锁线程 panic）时取回内部数据继续用，不向外传播 panic。
fn lock() -> MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// `notify` 扩展。
pub struct Notify;

impl Extension for Notify {
    /// 扩展注册名（`/extension` 面板与 `/notify` 命令共用）。
    fn name(&self) -> &str {
        EXT
    }

    /// 面板里的功能说明：settle 时执行外部命令，命令模板与 `${message}` 占位符的语义。
    fn description(&self) -> &str {
        "Run an external command when the agent settles (busy → idle). Command template is read \
         from agent_dir()/extensions/notify.json (settledCommand); ${message} is replaced with a \
         fixed line built from session name · status · elapsed"
    }

    /// 声明 `Dev` / `Creator` 两个并排模式：`Minimal` 下不可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认不启用：需要外部命令属于用户主动配置的能力，不该开箱即跑。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供工具：本扩展只跑外部命令。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/notify` 命令（show / settled / reset 三个子命令）；`busy_safe` 允许忙碌时执行。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description:
                "/notify show prints the current settledCommand; /notify settled <command> sets it; \
                 /notify reset <all|settled> clears it"
                    .to_string(),
            busy_safe: true, // handler 只读写扩展配置 + 聊天区提示，不锁 agent，忙碌时可安全执行
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 接线 `/notify` 执行入口（命令的"存在性"由已启用扩展的 [`Self::commands`] 声明）。
    fn on_registered(&self) {
        register_slash_command(EXT, CMD, command_notify);
    }

    /// 启用时物化配置文件（首次加载自动创建并回填缺省键），让用户即使尚未触发
    /// settle 也能看到并编辑 `settledCommand`；禁用时清掉本轮缓存。
    fn on_enabled_changed(&self, enabled: bool) {
        if enabled {
            _ = config::load_config();
        } else {
            *lock() = State::default();
        }
    }

    /// 只订阅 agent 事件：据此记录会话名 / 状态，并在整轮结算时发通知。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![ExtensionHook::AgentEvent]
    }

    /// 记录会话名（会话加载 / 切换时）。
    fn on_session_start(&self, _cwd: &str, _messages: &[AgentMessage], session: Option<&Session>) {
        lock().session_name = session.and_then(|s| s.name.clone());
    }

    /// 会话切换（/new /resume /import /fork /clone）：从新会话文件读取名称。
    /// `None`（全新会话）时清空，避免沿用上一个会话的名字。
    fn on_session_switched(&self, session_path: Option<&str>, _messages: &[AgentMessage]) {
        let name = session_path
            .and_then(|p| session_manager::session_meta(Path::new(p)))
            .and_then(|m| m.name);
        lock().session_name = name;
    }

    /// 按事件类型分发：`agent_start` 记本轮起点、`message_end` 记末条 assistant、
    /// `session_info_changed` 更新会话名、`agent_settled` 结算发通知、
    /// `agent_end(aborted)` 复位被硬中断的本轮状态。
    fn on_agent_event(&self, event: &Value) {
        match event.get("type").and_then(|v| v.as_str()) {
            Some("agent_start") => {
                // 重试 / 续跑会重复发 agent_start：只记本轮首个，量整轮耗时。
                lock().started_at.get_or_insert_with(Instant::now);
            }
            Some("session_info_changed") => {
                if let Some(name) = event.get("name").and_then(|v| v.as_str()) {
                    lock().session_name = Some(name.to_string());
                }
            }
            Some("agent_end") => self.reset_on_hard_abort(event),
            Some("message_end") => self.track_assistant(event),
            Some("agent_settled") => self.settle(event),
            _ => {}
        }
    }
}

impl Notify {
    /// `agent_settled`：真正空闲（无排队用户输入）时才发通知。
    ///
    /// 有排队 steer / follow-up 时 UI 会立刻起下一轮，跳过通知但**保留**本轮状态
    /// （`started_at` / `last_assistant`），使最终那次通知报告的是整段忙碌时长与最后一轮的状态。
    fn settle(&self, event: &Value) {
        if has_queued_messages(event) {
            return;
        }

        self.fire_settled();
    }

    /// 硬中断（Esc/Ctrl+C / `AbortRun`）不经过 `agent_settled`：此处主动复位本轮状态，
    /// 否则下一次通知的耗时/状态会沿用被中断的回合并失真。
    fn reset_on_hard_abort(&self, event: &Value) {
        if event.get("reason").and_then(|v| v.as_str()) == Some("aborted") {
            let mut st = lock();
            st.last_assistant = None;
            st.started_at = None;
        }
    }

    /// 记录本轮最后一条 assistant 消息。
    fn track_assistant(&self, event: &Value) {
        let Some(raw) = event.get("message") else {
            return;
        };
        let Ok(msg) = serde_json::from_value::<AgentMessage>(raw.clone()) else {
            return;
        };
        if msg.role == "assistant" {
            lock().last_assistant = Some(msg);
        }
    }

    /// 整轮结算：读取配置、生成固定文案、执行外部命令，并复位本轮状态。
    fn fire_settled(&self) {
        let (session, status, elapsed) = {
            let mut st = lock();
            let session = st.session_name.clone();
            let status = status_of(st.last_assistant.as_ref());
            let elapsed = st.started_at.take().map(|t| t.elapsed());
            st.last_assistant = None;
            (session, status, elapsed)
        };

        let cmd = config::load_config().text("settledCommand");
        if cmd.trim().is_empty() {
            return;
        }
        spawn(&cmd, &build_message(session.as_deref(), status, elapsed));
    }
}

/// `agent_settled` 事件是否表示结算后仍**有排队用户输入**（steer / follow-up）——
/// 由 UI 在转发前补上（见 `interactive::handlers::events::apply_sink_event`）。
/// 缺字段时按「无排队」处理（直接调用 `on_agent_event` 的单测即走此路径）。
fn has_queued_messages(event: &Value) -> bool {
    event
        .get("hasQueuedMessages")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 由最后一条 assistant 的 `stop_reason` 推导状态。
pub(super) fn status_of(last: Option<&AgentMessage>) -> &'static str {
    match last.and_then(|m| m.stop_reason.as_deref()) {
        Some("aborted") => "aborted",
        Some("error") => "error",
        _ => "completed",
    }
}

/// 固定通知文案：`会话名 · 状态 · 耗时`（会话名未知时回落 [`DEFAULT_SESSION`]）。
///
/// 三段用 ` · ` 分隔；耗时未知时用 `-` 占位，保证文案结构稳定。
pub(super) fn build_message(
    session: Option<&str>,
    status: &str,
    elapsed: Option<Duration>,
) -> String {
    let who = session
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_SESSION);
    let dur = elapsed
        .map(|d| format_duration(d.as_millis() as u64))
        .unwrap_or_else(|| "-".to_string());
    format!("{who} · {status} · {dur}")
}

/// `/notify` 命令分发：`show` 展示当前配置，`settled <command>` 写入配置。
fn command_notify(st: &mut App, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b.trim()),
        None => (arg, ""),
    };

    match sub {
        "show" => {
            let cmd = config::load_config().text("settledCommand");
            if cmd.trim().is_empty() {
                st.push_msg("notify settledCommand: (unset)".to_string(), MsgLevel::Info);
            } else {
                st.push_msg(format!("notify settledCommand: {cmd}"), MsgLevel::Info);
            }
        }
        "settled" if rest.is_empty() => st.push_msg(
            "Usage: /notify settled <command> (use `${message}` for the notification text)"
                .to_string(),
            MsgLevel::Warning,
        ),
        "settled" => {
            config::set_settled_command(rest);
            st.push_msg(
                format!("notify settledCommand set: {rest}"),
                MsgLevel::Success,
            );
        }
        "reset" if rest.is_empty() => st.push_msg(
            "Usage: /notify reset <all|settled>".to_string(),
            MsgLevel::Warning,
        ),
        "reset" => match rest {
            "all" => {
                config::reset_all();
                st.push_msg(
                    "notify: all commands cleared".to_string(),
                    MsgLevel::Success,
                );
            }
            "settled" => {
                config::reset_settled_command();
                st.push_msg(
                    "notify settledCommand cleared".to_string(),
                    MsgLevel::Success,
                );
            }
            other => st.push_msg(
                format!("Unknown /notify reset target: {other}. Use `all` or `settled`."),
                MsgLevel::Warning,
            ),
        },
        "" => st.push_msg(
            "Usage: /notify show | /notify settled <command> | /notify reset <all|settled>"
                .to_string(),
            MsgLevel::Info,
        ),
        other => st.push_msg(
            format!("Unknown /notify subcommand: {other}. Use `show`, `settled` or `reset`."),
            MsgLevel::Warning,
        ),
    }
    false
}

/// 把命令模板展开为 argv：按空白切词，逐词替换 `${message}`。
///
/// 模板不含 `${message}` 时把文案作为末位参数追加（文案为空则不追加）。
pub(super) fn command_args(template: &str, message: &str) -> Vec<String> {
    let mut args: Vec<String> = template
        .split_whitespace()
        .map(|token| token.replace("${message}", message))
        .collect();

    if !template.contains("${message}") && !message.is_empty() {
        args.push(message.to_string());
    }
    args
}

/// 外部命令，fire-and-forget；超时强杀。
fn spawn(cmd: &str, message: &str) {
    let args = command_args(cmd, message);
    let Some((program, rest)) = args.split_first() else {
        return;
    };

    let mut command = Command::new(program);
    command
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    match command.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let deadline = Instant::now() + HOOK_TIMEOUT;
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) => {
                            if Instant::now() >= deadline {
                                _ = child.kill();
                                _ = child.wait();
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        Err(_) => break,
                    }
                }
            });
        }
        Err(err) => {
            if !HOOK_WARNED.swap(true, Ordering::Relaxed) {
                notify_text(
                    &format!("notify: settledCommand failed — {err}"),
                    UiNotifyLevel::Warning,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(text: &str) -> AgentMessage {
        let mut m = AgentMessage::user_text(text);
        m.role = "assistant".to_string();
        m
    }

    #[test]
    fn declares_dev_and_creator_modes_and_default_disabled() {
        let ext = Notify;
        assert!(!ext.default_enabled(), "notify 应默认不启用");
        assert_eq!(
            ext.modes(),
            vec![ExtensionMode::Dev, ExtensionMode::Creator],
            "notify 应在 Dev / Creator 两个模式下可用"
        );
        // 可用性：Dev/Creator 内可用，Minimal 下不可用
        assert!(ExtensionMode::Dev.usable_in(ExtensionMode::Dev));
        assert!(ExtensionMode::Creator.usable_in(ExtensionMode::Creator));
        assert!(!ExtensionMode::Dev.usable_in(ExtensionMode::Minimal));
        assert!(!ExtensionMode::Creator.usable_in(ExtensionMode::Minimal));
        assert!(ext.hooks().contains(&ExtensionHook::AgentEvent));
    }

    #[test]
    fn build_message_is_fixed_session_status_duration() {
        assert_eq!(
            build_message(
                Some("my-session"),
                "completed",
                Some(Duration::from_millis(72_000))
            ),
            "my-session · completed · 1m12s"
        );
        // 会话名未知 / 空 → 回落默认
        assert_eq!(
            build_message(None, "error", Some(Duration::from_secs(9))),
            "prux · error · 9s"
        );
        assert_eq!(
            build_message(Some("   "), "completed", None),
            "prux · completed · -"
        );
    }

    #[test]
    fn status_of_reads_last_assistant_stop_reason() {
        let mut m = assistant("x");
        m.stop_reason = Some("error".to_string());
        assert_eq!(status_of(Some(&m)), "error");

        let mut m = assistant("x");
        m.stop_reason = Some("aborted".to_string());
        assert_eq!(status_of(Some(&m)), "aborted");

        assert_eq!(status_of(Some(&assistant("x"))), "completed");
        assert_eq!(status_of(None), "completed");
    }

    #[test]
    fn command_args_substitutes_placeholder_token() {
        let args = command_args("notify-send -u normal ${message}", "hello world");
        assert_eq!(
            args,
            vec!["notify-send", "-u", "normal", "hello world"],
            "含空格的消息应保持单个参数"
        );
    }

    #[test]
    fn command_args_appends_message_when_no_placeholder() {
        let args = command_args("notify-send prux", "hello");
        assert_eq!(args, vec!["notify-send", "prux", "hello"]);
    }

    #[test]
    fn command_args_handles_multiple_placeholders() {
        let args = command_args("sh -c ${message} ${message}", "x");
        assert_eq!(args, vec!["sh", "-c", "x", "x"]);
    }

    /// 端到端：写配置 → 记录会话/起点/assistant → agent_settled → 外部命令收到固定文案。
    #[test]
    fn settled_command_runs_with_fixed_message() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = crate::core::settings_manager::agent_dir();
        let out = dir.join("msg.txt");
        let script = dir.join("capture.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s' \"$1\" > {}\n", out.display()),
        )
        .unwrap();

        let cfg_dir = dir.join("extensions");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join(config::CONFIG_FILE),
            serde_json::to_string(&serde_json::json!({
                "settledCommand": format!("sh {} ${{message}}", script.display()),
                "configVersion": 1,
            }))
            .unwrap(),
        )
        .unwrap();

        let ext = Notify;
        {
            let mut st = lock();
            st.session_name = Some("e2e".to_string());
            st.started_at = Some(Instant::now());
            let mut last = assistant("whatever");
            last.stop_reason = Some("error".to_string());
            st.last_assistant = Some(last);
        }
        ext.on_agent_event(&serde_json::json!({ "type": "agent_settled" }));

        // 外部命令异步执行：轮询等待输出文件。
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = std::fs::read_to_string(&out) {
                assert!(text.starts_with("e2e · error · "), "got: {text}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "settledCommand 未在超时内写出消息"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // 结算后本轮状态应复位
        let st = lock();
        assert!(st.last_assistant.is_none());
        assert!(st.started_at.is_none());
        assert_eq!(st.session_name.as_deref(), Some("e2e"));
    }

    #[test]
    fn track_assistant_only_keeps_assistant_role() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *lock() = State::default();
        let ext = Notify;
        ext.track_assistant(&serde_json::json!({
            "type": "message_end",
            "message": serde_json::to_value(assistant("hi")).unwrap(),
        }));
        assert_eq!(
            lock().last_assistant.as_ref().map(|m| m.text()),
            Some("hi".to_string())
        );

        let mut user = AgentMessage::user_text("nope");
        user.role = "user".to_string();
        ext.track_assistant(&serde_json::json!({
            "type": "message_end",
            "message": serde_json::to_value(user).unwrap(),
        }));
        assert_eq!(
            lock().last_assistant.as_ref().map(|m| m.text()),
            Some("hi".to_string()),
            "user 消息不应覆盖缓存的 assistant"
        );
        *lock() = State::default();
    }

    #[test]
    fn agent_start_keeps_first_timestamp() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *lock() = State::default();
        let ext = Notify;
        ext.on_agent_event(&serde_json::json!({ "type": "agent_start" }));
        let first = lock().started_at;
        ext.on_agent_event(&serde_json::json!({ "type": "agent_start" }));
        assert_eq!(lock().started_at, first, "重试的 agent_start 不应重置起点");
        *lock() = State::default();
    }

    /// 结算时仍有排队用户输入（UI 已补 `hasQueuedMessages`）：不发通知，
    /// 且保留本轮状态，让最终空闲那次通知报告整段忙碌期。
    #[test]
    fn settle_with_queued_messages_skips_notification_and_keeps_state() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *lock() = State::default();
        {
            let mut st = lock();
            st.session_name = Some("s".to_string());
            st.started_at = Some(Instant::now());
            st.last_assistant = Some(assistant("done"));
        }

        Notify.on_agent_event(&serde_json::json!({
            "type": "agent_settled",
            "hasQueuedMessages": true,
        }));

        let st = lock();
        assert!(
            st.started_at.is_some() && st.last_assistant.is_some(),
            "排队未清空时不得复位本轮状态（否则耗时/状态会丢失）"
        );
        drop(st);
        *lock() = State::default();
    }

    /// 缺字段（直接调用 `on_agent_event` 的路径）按无排队处理。
    #[test]
    fn settle_without_queue_field_treats_as_idle() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        *lock() = State::default();
        {
            let mut st = lock();
            st.started_at = Some(Instant::now());
            st.last_assistant = Some(assistant("done"));
        }

        Notify.on_agent_event(&serde_json::json!({ "type": "agent_settled" }));

        let st = lock();
        assert!(
            st.started_at.is_none() && st.last_assistant.is_none(),
            "无排队时应正常结算并复位本轮状态"
        );
        drop(st);
        *lock() = State::default();
    }

    /// 硬中断不发 `agent_settled`：`agent_end(reason=aborted)` 要复位本轮状态，
    /// 避免下一次通知沿用被中断回合的起点/状态。
    #[test]
    fn hard_abort_agent_end_resets_round_state() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *lock() = State::default();
        {
            let mut st = lock();
            st.started_at = Some(Instant::now());
            st.last_assistant = Some(assistant("partial"));
        }

        Notify.on_agent_event(&serde_json::json!({ "type": "agent_end" }));
        assert!(
            lock().started_at.is_some(),
            "普通 agent_end（每次 run_loop 尝试）不应复位"
        );

        Notify.on_agent_event(&serde_json::json!({
            "type": "agent_end",
            "reason": "aborted",
        }));
        let st = lock();
        assert!(st.started_at.is_none() && st.last_assistant.is_none());
        drop(st);
        *lock() = State::default();
    }

    #[test]
    fn session_info_changed_updates_name() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *lock() = State::default();
        Notify.on_agent_event(&serde_json::json!({
            "type": "session_info_changed",
            "name": "renamed",
        }));
        assert_eq!(lock().session_name.as_deref(), Some("renamed"));
        *lock() = State::default();
    }

    /// 最后一条系统消息的纯文本（命令 handler 的输出）。
    fn last_sys_text(st: &App) -> String {
        st.system_messages
            .last()
            .map(|(_, m)| crate::modes::interactive::app::sys_msg_text(m))
            .unwrap_or_default()
    }

    #[test]
    fn commands_declares_show_settled_and_reset_subcommands() {
        let cmds = Notify.commands();
        assert_eq!(cmds.len(), 1, "notify 只声明一个 /notify 命令");
        assert_eq!(cmds[0].name, "notify");
        assert!(cmds[0].busy_safe, "handler 不锁 agent，忙碌时可安全执行");
        let subs: Vec<&str> = cmds[0].subcommands.iter().map(|s| s.name).collect();
        assert_eq!(subs, vec!["show", "settled", "reset"]);
    }

    #[test]
    fn notify_show_prints_current_settled_command() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        config::set_settled_command("notify-send prux ${message}");

        let mut st = App::new();
        assert!(!command_notify(&mut st, "notify show"), "不应退出 TUI");
        assert!(
            last_sys_text(&st).contains("notify-send prux ${message}"),
            "got: {}",
            last_sys_text(&st)
        );
    }

    #[test]
    fn notify_settled_writes_config() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();

        let mut st = App::new();
        assert!(!command_notify(
            &mut st,
            "notify settled notify-send hi ${message}"
        ));
        assert_eq!(
            config::load_config().text("settledCommand"),
            "notify-send hi ${message}"
        );
        assert!(last_sys_text(&st).contains("notify-send hi ${message}"));
    }

    #[test]
    fn notify_reset_settled_clears_command() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        config::set_settled_command("notify-send hi ${message}");

        let mut st = App::new();
        assert!(!command_notify(&mut st, "notify reset settled"));
        assert_eq!(config::load_config().text("settledCommand"), "");
        assert!(last_sys_text(&st).contains("cleared"));
    }

    #[test]
    fn notify_reset_all_clears_command() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();

        config::set_settled_command("notify-send hi ${message}");
        let mut st = App::new();
        assert!(!command_notify(&mut st, "notify reset all"));
        assert_eq!(config::load_config().text("settledCommand"), "");
        assert!(last_sys_text(&st).contains("cleared"));
    }

    #[test]
    fn notify_reset_without_target_warns_and_keeps_command() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        config::set_settled_command("notify-send hi ${message}");

        let mut st = App::new();
        assert!(!command_notify(&mut st, "notify reset"));
        assert!(
            last_sys_text(&st).contains("Usage"),
            "got: {}",
            last_sys_text(&st)
        );
        assert_eq!(
            config::load_config().text("settledCommand"),
            "notify-send hi ${message}",
            "缺参不应清空配置"
        );
    }

    #[test]
    fn notify_reset_unknown_target_warns() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();

        let mut st = App::new();
        assert!(!command_notify(&mut st, "notify reset bogus"));
        assert!(
            last_sys_text(&st).contains("Unknown"),
            "got: {}",
            last_sys_text(&st)
        );
    }

    #[test]
    fn notify_usage_and_unknown_subcommands() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();

        let mut st = App::new();
        assert!(!command_notify(&mut st, "notify"));
        assert!(last_sys_text(&st).contains("Usage"), "裸命令应提示用法");

        let mut st2 = App::new();
        assert!(!command_notify(&mut st2, "notify settled"));
        assert!(last_sys_text(&st2).contains("Usage"), "缺参应提示用法");

        let mut st3 = App::new();
        assert!(!command_notify(&mut st3, "notify bogus"));
        assert!(last_sys_text(&st3).contains("Unknown"), "未知子命令应提示");
    }
}
