//! git worktree 隔离
//!
//! `Agent({ isolation: "worktree" })` 让子代理在**仓库的一份临时副本**里干活：
//! 并行改同一批文件时才不会互相踩。跑完就把副本删掉；真有改动就提交到一个分支上，
//! 分支名写进结果——人要 review 的东西因此在主干仓库里，而不是在某个被删掉的临时目录里。
//!
//! ## 三件容易写错的事
//!
//! 1. **严格失败，不静默回退**：不在 git 仓库里 / 还没有任何 commit / `worktree add` 失败时直接报错。
//!    悄悄改用主目录继续跑，等于让调用方以为自己在隔离环境里。
//! 2. **工作目录要保留子目录层级**：调用方在主仓库的 `packages/foo` 下时，副本里的工作目录
//!    得是副本的 `packages/foo`，否则一个 monorepo 包的改动范围会悄悄扩大到整个仓库。
//! 3. **分支名撞了要换一个**：`prux-agent-<id>` 已存在时加时间戳后缀，别覆盖上一份成果。
//!
//! 所有 git 调用都走 `tokio::process`（一次副本可能要几秒，同步调用会把 UI 线程一起堵住），
//! 并且都带超时——`git worktree add` 卡住时不能把整个回合拖死。

use super::super::util::{git, truncate};
use crate::{APP_NAME, utils::time::now_ms};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// 一次隔离运行的副本信息。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Worktree {
    /// 副本根目录
    pub path: String,
    /// 有改动时给这份改动建的分支名
    pub branch: String,
    /// 建副本时的 HEAD（判断"有没有真改过"）
    pub base_sha: String,
    /// 调用方应该在副本里的哪个目录干活（调用方 cwd 在仓库里更深时为副本内的对应子目录）
    pub work_path: String,
}

/// 收尾结果。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct WorktreeResult {
    /// 副本里是否有改动（有则提交成了分支）。
    pub has_changes: bool,
    /// 改动提交到的分支名（留在主仓库供 review）；无改动或提交失败为 None
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// 改动所在的临时副本目录；无改动或提交失败为 None（副本此时已删除）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// 建副本。失败时返回**可读原因**（调用方据此报错，不回退）。
pub(crate) async fn create(cwd: &str, agent_id: &str) -> Result<Worktree, String> {
    git(
        cwd,
        &["rev-parse", "--is-inside-work-tree"],
        Duration::from_secs(5),
    )
    .await
    .map_err(|e| format!("not a git work tree: {e}"))?;

    let base_sha = git(cwd, &["rev-parse", "HEAD"], Duration::from_secs(5))
        .await
        .map_err(|e| format!("the repository has no commit yet: {e}"))?;
    let top = git(
        cwd,
        &["rev-parse", "--show-toplevel"],
        Duration::from_secs(5),
    )
    .await?;

    // 调用方 cwd 相对仓库根的位置（根上就是空串）。两侧都取 realpath：
    // git 给的是解析过的路径，而 cwd 可能经过符号链接进来（macOS 的 /tmp）。
    let subdir = {
        let top = std::fs::canonicalize(&top).unwrap_or_else(|_| PathBuf::from(&top));
        let here = std::fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
        here.strip_prefix(&top)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    };

    // 路径带上 pid：同一 agent id 跨进程不会撞（残留目录会让 `worktree add` 直接失败）
    let path = std::env::temp_dir().join(format!(
        "{APP_NAME}-agent-{agent_id}-{}",
        std::process::id()
    ));
    let path_str = path.to_string_lossy().to_string();
    let branch = format!("{APP_NAME}-agent-{agent_id}");

    git(
        cwd,
        &["worktree", "add", "--detach", &path_str, "HEAD"],
        Duration::from_secs(30),
    )
    .await
    .map_err(|e| format!("could not create the worktree: {e}"))?;

    let work_path = if subdir.is_empty() {
        path_str.clone()
    } else {
        path.join(&subdir).to_string_lossy().to_string()
    };

    Ok(Worktree {
        path: path_str,
        branch,
        base_sha,
        work_path,
    })
}

/// 收尾：没改动就删副本；有改动就提交到分支（分支留在主仓库）再删副本。
///
/// 任何一步失败都尽力删掉副本、返回"无改动"——收尾不该把一个已经跑完的结果变成失败。
pub(crate) async fn cleanup(
    cwd: &str,
    worktree: &mut Worktree,
    description: &str,
) -> WorktreeResult {
    if !Path::new(&worktree.path).exists() {
        return WorktreeResult {
            has_changes: false,
            branch: None,
            path: None,
        };
    }

    let path = worktree.path.clone();
    let status = git(&path, &["status", "--porcelain"], Duration::from_secs(10)).await;

    // 有东西要留成分支吗？（干净副本且 HEAD 没动 → 没有）
    let _keep = match status {
        Ok(text) if !text.trim().is_empty() => {
            // 有改动：全部暂存并提交（`--no-verify`：用户的钩子不该在这个临时副本里跑）
            let message = format!("{APP_NAME}-agent: {}", truncate(description, 200));
            let staged = git(&path, &["add", "-A"], Duration::from_secs(10))
                .await
                .is_ok();
            let committed = staged
                && git(
                    &path,
                    &["commit", "--no-verify", "-m", &message],
                    Duration::from_secs(10),
                )
                .await
                .is_ok();

            // 提交失败（比如缺 user.email）：如实说"没留下分支"
            if !committed {
                _ = remove_worktree(cwd, &path).await;
                return WorktreeResult {
                    has_changes: false,
                    branch: None,
                    path: None,
                };
            }
            true
        }
        Ok(_) => {
            // 干净副本：HEAD 还是建的时候那个 sha → 什么都没发生，直接删
            let moved = git(&path, &["rev-parse", "HEAD"], Duration::from_secs(5))
                .await
                .map(|now| now != worktree.base_sha)
                .unwrap_or(false);

            if !moved {
                _ = remove_worktree(cwd, &path).await;
                return WorktreeResult {
                    has_changes: false,
                    branch: None,
                    path: None,
                };
            }
            true
        }
        Err(_) => {
            _ = remove_worktree(cwd, &path).await;
            return WorktreeResult {
                has_changes: false,
                branch: None,
                path: None,
            };
        }
    };

    // 建分支指向副本的 HEAD；同名已存在就换一个带时间戳的（不覆盖上一份成果）
    let mut branch = worktree.branch.clone();
    if git(&path, &["branch", &branch], Duration::from_secs(5))
        .await
        .is_err()
    {
        branch = format!("{}-{}", worktree.branch, now_ms());
        if git(&path, &["branch", &branch], Duration::from_secs(5))
            .await
            .is_err()
        {
            _ = remove_worktree(cwd, &path).await;
            return WorktreeResult {
                has_changes: false,
                branch: None,
                path: None,
            };
        }
    }

    worktree.branch = branch.clone();
    _ = remove_worktree(cwd, &path).await;
    WorktreeResult {
        has_changes: true,
        branch: Some(branch),
        path: Some(path),
    }
}

