//! `.output` 运行转写。
//!
//! 每个子代理一份 JSONL：每行一个 child 事件（`turn_end` / `message_end` / `tool_execution_end`）
//!
//! 路径形状：`<tmp>/prux-subagents-<uid>/<encoded-cwd>/<session>/tasks/<id>.output`。
//! `session` 是会话文件的 stem（[`super::session`] 缓存，
//! `on_session_start` 给 `Session`、`on_session_switched` 给路径）；
//! **没有落盘文件的会话**退化为不带该层子目录的老形状。
//! 文件路径记入 [`super::types::AgentRecord::output_path`]，随卡片展示。

use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

/// 编码 cwd 为文件名安全的一段（`/home/u/p` → `home-u-p`）。
pub fn encode_cwd(cwd: &str) -> String {
    let encoded: String = cwd
        .chars()
        .map(|c| if c == '/' || c == '\\' { '-' } else { c })
        .collect();
    let encoded = encoded.trim_matches('-').to_string();
    if encoded.is_empty() {
        "root".to_string()
    } else {
        encoded
    }
}

/// 运行转写的根目录（每用户一个，跨会话保留，便于事后回看）。
///
/// unix 下取真实 uid；非 unix 退化为进程 id。
fn root_dir() -> PathBuf {
    std::env::temp_dir().join(format!("prux-subagents-{}", user_tag()))
}

/// 运行转写根目录所用的用户标识：unix 下取真实 uid，非 unix 退化为进程 id。
fn user_tag() -> u32 {
    #[cfg(unix)]
    {
        nix::unistd::getuid().as_raw()
    }

    #[cfg(not(unix))]
    {
        std::process::id()
    }
}

/// 某 cwd（+ 可选会话）下的任务目录：转写与工作流脚本/journal 都落在这里。
///
/// `session` 为 `None`（没有落盘文件的会话）时不建会话层，与改造前的形状一致。
pub fn tasks_dir(cwd: &str, session: Option<&str>) -> PathBuf {
    let base = root_dir().join(encode_cwd(cwd));

    // 会话键来自文件名，正常不含分隔符；仍然挡一道，免得它变成路径穿越。
    match session.filter(|s| !s.is_empty() && !s.contains(['/', '\\']) && !s.contains("..")) {
        Some(key) => base.join(key).join("tasks"),
        None => base.join("tasks"),
    }
}

/// 某一 cwd + 会话 + agent id 的转写文件路径。
pub fn output_path(cwd: &str, session: Option<&str>, agent_id: &str) -> PathBuf {
    tasks_dir(cwd, session).join(format!("{agent_id}.output"))
}

/// 同目录下的一个具名文件（工作流脚本 / journal）。
pub fn artifact_path(cwd: &str, session: Option<&str>, name: &str) -> PathBuf {
    tasks_dir(cwd, session).join(name)
}

/// 创建（或截断）转写文件并写入头行；返回路径（失败返回 None，不影响子代理执行）。
pub fn create_output_file(
    cwd: &str,
    session: Option<&str>,
    agent_id: &str,
    meta: &Value,
) -> Option<PathBuf> {
    let path = output_path(cwd, session, agent_id);
    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return None;
    }

    let mut file = fs::File::create(&path).ok()?;
    writeln!(file, "{meta}").ok()?;
    Some(path)
}

/// 追加一行事件（失败静默：转写是旁路，不能影响子代理执行）。
pub fn append_event(path: &Path, event: &Value) {
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    _ = writeln!(file, "{event}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encode_cwd_matches_upstream_conventions() {
        assert_eq!(encode_cwd("/home/user/project"), "home-user-project");
        assert_eq!(encode_cwd("/"), "root");
        assert_eq!(encode_cwd(""), "root");
        // Windows 盘符前缀保留（上游会再削掉 "C:-"，这里保留以便一眼认出盘符）
        assert_eq!(encode_cwd("C:\\Users\\foo"), "C:-Users-foo");
    }

    #[test]
    fn create_and_append_writes_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let id = format!("test-{}", std::process::id());
        let path = create_output_file(
            &cwd,
            Some("s1"),
            &id,
            &json!({"type": "agent_start", "id": id}),
        )
        .expect("创建转写文件");
        append_event(&path, &json!({"type": "turn_end", "turn": 1}));
        append_event(&path, &json!({"type": "turn_end", "turn": 2}));

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "头行 + 2 事件: {text}");
        assert!(lines[0].contains("agent_start"));
        assert!(lines[2].contains("\"turn\":2"));
        // 路径形状：<tmp>/prux-subagents-*/<encoded-cwd>/<session>/tasks/<id>.output
        assert!(
            path.to_string_lossy().contains("/s1/tasks/"),
            "{}",
            path.display()
        );
        assert!(path.ends_with(format!("{id}.output")));
        // 无会话键（未落盘的会话）退化为不带会话层
        let bare = output_path(&cwd, None, "x");
        assert!(
            bare.to_string_lossy().contains("/tasks/"),
            "{}",
            bare.display()
        );
        // 会话键里的路径分隔符一律当没有（防穿越）
        let dodgy = tasks_dir(&cwd, Some("../../etc"));
        assert!(
            !dodgy.to_string_lossy().contains("etc"),
            "{}",
            dodgy.display()
        );
        assert!(create_output_file(&cwd, Some("s1"), "x", &json!({})).is_some());
        let _ = std::fs::remove_file(&path);
    }
}
