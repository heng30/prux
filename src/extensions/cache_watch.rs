//! `cache-watch` 扩展：提示 prompt cache 的两种异常。
//!
//! | 情形 | 触发时机 | 判定 |
//! |---|---|---|
//! | 缓存已冷 | 用户提交新一轮输入时 | 距上次写入缓存的请求已超过目录 TTL |
//! | 意外未命中 | 每次 LLM 响应结束（`turn_end`） | 本次没有命中任何缓存却重新写入，且上次写入仍在 TTL 内 |
//!
//! TTL 取目录 `promptCache.{short,long}`（按 [`read_cache_retention`] 选档），
//! 与 [`crate::core::cache_warmer`] 的保温判据同源；开启保温时缓存通常不会冷，
//! 因此冷警告主要出现在保温关闭 / 空闲超过 TTL 之后。
//!
//! 提示经 [`ExtensionUiRequest::Notify`] 发到聊天区（系统消息，不落库、不进模型上下文）。
//! 「意外未命中」是**近似判定**：扩展只能看到用量，看不到前缀到底哪里变了，
//! 因此用「本轮重新写入 + 上轮 TTL 内写过」这个保守信号，仍可能误报（压缩、改工具集等）。
//!
//! 默认**不启用**（`Dev` / `Creator`）。

use crate::{
    core::{
        cache_warmer::{CacheRetention, read_cache_retention},
        extensions::{
            Extension, ExtensionHook, ExtensionMode, ExtensionTool, ExtensionUiRequest,
            ForkProjectInfo, UiNotifyLevel, request_ui,
        },
        model_resolver,
        provider::AgentMessage,
        session_manager::Session,
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_CACHE_WATCH},
    utils::time::{format_duration, now_ms},
};
use serde_json::Value;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// 扩展名（也是 `/extension` 面板中的标识）。
const EXT: &str = "cache-watch";

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static CACHE_WATCH_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_CACHE_WATCH,
    make: || -> Arc<dyn Extension> { Arc::new(CacheWatch) },
};

/// 一次「向 prompt cache 写入」的请求快照。
#[derive(Debug, Clone)]
struct CacheStamp {
    /// 产生写入的 provider（用于回查目录 TTL）。
    provider: String,
    /// 产生写入的模型 id（用于回查目录 TTL）。
    model: String,
    /// 该次请求完成的时刻（[`now_ms`] 的墙钟毫秒，只用于算经过时间）。
    at: u64,
    /// 写入缓存的 token 数（> 0 才说明缓存里确实有内容）。
    written: u64,
}

/// 一次 assistant 用量里与缓存相关的读数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UsageView {
    /// 未命中缓存的输入 token。
    input: u64,
    /// 命中缓存的输入 token。
    read: u64,
    /// 写入缓存的 token。
    write: u64,
}

/// 扩展状态（进程级单例）。
#[derive(Default)]
struct State {
    /// 最近一次写入缓存的请求；None = 本会话还没写过（或已复位）。
    last_write: Option<CacheStamp>,
    /// 已就哪一次写入发过冷警告（比对写入时刻，避免同一份缓存重复刷屏）。
    cold_warned_at: Option<u64>,
    /// 已就哪一次写入发过意外未命中警告。
    miss_warned_at: Option<u64>,
}

/// 取扩展状态的进程级单例（惰性初始化）。
fn state() -> &'static Mutex<State> {
    /// 扩展状态的进程级单例。
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// 取状态锁；锁被毒化时取回内部值。
fn lock() -> MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// 清空状态：换会话或禁用后，上一会话的缓存时间线不再适用。
fn reset() {
    *lock() = State::default();
}

/// `cache-watch` 扩展主体。
pub struct CacheWatch;

impl Extension for CacheWatch {
    /// 返回固定扩展名 `cache-watch`。
    fn name(&self) -> &str {
        EXT
    }

