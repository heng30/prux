//! 自动抽取：一次性模型请求把对话里「跨会话仍然成立」的事抽成结构化条目。
//!
//! 抽取**不塞进主回合**：它是独立的一次性请求（[`crate::core::provider::simple_completion`]），
//! 只带最近一段摘录，输出是 JSON 数组。节奏由 [`super`] 的 `Boundary` 钩子控制
//! （每 N 次 `turn_end` 触发一次）。
//!
//! 这里只放纯逻辑：拼提示词、解析回复、把回复折成 [`ExtractedMemory`]。请求本身在
//! [`super::run_extraction`]，因为它要碰鉴权与模型解析。

use super::graph::{MemoryCategory, MemoryScope, TrustLevel};
use crate::{
    core::provider::{AgentMessage, ModelConfig},
    utils::{glyphs::DEF_ELLIPSIS, truncate::truncate_chars},
};
use serde::Deserialize;

/// 摘录的消息条数上限。
pub(super) const EXCERPT_MESSAGES: usize = 24;

/// 摘录的字符上限。
pub(super) const EXCERPT_CHARS: usize = 12_000;

/// 请求的输出 token 上限。
pub(super) const MAX_TOKENS: u32 = 1024;

/// 关不掉思考的模型上，一次性请求的输出上限下限。
///
/// 与 `recap` / `btw` 同一取舍：这些模型即使不发 `reasoning_effort` 也会思考，
/// 而思考 token 计入输出上限，1k 会被吃光、正文为空。
pub(super) const REASONING_OUTPUT_FLOOR: u32 = 4096;

/// 提示里列出的「已知条目」条数上限（太多会挤掉摘录）。
const MAX_KNOWN: usize = 60;

/// 单次抽取最多接受的条数（模型的输出上限之外再加一道闸门）。
pub(super) const MAX_EXTRACTED: usize = 8;

/// 抽取用的系统提示。
pub(super) const SYSTEM: &str = "You extract durable memories from a coding session for future \
sessions. Reply with a JSON array and nothing else. Write memory content in the language the \nconversation is in.";

/// 一条被抽出来的记忆。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ExtractedMemory {
    /// 分类。
    pub(super) category: MemoryCategory,
    /// 正文（一句话、自洽）。
    pub(super) content: String,
    /// 标签。
    pub(super) tags: Vec<String>,
    /// 该写进哪份存储。
    pub(super) scope: MemoryScope,
    /// 可信度。
    pub(super) trust: TrustLevel,
}

/// 模型回复里的原始条目（宽松反序列化，缺字段都有缺省）。
#[derive(Debug, Deserialize)]
struct RawMemory {
    /// 分类字符串；缺失按事实处理。
    #[serde(default)]
    category: String,
    /// 正文。
    #[serde(default)]
    content: String,
    /// 标签。
    #[serde(default)]
    tags: Vec<String>,
    /// 作用域字符串；缺失按项目处理。
    #[serde(default)]
    scope: String,
    /// 可信度字符串；缺失按中档处理。
    #[serde(default)]
    trust: String,
}

/// 拼抽取提示词：规则 + 已知条目 + 对话摘录。
pub(super) fn build_prompt(excerpt: &str, known: &[String]) -> String {
    let mut prompt = String::from(
        "Extract durable memories from the session excerpt below.\n\n\
         Return a JSON array. Each element must be an object with:\n\
         - \"category\": one of \"fact\", \"preference\", \"entity\", \"correction\"\n\
         - \"content\": one self-contained sentence, understandable without the conversation\n\
         - \"tags\": 1-3 short lowercase keywords\n\
         - \"scope\": \"project\" for repo/environment specifics, \"global\" for user-wide \
         preferences\n\
         - \"trust\": \"high\" when the user stated it directly, \"medium\" when observed, \
         \"low\" when inferred\n\n\
         Rules:\n\
         - Keep only things that stay true across sessions: user preferences, corrections, \
         project conventions, environment/tooling facts, named entities.\n\
         - Drop progress reports, todos, transient state, and anything only meaningful in this turn.\n\
         - Write \"content\" in the language of the conversation, and keep code identifiers verbatim.\n\
         - Never invent; if nothing qualifies return [].\n\
         - At most 8 items. Prefer fewer, better items.\n\
         - Do not repeat entries listed under \"Already known\".\n",
    );

    if !known.is_empty() {
        prompt.push_str("\nAlready known (do not repeat):\n");
        for item in known.iter().take(MAX_KNOWN) {
            prompt.push_str("- ");
            prompt.push_str(&super::recall::single_line(item));
            prompt.push('\n');
        }
    }

    prompt.push_str("\nSession excerpt:\n");
    prompt.push_str(excerpt.trim());
    prompt.push('\n');
    prompt
}

