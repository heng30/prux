//! git 仓库查询与操作：仓库根、分支、工作区状态、提交与推送。
//!
//! 这些是「把改动交给远方」所需的**最小** git 面，来自交付链路，但没有任何交付语义
//! （不知道 PR、不知道会话）：谁能用上谁调。
//!
//! 两条硬口径：
//!
//! - **命令不经 shell、输出全部走管道**：参数是数组直接交给 `Command`，没有引号拼接与注入面；
//!   stdout/stderr 收成管道（[`crate::utils::process::run_timed`]），绝不继承 TUI 的终端。
//! - **非零退出码是正常结果**：只有进程起不来（`git` 不在 PATH）才算 `Err`；其余由调用方
//!   按失败文案决定怎么办（「不是 git 仓库」和「git 崩了」在调用方眼里常常是同一件事：
//!   跳过）。

use crate::utils::{glyphs::DEF_ELLIPSIS, process::run_timed, truncate::truncate_chars};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

/// 单条 git 命令的超时（本地操作，正常是毫秒级）。
pub const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// 失败文案里 stderr / stdout 的字符上限。
const DETAIL_CHARS: usize = 400;

/// 一条外部命令的结果。
pub struct Output {
    /// 退出码是否为 0。
    pub ok: bool,
    /// stdout（尾部；`run_timed` 保留前 64 KiB 内的内容）。
    pub stdout: String,
    /// stderr。
    pub stderr: String,
}

impl Output {
    /// 一句话的失败原因（stderr 优先，其次是 stdout；两者都空时只说 `{what} failed`）。
    pub fn failure(&self, what: &str) -> String {
        let detail = if self.stderr.trim().is_empty() {
            self.stdout.trim()
        } else {
            self.stderr.trim()
        };
        let detail = truncate_chars(detail, DETAIL_CHARS, Some(DEF_ELLIPSIS), true);
        if detail.is_empty() {
            format!("{what} failed")
        } else {
            format!("{what} failed: {detail}")
        }
    }
}

/// 跑一条外部命令（不经 shell），工作目录固定为 `dir`。
///
/// `git` 子命令与调用方自己的工具 CLI（如 `gh`）共用这套管道语义：stdin 接 null，
/// stdout/stderr 收管道，超时连子进程一起杀。只有进程起不来才返回 `Err`。
pub fn run(dir: &Path, program: &str, args: &[&str], timeout: Duration) -> Result<Output, String> {
    run_env(dir, program, args, &[], timeout)
}

