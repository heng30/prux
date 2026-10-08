//! 上下文压缩示例（core::compaction）
//! 运行：cargo run --example compaction_demo
//! 演示：token 估算 → 阈值判断 → 对话序列化 → 文件操作跟踪 → 结构化摘要 prompt。

use prux::core::compaction::{
    CompactionSettings, FileOperations, build_summary_prompt, compute_file_lists,
    estimate_context_tokens, estimate_tokens, extract_file_ops_from_message,
    format_file_operations, serialize_conversation, should_compact,
};
use prux::core::provider::{AgentMessage, ContentBlock, Usage};

fn main() {
    // 1. 构建一段对话（user / assistant+工具调用 / toolResult）
    println!("== conversation simulation ==");
    let mut assistant = AgentMessage::user_text("let me read src/main.rs first");
    assistant.role = "assistant".into();
    assistant.content.push(ContentBlock::ToolCall {
        id: "call_1".into(),
        name: "read".into(),
        arguments: serde_json::json!({ "path": "src/main.rs" }),
        thought_signature: None,
        namespace: None,
    });
    assistant.usage = Some(Usage {
        input: 1_200,
        output: 80,
        total_tokens: 1_280,
        ..Default::default()
    });
    assistant.stop_reason = Some("toolUse".into());

    let tool_result = AgentMessage {
        role: "toolResult".into(),
        content: vec![ContentBlock::Text {
            text: "fn main() { println!(\"hello\"); }".into(),
            text_signature: None,
        }],
        ..AgentMessage::user_text("")
    };

    let messages = vec![
        AgentMessage::user_text("help me look at this project"),
        assistant.clone(),
        tool_result,
    ];

    // 2. 单条消息 token 估算（chars/4 启发式）
    println!("\n== token estimation ==");
    for (i, m) in messages.iter().enumerate() {
        println!(
            "  msg[{}] role={:<12} ≈ {} tokens",
            i,
            m.role,
            estimate_tokens(m)
        );
    }

    // 3. 整体上下文估算（以最近 assistant usage 为锚点）
    let est = estimate_context_tokens(&messages);
    println!("  total context estimate: {} tokens", est.tokens);

    // 4. 压缩阈值判断：窗口 200_000、预留 16_384 → 阈值 183_616
    println!("\n== threshold check ==");
    let settings = CompactionSettings::default();
    println!(
        "  current {} vs threshold {} -> compact: {}",
        est.tokens,
        200_000 - settings.reserve_tokens,
        should_compact(est.tokens, 200_000, &settings)
    );
    println!(
        "  if it grows to 200_000 -> compact: {}",
        should_compact(200_000, 200_000, &settings)
    );

    // 5. 序列化对话（喂给摘要 LLM 的文本）
    println!("\n== conversation serialization ==");
    let text = serialize_conversation(&messages);
    println!("{}", text);

    // 6. 文件操作跟踪（readFiles / modifiedFiles 跨压缩累积）
    println!("\n== file tracking ==");
    let mut ops = FileOperations::default();
    extract_file_ops_from_message(&assistant, &mut ops);
    ops.read.insert("src/lib.rs".into());
    ops.edited.insert("src/main.rs".into());
    let (read_files, modified_files) = compute_file_lists(&ops);
    println!("  readFiles:     {:?}", read_files);
    println!("  modifiedFiles: {:?}", modified_files);
    println!(
        "  XML snippet:{}",
        format_file_operations(&read_files, &modified_files)
    );

    // 7. 结构化摘要 prompt（6-section 模板 + 增量更新）
    println!("\n== summary prompt ==");
    let fresh = build_summary_prompt(None, None);
    println!(
        "  first summary ({} chars): has Goal/Progress/Next Steps: {}",
        fresh.len(),
        fresh.contains("## Goal")
            && fresh.contains("## Progress")
            && fresh.contains("## Next Steps")
    );
    let update = build_summary_prompt(Some("old summary"), Some("keep error info"));
    println!(
        "  incremental update: has previous-summary: {}, has custom instructions: {}",
        update.contains("<previous-summary>"),
        update.contains("keep error info")
    );
}