    /// 返回 `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Warn when the prompt cache goes cold or misses unexpectedly (cache-watch)."
    }

    /// 声明本扩展移植自 jcode 0.94.0，供 `/extension` 面板展示来源链接。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "jcode".to_string(),
            plugin_version: "0.94.0".to_string(),
            url: "https://github.com/1jehuang/jcode".to_string(),
        })
    }

    /// 默认关闭：会主动往聊天区推提示，由用户在 `/extension` 面板开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 只在 `Dev` / `Creator` 扩展模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 不提供模型工具（只做提示）。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 订阅 agent 事件（看用量）与用户提交（判冷）。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![ExtensionHook::AgentEvent, ExtensionHook::UserPrompt]
    }

    /// 每个 LLM 响应结束时看一次缓存用量。
    fn on_agent_event(&self, event: &Value) {
        if event.get("type").and_then(Value::as_str) == Some("turn_end") {
            handle_turn_end(event);
        }
    }

    /// 用户提交新一轮输入（非 steer）时判一次缓存是否已冷。
    fn on_user_submit(&self, _text: &str, busy: bool) {
        if !busy {
            warn_if_cold();
        }
    }

    /// 新会话：清空时间线。
    fn on_session_start(&self, _cwd: &str, _messages: &[AgentMessage], _session: Option<&Session>) {
        reset();
    }

    /// 换会话：清空时间线。
    fn on_session_switched(&self, _session_path: Option<&str>, _messages: &[AgentMessage]) {
        reset();
    }

    /// 禁用时清空状态：禁用后框架不再投递事件。
    fn on_enabled_changed(&self, enabled: bool) {
        if !enabled {
            reset();
        }
    }
}

/// 处理一次 `turn_end`：先判意外未命中（要用上一轮的信息），再更新本轮时间线。
fn handle_turn_end(event: &Value) {
    let Some((view, provider, model)) = usage_view(event) else {
        return;
    };

    if provider.is_empty() || model.is_empty() {
        return;
    }

    let now = now_ms();
    let mut st = lock();

    if let Some(ttl) = cache_ttl_ms(&provider, &model)
        && let Some(last) = st.last_write.clone()
        && last.model == model
        && is_unexpected_miss(&view, last.written, elapsed_ms(last.at, now), ttl)
        && st.miss_warned_at != Some(last.at)
    {
        let text = format!(
            "Unexpected prompt cache miss: the previous request wrote {} tokens of cache {} ago (TTL {}) but this one read none back — the system prompt, tool set, or message prefix likely changed.",
            last.written,
            format_duration(elapsed_ms(last.at, now)),
            format_duration(ttl)
        );
        st.miss_warned_at = Some(last.at);
        drop(st);
        notify(text, UiNotifyLevel::Warning);
        return;
    }

    if view.write > 0 {
        st.last_write = Some(CacheStamp {
            provider,
            model,
            at: now,
            written: view.write,
        });
        st.cold_warned_at = None;
        st.miss_warned_at = None;
    } else if view.read > 0 {
        // 命中说明缓存仍活着：清掉上一份缓存的告警记录
        st.cold_warned_at = None;
    }
}

/// 用户提交新一轮输入时，若上次写入的缓存已超 TTL 就提示一次。
fn warn_if_cold() {
    let now = now_ms();
    let mut st = lock();
    let Some(last) = st.last_write.clone() else {
        return;
    };
    let Some(ttl) = cache_ttl_ms(&last.provider, &last.model) else {
        return;
    };
    if !is_cold(elapsed_ms(last.at, now), ttl) || st.cold_warned_at == Some(last.at) {
        return;
    }

    let text = format!(
        "Prompt cache went cold: the last cache write was {} ago (TTL {}), so the next request re-reads the whole context at full input price.",
        format_duration(elapsed_ms(last.at, now)),
        format_duration(ttl)
    );
    st.cold_warned_at = Some(last.at);
    drop(st);
    notify(text, UiNotifyLevel::Warning);
}

/// 把提示发到聊天区（系统消息，不落库、不进模型上下文）。
fn notify(text: String, level: UiNotifyLevel) {
    request_ui(ExtensionUiRequest::Notify { text, level });
}

/// 从 `turn_end` 事件里取用量读数与 provider/model；缺 `message` / `usage` 时返回 `None`。
fn usage_view(event: &Value) -> Option<(UsageView, String, String)> {
    let message = event.get("message")?;
    let usage = message.get("usage")?;

    let view = UsageView {
        input: usage.get("input").and_then(Value::as_u64).unwrap_or(0),
        read: usage.get("cache_read").and_then(Value::as_u64).unwrap_or(0),
        write: usage
            .get("cache_write")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    };
    let provider = message
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    Some((view, provider, model))
}

/// 目录里当前保留档位下该模型的 prompt cache TTL（毫秒）。
///
/// 关闭缓存、provider/model 不在目录里、或该档位没给 TTL 时返回 `None`
/// （判定口径与 [`crate::core::cache_warmer::prompt_cache_ttl`] 一致）。
fn cache_ttl_ms(provider: &str, model: &str) -> Option<u64> {
    let tier = match read_cache_retention() {
        CacheRetention::None => return None,
        CacheRetention::Short => "short",
        CacheRetention::Long => "long",
    };

    model_resolver::provider_models(provider)
        .iter()
        .find(|m| m.get("id").and_then(Value::as_str) == Some(model))
        .and_then(|m| m.get("promptCache"))
        .and_then(|p| p.get(tier))
        .and_then(Value::as_u64)
        .filter(|s| *s > 0)
        .map(|s| s.saturating_mul(1000))
}

