//! core::session_manager 集成测试：pi v4 record-log 会话的创建/追加/恢复/分支/压缩。

use prux::core::provider::AgentMessage;
use prux::core::session_manager::{
    Session, default_session_dir, encode_cwd_dir, find_most_recent_session, list_sessions,
};

fn user_msg(text: &str) -> AgentMessage {
    AgentMessage::user_text(text)
}

#[test]
fn clone_target_walks_past_config_entries_and_fork_at_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let sess_dir = dir.path().join("sessions");

    let mut s = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
    s.append_message(&user_msg("first"));
    let last_msg = s.append_message(&AgentMessage {
        role: "assistant".into(),
        content: vec![],
        ..AgentMessage::user_text("")
    });
    // 配置条目（工具集变更）顶到 lane 叶子，复现 /clone 报 corruption<fork_target_not_message> 的场景
    s.append_active_tools_change(&["read".to_string(), "bash".to_string()]);
    assert_ne!(s.get_leaf_id(), Some(last_msg.as_str()));

    // last_active_tools 取最近一次 ActiveToolsChange 记录
    assert_eq!(
        s.last_active_tools(),
        Some(vec!["read".to_string(), "bash".to_string()])
    );

    // clone 目标回退到最近一条 message entry
    assert_eq!(s.clone_target_id(), Some(last_msg.clone()));

    // 用该目标 fork_at 成功（此前会因目标非 message 直接 corrupt）
    drop(s);
    let path = list_sessions(&sess_dir).pop().expect("has session files");
    let forked = Session::fork_at(&path.to_string_lossy(), &last_msg, cwd, Some(sess_dir)).unwrap();
    assert_eq!(forked.build_context_messages().len(), 2);
}

#[test]
fn encode_cwd_dir_sanitizes() {
    assert_eq!(encode_cwd_dir("/home/user/proj"), "--home-user-proj--");
    assert_eq!(encode_cwd_dir("C:\\Users\\x"), "--C--Users-x--");
}

#[test]
fn create_and_persist_session() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let sess_dir = dir.path().join("sessions");

    let mut s = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
    assert!(s.is_persisted());
    // 对齐 pi v4 repo.create：立即写 header 文件；main lane leaf 为空
    assert!(s.get_session_file().is_some(), "persist 会话创建即落盘");
    assert!(s.get_leaf_id().is_none());

    // 追加 user 消息：写 entry mutation，leaf 前移
    let id1 = s.append_message(&user_msg("hello"));
    assert!(!id1.is_empty());
    assert_eq!(s.get_entries().len(), 1, "1 entry (不含 header)");
    assert_eq!(s.get_leaf_id(), Some(id1.as_str()));

    // 追加 assistant 消息
    let id2 = s.append_message(&AgentMessage {
        role: "assistant".into(),
        content: vec![],
        ..AgentMessage::user_text("")
    });
    assert!(id2 != id1);
    assert!(s.get_session_file().is_some());

    // 文件存在且首行是 header（v4 record-log 头：kind=header/version=4）
    let file = s.get_session_file().unwrap().to_path_buf();
    let text = std::fs::read_to_string(&file).unwrap();
    let first = text.lines().next().unwrap();
    let v: serde_json::Value = serde_json::from_str(first).unwrap();
    assert_eq!(v["kind"], "header");
    assert_eq!(v["version"], 4);
}

#[test]
fn open_rejects_v3_legacy_files() {
    // M5-p3：v3 旧文件不再兼容（session 文件全部基于 v4 record-log）
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("legacy_v3.jsonl");
    let content = "{\"type\":\"session\",\"version\":3,\"id\":\"legacy\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"cwd\":\"/tmp\"}\n";
    std::fs::write(&file, content).unwrap();
    let err = Session::open(&file.to_string_lossy())
        .unwrap_err()
        .to_string();
    assert!(err.contains("v4"), "应拒绝 v3 旧文件: {err}");
}