/// 同 [`run`]，追加一组环境变量：凭证这类不该出现在 argv 里（`ps` 可见）的值走这里。
pub fn run_env(
    dir: &Path,
    program: &str,
    args: &[&str],
    envs: &[(&str, String)],
    timeout: Duration,
) -> Result<Output, String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        // 认证失败时让 git 立刻报错，而不是去问用户名密码——TUI 里没有终端可问，
        // 提示会一直挂着直到超时。
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never");
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let out = run_timed(cmd, None, timeout).map_err(|err| format!("{program}: {err}"))?;
    Ok(Output {
        ok: out.code == Some(0),
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

/// 跑一条 git 子命令。
fn git(dir: &Path, args: &[&str]) -> Result<Output, String> {
    run(dir, "git", args, GIT_TIMEOUT)
}

/// 仓库根（`git rev-parse --show-toplevel`）。
///
/// 不是 git 仓库、`git` 不可用时返回 `Err`——调用方通常据此静默跳过。
pub fn toplevel(dir: &Path) -> Result<PathBuf, String> {
    let out = git(dir, &["rev-parse", "--show-toplevel"])?;
    if !out.ok {
        return Err(out.failure("git rev-parse"));
    }
    Ok(PathBuf::from(out.stdout.trim()))
}

/// 当前分支名；分离 HEAD 时返回字面量 `"HEAD"`。
pub fn branch(dir: &Path) -> Result<String, String> {
    let out = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if !out.ok {
        return Err(out.failure("git rev-parse --abbrev-ref"));
    }
    Ok(out.stdout.trim().to_string())
}

/// 工作区是否有未提交改动（`git status --porcelain`，含未跟踪文件）。
pub fn dirty(dir: &Path) -> Result<bool, String> {
    let out = git(dir, &["status", "--porcelain"])?;
    if !out.ok {
        return Err(out.failure("git status"));
    }
    Ok(!out.stdout.trim().is_empty())
}

/// 本地记录的默认分支（`origin/HEAD` 指向的分支）；没有远端或未 fetch 过时 `None`。
///
/// 只读本地引用，不发网络请求——新克隆的仓库往往没有它，调用方需要自己兜底。
pub fn local_default_branch(dir: &Path) -> Option<String> {
    let out = git(
        dir,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .ok()?;
    if !out.ok {
        return None;
    }
    let name = out.stdout.trim();
    let name = name.split_once('/').map(|(_, rest)| rest).unwrap_or(name);
    (!name.is_empty()).then(|| name.to_string())
}

/// 创建一个新分支并切过去（工作区改动跟着一起走，不丢）。
pub fn create_branch(dir: &Path, name: &str) -> Result<(), String> {
    let out = git(dir, &["checkout", "-b", name])?;
    if !out.ok {
        return Err(out.failure("git checkout -b"));
    }
    Ok(())
}

/// 本地是否已存在同名分支。
pub fn branch_exists(dir: &Path, name: &str) -> bool {
    git(
        dir,
        &["rev-parse", "--verify", &format!("refs/heads/{name}")],
    )
    .map(|out| out.ok)
    .unwrap_or(false)
}

/// 把工作区全部改动暂存（`git add -A`，含未跟踪文件）。
///
/// `git add -A` 的语义是「把眼前这份工作区整个交出去」；调用方要自己保证工作区里
/// 只有它想提交的东西。
pub fn stage_all(dir: &Path) -> Result<(), String> {
    let out = git(dir, &["add", "-A"])?;
    if !out.ok {
        return Err(out.failure("git add"));
    }
    Ok(())
}

/// 把工作区全部改动提交（`git add -A` 后提交，含未跟踪文件）。
pub fn commit_all(dir: &Path, message: &str) -> Result<(), String> {
    stage_all(dir)?;
    commit(dir, message)
}

/// 提交已暂存的内容（不 `add`）；暂存区为空时 `git commit` 失败。
pub fn commit(dir: &Path, message: &str) -> Result<(), String> {
    let out = git(dir, &["commit", "-m", message])?;
    if !out.ok {
        return Err(out.failure("git commit"));
    }
    Ok(())
}

/// 当前分支上有没有任何远端都没有的提交。
///
/// 不看上游：`HEAD --not --remotes` 同时覆盖「有上游但还没推」和「新分支还没有上游」
/// 两种情况，调用方不需要先问 `@{upstream}` 存不存在。
pub fn unpushed(dir: &Path) -> Result<bool, String> {
    let out = git(dir, &["rev-list", "--count", "HEAD", "--not", "--remotes"])?;
    if !out.ok {
        return Err(out.failure("git rev-list"));
    }
    Ok(out.stdout.trim().parse::<u64>().unwrap_or(0) > 0)
}

/// 暂存区改动的文字摘要：`--stat` 加截断后的补丁，`chars` 是补丁部分的字符上限。
///
/// 用途是给模型当「这批改动是什么」的上下文，不是给人读的，所以补丁按 `--unified=0`
/// 只留改动行；`--stat` 失败（暂存区空、不是仓库）时返回 `Err`。
pub fn staged_outline(dir: &Path, chars: usize) -> Result<String, String> {
    diff_outline(dir, &["--cached"], chars)
}

/// 分支相对基线分支的改动摘要（格式同 [`staged_outline`]）：给模型写 PR 标题 / 正文用。
///
/// 先比 `origin/<base>`，拿不到再比本地 `<base>`：交付分支是从本地基线建的，两者
/// merge-base 一致，取远端名是为了让 `origin` 上后来新增的提交也算进「这次交付」。
pub fn branch_outline(dir: &Path, base: &str, chars: usize) -> Result<String, String> {
    let remote = format!("origin/{base}...HEAD");
    let local = format!("{base}...HEAD");
    match diff_outline(dir, &[&remote], chars) {
        Ok(outline) => Ok(outline),
        Err(_) => diff_outline(dir, &[&local], chars),
    }
}

/// `git diff <range> --stat` 加 `git diff <range> --unified=0` 的合并摘要。
///
/// 两者都空（没有任何改动）时返回空串，调用方不必区分「空」与「失败」。
fn diff_outline(dir: &Path, range: &[&str], chars: usize) -> Result<String, String> {
    let stat = diff(dir, "--stat", range)?;
    if !stat.ok {
        return Err(stat.failure("git diff --stat"));
    }
    let patch = diff(dir, "--unified=0", range)?;
    let patch = if patch.ok {
        truncate_chars(&patch.stdout, chars, Some(DEF_ELLIPSIS), true)
    } else {
        String::new()
    };
    let stat = stat.stdout.trim();
    if stat.is_empty() && patch.is_empty() {
        return Ok(String::new());
    }
    Ok(format!("{stat}\n\n{patch}"))
}

/// 跑一条 `git diff`：`mode` 决定要 `--stat` 还是 `--unified=0` 的补丁。
fn diff(dir: &Path, mode: &str, range: &[&str]) -> Result<Output, String> {
    let mut args: Vec<&str> = vec!["diff", "--no-color", mode];
    args.extend_from_slice(range);
    git(dir, &args)
}

/// 把分支推到 `origin`（首次推送建立上游跟踪）。
pub fn push(dir: &Path, branch: &str) -> Result<(), String> {
    let out = git(dir, &["push", "-u", "origin", branch])?;
    if !out.ok {
        return Err(out.failure("git push"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 进程内自增，避免并行测试撞同一个目录。
    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// 造一个临时仓库并在 `main` 上留一个提交；`git` 不可用时返回 `None`。
    fn temp_repo() -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!(
            "prux-git-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).ok()?;
        let ok = Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?
            .success();
        if !ok {
            return None;
        }
        for (key, value) in [("user.email", "t@example.com"), ("user.name", "prux test")] {
            Command::new("git")
                .args(["config", key, value])
                .current_dir(&dir)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .ok()?;
        }
        std::fs::write(dir.join("README.md"), "hello\n").ok()?;
        commit_all(&dir, "init").ok()?;
        Some(dir)
    }

    #[test]
    fn queries_and_commits_round_trip() {
        let Some(repo) = temp_repo() else {
            return; // 没有 git 的环境跳过
        };
        assert_eq!(
            toplevel(&repo).unwrap(),
            repo.canonicalize().unwrap_or(repo.clone())
        );
        assert_eq!(branch(&repo).unwrap(), "main");
        assert!(!dirty(&repo).unwrap(), "刚提交过应为干净");
        assert_eq!(
            local_default_branch(&repo),
            None,
            "没有远端就没有 origin/HEAD"
        );

        std::fs::write(repo.join("work.txt"), "x\n").unwrap();
        assert!(dirty(&repo).unwrap(), "未跟踪文件也算脏");
        commit_all(&repo, "add work").unwrap();
        assert!(!dirty(&repo).unwrap());

        assert!(branch_exists(&repo, "main"));
        assert!(!branch_exists(&repo, "nope"));
        create_branch(&repo, "feature").unwrap();
        assert_eq!(branch(&repo).unwrap(), "feature");
    }

    #[test]
    fn unpushed_is_true_until_the_commit_reaches_a_remote() {
        let Some(repo) = temp_repo() else {
            return; // 没有 git 的环境跳过
        };
        assert!(unpushed(&repo).unwrap(), "没有远端时本地提交都算没推出去");

        let origin = std::env::temp_dir().join(format!(
            "{}-origin.git",
            repo.file_name().unwrap().to_string_lossy()
        ));
        let origin_text = origin.to_string_lossy().to_string();
        let ok = |args: &[&str]| {
            run(&repo, "git", args, GIT_TIMEOUT)
                .map(|out| out.ok)
                .unwrap_or(false)
        };
        assert!(ok(&["init", "--bare", &origin_text]), "建 bare 仓库失败");
        assert!(ok(&["remote", "add", "origin", &origin_text]));
        assert!(ok(&["push", "-u", "origin", "main"]));
        assert!(!unpushed(&repo).unwrap(), "推上去之后就没有未推送提交了");

        std::fs::write(repo.join("next.txt"), "x\n").unwrap();
        commit_all(&repo, "next").unwrap();
        assert!(unpushed(&repo).unwrap(), "本地又提交了一个");
        _ = std::fs::remove_dir_all(&origin);
    }

    #[test]
    fn staged_outline_sees_only_staged_changes() {
        let Some(repo) = temp_repo() else {
            return;
        };
        assert_eq!(
            staged_outline(&repo, 1000).unwrap(),
            "",
            "暂存区空时没有摘要"
        );

        std::fs::write(repo.join("work.txt"), "x\n").unwrap();
        assert_eq!(staged_outline(&repo, 1000).unwrap(), "", "未暂存的改动不算");
        stage_all(&repo).unwrap();
        let outline = staged_outline(&repo, 1000).unwrap();
        assert!(outline.contains("work.txt"), "{outline}");
        assert!(outline.contains("+x"), "补丁里带改动行: {outline}");
    }

    #[test]
    fn branch_outline_covers_the_commits_since_the_base() {
        let Some(repo) = temp_repo() else {
            return;
        };
        create_branch(&repo, "feature").unwrap();
        std::fs::write(repo.join("work.txt"), "x\n").unwrap();
        commit_all(&repo, "work").unwrap();

        let outline = branch_outline(&repo, "main", 2000).unwrap();
        assert!(outline.contains("work.txt"), "{outline}");
        assert_eq!(
            branch_outline(&repo, "feature", 2000).unwrap(),
            "",
            "相对自己没有改动"
        );
    }

    #[test]
    fn non_repository_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir();
        // 临时目录通常不是 git 仓库；若恰好是，跳过这条断言
        if let Ok(top) = toplevel(&dir) {
            assert!(top.is_absolute());
            return;
        }
        assert!(toplevel(&dir).is_err());
    }

    #[test]
    fn failure_text_prefers_stderr_and_is_bounded() {
        let out = Output {
            ok: false,
            stdout: String::new(),
            stderr: "fatal: not a git repository".to_string(),
        };
        assert_eq!(
            out.failure("git status"),
            "git status failed: fatal: not a git repository"
        );
        let silent = Output {
            ok: false,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert_eq!(silent.failure("git push"), "git push failed");
        let long = Output {
            ok: false,
            stdout: String::new(),
            stderr: "字".repeat(DETAIL_CHARS * 2),
        };
        assert!(long.failure("git add").chars().count() <= DETAIL_CHARS + 40);
    }
}
