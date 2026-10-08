//! `/debug-provider`：供应商原始流事件查看器
//!
//! 订阅 [`ExtensionHook::ProviderStreamEvent`]，把**归一化之前**的 provider 流事件缓冲起来，
//! 每轮 assistant 消息结束时渲染一张卡片（provider/model/api + 捕获条数 + 事件摘要），
//! 并把整份事件载荷落成会话自定义条目（供导出与排查）。
//!
//! 默认**不启用**（声明 `default_enabled() == false`）：只在 `/extension` 面板开启，
//! 避免默认给所有用户多挂一个高频订阅。

use super::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_DEBUG_PROVIDER, command_arg};
use crate::{
    core::extensions::{
        Extension, ExtensionCommand, ExtensionHook, ExtensionMode, ExtensionTool,
        ExtensionUiRequest, RichSpan, SubcommandDef, request_ui,
    },
    extensions::util::truncate_chars,
    modes::interactive::{
        app::{App, MsgLevel},
        handlers::register_slash_command,
    },
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

/// 扩展名（也是 `/extension` 面板中的标识）
const EXT: &str = "debug-provider";
/// 斜杠命令名（与扩展名一致）
const CMD: &str = "debug-provider";
/// 会话自定义条目类型（导出页按 customType 显示为 `[debug-provider-events]` 卡片）
const ENTRY_TYPE: &str = "debug-provider-events";
/// 单轮捕获的事件条数上限（超出丢弃并计数，防止巨型流吃内存）
const MAX_EVENTS: usize = 2000;
/// 卡片正文内联展示的事件条数（其余只在落盘载荷 / `dump` 里）
const CARD_INLINE_EVENTS: usize = 8;
/// 卡片正文单行 JSON 的截断长度
const CARD_LINE_MAX: usize = 200;

/// `/debug-provider` 子命令（顺序 = 输入框候选顺序）
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "on",
        description: "Start capturing raw provider stream events",
    },
    SubcommandDef {
        name: "off",
        description: "Stop capturing and drop the buffer",
    },
    SubcommandDef {
        name: "dump",
        description: "Print the currently buffered events as JSON",
    },
];

/// 捕获总开关（`/debug-provider on|off` 控制，默认关闭）
static ENABLED: AtomicBool = AtomicBool::new(false);
/// 本轮因超出 [`MAX_EVENTS`] 而被丢弃的事件条数
static DROPPED: AtomicUsize = AtomicUsize::new(0);
/// 本轮已捕获的 provider 原始流事件（`message_end` 时整批取走）
static EVENTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
/// 最近一轮已完成捕获的载荷（`dump` 用；不像 [`EVENTS`] 会被 message_end 掏空）
static LAST: Mutex<Option<Value>> = Mutex::new(None);

/// 自声明工厂：linkme 分布式切片
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static DEBUG_PROVIDER_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_DEBUG_PROVIDER,
    make: || -> Arc<dyn Extension> { Arc::new(DebugProvider) },
};

/// 捕获开关（`/debug-provider on|off`）
fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 取走本轮的捕获（清空缓冲）
fn take_events() -> (Vec<Value>, usize) {
    let mut buf = EVENTS.lock().unwrap();
    let events = std::mem::take(&mut *buf);
    let dropped = DROPPED.swap(0, Ordering::Relaxed);
    (events, dropped)
}

/// 清空本轮捕获的事件缓冲并重置丢弃计数（turn_start / agent_end 时调用）。
fn reset_capture() {
    EVENTS.lock().unwrap().clear();
    DROPPED.store(0, Ordering::Relaxed);
}

/// provider 原始流事件查看器扩展（`/debug-provider`）
pub struct DebugProvider;

impl Extension for DebugProvider {
    /// 返回固定扩展名 `debug-provider`。
    fn name(&self) -> &str {
        EXT
    }