/// 把最近几条消息拼成带角色前缀的摘录，总长不超过 `max_chars`。
///
/// 空文本的消息（只有工具调用的 assistant 轮）被跳过，避免摘录里一堆空行。
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

    truncate_chars(&out, max_chars, Some(DEF_ELLIPSIS), true)
}

/// 消息角色 → 摘录前缀。
fn role_label(role: &str) -> &str {
    match role {
        "assistant" => "assistant",
        "toolResult" => "tool",
        "system" => "system",
        _ => "user",
    }
}

/// 解析模型回复：抽出其中的 JSON 数组并折成条目。
///
/// 容忍代码围栏与前后说明文字；数组元素不是对象、缺 `content`、或正文为空的一律丢弃。
/// 重复（分类 + 归一化正文相同）只保留第一条。返回空向量表示「没有可记的」。
pub(super) fn parse_response(text: &str) -> Vec<ExtractedMemory> {
    let Some(array) = first_json_array(text) else {
        return Vec::new();
    };
    let raw: Vec<RawMemory> = match serde_json::from_str(array) {
        Ok(items) => items,
        Err(_) => return Vec::new(),
    };

    let mut out: Vec<ExtractedMemory> = Vec::new();
    for item in raw {
        let content = item.content.trim();
        if content.is_empty() {
            continue;
        }
        let category = MemoryCategory::parse_lenient(&item.category);
        let scope = match item.scope.trim().to_ascii_lowercase().as_str() {
            "global" => MemoryScope::Global,
            _ => MemoryScope::Project,
        };
        let trust = match item.trust.trim().to_ascii_lowercase().as_str() {
            "high" => TrustLevel::High,
            "low" => TrustLevel::Low,
            _ => TrustLevel::Medium,
        };
        let tags: Vec<String> = item
            .tags
            .iter()
            .map(|tag| tag.trim().to_lowercase())
            .filter(|tag| !tag.is_empty())
            .take(3)
            .collect();
        let duplicate = out
            .iter()
            .any(|seen| seen.category == category && seen.content.eq_ignore_ascii_case(content));
        if duplicate {
            continue;
        }
        out.push(ExtractedMemory {
            category,
            content: content.to_string(),
            tags,
            scope,
            trust,
        });
        if out.len() >= MAX_EXTRACTED {
            break;
        }
    }
    out
}

/// 在文本里找第一个平衡的 JSON 数组（跳过字符串里的方括号与转义）。
///
/// 模型经常把 JSON 包在 ```json 围栏里，或在前面写一句「Here are the memories:」。
fn first_json_array(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = text.find('[')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        let index = start + offset;
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '[' => depth += 1,
            ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = index + ch.len_utf8();
                    return text
                        .get(start..end)
                        .filter(|slice| slice.len() <= bytes.len());
                }
            }
            _ => {}
        }
    }
    None
}

