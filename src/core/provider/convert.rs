//! 三协议共享的转换辅助：compaction/branch 摘要包装、sanitize、错误截断、
//! 以及模型 `compat` 开关与对话中途系统消息的处理。

use super::{AgentMessage, ModelConfig};
use crate::core::model_resolver;
use serde_json::{Value, json};

/// compaction 摘要的包裹前缀，说明此前历史已被压缩为摘要。
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
/// compaction 摘要的结束标记，与前缀配对闭合 summary 标签。
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
/// 分支摘要的包裹前缀，说明对话来自某条回退分支。
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
/// 分支摘要的结束标记，与分支前缀配对闭合 summary 标签。
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// 把错误文本截到 1000 字节以内（超出时加省略号），避免错误信息撑爆上下文。
pub(crate) fn truncate_for_error(s: &str) -> String {
    if s.len() > 1000 {
        format!("{}...", &s[..1000])
    } else {
        s.to_string()
    }
}

// Rust String 不含孤立 surrogate；直接原样返回
/// 清理字符串中的孤立 surrogate；Rust 的 String 本就不含此类字符，故原样返回。
pub(crate) fn sanitize_surrogates(s: &str) -> String {
    s.to_string()
}

/// 读取模型目录条目的 `compat.<key>` 布尔开关。
///
/// 条目不在目录、缺 `compat` 或缺该键时为 `false`（对齐目录 schema 的可选布尔语义）。
pub(crate) fn compat_flag(model: &ModelConfig, key: &str) -> bool {
    model_resolver::provider_models(&model.provider)
        .iter()
        .find(|m| m.get("id").and_then(|v| v.as_str()) == Some(model.model_id.as_str()))
        .and_then(|m| m.get("compat").and_then(|c| c.get(key)))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 模型目录 `compat.supportsMidConvoSystemMessages`：模型是否原生接受对话中途（`tool_use` 与
/// `tool_result` 之外的普通位置）的 `role: "system"` 消息。为假时这些消息会被并入首部系统提示词。
pub(crate) fn supports_mid_convo_system_messages(model: &ModelConfig) -> bool {
    compat_flag(model, "supportsMidConvoSystemMessages")
}

/// 模型不支持原生对话中途系统消息时，把它们按出现顺序并入首部系统提示词（空行分隔）。
///
/// 支持原生中途系统消息时原样返回 `system_prompt`；没有中途系统消息时也不产生额外分配。
pub(crate) fn collapse_mid_convo_system_messages(
    messages: &[AgentMessage],
    system_prompt: &str,
    model: &ModelConfig,
) -> String {
    if supports_mid_convo_system_messages(model) {
        return system_prompt.to_string();
    }

    let extra: Vec<String> = messages
        .iter()
        .filter(|m| m.role == "system")
        .map(|m| m.text())
        .filter(|t| !t.trim().is_empty())
        .collect();
    if extra.is_empty() {
        return system_prompt.to_string();
    }

    let mut parts: Vec<&str> = Vec::with_capacity(extra.len() + 1);
    if !system_prompt.trim().is_empty() {
        parts.push(system_prompt);
    }

    parts.extend(extra.iter().map(String::as_str));
    parts.join("\n\n")
}

/// 把缓发的中途系统消息追加到 wire 消息末尾并清空缓冲。
pub(crate) fn flush_pending_system_messages(params: &mut Vec<Value>, pending: &mut Vec<Value>) {
    params.append(pending);
}

/// 系统/开发者指令消息的角色名：reasoning 模型且目录声明支持 developer 角色时用 `developer`。
pub(crate) fn instruction_role(model: &ModelConfig) -> &'static str {
    if model.reasoning && model.supports_developer_role {
        "developer"
    } else {
        "system"
    }
}

/// 把一条内部 `system` 消息转成 openai 指令消息（chat completions 与 responses 形状一致）；
/// 文本为空时返回 `None`（不发空消息）。
pub(crate) fn convert_system_message(msg: &AgentMessage, model: &ModelConfig) -> Option<Value> {
    let text = msg.text();
    if text.trim().is_empty() {
        return None;
    }

    Some(json!({ "role": instruction_role(model), "content": sanitize_surrogates(&text) }))
}
