//! 会话生命周期示例（core::session_manager）
//! 运行：cargo run --example session_demo
//! 演示 Session Tree：创建 → 追加 → 分支 → 回退 → 压缩 → 恢复。

use prux::core::provider::AgentMessage;
use prux::core::session_manager::{Session, list_sessions};

fn user(text: &str) -> AgentMessage {
    AgentMessage::user_text(text)
}

fn main() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let cwd = dir.path().to_str().unwrap();
    let sess_dir = dir.path().join("sessions");

    // 1. 创建会话（pi v4 record-log）
    println!("== create session ==");
    let mut s = Session::create(cwd, Some(sess_dir.clone()), true)?;
    println!("  session id: {}", s.session_id);
    println!("  file: {}", s.get_session_file().unwrap().display());

    // 2. 追加消息（树的主分支）
    println!("\n== append messages ==");
    let _id1 = s.append_message(&user("help me debug the auth error"));
    let id2 = s.append_message(&user("first look at the config"));
    let _id3 = s.append_message(&user("this is the third one"));

    // 3. 回退 + 分支（append-only：旧消息不删除）
    println!("\n== branching ==");
    s.set_leaf(Some(&id2))?;
    let id4 = s.append_message(&user("different approach, check the logs directly"));
    let msgs = s.build_context_messages();
    println!("  current branch message count: {}", msgs.len());
    println!("  leaf: {}", s.get_leaf_id().unwrap());
    assert_eq!(s.get_leaf_id(), Some(id4.as_str()));

    // 4. 压缩：写入 CompactionEntry（firstKeptEntryId = 保留尾部起点条目）
    println!("\n== compaction ==");
    let summary =
        "## Goal\ndebug the auth error\n## Progress\n### In Progress\n- locate the config issue";
    s.append_compaction(summary, 12_345, Some(&id4), None, None);
    let rebuilt = s.build_context_messages();
    println!("  message count after compaction: {}", rebuilt.len());
    println!(
        "  first message is the summary: {}",
        rebuilt[0].text().starts_with("## Goal")
    );

    // 5. 关闭并重新打开（持久化恢复）
    println!("\n== reopen ==");
    let path = s.get_session_file().unwrap().to_string_lossy().to_string();
    drop(s);
    let reopened = Session::open(&path)?;
    println!(
        "  restored message count: {}",
        reopened.build_context_messages().len()
    );

    // 6. 列出目录下所有会话
    println!("\n== session list ==");
    for p in list_sessions(&sess_dir) {
        println!("  {}", p.file_name().unwrap().to_string_lossy());
    }

    println!("\ndone.");
    Ok(())
}