/// 强制移除 worktree；失败时退回 `git worktree prune` 清理注册项。
/// 返回 `Err` 表示两条 git 命令都失败（调用方通常忽略）。
async fn remove_worktree(cwd: &str, path: &str) -> Result<(), String> {
    if git(
        cwd,
        &["worktree", "remove", "--force", path],
        Duration::from_secs(10),
    )
    .await
    .is_ok()
    {
        return Ok(());
    }

    git(cwd, &["worktree", "prune"], Duration::from_secs(5))
        .await
        .map(|_| ())
}

/// 清理孤儿副本（上次进程崩了留下的注册项）。
pub(crate) async fn prune(cwd: &str) {
    _ = git(cwd, &["worktree", "prune"], Duration::from_secs(5)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个真仓库（`git init` + 一次提交）。git 不在时跳过（返回 None）。
    async fn repo() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().ok()?;
        let cwd = dir.path().to_string_lossy().to_string();
        let init = git(&cwd, &["init", "-q"], Duration::from_secs(10)).await;
        if init.is_err() {
            return None; // 没有 git：跳过（不静默假装通过）
        }
        git(
            &cwd,
            &["config", "user.email", "t@example.com"],
            Duration::from_secs(5),
        )
        .await
        .ok()?;
        git(&cwd, &["config", "user.name", "t"], Duration::from_secs(5))
            .await
            .ok()?;
        std::fs::write(dir.path().join("a.txt"), "one\n").ok()?;
        git(&cwd, &["add", "-A"], Duration::from_secs(10))
            .await
            .ok()?;
        git(&cwd, &["commit", "-qm", "init"], Duration::from_secs(10))
            .await
            .ok()?;
        Some(dir)
    }

    #[tokio::test]
    async fn worktree_round_trip_commits_changes_and_removes_the_copy() {
        let Some(dir) = repo().await else {
            return;
        };
        let cwd = dir.path().to_string_lossy().to_string();
        let mut wt = create(&cwd, "ab12cd34").await.expect("create");
        assert_eq!(wt.work_path, wt.path, "仓库根上工作目录就是副本根");
        assert!(
            std::path::Path::new(&wt.path).join("a.txt").exists(),
            "副本里有文件"
        );
        assert_eq!(wt.branch, "prux-agent-ab12cd34");

        // 副本里真改一个文件
        std::fs::write(std::path::Path::new(&wt.path).join("a.txt"), "two\n").unwrap();
        let result = cleanup(&cwd, &mut wt, "an agent that edits a.txt").await;
        assert!(result.has_changes, "{result:?}");
        let branch = result.branch.expect("branch");
        assert_eq!(branch, "prux-agent-ab12cd34");
        assert!(!std::path::Path::new(&wt.path).exists(), "副本应被删掉");
        // 分支留在主仓库里，内容是 agent 的改动
        let content = git(
            &cwd,
            &["show", &format!("{branch}:a.txt")],
            Duration::from_secs(5),
        )
        .await
        .expect("branch content");
        assert_eq!(content, "two");

        // 没改动的副本：直接删、不建分支
        let mut wt2 = create(&cwd, "ef56").await.expect("create");
        let clean = cleanup(&cwd, &mut wt2, "did nothing").await;
        assert!(!clean.has_changes, "{clean:?}");
        assert!(clean.branch.is_none());
        assert!(!std::path::Path::new(&wt2.path).exists());
    }

    #[tokio::test]
    async fn create_fails_loudly_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let err = create(&cwd, "x").await.unwrap_err();
        assert!(err.contains("not a git work tree"), "{err}");
    }

    #[tokio::test]
    async fn subdirectory_cwd_keeps_its_scope_inside_the_copy() {
        let Some(dir) = repo().await else {
            return;
        };
        let root = dir.path().to_string_lossy().to_string();
        let sub = dir.path().join("packages").join("foo");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("b.txt"), "x\n").unwrap();
        git(&root, &["add", "-A"], Duration::from_secs(10))
            .await
            .unwrap();
        git(
            &root,
            &["commit", "-qm", "add pkg"],
            Duration::from_secs(10),
        )
        .await
        .unwrap();

        let wt = create(&sub.to_string_lossy(), "deep")
            .await
            .expect("create");
        let expected = std::path::Path::new(&wt.path)
            .join("packages")
            .join("foo")
            .to_string_lossy()
            .to_string();
        assert_eq!(wt.work_path, expected, "副本里要保持子目录层级");
    }
}
