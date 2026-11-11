//! 上下文拆分：把当前上下文窗口按来源拆开（系统提示词各段 / 工具 schema / 会话消息）。
//!
//! 口径与底栏一致：[`estimate_context_tokens`] 给出「已占用」总数（以最近一条 assistant
//! usage 为锚点，锚点之后的消息按 tiktoken 估算补上）。请求体里真实发出的就三块——
//! 系统提示词、工具 schema、会话消息，因此有锚点时三块里的前两块按词表数出来，
//! 差额归给会话消息；没有锚点（首轮回合前）时三块都是估算值，总额也按三块相加，
//! 免得出现「系统提示词与工具 schema 已经几十 K，占用却显示 0」。
//!
//! 系统提示词的分段靠 [`super::system_prompt`] 拼进去的 XML 标签识别，
//! 标签名放在 [`PROMPT_SECTIONS`] 一张表里，改标签要同步改表（有测试兜底）。

use super::{
    compaction::{estimate_context_tokens, estimate_tokens},
    provider::AgentMessage,
};
use crate::utils::{display::fmt_count, tokens};
use serde_json::Value;

/// 系统提示词里可归因的段落 `(标签名, 展示名)`，顺序 = 明细展示顺序。
///
/// 段外内容（内置模板、行为准则、文档段、cwd 行）统一算作 `base`。
pub const PROMPT_SECTIONS: &[(&str, &str)] = &[
    ("project_context", "project context"),
    ("available_skills", "skills"),
    ("mcp_servers", "mcp servers"),
];

/// 拆分出来的一块来源。
pub struct ContextPart {
    /// 展示名（`system prompt` / `tools` / `conversation`）
    pub label: &'static str,
    /// 该块占用的 token 数
    pub tokens: u32,
    /// 该块的明细 `(名称, token)`，系统提示词用它拆出各段
    pub detail: Vec<(String, u32)>,
}

/// 一次上下文拆分的完整结果。
pub struct ContextBreakdown {
    /// 三块来源，顺序固定为 system prompt / tools / conversation
    pub parts: Vec<ContextPart>,
    /// 上下文已占用 token（有锚点用实测量，否则三块估算相加）
    pub total: u32,
    /// `total` 是否来自 provider 实测 usage；false 表示纯估算
    pub measured: bool,
    /// 模型上下文窗口 token；0 表示窗口未知
    pub window: u32,
    /// 参与计算的会话消息条数
    pub message_count: usize,
    /// 发给模型的工具 schema 个数
    pub tool_count: usize,
}

/// 取 `<tag>…</tag>` 整段（含标签）；缺任一端返回空串。
fn tagged<'a>(text: &'a str, tag: &str) -> &'a str {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let Some(start) = text.find(&open) else {
        return "";
    };
    let Some(end) = text[start..].find(&close) else {
        return "";
    };
    &text[start..start + end + close.len()]
}

/// 把系统提示词拆成 `base + 各命名段`：`detail` 首项恒为 base，其后的段只列出非空的。
fn split_prompt(system_prompt: &str, enc: &tiktoken_rs::CoreBPE) -> ContextPart {
    let total = tokens::count(enc, system_prompt) as u32;
    let mut detail = Vec::new();
    let mut tagged_tokens = 0u32;
    let mut sections = Vec::new();
    for (tag, label) in PROMPT_SECTIONS {
        let body = tagged(system_prompt, tag);
        if body.is_empty() {
            continue;
        }
        let n = tokens::count(enc, body) as u32;
        tagged_tokens += n;
        sections.push(((*label).to_string(), n));
    }
    detail.push(("base".to_string(), total.saturating_sub(tagged_tokens)));
    detail.extend(sections);
    ContextPart {
        label: "system prompt",
        tokens: total,
        detail,
    }
}

/// 工具 schema 的 token：名字 + 描述 + 参数 JSON（模型看到的就是这些）。
fn tool_tokens(tools: &[(String, String, Value)], enc: &tiktoken_rs::CoreBPE) -> u32 {
    tools
        .iter()
        .map(|(name, description, parameters)| {
            (tokens::count(enc, name)
                + tokens::count(enc, description)
                + tokens::count(enc, &parameters.to_string())) as u32
        })
        .sum()
}

