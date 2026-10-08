// AGENTS.md / CLAUDE.md 上下文文件加载

use crate::PROJECT_SCOPE_NAME;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

/// 项目上下文文件的候选文件名，按优先级从高到低依次查找。
const CANDIDATES: [&str; 5] = [
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];

/// 加载项目上下文文件：全局（agentDir） + 从 cwd 到根目录的祖先链
/// git 链接 worktree 遮蔽判定：cwd 位于 git worktree 时（`git rev-parse --git-common-dir` 指向非本地 .git），
/// 主仓库的上下文文件与 worktree 指向同一逻辑仓库，跳过主仓库文件避免双重加载。
fn is_worktree_shadow(cwd: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(cwd)
        .output()
        .ok()?;

    if !out.status.success() {
        return None;
    }

    let common = String::from_utf8_lossy(&out.stdout).trim().to_string();

    // 普通仓库 common-dir == .git（相对/绝对）；worktree 时指向 gitdir/common 目录
    let local = PathBuf::from(&common);
    let local_in_repo = local.is_absolute() || cwd.join(&local).exists();
    if !local_in_repo || common.ends_with("/common") || common == "common" {
        // 处于 worktree：返回其 worktree 根（git rev-parse --show-toplevel）
        return std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(cwd)
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(PathBuf::from(
                        String::from_utf8_lossy(&o.stdout).trim().to_string(),
                    ))
                } else {
                    None
                }
            });
    }
    None
}

/// 主仓库目录（cwd 所在 git 仓库的 toplevel；worktree 时为其主 repo）——遮蔽判定对每个文件用：
/// 若文件 canonical 路径在 worktree 根之外且属于同一 git common dir → 跳过
fn shadowed_by_worktree(file: &Path, worktree_root: &Path, cwd: &Path) -> bool {
    let Ok(fc) = std::fs::canonicalize(file) else {
        return false;
    };
    let Ok(tc) = std::fs::canonicalize(worktree_root) else {
        return false;
    };
    if fc.starts_with(&tc) {
        return false;
    }

    // 文件不在 worktree 根内：属主仓库/其他——如与 cwd 在同一 common dir 则跳过
    let file_git = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(fc.parent().unwrap_or_else(|| Path::new("/")))
        .output()
        .ok();
    let cwd_git = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok();

    match (file_git, cwd_git) {
        (Some(f), Some(c)) if f.status.success() && c.status.success() => {
            let fp = String::from_utf8_lossy(&f.stdout).trim().to_string();
            let cp = String::from_utf8_lossy(&c.stdout).trim().to_string();
            fp == cp && fp != worktree_root.to_string_lossy()
        }
        _ => false,
    }
}

/// 收集需要注入的项目上下文文件，返回 `(绝对路径, 内容)` 列表。
/// 先取全局 `agent_dir` 下的文件，再取 cwd 到根目录的祖先链（根在前、cwd 在后），按路径去重；
/// 处于 git worktree 时跳过被主仓库遮蔽的祖先文件，避免同一逻辑仓库重复加载。
pub fn load_project_context_files(cwd: &str, agent_dir: &Path) -> Vec<(String, String)> {
    let resolved_cwd = PathBuf::from(cwd);
    let resolved_agent_dir = agent_dir.to_path_buf();

    let mut context_files: Vec<(String, String)> = Vec::new();
    let mut seen_paths = HashSet::new();

    if let Some(global) = load_context_file_from_dir(&resolved_agent_dir) {
        seen_paths.insert(global.0.clone());
        context_files.push(global);
    }

    // 从当前目录一直搜索到根节点
    let mut ancestor: Vec<(String, String)> = Vec::new();
    let mut current_dir = resolved_cwd.clone();

    // git 链接 worktree 时跳过主仓库上下文文件
    let worktree_root = is_worktree_shadow(&resolved_cwd);

    loop {
        if let Some(context_file) = load_context_file_from_dir(&current_dir) {
            let path = context_file.0.clone();
            let shadowed = worktree_root.as_ref().is_some_and(|wt| {
                let p = Path::new(&path);
                p.starts_with(&resolved_cwd) && shadowed_by_worktree(p, wt, &resolved_cwd)
            });

            if !seen_paths.contains(&path) && !shadowed {
                ancestor.push(context_file);
                seen_paths.insert(path);
            }
        }

        let parent_dir = match current_dir.parent() {
            Some(p) if p != current_dir => p.to_path_buf(),
            _ => break,
        };
        current_dir = parent_dir;
    }

    ancestor.reverse(); // 根目录的在前，cwd 的在后
    context_files.extend(ancestor);
    context_files
}