/// 一次性请求的实际输出上限：关不掉思考的模型抬到 [`REASONING_OUTPUT_FLOOR`]。
pub(super) fn output_cap(model: &ModelConfig, requested: u32) -> u32 {
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

/// 模型能否显式关闭思考（`thinkingLevelMap.off == null` 表示不支持）。
fn thinking_can_be_disabled(model: &ModelConfig) -> bool {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|m| m.get("off"))
        .is_none_or(|v| v.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_lists_known_entries_and_excerpt() {
        let prompt = build_prompt(
            "user: we use pnpm\nassistant: noted",
            &["repo uses cargo".to_string()],
        );
        assert!(prompt.contains("Already known"));
        assert!(prompt.contains("- repo uses cargo"));
        assert!(prompt.contains("Session excerpt:"));
        assert!(prompt.contains("user: we use pnpm"));
        assert!(prompt.contains("JSON array"));
    }

    #[test]
    fn parse_plain_array() {
        let items = parse_response(
            r#"[{"category":"preference","content":"Prefers pnpm.","tags":["tooling"],"scope":"global","trust":"high"}]"#,
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].category, MemoryCategory::Preference);
        assert_eq!(items[0].content, "Prefers pnpm.");
        assert_eq!(items[0].tags, vec!["tooling".to_string()]);
        assert_eq!(items[0].scope, MemoryScope::Global);
        assert_eq!(items[0].trust, TrustLevel::High);
    }

    #[test]
    fn parse_keeps_chinese_content_and_tags() {
        let text = r#"[{"category":"纠正","content":"这个仓库要用 make check 而不是 cargo test","tags":["工具链"],"scope":"project"}]"#;
        let items = parse_response(text);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].content,
            "这个仓库要用 make check 而不是 cargo test"
        );
        assert_eq!(items[0].tags, vec!["工具链".to_string()]);
        assert_eq!(
            items[0].category,
            MemoryCategory::Correction,
            "中文分类名同样识别"
        );
    }

    #[test]
    fn parse_tolerates_code_fences_and_prose() {
        let text = "Sure, here you go:\n```json\n[{\"content\":\"Repo uses make check\",\"category\":\"fact\"}]\n```\n";
        let items = parse_response(text);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].content, "Repo uses make check");
        assert_eq!(items[0].scope, MemoryScope::Project, "缺省按项目");
        assert_eq!(items[0].trust, TrustLevel::Medium, "缺省中档");
    }

    #[test]
    fn parse_handles_brackets_inside_strings() {
        let text =
            r#"[{"content":"indexes are written as arr[0] in the parser","category":"fact"}]"#;
        let items = parse_response(text);
        assert_eq!(items.len(), 1);
        assert!(items[0].content.contains("arr[0]"));
    }

    #[test]
    fn parse_drops_empty_unknown_and_duplicate_items() {
        let text = r#"[
            {"content":"   "},
            {"content":"same thing","category":"fact"},
            {"content":"same thing","category":"fact"},
            {"content":"other","category":"weird","tags":[" a ","","b","c","d"]}
        ]"#;
        let items = parse_response(text);
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[1].category, MemoryCategory::Fact, "未知分类归事实");
        assert_eq!(
            items[1].tags,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn parse_caps_item_count() {
        let mut parts = Vec::new();
        for i in 0..20 {
            parts.push(format!(r#"{{"content":"item {i}"}}"#));
        }
        let text = format!("[{}]", parts.join(","));
        assert_eq!(parse_response(&text).len(), MAX_EXTRACTED);
    }

    #[test]
    fn parse_returns_empty_for_garbage() {
        assert!(parse_response("no json here").is_empty());
        assert!(parse_response("[]").is_empty());
        assert!(parse_response("[{\"content\": 1, \"broken\"").is_empty());
        assert!(parse_response("").is_empty());
    }

    #[test]
    fn excerpt_skips_empty_messages_and_caps_chars() {
        let mut empty = AgentMessage::user_text("   ");
        empty.role = "assistant".to_string();
        let messages = vec![
            AgentMessage::user_text("hello world"),
            empty,
            AgentMessage::user_text(&"字".repeat(200)),
        ];
        let excerpt = build_excerpt(&messages, 10, 50);
        assert!(excerpt.starts_with("user: hello world"));
        assert!(!excerpt.contains("assistant:"), "空正文消息被跳过");
        assert!(excerpt.chars().count() <= 51);
        assert_eq!(build_excerpt(&[], 5, 100), "");
    }

    #[test]
    fn output_cap_only_raises_when_thinking_cannot_be_disabled() {
        let base = serde_json::json!({
            "provider": "test",
            "model_id": "m",
            "base_url": "https://example.com/v1",
            "api_key": "",
            "input": ["text"],
            "reasoning": true,
            "max_tokens": null,
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
            "supports_strict_mode": false
        });
        let plain: ModelConfig = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(output_cap(&plain, MAX_TOKENS), MAX_TOKENS);

        let mut stuck = base.clone();
        stuck["thinking_level_map"] = serde_json::json!({ "off": null });
        let model: ModelConfig = serde_json::from_value(stuck).unwrap();
        assert_eq!(output_cap(&model, MAX_TOKENS), REASONING_OUTPUT_FLOOR);
    }
}
