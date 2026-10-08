//! 持久记忆：per-agent memory 目录，跨会话保留。
//!
//! 作用域：
//! - `user`    → `<agent_dir()>/agent-memory/<name>/`
//! - `project` → `<cwd>/PROJECT_SCOPE_NAME/agent-memory/<name>/`
//! - `local`   → `<cwd>/PROJECT_SCOPE_NAME/agent-memory-local/<name>/`
//!
//! **project/local 读写都要求项目受信任**（未受信不注入、不写）；
//! 非法 agent 名与 symlink 目录/文件一律拒绝。`MEMORY.md` 只取前 200 行。

use super::types::MemoryScope;
use crate::{
    PROJECT_SCOPE_NAME,
    core::{project_trust::is_project_trusted, settings_manager::agent_dir},
    utils::paths::is_symlink,
};
use std::path::{Path, PathBuf};

/// `MEMORY.md` 最多读取的行数。
const MAX_MEMORY_LINES: usize = 200;

// 最大名字长度
const MAX_NAME_LEN: usize = 128;

/// 名字白名单：字母/数字开头，随后允许字母、数字、`.`、`_`、`-`（不允许路径分隔）。
///
/// agent 名与具名工作流名共用同一条校验（[`crate::extensions::subagent::workflow::saved`]）。
/// **不做 trim**：调用方要先 trim 再传进来，否则拼出来的目录/文件名会带空白。
pub(crate) fn is_unsafe_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return true;
    }

    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphanumeric()) {
        return true;
    }

    !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// 解析 memory 目录；project/local 未受信任时返回可读错误。
fn resolve_dir(name: &str, scope: MemoryScope, cwd: &str) -> Result<PathBuf, String> {
    if is_unsafe_name(name) {
        return Err(format!(
            "unsafe agent name for memory directory: {name:?} (use letters, digits, '.', '_', '-')"
        ));
    }

    match scope {
        MemoryScope::User => Ok(agent_dir().join("agent-memory").join(name)),
        MemoryScope::Project | MemoryScope::Local => {
            let cwd_path = Path::new(cwd);

            if !is_project_trusted(cwd_path, &agent_dir()) {
                return Err(
                    "project is not trusted; refusing to read or write persistent memory"
                        .to_string(),
                );
            }

            let sub = if scope == MemoryScope::Project {
                "agent-memory"
            } else {
                "agent-memory-local"
            };

            Ok(cwd_path.join(PROJECT_SCOPE_NAME).join(sub).join(name))
        }
    }
}

/// 只在目录不是 symlink 时创建（防 symlink 目录穿越）。
fn ensure_dir(dir: &Path) -> Result<(), String> {
    if dir.exists() {
        if is_symlink(dir) {
            return Err(format!(
                "refusing to use symlinked memory directory: {}",
                dir.display()
            ));
        }

        return Ok(());
    }

    std::fs::create_dir_all(dir)
        .map_err(|e| format!("cannot create memory directory {}: {e}", dir.display()))
}

