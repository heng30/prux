//! `/btw` 与 `/recap` 的共用底座：会话 id 槽、转录摘录、一次性模型请求。
//!
//! 两个扩展各自持有自己的状态与命令，这里只放"两边都要、且与 UI 无关"的部分，
//! 免得分叉出两套语义不一致的摘录/请求实现。

use crate::{
    core::{
        auth, model_resolver,
        provider::{self, AgentMessage, ModelConfig},
    },
    extensions::util::{session_id_of, truncate_chars},
    modes::interactive::app::App,
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
/// 默认不启用的扩展（`btw` / `recap`）常常是**本会话中途**才被用户在 `/extension`
/// 面板打开的，此时会话钩子从没为该扩展跑过，槽里没有 id；opencode 系 provider 会因
/// 缺 `x-opencode-session` 直接回 400。命令执行时会话文件路径一定就绪，补写一次即可。
/// 新建会话尚未落盘（路径为 `None`）时不动槽，避免把好 id 抹掉。
pub(super) fn refresh_session_from_app(st: &App) {
    // 新建会话尚未落盘（路径为 `None`）或文件名推不出键时不动槽，避免把好 id 抹掉。
    if let Some(id) = session_id_of(st.current_session_path.as_deref()) {
        remember_session(Some(id));
    }
}

/// 当前 `(provider, model)`：取 TUI 镜像（`/model` 切过就是最新的）。
pub(super) fn current_model(st: &App) -> Option<(String, String)> {
    match (&st.current_provider, &st.current_model) {
        (Some(provider), Some(model_id)) => Some((provider.clone(), model_id.clone())),
        _ => None,
    }
}

/// 把最近 `max_messages` 条消息拼成带角色前缀的摘录，总长不超过 `max_chars`（按字符边界）。
///
/// 空文本的消息（只有工具调用的 assistant 轮）会被跳过，避免摘录里出现一堆空行。
pub(super) fn build_excerpt(
    messages: &[AgentMessage],
    max_messages: usize,
    max_chars: usize,
) -> String {
    let mut out = String::new();
    let start = messages.len().saturating_sub(max_messages);
    for message in &messages[start..] {
        let text = message.text();
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(role_label(&message.role));
        out.push_str(": ");
        out.push_str(text);
    }
    truncate_chars(&out, max_chars)
}

/// 消息角色 → 摘录里的前缀。
fn role_label(role: &str) -> &str {
    match role {
        "assistant" => "assistant",
        "toolResult" => "tool",
        "system" => "system",
        _ => "user",
    }
}

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
    let handle =
        tokio::runtime::Handle::try_current().map_err(|_| "no async runtime".to_string())?;
    let session_id = lock_session().clone();

    handle.spawn(async move {
        done(
            one_shot(
                &provider, &model_id, session_id, system, &prompt, max_tokens,
            )
            .await,
        );
    });

    Ok(())
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

    /// 一条指定角色的消息。
    fn message(role: &str, text: &str) -> AgentMessage {
        let mut message = AgentMessage::user_text(text);
        message.role = role.to_string();
        message
    }

    #[test]
    fn excerpt_keeps_recent_messages_and_caps_chars() {
        let messages = vec![
            message("user", "old"),
            message("assistant", "a1"),
            message("user", "u2"),
            message("assistant", "a3"),
        ];
        assert_eq!(
            build_excerpt(&messages, 2, 1000),
            "user: u2\nassistant: a3",
            "只取最近两条"
        );
        let long = build_excerpt(&messages, 4, 12);
        assert!(
            long.chars().count() <= 13,
            "总长受字符上限约束（省略号额外占 1 个字符）"
        );
        assert!(long.starts_with("user: old"), "超长时从头截断");
        let tool = build_excerpt(&[message("toolResult", "out")], 1, 100);
        assert_eq!(tool, "tool: out", "工具结果用 tool 前缀");
        assert_eq!(build_excerpt(&[], 4, 100), "");
        // 只有工具调用的 assistant 轮（正文为空）不该在摘录里留空行
        assert_eq!(
            build_excerpt(
                &[message("assistant", "   "), message("user", "hi")],
                4,
                100
            ),
            "user: hi"
        );
    }

    #[test]
    fn current_model_reads_tui_mirror() {
        let mut st = App::new();
        st.current_provider = Some("openai".to_string());
        st.current_model = Some("gpt-4".to_string());
        assert_eq!(
            current_model(&st),
            Some(("openai".to_string(), "gpt-4".to_string()))
        );

        st.current_provider = None;
        st.current_model = None;
        assert_eq!(
            current_model(&st),
            None,
            "TUI 还没定型号时没有可用的 provider/model"
        );
    }

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