/// 计算上下文拆分。
///
/// `tools` 必须是**本次请求真正声明**的工具（调用方负责滤掉 `hidden_declarations`），
/// `window` 取实际发包所用模型的上下文窗口（0 表示未知）。
pub fn breakdown(
    system_prompt: &str,
    tools: &[(String, String, Value)],
    messages: &[AgentMessage],
    window: u32,
) -> ContextBreakdown {
    let enc = tokens::encoding_for(None);
    let system = split_prompt(system_prompt, enc);
    let tool_total = tool_tokens(tools, enc);
    let estimate = estimate_context_tokens(messages);
    let conversation_estimate: u32 = messages.iter().map(estimate_tokens).sum();

    let known = system.tokens + tool_total;
    let (total, conversation) = match estimate.last_usage_index {
        Some(_) => (estimate.tokens, estimate.tokens.saturating_sub(known)),
        None => (known + conversation_estimate, conversation_estimate),
    };

    ContextBreakdown {
        parts: vec![
            system,
            ContextPart {
                label: "tools",
                tokens: tool_total,
                detail: Vec::new(),
            },
            ContextPart {
                label: "conversation",
                tokens: conversation,
                detail: Vec::new(),
            },
        ],
        total,
        measured: estimate.last_usage_index.is_some(),
        window,
        message_count: messages.len(),
        tool_count: tools.len(),
    }
}

/// 占比文本：窗口为 0（未知）时给 `-`，避免除零。
fn percent(part: u32, whole: u32) -> String {
    if whole == 0 {
        "-".to_string()
    } else {
        format!("{:.1}%", part as f64 / whole as f64 * 100.0)
    }
}