    /// 返回 `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Capture raw provider stream events (pre-normalization) and show them as session cards"
    }

    /// 默认关闭：高频订阅按需开启（/extension 面板空格）
    fn default_enabled(&self) -> bool {
        false
    }

    /// 仅在 Dev 模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev]
    }

    /// 不提供任何工具。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/debug-provider` 斜杠命令（busy-safe，带 on/off/dump 子命令）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description:
                "/debug-provider [on|off|dump] toggles capturing raw provider stream events"
                    .to_string(),
            busy_safe: true, // handler 只碰扩展自身状态与 App 消息，不锁 agent
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 订阅 agent 事件（回合边界）与 provider 原始流事件。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![
            ExtensionHook::AgentEvent,
            ExtensionHook::ProviderStreamEvent,
        ]
    }

    /// 注册 `/debug-provider` 的命令处理器。
    fn on_registered(&self) {
        register_slash_command(EXT, CMD, command_debug_provider);
    }

    /// 捕获开启时把 `event["data"]` 追加进缓冲；超过 [`MAX_EVENTS`] 只累加丢弃计数。
    fn provider_stream_event(&self, event: &Value) {
        if !enabled() {
            return;
        }

        let Some(data) = event.get("data") else {
            return;
        };

        let mut buf = EVENTS.lock().unwrap();
        if buf.len() >= MAX_EVENTS {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            return;
        }

        buf.push(data.clone());
    }

    /// 按事件类型分派：turn_start / agent_end 重置缓冲，message_end 把本轮捕获渲染成卡片。
    fn on_agent_event(&self, event: &Value) {
        match event.get("type").and_then(|v| v.as_str()) {
            Some("turn_start") => reset_capture(),
            Some("message_end") => self.flush_assistant(event),
            Some("agent_end") => reset_capture(),
            _ => {}
        }
    }

    /// 只认领 [`ENTRY_TYPE`]：渲染 provider/model/api + 捕获条数 + 前若干条事件摘要；其它类型返回 `None`。
    fn render_custom_message(&self, custom_type: &str, data: &Value) -> Option<Vec<RichSpan>> {
        if custom_type != ENTRY_TYPE {
            return None;
        }

        let provider = data.get("provider").and_then(|v| v.as_str()).unwrap_or("?");
        let model = data.get("model").and_then(|v| v.as_str()).unwrap_or("?");
        let api = data.get("api").and_then(|v| v.as_str()).unwrap_or("?");
        let events = data.get("events").and_then(|v| v.as_array());
        let count = events.map(|e| e.len()).unwrap_or(0);

        let mut spans = vec![RichSpan {
            fg: Some("accent".to_string()),
            text: format!("{provider}/{model} ({api}) · {count} raw event(s)"),
        }];

        if let Some(events) = events {
            for ev in events.iter().take(CARD_INLINE_EVENTS) {
                spans.push(RichSpan::plain(format!(
                    "\n  {}",
                    truncate_chars(&ev.to_string(), CARD_LINE_MAX)
                )));
            }

            if count > CARD_INLINE_EVENTS {
                spans.push(RichSpan {
                    fg: Some("dim".to_string()),
                    text: format!(
                        "\n  … {} more (run /debug-provider dump for the full payload)",
                        count - CARD_INLINE_EVENTS
                    ),
                });
            }
        }
        Some(spans)
    }
}

impl DebugProvider {
    /// assistant 消息结束：把本轮捕获渲染成卡片并落盘（无捕获则什么都不做）
    fn flush_assistant(&self, event: &Value) {
        if !enabled() {
            return;
        }

        let Some(raw) = event.get("message") else {
            return;
        };

        if raw.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            return;
        }

        let (events, dropped) = take_events();
        if events.is_empty() {
            return;
        }

        let payload = capture_payload(raw, events, dropped);
        *LAST.lock().unwrap() = Some(payload.clone());

        // 落盘（导出页可切换显示）+ 实时卡片
        request_ui(ExtensionUiRequest::PersistSessionEntry {
            custom_type: ENTRY_TYPE.to_string(),
            data: payload.clone(),
        });

