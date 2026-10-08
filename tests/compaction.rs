//! core::compaction 集成测试：token 估算、压缩触发、对话序列化、文件清单、摘要 prompt。

use prux::core::compaction::{
    BRANCH_SUMMARY_PROMPT, SUMMARIZATION_PROMPT, SUMMARIZATION_SYSTEM_PROMPT,
    UPDATE_SUMMARIZATION_PROMPT, build_summary_prompt,
};
use prux::core::compaction::{
    CompactionSettings, DEFAULT_KEEP_RECENT_TOKENS, DEFAULT_RESERVE_TOKENS, FileOperations,
    compute_file_lists, estimate_context_tokens, estimate_tokens, extract_file_ops_from_message,
    format_file_operations, serialize_conversation, should_compact,
};
use prux::core::provider::{AgentMessage, ContentBlock, Usage};

fn msg(role: &str, text: &str) -> AgentMessage {
    let mut m = AgentMessage::user_text(text);
    m.role = role.into();
    m
}

#[test]
fn default_settings() {
    let s = CompactionSettings::default();
    assert!(s.enabled);
    assert_eq!(s.reserve_tokens, DEFAULT_RESERVE_TOKENS);
    assert_eq!(s.keep_recent_tokens, DEFAULT_KEEP_RECENT_TOKENS);
}

#[test]
fn estimate_tokens_basic() {
    // tiktoken cl100k 真实计数："abcdefghijklmnop" 是词表内的整体 token（不再是 chars/4 的 4）
    let m = msg("user", "abcdefghijklmnop");
    assert_eq!(estimate_tokens(&m), 1);
    // 图片按固定当量（ESTIMATED_IMAGE_TOKENS=1200）
    let img = AgentMessage {
        role: "user".into(),
        content: vec![ContentBlock::Image {
            data: "AA==".into(),
            mime_type: "image/png".into(),
        }],
        ..AgentMessage::user_text("")
    };
    assert_eq!(estimate_tokens(&img), 1200);
    // toolResult 计入（"12345678" 在 cl100k 下拆成 3 个 token）
    let tr = msg("toolResult", "12345678");
    assert_eq!(estimate_tokens(&tr), 3);
}

#[test]
fn calculate_context_tokens_prefers_total() {
    // 无 assistant usage 时走纯估算路径
    let msgs = vec![msg("user", "x")];
    let est = estimate_context_tokens(&msgs);
    assert!(est.tokens >= 1);
    assert_eq!(est.last_usage_index, None);
}

#[test]
fn estimate_context_tokens_uses_last_assistant_usage() {
    let mut assistant = msg("assistant", "");
    assistant.usage = Some(Usage {
        input: 500,
        output: 100,
        cache_read: 0,
        cache_write: 0,
        total_tokens: 600,
        cost: Default::default(),
        ..Default::default()
    });
    assistant.stop_reason = Some("stop".into());
    let msgs = vec![assistant, msg("user", "abcdefghijklmnop")];
    let est = estimate_context_tokens(&msgs);
    assert_eq!(est.usage_tokens, 600);
    // 尾部消息按 tiktoken 计数（"abcdefghijklmnop" = 1 token）
    assert_eq!(est.tokens, 600 + 1);
    assert_eq!(est.last_usage_index, Some(0));
}

#[test]
fn should_compact_threshold() {
    let settings = CompactionSettings::default();
    // 窗口 200_000，预留 16384 → 阈值 183616
    assert!(!should_compact(183_616, 200_000, &settings));
    assert!(should_compact(183_617, 200_000, &settings));
    // 禁用时不触发
    let disabled = CompactionSettings {
        enabled: false,
        ..Default::default()
    };
    assert!(!should_compact(999_999, 200_000, &disabled));
}

#[test]
fn serialize_conversation_formats_roles() {
    let msgs = vec![
        msg("user", "hello"),
        msg("assistant", "let me look"),
        msg("toolResult", "file contents"),
    ];
    let text = serialize_conversation(&msgs);
    assert!(text.contains("[User]: hello"));
    assert!(text.contains("[Assistant]: let me look"));
    assert!(text.contains("[Tool result]: file contents"));
}

#[test]
fn serialize_conversation_includes_thinking_and_tool_calls() {
    let mut assistant = msg("assistant", "body");
    assistant.content.push(ContentBlock::Thinking {
        thinking: "deep thinking".into(),
        thinking_signature: None,
        redacted: None,
    });
    assistant.content.push(ContentBlock::ToolCall {
        id: "t1".into(),
        name: "read".into(),
        arguments: serde_json::json!({ "path": "a.rs" }),
        thought_signature: None,
        namespace: None,
    });
    let text = serialize_conversation(&[assistant]);
    assert!(text.contains("[Assistant thinking]: deep thinking"));
    assert!(text.contains("read(path=\"a.rs\")"));
}