#[test]
fn v4_session_reduce_recovery_and_effective_config() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let sess_dir = dir.path().join("sessions");
    let mut s = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
    s.append_message(&user_msg("first"));
    s.append_model_change("anthropic", "claude");
    s.append_thinking_level_change("high");
    s.append_active_tools_change(&["read".to_string(), "bash".to_string()]);
    s.append_message(&AgentMessage {
        role: "assistant".into(),
        content: vec![],
        ..AgentMessage::user_text("")
    });
    drop(s);

    let path = list_sessions(&sess_dir).pop().expect("has session files");
    let opened = Session::open(&path.to_string_lossy()).unwrap();

    // v4 会话条目带 seq
    assert!(opened.get_entries()[0].get("seq").is_some());

    // 恢复路径：build_context_messages 经 reduce_lane 校验后产出消息
    let messages = opened.build_context_messages();
    assert_eq!(messages.len(), 2, "1 user + 1 assistant");
    assert_eq!(messages[0].text(), "first");

    // effective_config 从条目归约出模型/推理级别/活动工具
    let cfg = opened.effective_config();
    assert_eq!(cfg.provider, "anthropic");
    assert_eq!(cfg.model_id, "claude");
    assert_eq!(cfg.thinking_level, "high");
    assert_eq!(
        cfg.active_tool_names,
        vec!["read".to_string(), "bash".to_string()]
    );
}

#[test]
fn open_session_restores_messages() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let sess_dir = dir.path().join("sessions");
    let mut s = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
    s.append_message(&user_msg("first"));
    s.append_message(&user_msg("second"));
    s.append_message(&AgentMessage {
        role: "assistant".into(),
        content: vec![],
        ..AgentMessage::user_text("")
    });
    drop(s);

    let path = list_sessions(&sess_dir).pop().expect("has session files");
    let opened = Session::open(&path.to_string_lossy()).unwrap();
    assert_eq!(opened.cwd, cwd);
    assert!(opened.get_leaf_id().is_some());

    let messages = opened.build_context_messages();
    assert_eq!(messages.len(), 3, "2 user + 1 assistant");
    assert_eq!(messages[0].text(), "first");
    assert_eq!(messages[1].text(), "second");
    assert_eq!(messages[2].role, "assistant");
}

#[test]
fn continue_recent_finds_latest() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let sess_dir = dir.path().join("sessions");

    // 两个会话（间隔 50ms，确保时间戳可分辨——同毫秒内创建的会话在
    // 文件系统层面无法区分先后，仅凭 mtime/header 时间会随机并列）
    let mut s1 = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
    s1.append_message(&user_msg("old"));
    drop(s1);
    std::thread::sleep(std::time::Duration::from_millis(50));
    let mut s2 = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
    s2.append_message(&user_msg("new"));
    s2.append_message(&AgentMessage {
        role: "assistant".into(),
        content: vec![],
        ..AgentMessage::user_text("")
    });
    drop(s2);

    let found = find_most_recent_session(&sess_dir).expect("find most recent session");
    let opened = Session::open(&found.to_string_lossy()).unwrap();
    assert_eq!(opened.build_context_messages()[0].text(), "new");
}

#[test]
fn fork_creates_branch() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let sess_dir = dir.path().join("sessions");
    let mut s = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
    s.append_message(&user_msg("original"));
    s.append_message(&AgentMessage {
        role: "assistant".into(),
        content: vec![],
        ..AgentMessage::user_text("")
    });
    let path = s.get_session_file().unwrap().to_string_lossy().to_string();
    drop(s);

    let forked = Session::fork_from(&path, cwd, Some(sess_dir.clone())).unwrap();
    assert_eq!(
        forked.build_context_messages().len(),
        2,
        "1 user + 1 assistant"
    );
    assert_eq!(forked.build_context_messages()[0].text(), "original");
    // fork 出的会话有独立文件
    assert!(forked.get_session_file().is_some());
}