        request_ui(ExtensionUiRequest::CustomMessage {
            label: "provider debug".to_string(),
            custom_type: ENTRY_TYPE.to_string(),
            data: payload,
        });
    }
}

/// 一轮捕获的载荷（落盘条目与实时卡片共用同一形状）：
/// `{provider, api, model, dropped, events}`。
fn capture_payload(message: &Value, events: Vec<Value>, dropped: usize) -> Value {
    json!({
        "provider": message.get("provider").and_then(|v| v.as_str()).unwrap_or("?"),
        "api": message.get("api").and_then(|v| v.as_str()).unwrap_or("?"),
        "model": message.get("model").and_then(|v| v.as_str()).unwrap_or("?"),
        "dropped": dropped,
        "events": events,
    })
}

/// `/debug-provider [on|off|dump]`：无参切换捕获；`dump` 打印当前缓冲的完整 JSON。
fn command_debug_provider(st: &mut App, raw: &str) -> bool {
    match command_arg(raw).trim().to_lowercase().as_str() {
        "" => {
            let next = !enabled();
            set_enabled(st, next);
        }
        "on" => set_enabled(st, true),
        "off" => set_enabled(st, false),
        "dump" => dump(st),
        other => st.push_msg(
            format!("Usage: /debug-provider [on|off|dump] (got `{other}`)"),
            MsgLevel::Warning,
        ),
    }
    false
}

/// 设置捕获开关并清空缓冲；关闭时同时丢弃最近一轮载荷，并在 App 里提示当前状态。
fn set_enabled(st: &mut App, value: bool) {
    ENABLED.store(value, Ordering::Relaxed);
    reset_capture();

    if !value {
        *LAST.lock().unwrap() = None;
    }

    st.push_msg(
        format!(
            "Provider event capture {}",
            if value { "enabled" } else { "disabled" }
        ),
        MsgLevel::Info,
    );
}

/// 把最近一轮已完成捕获的 JSON 格式化打印到消息区；没有捕获时提示先开启。
fn dump(st: &mut App) {
    let Some(payload) = LAST.lock().unwrap().clone() else {
        st.push_msg(
            "No captured provider events. Run /debug-provider on first.".to_string(),
            MsgLevel::Info,
        );
        return;
    };

    let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string());
    st.push_msg(text, MsgLevel::Info);
}

