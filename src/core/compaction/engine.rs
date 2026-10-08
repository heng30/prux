//! 压缩主逻辑：阈值判断与上下文估算

use super::utils::estimate_tokens;
use crate::core::{
    provider::{AgentMessage, Usage},
    settings_manager,
};
use strum_macros::IntoStaticStr;

/// 默认保留tokens，避免压缩时没有tokens
pub const DEFAULT_RESERVE_TOKENS: u32 = 16384;

/// 压缩时，保留多少tokens对应的文本不进入压缩。
/// 因为最新的信息关联性越强，不进行压缩能够更好的保留上下信息
pub const DEFAULT_KEEP_RECENT_TOKENS: u32 = 20000;

/// 压缩触发原因
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum CompactionReason {
    /// 手动触发（/compact、溢出恢复前的强制压缩）
    Manual,
    /// 阈值触发（上下文接近窗口上限）
    Threshold,
    /// 溢出后自动重试压缩（带 willRetry）
    Overflow,
}

impl CompactionReason {
    /// 返回该原因的稳定小写字符串（`manual` / `threshold` / `overflow`），供日志与持久化使用。
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

/// 自动压缩的开关与额度：是否启用、预留 tokens、保留最近 tokens。
#[derive(Debug, Clone)]
pub struct CompactionSettings {
    /// 是否启用自动压缩；false 时任何阈值都不会触发
    pub enabled: bool,
    /// 为模型回复预留的 token 额度：上下文超过「窗口 − 它」才判定需压缩
    pub reserve_tokens: u32,
    /// 压缩切点预算：从最新消息往回累计到此 token 数即作为切点，之后的内容保留
    pub keep_recent_tokens: u32,
}

impl Default for CompactionSettings {
    /// 内置默认：启用自动压缩，预留/保留 token 取常量默认值。
    fn default() -> Self {
        CompactionSettings {
            enabled: true,
            reserve_tokens: DEFAULT_RESERVE_TOKENS,
            keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
        }
    }
}

impl CompactionSettings {
    /// 从 settings.json 读取可配置的压缩参数（reserve/keepRecent/enabled；缺失回落默认值）
    pub fn from_settings() -> Self {
        CompactionSettings {
            enabled: settings_manager::read_settings_auto_compact(),
            reserve_tokens: settings_manager::read_settings_compact_reserve(),
            keep_recent_tokens: settings_manager::read_settings_compact_keep_recent(),
        }
    }

