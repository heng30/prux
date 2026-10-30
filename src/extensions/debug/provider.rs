//! `/debug provider`：供应商原始流事件查看器
//!
//! 订阅 [`ExtensionHook::ProviderStreamEvent`]，把**归一化之前**的 provider 流事件缓冲起来，
//! 每轮 assistant 消息结束时渲染一张卡片（provider/model/api + 捕获条数 + 事件摘要），
//! 并把整份事件载荷落成会话自定义条目（供导出与排查）。
//!
//! 捕获默认关闭，由 `/debug provider [on|off|dump]` 控制——扩展被启用不等于开始吃事件。

use crate::{
    core::extensions::{CommandHost, ExtensionUiRequest, RichSpan, UiNotifyLevel, request_ui},
    extensions::util::truncate_chars,
};
use serde_json::{Value, json};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

/// 会话自定义条目类型（导出页按 customType 显示为 `[debug-provider-events]` 卡片）。
///
/// 扩展改名为 `debug` 后刻意保持不变：此前落盘的会话条目仍要能被本模块认领渲染。
const ENTRY_TYPE: &str = "debug-provider-events";
/// 单轮捕获的事件条数上限（超出丢弃并计数，防止巨型流吃内存）
const MAX_EVENTS: usize = 2000;
/// 卡片正文内联展示的事件条数（其余只在落盘载荷 / `dump` 里）
const CARD_INLINE_EVENTS: usize = 8;
/// 卡片正文单行 JSON 的截断长度
const CARD_LINE_MAX: usize = 200;

/// 捕获总开关（`/debug provider on|off` 控制，默认关闭）
static ENABLED: AtomicBool = AtomicBool::new(false);
/// 本轮因超出 [`MAX_EVENTS`] 而被丢弃的事件条数
static DROPPED: AtomicUsize = AtomicUsize::new(0);
/// 本轮已捕获的 provider 原始流事件（`message_end` 时整批取走）
static EVENTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());
/// 最近一轮已完成捕获的载荷（`dump` 用；不像 [`EVENTS`] 会被 message_end 掏空）
static LAST: Mutex<Option<Value>> = Mutex::new(None);

/// 捕获开关（`/debug provider on|off`）
pub(super) fn enabled() -> bool {
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
pub(super) fn reset_capture() {
    EVENTS.lock().unwrap().clear();
    DROPPED.store(0, Ordering::Relaxed);
}

/// 捕获开启时把 `event["data"]` 追加进缓冲；超过 [`MAX_EVENTS`] 只累加丢弃计数。
pub(super) fn on_stream_event(event: &Value) {
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
pub(super) fn on_agent_event(event: &Value) {
    match event.get("type").and_then(|v| v.as_str()) {
        Some("turn_start") => reset_capture(),
        Some("message_end") => flush_assistant(event),
        Some("agent_end") => reset_capture(),
        _ => {}
    }
}

/// 只认领 [`ENTRY_TYPE`]：渲染 provider/model/api + 捕获条数 + 前若干条事件摘要；其它类型返回 `None`。
pub(super) fn render_custom_message(custom_type: &str, data: &Value) -> Option<Vec<RichSpan>> {
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
                    "\n  … {} more (run /debug provider dump for the full payload)",
                    count - CARD_INLINE_EVENTS
                ),
            });
        }
    }
    Some(spans)
}

/// `/debug provider [on|off|dump]`：无参切换捕获；`dump` 打印最近一轮已完成的完整 JSON。
///
/// `args` 是命令名之后、子命令 `provider` 之后的剩余参数（由 `/debug` 的处理器切分）。
pub(super) fn command(host: &mut dyn CommandHost, args: &str) {
    match args.trim().to_lowercase().as_str() {
        "" => set_enabled(host, !enabled()),
        "on" => set_enabled(host, true),
        "off" => set_enabled(host, false),
        "dump" => dump(host),
        other => host.push_message(
            &format!("Usage: /debug provider [on|off|dump] (got `{other}`)"),
            UiNotifyLevel::Warning,
        ),
    }
}