#[test]
fn serialize_summary_messages_wrapped() {
    let msgs = vec![msg("compactionSummary", "summary content")];
    let text = serialize_conversation(&msgs);
    assert!(text.contains("[User]: "));
    assert!(text.contains("<summary>"));
    assert!(text.contains("summary content"));
}

#[test]
fn file_ops_tracking() {
    let mut ops = FileOperations::default();
    let mut assistant = msg("assistant", "");
    assistant.content.push(ContentBlock::ToolCall {
        id: "1".into(),
        name: "read".into(),
        arguments: serde_json::json!({ "path": "a.rs" }),
        thought_signature: None,
        namespace: None,
    });
    assistant.content.push(ContentBlock::ToolCall {
        id: "2".into(),
        name: "write".into(),
        arguments: serde_json::json!({ "path": "b.rs" }),
        thought_signature: None,
        namespace: None,
    });
    assistant.content.push(ContentBlock::ToolCall {
        id: "3".into(),
        name: "edit".into(),
        arguments: serde_json::json!({ "path": "b.rs" }),
        thought_signature: None,
        namespace: None,
    });
    extract_file_ops_from_message(&assistant, &mut ops);
    assert!(ops.read.contains("a.rs"));
    assert!(ops.written.contains("b.rs"));
    assert!(ops.edited.contains("b.rs"));

    let (read_files, modified_files) = compute_file_lists(&ops);
    assert_eq!(read_files, vec!["a.rs"]);
    assert_eq!(modified_files, vec!["b.rs"]);
    // 被修改过的文件不再出现在 readFiles
    let mut ops2 = FileOperations::default();
    ops2.read.insert("b.rs".into());
    ops2.edited.insert("b.rs".into());
    let (read_files2, _) = compute_file_lists(&ops2);
    assert!(read_files2.is_empty());
}

#[test]
fn format_file_operations_xml() {
    let s = format_file_operations(&["a.rs".into()], &["b.rs".into()]);
    assert!(s.contains("<read-files>"));
    assert!(s.contains("a.rs"));
    assert!(s.contains("<modified-files>"));
    assert!(s.contains("b.rs"));
    let empty = format_file_operations(&[], &[]);
    assert!(empty.is_empty());
}

#[test]
fn summary_prompts() {
    // 无前次摘要 → 基础模板
    let p = build_summary_prompt(None, None);
    assert!(p.contains("## Goal"));
    assert!(!p.contains("<previous-summary>"));
    // 有前次摘要 → UPDATE 模板 + 前次摘要块（含换行）
    let p2 = build_summary_prompt(Some("old summary"), None);
    assert!(p2.contains("<previous-summary>\nold summary\n</previous-summary>"));
    assert!(p2.contains(UPDATE_SUMMARIZATION_PROMPT));
    // 自定义指令追加
    let p3 = build_summary_prompt(None, Some("focus on errors"));
    assert!(p3.contains("focus on errors"));
    // 常量完整性
    assert!(SUMMARIZATION_SYSTEM_PROMPT.contains("summarization assistant"));
    assert!(SUMMARIZATION_PROMPT.contains("## Goal"));
    assert!(BRANCH_SUMMARY_PROMPT.contains("## Goal"));
}

#[test]
fn estimated_tokens_after_compaction_drops_stale_usage_anchor() {
    // 回归：压缩后消息流 = compactionSummary + 保留尾部，而被保留的最后一条
    // assistant 携带的是「压缩前」的 usage。若不按时间戳判定锚点有效性，
    // estimate_context_tokens 会原样返回压缩前的 usage → 提示出现 "N → N"。
    let mut usage_msg = msg("assistant", "answer");
    usage_msg.usage = Some(Usage {
        input: 286_000,
        output: 101,
        total_tokens: 286_101,
        ..Default::default()
    });
    usage_msg.stop_reason = Some("stop".into());
    usage_msg.timestamp = 100;

    let mut old = msg("user", "old context");
    old.timestamp = 1;
    let mut tail = msg("user", "keep me");
    tail.timestamp = 200;
    let before = vec![old, usage_msg.clone(), tail.clone()];
    let before_tokens = estimate_context_tokens(&before).tokens;
    assert_eq!(before_tokens, 286_101 + estimate_tokens(&tail));

    let mut summary = msg("compactionSummary", "summary text");
    summary.timestamp = 300; // 压缩摘要插在旧 usage 之后生成 → 时间戳更新
    let after = vec![summary, tail, usage_msg];
    let after_tokens = estimate_context_tokens(&after).tokens;

    assert!(
        after_tokens < before_tokens,
        "压缩后估算应显著变小: before={before_tokens} after={after_tokens}"
    );
    // 无有效 usage 锚点 → 纯 token 估算（summary + tail + answer）
    assert_eq!(estimate_context_tokens(&after).last_usage_index, None);
}