/// 在 `dir` 下按 [`CANDIDATES`] 的优先级取第一个存在且可读的文件；都不存在时返回 `None`。
fn load_context_file_from_dir(dir: &Path) -> Option<(String, String)> {
    for filename in CANDIDATES {
        let file_path = dir.join(filename);
        if let Ok(meta) = std::fs::metadata(&file_path)
            && meta.is_file()
            && let Ok(content) = std::fs::read_to_string(&file_path)
        {
            return Some((file_path.to_string_lossy().to_string(), content));
        }
    }
    None
}

/// 发现 SYSTEM.md 替换系统提示：
/// 项目 `PROJECT_SCOPE_NAME/SYSTEM.md`（需项目信任）优先，回退全局 `agentDir/SYSTEM.md`；
/// 存在时其内容**替换**整个默认系统提示（CLI --system-prompt 为文本其余情况保留）。
pub fn discover_system_prompt(cwd: &str, agent_dir: &Path, trusted: bool) -> Option<String> {
    let project_path = PathBuf::from(cwd)
        .join(PROJECT_SCOPE_NAME)
        .join("SYSTEM.md");

    if trusted
        && project_path.is_file()
        && let Ok(content) = std::fs::read_to_string(&project_path)
    {
        return Some(content);
    }

    let global_path = agent_dir.join("SYSTEM.md");
    if global_path.is_file()
        && let Ok(content) = std::fs::read_to_string(&global_path)
    {
        return Some(content);
    }
    None
}

/// 发现追加系统提示文件：项目 `PROJECT_SCOPE_NAME/APPEND_SYSTEM.md`（需项目信任）优先，
/// 回退全局 `agentDir/APPEND_SYSTEM.md`
pub fn discover_append_system_prompt(cwd: &str, agent_dir: &Path, trusted: bool) -> Option<String> {
    let project_path = PathBuf::from(cwd)
        .join(PROJECT_SCOPE_NAME)
        .join("APPEND_SYSTEM.md");
    if trusted
        && project_path.is_file()
        && let Ok(content) = std::fs::read_to_string(&project_path)
    {
        return Some(content);
    }

    let global_path = agent_dir.join("APPEND_SYSTEM.md");
    if global_path.is_file()
        && let Ok(content) = std::fs::read_to_string(&global_path)
    {
        return Some(content);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn project_preferred_when_trusted_else_global() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join(PROJECT_SCOPE_NAME);
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("APPEND_SYSTEM.md"), "project append").unwrap();
        let agent_dir = tmp.path().join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(agent_dir.join("APPEND_SYSTEM.md"), "global append").unwrap();

        // 信任时项目优先
        assert_eq!(
            discover_append_system_prompt(tmp.path().to_str().unwrap(), &agent_dir, true)
                .as_deref(),
            Some("project append")
        );
        // 未信任时跳过项目，回退全局
        assert_eq!(
            discover_append_system_prompt(tmp.path().to_str().unwrap(), &agent_dir, false)
                .as_deref(),
            Some("global append")
        );
    }

    #[test]
    fn global_fallback_and_none() {
        let tmp = TempDir::new().unwrap();
        let agent_dir = tmp.path().join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(agent_dir.join("APPEND_SYSTEM.md"), "global append").unwrap();
        assert_eq!(
            discover_append_system_prompt(tmp.path().to_str().unwrap(), &agent_dir, true)
                .as_deref(),
            Some("global append")
        );

        let empty = TempDir::new().unwrap();
        let empty_agent = empty.path().join("agent");
        fs::create_dir_all(&empty_agent).unwrap();
        assert_eq!(
            discover_append_system_prompt(empty.path().to_str().unwrap(), &empty_agent, true),
            None
        );
    }
}
