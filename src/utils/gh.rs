//! `gh`（GitHub CLI）的调用与 PR 状态解析：登录检查、默认分支、PR 号、开 PR、读检查与评审。
//!
//! 这是「把改动变成可评审的东西」所需的**最小**托管平台面：只读结构化字段（`--json`），
//! 不解析人类可读的输出，也不假设 prompt 文案。交付语义（什么时候开 PR、失败后怎么续跑）
//! 不在这里。
//!
//! 两条硬口径：
//!
//! - **命令不经 shell、输出全部走管道**：参数是数组直接交给 `Command`，通过
//!   [`crate::utils::git::run`] 统一收 stdout/stderr，绝不继承 TUI 的终端。
//! - **非零退出码是正常结果**：只有进程起不来（`gh` 不在 PATH）才算 `Err`；`gh pr view`
//!   在没有 PR 时也是非零退出，所以「没有 PR」与「真出错」只能靠文案区分
//!   （见 [`pr_number`]）。

use crate::utils::{
    git::{self, Output},
    glyphs::DEF_ELLIPSIS,
    truncate::truncate_chars,
};
use serde_json::Value;
use std::{path::Path, time::Duration};

/// 单条 `gh` 命令的超时（网络操作；`gh pr view` 偶尔要几十秒）。
pub const GH_TIMEOUT: Duration = Duration::from_secs(120);

/// 失败报告里每条正文的字符上限。
const DETAIL_CHARS: usize = 400;

/// 跑一条 `gh` 子命令。
fn gh(dir: &Path, args: &[&str]) -> Result<Output, String> {
    git::run(dir, "gh", args, GH_TIMEOUT)
}

/// `gh` 是否可用且已登录。
///
/// 只回答「能不能用」：未安装与未登录都是 `Err`，文案里带 `gh` 自己的说明。
pub fn auth_ok(dir: &Path) -> Result<(), String> {
    let out = gh(dir, &["auth", "status"])?;
    if !out.ok {
        return Err(out.failure("gh auth status"));
    }
    Ok(())
}

/// 仓库的默认分支（`gh repo view`，需要网络）。
pub fn default_branch(dir: &Path) -> Result<String, String> {
    let out = gh(
        dir,
        &[
            "repo",
            "view",
            "--json",
            "defaultBranchRef",
            "--jq",
            ".defaultBranchRef.name",
        ],
    )?;
    if !out.ok {
        return Err(out.failure("gh repo view"));
    }
    let name = out.stdout.trim();
    if name.is_empty() {
        return Err("gh repo view returned no default branch".to_string());
    }
    Ok(name.to_string())
}

/// 该分支的 PR 号；没有 PR 时 `Ok(None)`。
///
/// 「没有 PR」与「真出错」都是非零退出码，只能看 `gh` 的文案区分；认不出的错误按出错返回，
/// 宁可停下来报告，也不要在没 PR 的时候误以为已经开过。
pub fn pr_number(dir: &Path, branch: &str) -> Result<Option<u64>, String> {
    let out = gh(
        dir,
        &["pr", "view", branch, "--json", "number", "--jq", ".number"],
    )?;
    if !out.ok {
        let text = format!("{}{}", out.stdout, out.stderr).to_ascii_lowercase();
        if text.contains("no pull requests found") || text.contains("no open pull requests") {
            return Ok(None);
        }
        return Err(out.failure("gh pr view"));
    }
    Ok(out.stdout.trim().parse::<u64>().ok())
}

/// 新建 PR，返回它的 URL。
///
/// `body` 直接作为 PR 正文；`draft` 为 true 时加 `--draft`。
pub fn create_pr(
    dir: &Path,
    branch: &str,
    base: &str,
    title: &str,
    body: &str,
    draft: bool,
) -> Result<String, String> {
    let mut args = vec![
        "pr", "create", "--base", base, "--head", branch, "--title", title, "--body", body,
    ];
    if draft {
        args.push("--draft");
    }
    let out = gh(dir, &args)?;
    if !out.ok {
        return Err(out.failure("gh pr create"));
    }
    Ok(out.stdout.trim().to_string())
}

/// PR 的当前状态快照。
pub struct Status {
    /// PR 号；输出里没有时为 `0`。
    pub number: u64,
    /// PR 链接；输出里没有时为空串。
    pub url: String,
    /// 结论。
    pub verdict: Verdict,
}

/// 一次快照的三种结论。
pub enum Verdict {
    /// 全部检查通过、没有待处理的变更请求。
    Green,
    /// 还有检查没出结论（`"3 checks pending"`）。
    Pending(String),
    /// 有阻塞：检查失败或评审要求改。
    Blocked(Failure),
}

/// 阻塞项。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Failure {
    /// 失败的检查：`名称 — 结论`。
    pub checks: Vec<String>,
    /// 要求修改的评审意见：`作者: 正文`。
    pub reviews: Vec<String>,
}

impl Failure {
    /// 是否什么都没拦（既无失败检查也无变更请求）。
    pub fn is_empty(&self) -> bool {
        self.checks.is_empty() && self.reviews.is_empty()
    }

    /// 逐条列出阻塞项（`失败检查` / `评审意见` 两段，各段仅在非空时出现）。
    ///
    /// 只给事实，不掺「接下来该怎么办」——那是调用方的事。
    pub fn details(&self) -> String {
        let mut out = String::new();
        if !self.checks.is_empty() {
            out.push_str("Failing checks:\n");
            for check in &self.checks {
                out.push_str("- ");
                out.push_str(check);
                out.push('\n');
            }
        }
        if !self.reviews.is_empty() {
            out.push_str("\nReview feedback:\n");
            for review in &self.reviews {
                out.push_str("- ");
                out.push_str(review);
                out.push('\n');
            }
        }
        out
    }
}

