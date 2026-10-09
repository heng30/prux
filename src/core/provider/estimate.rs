//! 上下文 token 估算：请求前钳制输出上限与压缩阈值判断共用同一口径。
//!
//! 文本走 tiktoken BPE 真实计数（中英混合下比 `chars/4` 启发式准：
//! 中文 cl100k ≈ 0.9~1.2 token/字），图片没有公开 BPE 口径，按固定当量折算。
//! 放在 provider 层是因为它只依赖本层的消息 / usage 类型，
//! 上层（压缩、请求构造）反过来复用，避免两份估算算法分叉。

use super::{AgentMessage, ContentBlock, Usage};
use crate::utils::tokens;
use serde_json::Value;
use tiktoken_rs::CoreBPE;

/// 图片 token 当量（无对应 BPE 口径，按 4800 字符 ≈ 1200 token 折算）
const ESTIMATED_IMAGE_TOKENS: u32 = 1200;

/// 内容块 → token（文本走 tiktoken BPE 真实计数，图片按固定当量）
fn estimate_content_tokens(enc: &CoreBPE, content: &[ContentBlock]) -> u32 {
    let mut tokens = 0u32;
    for block in content {
        match block {
            ContentBlock::Text { text, .. } => tokens += tokens::count(enc, text) as u32,
            ContentBlock::Image { .. } => tokens += ESTIMATED_IMAGE_TOKENS,
            _ => {}
        }
    }
    tokens
}

/// 估算单条消息 token：text/thinking/toolCall 走 tiktoken BPE 真实计数
/// （中英混合下比 chars/4 启发式准：中文 cl100k ≈ 0.9~1.2 token/字），
/// 图片没有公开 BPE 口径，用固定当量。
pub fn estimate_tokens(message: &AgentMessage) -> u32 {
    // 历史消息没有模型上下文，统一用 cl100k（对中英混合/代码的综合偏差最小）
    let enc = tokens::encoding_for(None);
    match message.role.as_str() {
        "user" | "toolResult" => estimate_content_tokens(enc, &message.content),
        "assistant" => {
            let mut tokens = 0u32;
            for block in &message.content {
                match block {
                    ContentBlock::Text { text, .. } => tokens += tokens::count(enc, text) as u32,
                    ContentBlock::Thinking { thinking, .. } => {
                        tokens += tokens::count(enc, thinking) as u32
                    }
                    ContentBlock::ToolCall {
                        name, arguments, ..
                    } => {
                        tokens += tokens::count(enc, name) as u32;
                        tokens += tokens::count(
                            enc,
                            &serde_json::to_string(arguments).unwrap_or_default(),
                        ) as u32;
                    }
                    _ => {}
                }
            }
            tokens
        }
        _ => 0,
    }
}

/// 估算整段请求上下文的 token 合计：system 提示 + 工具声明 + 逐条消息。
///
/// 工具声明按序列化后的 JSON 计数，因为模型看到的是这份 JSON 而不是结构体。
pub fn estimate_request_tokens(
    messages: &[AgentMessage],
    system_prompt: &str,
    tools: &[(String, String, Value)],
) -> u32 {
    let enc = tokens::encoding_for(None);
    let mut tokens = tokens::count(enc, system_prompt) as u32;

    if !tools.is_empty() {
        let declared: Vec<Value> = tools
            .iter()
            .map(|(name, description, schema)| {
                serde_json::json!({
                    "name": name,
                    "description": description,
                    "parameters": schema,
                })
            })
            .collect();
        tokens += tokens::count(enc, &serde_json::to_string(&declared).unwrap_or_default()) as u32;
    }

    for message in messages {
        tokens = tokens.saturating_add(estimate_tokens(message));
    }

    tokens
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
    /// 总共花费的tokens
    pub tokens: u32,
    /// 截止最后一条assistant花费的tokens
    pub usage_tokens: u32,
    /// 最后一条assistant消息后的文本，估算使用的tokens
    pub trailing_tokens: u32,
    /// 最后一条assistant下标
    pub last_usage_index: Option<usize>,
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