/// 渲染成聊天区文本：首行总额 + 三块占比（系统提示词带分段明细），末行剩余空间。
///
/// 只靠单个空格分隔，不做列对齐：这条回执在终端（等宽）与桌面窗口（比例字体）里
/// 是同一份文本，空格填充在比例字体下只会把数字推向随机位置。
pub fn render(b: &ContextBreakdown) -> String {
    let source = if b.measured {
        "measured"
    } else {
        "estimated, no usage yet"
    };
    let window = if b.window == 0 {
        "unknown".to_string()
    } else {
        fmt_count(b.window)
    };
    let mut lines = vec![format!(
        "context: {}/{} tokens ({}), {}",
        fmt_count(b.total),
        window,
        percent(b.total, b.window),
        source
    )];
    for part in &b.parts {
        let mut note = percent(part.tokens, b.total);
        match part.label {
            "tools" => note.push_str(&format!(", {} tools", b.tool_count)),
            "conversation" => note.push_str(&format!(", {} messages", b.message_count)),
            _ => {}
        }
        lines.push(format!(
            "  {} {} ({note})",
            part.label,
            fmt_count(part.tokens)
        ));
        for (name, n) in &part.detail {
            if *n == 0 {
                continue;
            }
            lines.push(format!("    {name} {}", fmt_count(*n)));
        }
    }
    if b.window > 0 {
        lines.push(format!(
            "  free {}",
            fmt_count(b.window.saturating_sub(b.total))
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::{ContentBlock, Usage};

    /// 只带正文的用户消息。
    fn user(text: &str) -> AgentMessage {
        AgentMessage::user_text(text)
    }

    /// 带 usage 的 assistant 消息（上下文占用的锚点）。
    fn assistant_with_usage(text: &str, total_tokens: u32) -> AgentMessage {
        let mut msg = AgentMessage::user_text(text);
        msg.role = "assistant".to_string();
        msg.content = vec![ContentBlock::Text {
            text: text.to_string(),
            text_signature: None,
        }];
        msg.usage = Some(Usage {
            input: total_tokens.saturating_sub(100),
            output: 100,
            total_tokens,
            ..Default::default()
        });
        msg
    }

    #[test]
    fn sections_are_attributed_by_tag() {
        let prompt = "base text\n\n<project_context>\nalpha beta gamma delta\n</project_context>\n\n<available_skills>\nskill body here\n</available_skills>";
        let b = breakdown(prompt, &[], &[], 1000);
        let system = &b.parts[0];
        assert_eq!(
            system.tokens,
            tokens::count(tokens::encoding_for(None), prompt) as u32
        );
        let names: Vec<&str> = system.detail.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["base", "project context", "skills"]);
        let detail_sum: u32 = system.detail.iter().map(|(_, n)| n).sum();
        assert_eq!(detail_sum, system.tokens, "分段之和必须等于系统提示词总量");
    }

    #[test]
    fn without_usage_total_is_the_sum_of_parts() {
        let messages = vec![user("hello there, this is a message")];
        let b = breakdown("<project_context>x</project_context>", &[], &messages, 100);
        assert!(!b.measured);
        let sum: u32 = b.parts.iter().map(|p| p.tokens).sum();
        assert_eq!(b.total, sum, "没有 usage 锚点时总额必须是三块之和");
        assert!(b.parts[0].tokens > 0 && b.parts[2].tokens > 0);
    }

    #[test]
    fn measured_total_puts_the_difference_on_conversation() {
        let tools = vec![(
            "read".to_string(),
            "Read a file".to_string(),
            serde_json::json!({"type": "object"}),
        )];
        let b = breakdown(
            "system prompt body",
            &tools,
            &[assistant_with_usage("reply", 1000)],
            2000,
        );
        assert!(b.measured);
        assert_eq!(b.total, 1000);
        assert_eq!(b.tool_count, 1);
        // 差额归会话：三块之和恰好等于实测总额
        let sum: u32 = b.parts.iter().map(|p| p.tokens).sum();
        assert_eq!(sum, 1000);
    }

    #[test]
    fn render_shows_fractions_and_free_space() {
        let b = breakdown("prompt", &[], &[user("hi")], 1000);
        let text = render(&b);
        assert!(text.starts_with("context: "), "{text}");
        assert!(text.contains("system prompt"), "{text}");
        assert!(text.contains("free"), "{text}");
        assert!(text.contains('%'), "{text}");

        let unknown = render(&breakdown("prompt", &[], &[user("hi")], 0));
        assert!(unknown.contains("unknown"), "{unknown}");
        assert!(!unknown.contains("free"), "{unknown}");
    }

    #[test]
    fn count_formatting() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1234567), "1,234,567");
    }

    /// 系统提示词里真的会拼进这些标签：标签改名时这条先红，`PROMPT_SECTIONS` 要同步。
    #[test]
    fn prompt_section_tags_exist_in_system_prompt() {
        // 组装系统提示会读扩展注册表与 agent_dir 下的配置：走隔离目录与串行锁
        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _dir = crate::test_support::AgentDirGuard::temp();
        let options = crate::core::system_prompt::SystemPromptOptions {
            cwd: "/tmp".to_string(),
            selected_tools: Some(vec!["read".to_string()]),
            hidden_tools: Vec::new(),
            tool_snippets: Default::default(),
            prompt_guidelines: Vec::new(),
            append_system_prompt: None,
            context_files: vec![("AGENTS.md".to_string(), "be nice".to_string())],
            skills: vec![crate::core::skills::Skill {
                name: "demo".to_string(),
                description: "demo skill".to_string(),
                path: std::path::PathBuf::from("/s/SKILL.md"),
                base_dir: std::path::PathBuf::from("/s"),
                disable_model_invocation: false,
                source: "test".to_string(),
            }],
            custom_prompt: None,
        };
        let prompt = crate::core::system_prompt::build_system_prompt(&options);
        // `<mcp_servers>` 只在有 codemode 暴露的服务器时出现，由 system_prompt 自己的用例覆盖
        for tag in ["project_context", "available_skills"] {
            assert!(
                PROMPT_SECTIONS.iter().any(|(name, _)| *name == tag),
                "`{tag}` 不在 PROMPT_SECTIONS 里"
            );
            assert!(
                prompt.contains(&format!("<{tag}>")),
                "系统提示词里没有 `<{tag}>` 段：标签改名了要同步 PROMPT_SECTIONS"
            );
        }
    }
}