/// 读一次 PR 状态：`statusCheckRollup` 的失败/未完成项 + `latestReviews` 的 CHANGES_REQUESTED。
pub fn pr_status(dir: &Path, branch: &str) -> Result<Status, String> {
    let out = gh(
        dir,
        &[
            "pr",
            "view",
            branch,
            "--json",
            "number,url,statusCheckRollup,latestReviews",
        ],
    )?;
    if !out.ok {
        return Err(out.failure("gh pr view"));
    }
    let value: Value = serde_json::from_str(&out.stdout)
        .map_err(|err| format!("gh pr view: malformed JSON: {err}"))?;
    Ok(status_from_json(&value))
}

/// 把 `gh pr view --json ...` 的输出折成 [`Status`]（纯函数，便于测试）。
///
/// 检查项有两种形状：`CheckRun` 用 `name` + `status`/`conclusion`，`StatusContext` 用
/// `context` + `state`。无法归入「通过」与「失败」的结论一律算「未完成」——等它出结果，
/// 而不是当成通过把红的说成绿的。
pub fn status_from_json(value: &Value) -> Status {
    let number = value.get("number").and_then(Value::as_u64).unwrap_or(0);
    let url = value
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut failure = Failure::default();
    let mut pending = 0usize;
    for check in value
        .get("statusCheckRollup")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let name = check
            .get("name")
            .or_else(|| check.get("context"))
            .and_then(Value::as_str)
            .unwrap_or("check");
        let state = check
            .get("conclusion")
            .and_then(Value::as_str)
            .or_else(|| check.get("state").and_then(Value::as_str))
            .or_else(|| check.get("status").and_then(Value::as_str))
            .unwrap_or("");
        match state.to_ascii_uppercase().as_str() {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => {}
            "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
            | "ERROR" => failure.checks.push(format!("{name} — {state}")),
            _ => pending += 1,
        }
    }

    for review in value
        .get("latestReviews")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        if review.get("state").and_then(Value::as_str) != Some("CHANGES_REQUESTED") {
            continue;
        }
        let author = review
            .get("author")
            .and_then(|author| author.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("reviewer");
        let body = review
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let body = truncate_chars(body, DETAIL_CHARS, Some(DEF_ELLIPSIS), true);
        failure.reviews.push(if body.is_empty() {
            author.to_string()
        } else {
            format!("{author}: {body}")
        });
    }

    let verdict = if !failure.is_empty() {
        Verdict::Blocked(failure)
    } else if pending > 0 {
        Verdict::Pending(format!(
            "{pending} check{} pending",
            if pending == 1 { "" } else { "s" }
        ))
    } else {
        Verdict::Green
    };
    Status {
        number,
        url,
        verdict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn green_when_every_check_passed() {
        let status = status_from_json(&json!({
            "number": 7,
            "url": "https://example.com/pr/7",
            "statusCheckRollup": [
                {"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS"},
                {"__typename": "StatusContext", "context": "ci/lint", "state": "SUCCESS"},
                {"__typename": "CheckRun", "name": "docs", "status": "COMPLETED", "conclusion": "SKIPPED"}
            ],
            "latestReviews": [{"state": "APPROVED", "author": {"login": "bob"}, "body": "lgtm"}]
        }));
        assert_eq!(status.number, 7);
        assert!(matches!(status.verdict, Verdict::Green));
    }

    #[test]
    fn unknown_conclusions_count_as_pending() {
        let status = status_from_json(&json!({
            "number": 1,
            "statusCheckRollup": [
                {"name": "build", "status": "IN_PROGRESS", "conclusion": null},
                {"name": "test", "status": "QUEUED", "conclusion": null}
            ]
        }));
        match status.verdict {
            Verdict::Pending(desc) => assert_eq!(desc, "2 checks pending"),
            _ => panic!("expected pending"),
        }
    }

    #[test]
    fn blocked_collects_failed_checks_and_change_requests() {
        let status = status_from_json(&json!({
            "number": 3,
            "url": "https://example.com/pr/3",
            "statusCheckRollup": [
                {"name": "build", "status": "COMPLETED", "conclusion": "FAILURE"},
                {"name": "test", "status": "COMPLETED", "conclusion": "TIMED_OUT"},
                {"name": "lint", "status": "COMPLETED", "conclusion": "SUCCESS"}
            ],
            "latestReviews": [
                {"state": "CHANGES_REQUESTED", "author": {"login": "alice"}, "body": "add a test"},
                {"state": "APPROVED", "author": {"login": "bob"}, "body": "ok"}
            ]
        }));
        let Verdict::Blocked(failure) = status.verdict else {
            panic!("expected blocked");
        };
        assert_eq!(failure.checks, vec!["build — FAILURE", "test — TIMED_OUT"]);
        assert_eq!(failure.reviews, vec!["alice: add a test"]);
        let details = failure.details();
        assert!(details.contains("Failing checks:"));
        assert!(details.contains("FAILURE"));
        assert!(details.contains("alice: add a test"));
        assert!(!details.contains("bob"), "通过的意见不进报告");
    }

    #[test]
    fn missing_fields_do_not_panic() {
        let status = status_from_json(&json!({}));
        assert_eq!(status.number, 0);
        assert!(status.url.is_empty());
        assert!(matches!(status.verdict, Verdict::Green));
        assert!(Failure::default().details().is_empty());
    }
}