/// 安全读取：拒绝 symlink，缺失/不可读返回 None。
fn safe_read(path: &Path) -> Option<String> {
    if !path.exists() || is_symlink(path) {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// 读 `MEMORY.md`（截断到 [`MAX_MEMORY_LINES`] 行）。
fn read_index(dir: &Path) -> Option<String> {
    if is_symlink(dir) {
        return None;
    }

    let content = safe_read(&dir.join("MEMORY.md"))?;
    let lines: Vec<&str> = content.lines().collect();

    if lines.len() > MAX_MEMORY_LINES {
        Some(format!(
            "{}\n... (truncated at {MAX_MEMORY_LINES} lines)",
            lines[..MAX_MEMORY_LINES].join("\n")
        ))
    } else {
        Some(content)
    }
}

/// 读写记忆块：确保目录存在，附上现有 `MEMORY.md` 与写入指引。
fn build_block(agent_name: &str, scope: MemoryScope, dir: &Path) -> String {
    let existing = read_index(dir);
    let header = format!(
        "# Agent Memory\n\nYou have a persistent memory directory at: {}/\nMemory scope: {}\n\nThis memory persists across sessions. Use it to build up knowledge over time.",
        dir.display(),
        scope.as_str(),
    );

    let body = match existing {
        Some(text) => format!("\n\n## Current MEMORY.md\n{text}"),
        None => format!(
            "\n\nNo MEMORY.md exists yet. Create one at {} to start building persistent memory.",
            dir.join("MEMORY.md").display()
        ),
    };

    let instructions = format!(
        "\n\n## Memory Instructions\n\
         - MEMORY.md is an index file — keep it concise (under {MAX_MEMORY_LINES} lines). Lines after {MAX_MEMORY_LINES} are truncated.\n\
         - Store detailed memories in separate files within {}/ and link to them from MEMORY.md.\n\
         - Each memory file should use this frontmatter format:\n  ---\n  name: <memory name>\n  description: <one-line description>\n  type: <user|feedback|project|reference>\n  ---\n  <memory content>\n\
         - Update or remove memories that become outdated. Check for existing memories before creating duplicates.\n\
         - You have Read, Write, and Edit tools available for managing memory files.",
        dir.display()
    );

    format!("{header}{body}{instructions}\n\n_(agent: {agent_name})_")
}

/// 只读记忆块：不创建目录，只消费既有记忆。
fn build_read_only_block(agent_name: &str, scope: MemoryScope, dir: &Path) -> String {
    let existing = read_index(dir);
    let header = format!(
        "# Agent Memory (read-only)\n\nMemory scope: {}\nYou have read-only access to memory. You can reference existing memories but cannot create or modify them.",
        scope.as_str(),
    );
    let body = match existing {
        Some(text) => format!("\n\n## Current MEMORY.md\n{text}"),
        None => "\n\nNo memory is available yet. Other agents or sessions with write access can create memories for you to consume.".to_string(),
    };
    format!("{header}{body}\n\n_(agent: {agent_name})_")
}

/// 该名字是否在（已合并的）工具白名单里真正可用。
fn effective_has(tools: &Option<Vec<String>>, disallowed: &[String], name: &str) -> bool {
    if disallowed.iter().any(|d| d == name) {
        return false;
    }

    match tools {
        None => true,
        Some(list) => {
            if list.iter().any(|t| t == "*" || t == "all") {
                true
            } else if list.is_empty() || list.iter().any(|t| t == "none") {
                false
            } else {
                list.iter().any(|t| t == name)
            }
        }
    }
}

/// 记忆规划结果：要注入的提示块 + 需要补进工具白名单的名字。
#[derive(Debug)]
pub(crate) struct MemoryPlan {
    /// 要注入 append 提示的记忆块。
    pub block: String,
    /// 需要补进工具白名单的名字。
    pub add_tools: Vec<String>,
}

/// 按 frontmatter `memory` 规划记忆注入（读写/只读分支、补工具）。
///
/// 返回 `Err`（未受信 / 非法名 / symlink）时调用方应告警并跳过记忆。
pub(crate) fn plan(
    agent_name: &str,
    scope: MemoryScope,
    cwd: &str,
    tools: &Option<Vec<String>>,
    disallowed: &[String],
) -> Result<MemoryPlan, String> {
    let dir = resolve_dir(agent_name, scope, cwd)?;
    let has_write =
        effective_has(tools, disallowed, "write") || effective_has(tools, disallowed, "edit");

    if has_write {
        ensure_dir(&dir)?;
        let block = build_block(agent_name, scope, &dir);
        let add_tools = ["read", "write", "edit"]
            .into_iter()
            .filter(|n| !effective_has(tools, disallowed, n))
            .map(str::to_string)
            .collect();
        Ok(MemoryPlan { block, add_tools })
    } else {
        // 只读：不创建目录，只补 read
        let block = build_read_only_block(agent_name, scope, &dir);
        let add_tools = ["read"]
            .into_iter()
            .filter(|n| !effective_has(tools, disallowed, n))
            .map(str::to_string)
            .collect();
        Ok(MemoryPlan { block, add_tools })
    }
}

/// 把 `plan` 的结果补进工具白名单（None = 全部，不需补）。
pub(crate) fn apply_tools(tools: &mut Option<Vec<String>>, add: &[String]) {
    if add.is_empty() {
        return;
    }

    match tools {
        None => {}
        Some(list) => {
            if list.iter().any(|t| t == "*" || t == "all") {
                return;
            }

            for name in add {
                if !list.iter().any(|t| t == name) {
                    list.push(name.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;

    #[test]
    fn unsafe_names_are_rejected() {
        assert!(is_unsafe_name(""));
        assert!(is_unsafe_name("../escape"));
        assert!(is_unsafe_name(".hidden"));
        assert!(is_unsafe_name("a/b"));
        assert!(is_unsafe_name("a\\b"));
        assert!(!is_unsafe_name("Explore"));
        assert!(!is_unsafe_name("my-agent.v2"));
    }

    #[test]
    fn user_scope_writes_and_reads_back() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = AgentDirGuard::temp();
        let plan = plan(
            "Explore",
            MemoryScope::User,
            "/tmp",
            &Some(vec!["write".into()]),
            &[],
        )
        .expect("user scope 不需要信任");
        assert!(plan.add_tools.iter().any(|t| t == "read"));
        let dir = resolve_dir("Explore", MemoryScope::User, "/tmp").unwrap();
        assert!(dir.exists(), "读写分支应创建目录");
        std::fs::write(dir.join("MEMORY.md"), "line1\nline2").unwrap();
        let block = build_block("Explore", MemoryScope::User, &dir);
        assert!(block.contains("line1"), "{block}");
    }

    #[test]
    fn read_only_when_no_write_tools() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = AgentDirGuard::temp();
        let plan = plan(
            "Explore",
            MemoryScope::User,
            "/tmp",
            &Some(vec!["read".into(), "bash".into()]),
            &[],
        )
        .unwrap();
        assert!(plan.add_tools.is_empty(), "read 已在白名单里");
        assert!(plan.block.contains("read-only"), "{}", plan.block);
        let dir = resolve_dir("Explore", MemoryScope::User, "/tmp").unwrap();
        assert!(!dir.exists(), "只读分支不应创建目录");
    }

    #[test]
    fn project_scope_requires_trust() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd_s = cwd.path().to_string_lossy().to_string();
        // 未受信任：拒绝
        let err = plan("Explore", MemoryScope::Project, &cwd_s, &None, &[]).unwrap_err();
        assert!(err.contains("not trusted"), "{err}");
    }

    #[test]
    fn index_is_truncated_at_200_lines() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = AgentDirGuard::temp();
        let dir = resolve_dir("Explore", MemoryScope::User, "/tmp").unwrap();
        ensure_dir(&dir).unwrap();
        let content: String = (0..250).map(|i| format!("l{i}\n")).collect();
        std::fs::write(dir.join("MEMORY.md"), content).unwrap();
        let idx = read_index(&dir).unwrap();
        assert!(idx.contains("truncated at 200 lines"), "{idx}");
        assert_eq!(idx.lines().filter(|l| l.starts_with('l')).count(), 200);
    }
}