/// 设置捕获开关并清空缓冲；关闭时同时丢弃最近一轮载荷，并在 App 里提示当前状态。
fn set_enabled(host: &mut dyn CommandHost, value: bool) {
    ENABLED.store(value, Ordering::Relaxed);
    reset_capture();

    if !value {
        *LAST.lock().unwrap() = None;
    }

    host.push_message(
        &format!(
            "Provider event capture {}",
            if value { "enabled" } else { "disabled" }
        ),
        UiNotifyLevel::Info,
    );
}

/// 把最近一轮已完成捕获的 JSON 格式化打印到消息区；没有捕获时提示先开启。
fn dump(host: &mut dyn CommandHost) {
    let Some(payload) = LAST.lock().unwrap().clone() else {
        host.push_message(
            "No captured provider events. Run /debug provider on first.",
            UiNotifyLevel::Info,
        );
        return;
    };

    let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string());
    host.push_message(&text, UiNotifyLevel::Info);
}

/// assistant 消息结束：把本轮捕获渲染成卡片并落盘（无捕获则什么都不做）
fn flush_assistant(event: &Value) {
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

/// 复位到「未开启、无缓冲、无历史载荷」（测试用）
#[cfg(test)]
pub(super) fn reset_for_test() {
    ENABLED.store(false, Ordering::Relaxed);
    reset_capture();
    *LAST.lock().unwrap() = None;
}

/// 当前缓冲的事件副本（测试用，不清空）
#[cfg(test)]
fn peek_events() -> Vec<Value> {
    EVENTS.lock().unwrap().clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::app::App;
    use crate::{
        core::extensions::{Extension, dispatch_provider_stream_event},
        extensions::debug::Debug,
        test_support::{AUTH_TEST_LOCK, AgentDirGuard},
    };

    /// 串行锁：扩展注册表与 UI 队列都是进程级全局态。
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn capture_is_off_until_enabled() {
        let _g = lock();
        let _ad = AgentDirGuard::temp();
        super::super::tests::register_enabled();

        dispatch_provider_stream_event("openai", "openai-completions", "m", &json!({"n": 1}));
        assert!(peek_events().is_empty(), "捕获开关关闭时不缓冲");

        ENABLED.store(true, Ordering::Relaxed);
        dispatch_provider_stream_event("openai", "openai-completions", "m", &json!({"n": 2}));
        let captured = peek_events();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0]["n"], 2);

        super::super::tests::cleanup();
    }

    #[test]
    fn assistant_message_end_emits_card() {
        let _g = lock();
        let ext = Debug;
        ENABLED.store(true, Ordering::Relaxed);
        reset_capture();
        ext.provider_stream_event(&json!({"data": {"type": "response.created"}}));

        ext.on_agent_event(&json!({
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

        reset_for_test();
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
        command(&mut st, "dump");
        assert!(
            st.system_messages.iter().any(|(_, m)| {
                let text = crate::modes::interactive::app::sys_spans_text(&m.spans);
                text.contains("openai") && text.contains("response.created")
            }),
            "dump 应打印最近一轮已完成的载荷"
        );

        let mut st = App::new();
        reset_for_test();
        command(&mut st, "dump");
        assert!(st.system_messages.iter().any(|(_, m)| {
            crate::modes::interactive::app::sys_spans_text(&m.spans).contains("No captured")
        }));

        reset_for_test();
    }

    #[test]
    fn toggle_reports_state_and_rejects_unknown_action() {
        let _g = lock();
        reset_for_test();

        let mut st = App::new();
        command(&mut st, "");
        assert!(enabled(), "无参切换为开启");
        assert!(st.system_messages.iter().any(|(_, m)| {
            crate::modes::interactive::app::sys_spans_text(&m.spans).contains("enabled")
        }));

        let mut st2 = App::new();
        command(&mut st2, "off");
        assert!(!enabled(), "off 关闭捕获");

        let mut st3 = App::new();
        command(&mut st3, "bogus");
        assert!(st3.system_messages.iter().any(|(_, m)| {
            crate::modes::interactive::app::sys_spans_text(&m.spans)
                .contains("Usage: /debug provider")
        }));

        reset_for_test();
    }
}