    /// 应用按模型覆盖（`compaction.modelOverrides`）；未配置时保持全局值。
    /// modelOverrides → 全局 compaction 设置 → 内置默认。
    pub fn for_model(mut self, provider: &str, model_id: &str) -> Self {
        self.reserve_tokens =
            settings_manager::read_settings_compact_reserve_for(provider, model_id);
        self.keep_recent_tokens =
            settings_manager::read_settings_compact_keep_recent_for(provider, model_id);
        self
    }
}

/// 从 usage 计算上下文 token（优先 total_tokens，否则分项求和）
pub fn calculate_context_tokens(usage: &Usage) -> u32 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

/// 该消息携带的、可作上下文 usage 锚点的 usage。
///
/// 条件：assistant 且非 error/aborted（这两者 usage 不完整）且 token > 0。
/// 压缩后重算上下文占用、判定「新 usage 已落地」都以此为准。
pub fn usable_usage(msg: &AgentMessage) -> Option<&Usage> {
    if msg.role != "assistant" {
        return None;
    }

    if msg.stop_reason.as_deref() == Some("aborted") || msg.stop_reason.as_deref() == Some("error")
    {
        return None;
    }

    let usage = msg.usage.as_ref()?;
    (calculate_context_tokens(usage) > 0).then_some(usage)
}

/// 最近一条 usage 仍能描述当前上下文的 assistant（附下标）。
///
/// 若某条消息的时间戳晚于该 assistant（典型：压缩后插入的 compactionSummary），
/// 说明它是在该回复『之后』插入的前缀消息，该 usage 统计的是插入前的旧上下文，
/// 不能再用作当前上下文的锚点；此时跳过它继续向后找（压缩后的新回复 timestamp 更新，
/// 会重新成为锚点）。全部锚点失效则返回 None，走纯 token 估算。
fn get_last_assistant_usage_info(messages: &[AgentMessage]) -> Option<(Usage, usize)> {
    let mut latest_prefix_timestamp: u64 = 0;
    let mut anchor: Option<(Usage, usize)> = None;

    // NOTE: 可能很消耗性能
    for (i, msg) in messages.iter().enumerate() {
        let usage_applies_to_prefix = msg.timestamp >= latest_prefix_timestamp;
        if msg.role == "assistant"
            && usage_applies_to_prefix
            && let Some(usage) = usable_usage(msg)
        {
            anchor = Some((usage.clone(), i));
        }
        latest_prefix_timestamp = latest_prefix_timestamp.max(msg.timestamp);
    }

    anchor
}

/// 上下文tokens使用估计
pub struct ContextUsageEstimate {
    pub tokens: u32,                     // 总共花费的tokens
    pub usage_tokens: u32,               // 截止最后一条assistant花费的tokens
    pub trailing_tokens: u32,            // 最后一条assistant消息后的文本，估算使用的tokens
    pub last_usage_index: Option<usize>, // 最后一条assistant下标
}

/// 估算当前上下文 token：以最近一条 assistant usage 为锚点 + 其后消息估算
pub fn estimate_context_tokens(messages: &[AgentMessage]) -> ContextUsageEstimate {
    if let Some((usage, index)) = get_last_assistant_usage_info(messages) {
        let usage_tokens = calculate_context_tokens(&usage);
        let mut trailing_tokens = 0;

        for msg in &messages[index + 1..] {
            trailing_tokens += estimate_tokens(msg);
        }

        ContextUsageEstimate {
            tokens: usage_tokens + trailing_tokens,
            usage_tokens,
            trailing_tokens,
            last_usage_index: Some(index),
        }
    } else {
        let mut estimated = 0;
        for msg in messages {
            estimated += estimate_tokens(msg);
        }
        ContextUsageEstimate {
            tokens: estimated,
            usage_tokens: 0,
            trailing_tokens: estimated,
            last_usage_index: None,
        }
    }
}

/// 是否触发压缩：contextTokens > contextWindow - reserveTokens
pub fn should_compact(
    context_tokens: u32,
    context_window: u32,
    settings: &CompactionSettings,
) -> bool {
    if !settings.enabled {
        return false;
    }
    context_tokens > context_window.saturating_sub(settings.reserve_tokens)
}

/// 找压缩切点：从最新往回累积 token，达到 `keep_recent_tokens` 时切在该消息。
/// 返回切点下标；无可压缩内容时返回 `summarize_start`。
///
/// **tool result 的 token 必须计入保留预算**，
/// 否则超大的尾随工具结果会被静默忽略、预算永远达不到阈值，
/// 导致运行中阈值压缩直接跳过（旧历史无法压缩）。
/// tool result 计入预算但不能作为切点（必须跟随其 tool call）。
pub fn find_compaction_cut_index(
    messages: &[AgentMessage],
    summarize_start: usize,
    keep_recent_tokens: u32,
) -> usize {
    let mut accumulated = 0u32;
    let mut cut_index = summarize_start;
    let mut i = messages.len();
    while i > summarize_start {
        i -= 1;
        let msg = &messages[i];
        let tokens = estimate_tokens(msg);
        if tokens > 0 {
            accumulated = accumulated.saturating_add(tokens);
        }
        if msg.role == "toolResult" || tokens == 0 {
            continue;
        }
        if accumulated >= keep_recent_tokens {
            cut_index = i;
            break;
        }
    }

    // 切点落在 assistant 回复内部时回溯到该 turn 的起点，保留完整 turn，避免上下文以半截回复开头。
    if cut_index < messages.len() {
        let at_cut = &messages[cut_index];
        if at_cut.role == "assistant" || at_cut.role == "toolResult" {
            for i in (summarize_start..cut_index).rev() {
                if messages[i].role == "user" {
                    cut_index = i;
                    break;
                }
            }
        }
    }
    cut_index
}

/// 陈旧 usage 守卫（assistantMessage.timestamp <= compactionEntry.timestamp 不触发）：
/// estimate_context_tokens 的 usage 锚点（最后一条带 usage 的 assistant）若早于或等于最近
/// compaction boundary，说明该 usage 是压缩前的陈旧数据；压缩后首轮按它判断会重复触发压缩，
/// 因此跳过。无 usage 锚点（纯估算）或最近无 compaction 时不拦截。
pub fn usage_anchor_is_stale(
    estimate: &ContextUsageEstimate,
    messages: &[AgentMessage],
    latest_compaction_timestamp: Option<i64>,
) -> bool {
    let Some(anchor_idx) = estimate.last_usage_index else {
        return false;
    };
    let Some(anchor_ts) = messages.get(anchor_idx).map(|m| m.timestamp as i64) else {
        return false;
    };
    latest_compaction_timestamp.is_some_and(|c| anchor_ts <= c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::Usage;

    fn assistant_with_usage(timestamp: u64, total: u32) -> AgentMessage {
        let mut m = AgentMessage::user_text("x");
        m.role = "assistant".into();
        m.timestamp = timestamp;
        m.usage = Some(Usage {
            total_tokens: total,
            ..Usage::default()
        });
        m
    }

    fn estimate_of(messages: &[AgentMessage]) -> ContextUsageEstimate {
        estimate_context_tokens(messages)
    }

    fn msg(role: &str, text: &str) -> AgentMessage {
        let mut m = AgentMessage::user_text(text);
        m.role = role.into();
        m
    }

    fn assistant_tool_call(name: &str) -> AgentMessage {
        let mut m = AgentMessage::user_text("");
        m.role = "assistant".into();
        m.content = vec![crate::core::provider::ContentBlock::ToolCall {
            id: "call-1".into(),
            name: name.into(),
            arguments: serde_json::json!({ "path": "big.txt" }),
            thought_signature: None,
            namespace: None,
        }];
        m
    }

    fn tool_result(text: &str) -> AgentMessage {
        let mut m = AgentMessage::user_text(text);
        m.role = "toolResult".into();
        m.content = vec![crate::core::provider::ContentBlock::Text {
            text: text.into(),
            text_signature: None,
        }];
        m
    }

    #[test]
    fn oversized_trailing_tool_result_still_yields_cut_point() {
        // 回归 #9740：尾随工具结果本身超出保留预算时，仍应回退到本轮 user 起点，
        // 让更早的历史得以压缩（而非静默跳过压缩）。
        let msgs = vec![
            msg("user", "old history"),
            msg("assistant", "old answer"),
            msg("user", "read the large file"),
            assistant_tool_call("read"),
            tool_result(&"x".repeat(8000)),
        ];
        // 非工具消息合计远小于预算；只有工具结果能触发预算
        let cut = find_compaction_cut_index(&msgs, 0, 100);
        assert_eq!(cut, 2, "应回溯到本轮 user（保留整轮），旧历史进入压缩");
    }

    #[test]
    fn cut_point_is_never_a_tool_result() {
        // 单轮时无更早历史可压缩 → 回到起点
        let msgs = vec![
            msg("user", "q"),
            assistant_tool_call("read"),
            tool_result(&"y".repeat(8000)),
        ];
        assert_eq!(find_compaction_cut_index(&msgs, 0, 100), 0);
    }

    #[test]
    fn cut_point_respects_summarize_start_boundary() {
        // 已有 compactionSummary 时，切点不得早于 summarize_start
        let msgs = vec![
            msg("compactionSummary", "old summary"),
            msg("user", "q1"),
            msg("assistant", "a1"),
            msg("user", "q2"),
            msg("assistant", &"z".repeat(4000)),
        ];
        let cut = find_compaction_cut_index(&msgs, 1, 10);
        assert!(cut >= 1, "切点不得早于 summarize_start: {cut}");
    }

    #[test]
    fn estimate_ignores_usage_inserted_before_a_newer_message() {
        // 压缩场景：compactionSummary(ts=200) 插在 assistant(ts=100, usage=9500) 之前，
        // 该 usage 描述的是压缩前的旧上下文 → 不能作锚点，回退纯估算
        let mut summary = AgentMessage::user_text("summary");
        summary.role = "compactionSummary".into();
        summary.timestamp = 200;
        let stale = assistant_with_usage(100, 9_500);
        let mut tail = AgentMessage::user_text("tail");
        tail.timestamp = 300;

        let msgs = vec![summary, stale, tail];
        let e = estimate_of(&msgs);
        assert_eq!(e.last_usage_index, None);
        assert_eq!(e.usage_tokens, 0);
        assert_eq!(e.tokens, e.trailing_tokens);
    }

    #[test]
    fn estimate_reuses_usage_after_response_to_inserted_context() {
        // 压缩后模型又回了一次（ts=400）→ 新 usage 描述当前上下文，重新成为锚点
        let mut summary = AgentMessage::user_text("summary");
        summary.role = "compactionSummary".into();
        summary.timestamp = 200;
        let stale = assistant_with_usage(100, 9_500);
        let mut prompt = AgentMessage::user_text("new prompt");
        prompt.timestamp = 300;
        let fresh = assistant_with_usage(400, 2_000);
        let mut tail = AgentMessage::user_text("tail");
        tail.timestamp = 500;

        let msgs = vec![summary, stale, prompt, fresh, tail];
        let e = estimate_of(&msgs);
        assert_eq!(e.last_usage_index, Some(3));
        assert_eq!(e.usage_tokens, 2_000);
        assert_eq!(e.tokens, 2_000 + e.trailing_tokens);
    }

    #[test]
    fn stale_guard_ignores_when_no_compaction_boundary() {
        let msgs = vec![assistant_with_usage(100, 50)];
        let e = estimate_of(&msgs);
        assert!(!usage_anchor_is_stale(&e, &msgs, None));
    }

    #[test]
    fn stale_guard_ignores_when_no_usage_anchor() {
        let msgs = vec![AgentMessage::user_text("no usage")];
        let e = estimate_of(&msgs);
        assert_eq!(e.last_usage_index, None);
        assert!(!usage_anchor_is_stale(&e, &msgs, Some(1_000)));
    }

    #[test]
    fn stale_guard_detects_anchor_before_or_at_compaction() {
        // 锚点早于 compaction boundary → 陈旧
        let msgs = vec![assistant_with_usage(100, 50)];
        let e = estimate_of(&msgs);
        assert!(usage_anchor_is_stale(&e, &msgs, Some(200)));
        // 锚点恰好等于 boundary → 陈旧（pi 用 <=）
        assert!(usage_anchor_is_stale(&e, &msgs, Some(100)));
        // 锚点在 boundary 之后 → 不陈旧
        assert!(!usage_anchor_is_stale(&e, &msgs, Some(99)));
    }

    #[test]
    fn stale_guard_uses_last_usage_anchor() {
        // 两条带 usage 的 assistant：锚点是较新的那条（200），早于 boundary 的旧消息忽略
        let mut older = assistant_with_usage(100, 50);
        older.content = vec![crate::core::provider::ContentBlock::Text {
            text: "old".into(),
            text_signature: None,
        }];
        let newer = assistant_with_usage(200, 60);
        let msgs = vec![older, newer];
        let e = estimate_of(&msgs);
        assert_eq!(e.last_usage_index, Some(1));
        // boundary 在 150：锚点 200 不陈旧
        assert!(!usage_anchor_is_stale(&e, &msgs, Some(150)));
        // boundary 在 200：锚点等于 boundary → 陈旧
        assert!(usage_anchor_is_stale(&e, &msgs, Some(200)));
    }
}