#[test]
fn tree_branch_and_backtrack() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let sess_dir = dir.path().join("sessions");
    let mut s = Session::create(cwd, Some(sess_dir), true).unwrap();

    let _id1 = s.append_message(&user_msg("m1"));
    let id2 = s.append_message(&user_msg("m2"));
    let id3 = s.append_message(&user_msg("m3"));

    // 回退到 m2
    s.set_leaf(Some(&id2)).unwrap();
    assert_eq!(s.get_leaf_id(), Some(id2.as_str()));
    let branch = s.get_branch(&id2);
    assert_eq!(branch.len(), 2, "branch should contain only m1,m2");

    // 从 m2 长出分支
    let id4 = s.append_message(&user_msg("m4"));
    assert_eq!(s.get_leaf_id(), Some(id4.as_str()));

    // 树结构：m3 与 m4 共享父节点 id2
    let tree = s.get_tree();
    assert!(tree.get("leafId").is_some());
    // 构建上下文走当前路径 m1→m2→m4
    let msgs = s.build_context_messages();
    let texts: Vec<String> = msgs.iter().map(|m| m.text()).collect();
    assert_eq!(texts, vec!["m1", "m2", "m4"]);

    // 回退回去仍可看到 m3（append-only，未删除）
    s.set_leaf(Some(&id3)).unwrap();
    let msgs = s.build_context_messages();
    assert_eq!(msgs.len(), 3);
}

#[test]
fn label_and_session_info_entries() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let mut s = Session::create(cwd, Some(dir.path().join("s")), true).unwrap();
    let id = s.append_message(&user_msg("m"));
    s.append_label_change(&id, Some("my label")).unwrap();
    s.append_session_info("display name");
    assert_eq!(s.name.as_deref(), Some("display name"));

    // label 通过 get_tree 的 resolved label 暴露（挂在被标记节点上）
    let tree = s.get_tree();
    let found = find_label_in_tree(&tree, &id);
    assert_eq!(found.as_deref(), Some("my label"));
}

/// 在 get_tree 输出中递归查找 entry.id == target 的节点的 label
fn find_label_in_tree(tree: &serde_json::Value, target: &str) -> Option<String> {
    let nodes = tree.get("tree")?.as_array()?;
    fn walk(node: &serde_json::Value, target: &str) -> Option<String> {
        let entry_id = node
            .pointer("/entry/id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if entry_id == target {
            return node
                .get("label")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
        for child in node
            .get("children")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            if let Some(l) = walk(child, target) {
                return Some(l);
            }
        }
        None
    }
    nodes.iter().find_map(|n| walk(n, target))
}

#[test]
fn model_and_thinking_changes() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let mut s = Session::create(cwd, Some(dir.path().join("s")), true).unwrap();
    s.append_model_change("anthropic", "claude");
    s.append_thinking_level_change("high");

    // 节点类型可被 get_entries 读取
    let types: Vec<String> = s
        .get_entries()
        .iter()
        .filter_map(|e| {
            e.get("type")
                .and_then(|v| v.as_str())
                .map(|x| x.to_string())
        })
        .collect();
    assert!(types.contains(&"model_change".to_string()));
    assert!(types.contains(&"thinking_level_change".to_string()));
}

#[test]
fn compaction_entry_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let mut s = Session::create(cwd, Some(dir.path().join("s")), true).unwrap();
    let _id1 = s.append_message(&user_msg("old"));
    let summary = "## Goal\ntest compaction\n## Progress\n- done";
    let kept = s.append_message(&user_msg("new"));
    s.append_compaction(summary, 1000, Some(&kept), None, None);

    // v4 compaction entry：pi 式 firstKeptEntryId 指向保留区间的**原始条目**
    let comp = s
        .get_entries()
        .iter()
        .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("compaction"))
        .unwrap()
        .clone();
    assert_eq!(comp["firstKeptEntryId"], serde_json::json!(kept));
    assert!(
        comp.get("retainedTail").is_none(),
        "新格式不再内联保留尾部: {comp}"
    );

    let msgs = s.build_context_messages();
    // 压缩后重建：summary 在前 + 保留区间内的原始条目
    let texts: Vec<String> = msgs.iter().map(|m| m.text()).collect();
    assert_eq!(texts.len(), 2);
    assert!(texts[0].contains("## Goal"));
    assert_eq!(texts[1], "new");
    assert_eq!(msgs[1].entry_id.as_deref(), Some(kept.as_str()));
}

#[test]
fn no_session_ephemeral() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let s = Session::create(cwd, None, false).unwrap();
    assert!(!s.is_persisted());
    assert!(s.get_session_file().is_none());
}

#[test]
fn default_session_dir_layout() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    let cwd = "/home/user/my-proj";
    let d = default_session_dir(cwd, &agent_dir);
    assert_eq!(d, agent_dir.join("sessions").join(encode_cwd_dir(cwd)));
}
