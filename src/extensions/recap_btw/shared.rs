//! `/btw` 与 `/recap` 的共用底座：会话 id 槽与一次性模型请求。
//!
//! 两个扩展各自持有自己的状态与命令，这里只放"两边都要、且与 UI 无关"的部分，
//! 免得分叉出两套语义不一致的请求实现。转录摘录改由宿主提供
//! （`CommandHost::transcript_excerpt`），不再在两处各写一份。

use crate::{
    core::{
        auth, model_resolver,
        provider::{self, ModelConfig},
    },
    utils::async_task,
};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// 关不掉思考的模型上，一次性请求的输出上限下限。
///
/// 这类模型（目录里 `thinkingLevelMap.off = null`）即使不发 `reasoning_effort`，服务端也默认
/// 开着思考，而思考 token 计入输出上限：`recap` 的 160 / `btw` 的 500 会被思考吃光，正文为空，
/// `stop_reason = "length"` 让 [`provider::simple_completion`] 直接报错（“hit the token cap”）。
const REASONING_OUTPUT_FLOOR: u32 = 4096;

/// 取会话 id 槽（进程级单例）。
fn session_slot() -> &'static Mutex<Option<String>> {
    /// 当前会话 id 的进程级单例。
    static S: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// 取会话 id 锁；锁被毒化时取回内部值。
fn lock_session() -> MutexGuard<'static, Option<String>> {
    session_slot().lock().unwrap_or_else(|e| e.into_inner())
}

/// 记住当前会话 id（两个扩展的会话建立 / 切换钩子都会写同一个槽）。
///
/// 它会作为 `session_id` 传给 provider 层，用于会话亲和头与提示缓存路由。
pub(super) fn remember_session(session_id: Option<String>) {
    *lock_session() = session_id;
}

/// 命令侧的会话 id 兜底：从 [`App`] 的当前会话文件路径现取并写回槽。
///
/// 把最近 `max_messages` 条消息拼成带角色前缀的摘录，总长不超过 `max_chars`（按字符边界）。
///
/// 起一次异步一次性请求，结果经 `done` 回调交回调用方。
///
/// 没有 tokio 运行时（同步单测）时直接返回 `Err`，由调用方写进自己的 UI 状态——
/// 不 panic、也不假装请求成功了。
pub(super) fn spawn_one_shot(
    provider: String,
    model_id: String,
    system: &'static str,
    prompt: String,
    max_tokens: u32,
    done: impl FnOnce(Result<String, String>) + Send + 'static,
) -> Result<(), String> {
    let session_id = lock_session().clone();

    let started = async_task::spawn(async move {
        done(
            one_shot(
                &provider, &model_id, session_id, system, &prompt, max_tokens,
            )
            .await,
        );
    });

    if started {
        Ok(())
    } else {
        Err("no async runtime".to_string())
    }
}

/// 一次性请求：刷新 OAuth 凭据 → 解析模型 → 非流式补全。
///
/// 失败时返回可直接展示的错误文案（对齐 `provider::simple_completion` 的错误）。
async fn one_shot(
    provider: &str,
    model_id: &str,
    session_id: Option<String>,
    system: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<String, String> {
    // OAuth 凭据的刷新是异步的（`configured_model` 只做同步解析）
    if let Err(e) = auth::ensure_oauth_valid(provider).await {
        return Err(format!("{e}"));
    }

    let model = model_resolver::configured_model(provider, model_id, None, session_id)
        .map_err(|e| format!("{e}"))?;
    let max_tokens = output_cap(&model, max_tokens);
    let (text, _usage) = provider::simple_completion(&model, system, prompt, Some(max_tokens))
        .await
        .map_err(|e| format!("{e}"))?;
    Ok(text)
}

/// 一次性请求的实际输出上限：关不掉思考的模型抬到 [`REASONING_OUTPUT_FLOOR`]（仍受模型自身
/// `max_tokens` 约束），其它模型沿用调用方给的上限。
fn output_cap(model: &ModelConfig, requested: u32) -> u32 {
    if !model.reasoning || thinking_can_be_disabled(model) {
        return requested;
    }
    let ceiling = model
        .max_tokens
        .filter(|m| *m > 0)
        .unwrap_or(u32::MAX)
        .min(REASONING_OUTPUT_FLOOR);
    requested.max(ceiling)
}

/// 模型能否显式关闭思考：`thinkingLevelMap.off == null` 表示“不支持 off”（与 provider
/// 各协议里 `can_disable` / `should_disable` 的判定一致）。
fn thinking_can_be_disabled(model: &ModelConfig) -> bool {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|m| m.get("off"))
        .is_none_or(|v| v.is_some())
}

/// 当前槽里的会话 id（仅测试用来断言回填结果）。
#[cfg(test)]
pub(super) fn remembered_session() -> Option<String> {
    lock_session().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_without_runtime_reports_error_instead_of_panicking() {
        let result = spawn_one_shot(
            "anthropic".to_string(),
            "claude-fable-5".to_string(),
            "system",
            "prompt".to_string(),
            160,
            |_| panic!("没有运行时不该调用回调"),
        );
        assert!(result.is_err(), "无 tokio 运行时应返回 Err");
    }

    /// 构造一个最小 `ModelConfig`（只填反序列化必需的字段）。
    /// `off`：`None` = 没有 thinkingLevelMap；`Some(None)` = `off: null`（关不掉）；
    /// `Some(Some(v))` = 有关闭值。
    fn model(reasoning: bool, off: Option<Option<&str>>, max_tokens: Option<u32>) -> ModelConfig {
        let mut v = serde_json::json!({
            "provider": "test",
            "model_id": "m",
            "base_url": "https://example.com/v1",
            "api_key": "",
            "input": ["text"],
            "reasoning": reasoning,
            "max_tokens": max_tokens,
            "temperature": null,
            "context_window": 100000,
            "thinking_format": "deepseek",
            "supports_developer_role": false,
            "requires_reasoning_content_on_assistant_messages": false,
            "max_tokens_field": "max_tokens",
            "cost": null,
            "supports_usage_in_streaming": true,
            "supports_store": false,
            "supports_finish_reason": true,
            "requires_assistant_after_tool_result": false,
            "supports_strict_mode": false,
        });
        if let Some(off) = off {
            v["thinking_level_map"] = serde_json::json!({ "off": off });
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn output_cap_raises_only_when_thinking_cannot_be_disabled() {
        // 非推理模型：原样
        assert_eq!(output_cap(&model(false, None, None), 160), 160);
        // 推理模型但能关掉思考（off 有值 / 没有 map）：原样
        assert_eq!(output_cap(&model(true, Some(Some("none")), None), 160), 160);
        assert_eq!(output_cap(&model(true, None, None), 160), 160);
        // 关不掉思考：抬到下限，思考才有 token 可用
        assert_eq!(output_cap(&model(true, Some(None), None), 160), 4096);
        // 下限仍受模型自身 max_tokens 约束
        assert_eq!(output_cap(&model(true, Some(None), Some(2048)), 160), 2048);
        // 调用方给的上限本就更大时不缩水
        assert_eq!(output_cap(&model(true, Some(None), None), 8192), 8192);
        assert_eq!(output_cap(&model(true, Some(None), Some(1000)), 8192), 8192);
    }
}