/// 距写入时刻经过的毫秒数。
fn elapsed_ms(at: u64, now: u64) -> u64 {
    now.saturating_sub(at)
}

/// 距上次写入是否已超过 TTL（恰好等于 TTL 不算冷）。
fn is_cold(elapsed_ms: u64, ttl_ms: u64) -> bool {
    elapsed_ms > ttl_ms
}

/// 意外未命中：本次没有读回任何缓存却重新写入，且上一轮写入仍在 TTL 内。
///
/// 用「重新写入」而不是「什么都没读」作信号：只请求了短前缀、或模型本来就该重新写缓存时，
/// `read == 0` 但 `write == 0`，那种情况不算异常。
fn is_unexpected_miss(view: &UsageView, last_written: u64, elapsed_ms: u64, ttl_ms: u64) -> bool {
    view.read == 0 && view.write > 0 && last_written > 0 && !is_cold(elapsed_ms, ttl_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::extensions::take_pending_ui,
        test_support::{AUTH_TEST_LOCK, AgentDirGuard},
    };
    use serde_json::json;

    /// 串行锁 + 临时 agent_dir：扩展状态与 UI 队列都是进程级全局态。
    fn guard() -> (MutexGuard<'static, ()>, AgentDirGuard) {
        let lock = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        (lock, AgentDirGuard::temp())
    }

    /// 清空待处理的 UI 请求队列（避免用例间互相 `take_pending_ui`）。
    fn drain_ui() {
        while take_pending_ui().is_some() {}
    }

    /// 一份 `turn_end` 事件。
    fn turn_end(provider: &str, model: &str, read: u64, write: u64) -> Value {
        json!({
            "type": "turn_end",
            "message": {
                "role": "assistant",
                "provider": provider,
                "model": model,
                "usage": { "input": 10, "output": 5, "cache_read": read, "cache_write": write }
            }
        })
    }

    #[test]
    fn cold_boundary_is_strictly_greater_than_ttl() {
        assert!(!is_cold(300_000, 300_000), "恰好到 TTL 还不算冷");
        assert!(is_cold(300_001, 300_000));
    }

    #[test]
    fn unexpected_miss_requires_a_rewrite_within_ttl() {
        let rewrite = UsageView {
            input: 10,
            read: 0,
            write: 100,
        };
        assert!(is_unexpected_miss(&rewrite, 100, 1_000, 300_000));

        let hit = UsageView {
            input: 10,
            read: 100,
            write: 0,
        };
        assert!(!is_unexpected_miss(&hit, 100, 1_000, 300_000), "命中不算");

        let no_write = UsageView {
            input: 10,
            read: 0,
            write: 0,
        };
        assert!(
            !is_unexpected_miss(&no_write, 100, 1_000, 300_000),
            "既没读也没写不是异常"
        );
        assert!(
            !is_unexpected_miss(&rewrite, 0, 1_000, 300_000),
            "上一轮没写过缓存就没有「本应命中」"
        );
        assert!(
            !is_unexpected_miss(&rewrite, 100, 400_000, 300_000),
            "超 TTL 属于冷，不算意外未命中"
        );
    }

    #[test]
    fn duration_switches_to_minutes() {
        assert_eq!(format_duration(0), "0ms");
        assert_eq!(format_duration(59_999), "60s");
        assert_eq!(format_duration(60_000), "1m0s");
        assert_eq!(format_duration(300_000), "5m0s");
    }

    #[test]
    fn ttl_lookup_is_none_without_catalog_entry() {
        assert_eq!(cache_ttl_ms("no-such-provider", "no-such-model"), None);
    }

    #[test]
    fn usage_view_reads_snake_case_fields() {
        let (view, provider, model) = usage_view(&turn_end("anthropic", "claude-x", 7, 9)).unwrap();
        assert_eq!(
            view,
            UsageView {
                input: 10,
                read: 7,
                write: 9
            }
        );
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude-x");

        assert!(usage_view(&json!({ "type": "turn_end" })).is_none());
    }

    #[test]
    fn turn_end_records_cache_writes() {
        let (_g, _ad) = guard();
        reset();
        drain_ui();

        handle_turn_end(&turn_end("anthropic", "claude-x", 0, 1234));
        let st = lock();
        let last = st.last_write.as_ref().expect("写入应被记录");
        assert_eq!(last.written, 1234);
        assert_eq!(last.provider, "anthropic");
        drop(st);
        assert!(take_pending_ui().is_none(), "首次写入不该有提示");
    }

    #[test]
    fn session_switch_resets_the_timeline() {
        let (_g, _ad) = guard();
        drain_ui();
        handle_turn_end(&turn_end("anthropic", "claude-x", 0, 10));
        assert!(lock().last_write.is_some());

        CacheWatch.on_session_switched(None, &[]);
        assert!(lock().last_write.is_none());

        handle_turn_end(&turn_end("anthropic", "claude-x", 0, 10));
        CacheWatch.on_enabled_changed(false);
        assert!(lock().last_write.is_none());
    }

    #[test]
    fn cold_warning_fires_once_per_write() {
        let (_g, _ad) = guard();
        reset();
        drain_ui();

        // 目录里没有该模型 → 没有 TTL，判不了冷：不该提示
        let mut st = lock();
        st.last_write = Some(CacheStamp {
            provider: "no-such-provider".to_string(),
            model: "no-such-model".to_string(),
            at: now_ms(),
            written: 500,
        });
        drop(st);
        warn_if_cold();
        assert!(take_pending_ui().is_none(), "拿不到 TTL 时不该提示");
    }

    #[test]
    fn cold_warning_fires_and_is_not_repeated() {
        let (_g, _ad) = guard();
        reset();
        drain_ui();

        // 目录里 anthropic/claude-sonnet-4-5 的 short 档 TTL 是 300s：伪造 400s 前的写入
        let model = "claude-sonnet-4-5";
        assert!(
            cache_ttl_ms("anthropic", model) == Some(300_000),
            "目录应给出 300s 的 short TTL"
        );
        lock().last_write = Some(CacheStamp {
            provider: "anthropic".to_string(),
            model: model.to_string(),
            at: now_ms().saturating_sub(400_000),
            written: 500,
        });

        warn_if_cold();
        match take_pending_ui().expect("超 TTL 应提示缓存已冷") {
            ExtensionUiRequest::Notify { text, level } => {
                assert!(text.contains("went cold"), "{text}");
                assert_eq!(level, UiNotifyLevel::Warning);
            }
            other => panic!("期望 Notify，得到 {other:?}"),
        }

        warn_if_cold();
        assert!(take_pending_ui().is_none(), "同一份缓存只提示一次");

        // TTL 内写入：不提示，且冷告警记录被清掉
        handle_turn_end(&turn_end("anthropic", model, 0, 42));
        warn_if_cold();
        assert!(take_pending_ui().is_none(), "刚写过缓存不该提示冷");
        assert!(lock().cold_warned_at.is_none());
    }

    #[test]
    fn unexpected_miss_is_reported_once() {
        let (_g, _ad) = guard();
        reset();
        drain_ui();

        let model = "claude-sonnet-4-5";
        lock().last_write = Some(CacheStamp {
            provider: "anthropic".to_string(),
            model: model.to_string(),
            at: now_ms().saturating_sub(1_000),
            written: 900,
        });

        // TTL 内重新写入且没读回缓存 → 意外未命中
        handle_turn_end(&turn_end("anthropic", model, 0, 800));
        match take_pending_ui().expect("应提示意外未命中") {
            ExtensionUiRequest::Notify { text, level } => {
                assert!(text.contains("Unexpected prompt cache miss"), "{text}");
                assert_eq!(level, UiNotifyLevel::Warning);
            }
            other => panic!("期望 Notify，得到 {other:?}"),
        }

        // 命中缓存：既不该提示，也不该再记录
        handle_turn_end(&turn_end("anthropic", model, 900, 0));
        assert!(take_pending_ui().is_none(), "命中不该提示");
    }

    #[test]
    fn fork_project_reports_jcode_source() {
        let info = CacheWatch
            .fork_project()
            .expect("cache-watch 应声明 fork 来源");
        assert_eq!(info.plugin_name, "jcode");
        assert_eq!(info.plugin_version, "0.94.0");
        assert_eq!(info.url, "https://github.com/1jehuang/jcode");
    }

    #[test]
    fn extension_declares_hooks_and_stays_optional() {
        assert_eq!(CacheWatch.name(), EXT);
        assert!(!CacheWatch.default_enabled());
        assert_eq!(
            CacheWatch.modes(),
            vec![ExtensionMode::Dev, ExtensionMode::Creator]
        );
        assert_eq!(
            CacheWatch.hooks(),
            vec![ExtensionHook::AgentEvent, ExtensionHook::UserPrompt]
        );
        assert!(CacheWatch.tools().is_empty());
    }
}