/// 当前缓冲的事件副本（测试用，不清空）
#[cfg(test)]
fn peek_events() -> Vec<Value> {
    EVENTS.lock().unwrap().clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::{
            extensions::{
                ExtensionMode, command_provider, dispatch_provider_stream_event,
                register_extension, set_extension_enabled, set_extension_mode,
                unregister_extension,
            },
            settings_manager,
        },
        test_support::{AUTH_TEST_LOCK, AgentDirGuard},
    };

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 注册并启用扩展（默认关闭，需显式开启才会收到事件）
    fn register_enabled() {
        let _ = unregister_extension(EXT);
        settings_manager::write_disabled_extensions(&[]).ok();
        set_extension_mode(ExtensionMode::All);
        register_extension(DebugProvider);
        set_extension_enabled(EXT, true);
        ENABLED.store(false, Ordering::Relaxed);
        reset_capture();
        *LAST.lock().unwrap() = None;
    }

    fn cleanup() {
        ENABLED.store(false, Ordering::Relaxed);
        reset_capture();
        *LAST.lock().unwrap() = None;
        let _ = unregister_extension(EXT);
    }

    #[test]
    fn capture_is_off_until_enabled() {
        let _g = lock();
        let _ad = AgentDirGuard::temp();
        register_enabled();

        dispatch_provider_stream_event("openai", "openai-completions", "m", &json!({"n": 1}));
        assert!(peek_events().is_empty(), "捕获开关关闭时不缓冲");

        ENABLED.store(true, Ordering::Relaxed);
        dispatch_provider_stream_event("openai", "openai-completions", "m", &json!({"n": 2}));
        let captured = peek_events();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0]["n"], 2);

        cleanup();
    }

    #[test]
    fn assistant_message_end_emits_card() {
        let _g = lock();
        let ext = DebugProvider;
        ENABLED.store(true, Ordering::Relaxed);
        reset_capture();
        ext.provider_stream_event(&json!({"data": {"type": "response.created"}}));

        ext.flush_assistant(&json!({
            "type": "message_end",
            "message": {"role": "assistant", "provider": "openai", "api": "openai-completions", "model": "gpt-x"}
        }));
        assert!(
            peek_events().is_empty(),
            "flush 后缓冲清空（事件已随载荷交出）"
        );

        // 刻意**不**消费全局 UI 请求队列：那是进程级共享队列，
        // 偷看会吃掉并行测试（如 subagent 的完成通知）的请求。
        let payload = capture_payload(
            &json!({"provider": "openai", "api": "openai-completions", "model": "gpt-x"}),
            vec![json!({"a": 1})],
            0,
        );
        assert_eq!(payload["provider"], "openai");
        assert_eq!(payload["events"].as_array().map(|e| e.len()), Some(1));

        let rendered = ext
            .render_custom_message(ENTRY_TYPE, &payload)
            .expect("应认领自己的 custom_type");
        assert!(rendered[0].text.contains("openai/gpt-x"));
        assert!(rendered[0].text.contains("1 raw event"));
        assert!(
            ext.render_custom_message("someone-else", &json!({}))
                .is_none()
        );

        cleanup();
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
        // 候选元数据与 command_debug_provider 的分支一一对应
        let names: Vec<&str> = SUBCOMMANDS.iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["on", "off", "dump"]);

        let cmds = DebugProvider.commands();
        assert_eq!(cmds[0].subcommands, SUBCOMMANDS.to_vec());

        for name in names {
            let mut st = App::new();
            command_debug_provider(&mut st, &format!("debug-provider {name}"));
            assert!(
                !st.system_messages.iter().any(|(_, m)| {
                    crate::modes::interactive::app::sys_spans_text(&m.spans).contains("Usage:")
                }),
                "子命令 `{name}` 应被解析器识别"
            );
        }
    }

    #[test]
    fn dump_prints_last_completed_payload() {
        let _g = lock();
        ENABLED.store(true, Ordering::Relaxed);
        reset_capture();
        *LAST.lock().unwrap() = Some(json!({
            "provider": "openai",
            "api": "openai-completions",
            "model": "gpt-x",
            "dropped": 0,
            "events": [{"type": "response.created"}],
        }));

        let mut st = App::new();
        command_debug_provider(&mut st, "debug-provider dump");
        assert!(
            st.system_messages.iter().any(|(_, m)| {
                let text = crate::modes::interactive::app::sys_spans_text(&m.spans);
                text.contains("openai") && text.contains("response.created")
            }),
            "dump 应打印最近一轮已完成的载荷"
        );

        cleanup();
    }

    #[test]
    fn command_toggles_and_reports_usage() {
        let _g = lock();
        ENABLED.store(false, Ordering::Relaxed);
        reset_capture();
        *LAST.lock().unwrap() = None;

        let mut st = App::new();
        assert!(!command_debug_provider(&mut st, CMD));
        assert!(enabled(), "无参切换为开启");
        assert!(st.system_messages.iter().any(|(_, m)| {
            crate::modes::interactive::app::sys_spans_text(&m.spans).contains("enabled")
        }));

        let mut st2 = App::new();
        command_debug_provider(&mut st2, "debug-provider off");
        assert!(!enabled(), "off 关闭捕获");

        let mut st3 = App::new();
        command_debug_provider(&mut st3, "debug-provider bogus");
        assert!(st3.system_messages.iter().any(|(_, m)| {
            crate::modes::interactive::app::sys_spans_text(&m.spans).contains("Usage:")
        }));

        let mut st4 = App::new();
        command_debug_provider(&mut st4, "debug-provider dump");
        assert!(st4.system_messages.iter().any(|(_, m)| {
            crate::modes::interactive::app::sys_spans_text(&m.spans).contains("No captured")
        }));

        cleanup();
    }
}
