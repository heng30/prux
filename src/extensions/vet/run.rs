//! vet 的执行层：跑检查命令、超时回收、输出截断与报告渲染。
//!
//! # 为什么是同步阻塞
//!
//! 边界钩子（[`crate::core::extensions::ExtensionHook::Boundary`]）是同步接口，而
//! 「跑测试」本来就是分钟级的阻塞操作，因此整批检查经 [`crate::utils::process::blocking`]
//! 包裹：多线程运行时让出 worker、单线程运行时与普通线程里直接执行。
//!
//! # 检查怎么跑
//!
//! 每条检查都以 [`shell_command`] 起一个子进程（unix `sh -c`、windows `cmd /C`），
//! 独立进程组，stdout / stderr 各起一个读线程用 [`drain_tail`] 持续排空（只保留尾部，
//! 避免大输出撑爆内存，也不会因为管道写满把子进程卡死）。主线程每 25ms 轮询一次退出状态，
//! 超时就 [`kill_process_tree`] 连同子进程一起杀掉并标记 `TimedOut`——超时的检查按失败处理，
//! 因为「跑不完」同样不能算通过。

use super::config::{Check, describe_check};
use crate::utils::{
    glyphs::{DEF_DONE, DEF_DOT_EMPTY, DEF_FAILED, DEF_MIDDOT, DEF_TRAILING_ELLIPSIS},
    process::{blocking, drain_tail, kill_process_tree, shell_command},
    time::format_duration_ms,
    truncate::tail_chars,
};
use std::{
    path::Path,
    process::Stdio,
    thread,
    time::{Duration, Instant},
};

/// 单条检查保留的输出尾部字符数（失败信息通常就在尾部）。
pub(super) const PER_CHECK_TAIL_CHARS: usize = 4000;

/// 注入模型时全部检查输出的总字符预算（按检查顺序累加，超出即截断）。
pub(super) const TOTAL_REPORT_CHARS: usize = 12000;

/// 读线程最多保留的原始字节数；超过后只保留尾部（滚动丢弃前缀）。
const MAX_CAPTURE_BYTES: usize = 64 * 1024;

/// 退出状态轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// 一条检查的执行结果。
#[derive(Debug, Clone)]
pub(super) struct CheckResult {
    /// 展示名（来自配置）。
    pub name: String,
    /// 命令原文（来自配置）。
    pub command: String,
    /// 失败是否允许收工。
    pub optional: bool,
    /// 结局（通过与各类失败）。
    pub status: Status,
    /// 进程退出码；无法启动 / 被信号杀死 / 超时为 `None`。
    pub exit_code: Option<i32>,
    /// 合并后的输出尾部（stdout 在前、stderr 在后）。
    pub output: String,
    /// 输出是否被截断（用于给模型一句说明）。
    pub truncated: bool,
    /// 实际耗时。
    pub duration: Duration,
}

/// 一条检查的结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Status {
    /// 退出码为 0。
    Passed,
    /// 退出码非 0。
    Failed,
    /// 超过配置的超时，被强制终止。
    TimedOut,
    /// 进程没能启动起来（shell 缺失等），附原因。
    Unavailable(String),
}

impl Status {
    /// 是否算作通过。
    pub(super) fn passed(&self) -> bool {
        matches!(self, Status::Passed)
    }
}

impl CheckResult {
    /// 是否算失败（非通过且不是「可选」）。
    pub(super) fn blocking_failure(&self) -> bool {
        !self.status.passed() && !self.optional
    }

    /// 结局的简写标签：`passed` / `exit 101` / `timeout`。
    pub(super) fn short_status(&self) -> String {
        match &self.status {
            Status::Passed => "passed".to_string(),
            Status::Failed => match self.exit_code {
                Some(code) => format!("exit {code}"),
                None => "failed".to_string(),
            },
            Status::TimedOut => "timeout".to_string(),
            Status::Unavailable(_) => "unavailable".to_string(),
        }
    }
}

/// 跑完一批检查，返回与输入同序的结果。
pub(super) fn run_all(cwd: &Path, checks: &[Check], timeout: Duration) -> Vec<CheckResult> {
    blocking(|| {
        checks
            .iter()
            .map(|check| run_one(cwd, check, timeout))
            .collect()
    })
}

/// 跑一条检查。
fn run_one(cwd: &Path, check: &Check, timeout: Duration) -> CheckResult {
    let started = Instant::now();
    let mut command = shell_command(&check.command);
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // 独立进程组：超时时连同检查命令自己拉起的子进程一起杀掉。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return CheckResult {
                name: check.name.clone(),
                command: check.command.clone(),
                optional: check.optional,
                status: Status::Unavailable(format!("could not start shell: {err}")),
                exit_code: None,
                output: String::new(),
                truncated: false,
                duration: started.elapsed(),
            };
        }
    };

    let stdout = child
        .stdout
        .take()
        .map(|s| thread::spawn(move || drain_tail(s, MAX_CAPTURE_BYTES)));
    let stderr = child
        .stderr
        .take()
        .map(|s| thread::spawn(move || drain_tail(s, MAX_CAPTURE_BYTES)));

    let mut timed_out = false;
    let mut exit_status = None;
    let deadline = started + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_status = Some(status);
                break;
            }
            Ok(None) => {}
            // 退出状态查询出错：当成已结束，退出码按拿不到处理
            Err(_) => break,
        }
        if Instant::now() >= deadline {
            timed_out = true;
            kill_process_tree(child.id());
            let _ = child.kill();
            exit_status = child.wait().ok();
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }

    let exit_code = exit_status.and_then(|s| s.code());
    let (stdout, stdout_cut) = stdout
        .and_then(|h| h.join().ok())
        .unwrap_or_else(|| (String::new(), false));
    let (stderr, stderr_cut) = stderr
        .and_then(|h| h.join().ok())
        .unwrap_or_else(|| (String::new(), false));

    let mut output = String::new();
    if !stdout.trim().is_empty() {
        output.push_str(stdout.trim_end());
    }
    if !stderr.trim().is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(stderr.trim_end());
    }

    let status = if timed_out {
        Status::TimedOut
    } else if exit_code == Some(0) {
        Status::Passed
    } else {
        Status::Failed
    };

    CheckResult {
        name: check.name.clone(),
        command: check.command.clone(),
        optional: check.optional,
        status,
        exit_code,
        output,
        truncated: stdout_cut || stderr_cut,
        duration: started.elapsed(),
    }
}

/// 是否有阻塞级失败（有 = 不能收工）。
pub(super) fn has_blocking_failure(results: &[CheckResult]) -> bool {
    results.iter().any(CheckResult::blocking_failure)
}

/// 失败集合的指纹：检查名 + 归一化输出。用于识别「同一条失败反复出现」，
/// 避免把同一条错误无限喂回去。
pub(super) fn failure_fingerprint(results: &[CheckResult]) -> Option<u64> {
    let failures: Vec<(String, String)> = results
        .iter()
        .filter(|r| r.blocking_failure())
        .map(|r| (r.name.clone(), normalize_output(&r.output)))
        .collect();
    if failures.is_empty() {
        return None;
    }
    let mut text = String::new();
    for (name, body) in &failures {
        text.push_str(name);
        text.push('\u{1}');
        text.push_str(body);
        text.push('\u{2}');
    }
    Some(crate::extensions::util::content_hash(&text))
}

/// 归一化输出以便比较：去掉行尾空白、把耗时/路径噪声压掉一点（只做保守处理）。
fn normalize_output(output: &str) -> String {
    output
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 一条检查的一行摘要：`✓ test · `cargo test` · passed · 12.4s`。
pub(super) fn result_line(result: &CheckResult) -> String {
    let mark = if result.status.passed() {
        DEF_DONE
    } else if result.optional {
        DEF_DOT_EMPTY
    } else {
        DEF_FAILED
    };
    let label = describe_check(&Check {
        name: result.name.clone(),
        command: result.command.clone(),
        optional: result.optional,
    });
    let mut line = format!(
        "{mark} {label} {DEF_MIDDOT} {} {DEF_MIDDOT} {}",
        result.short_status(),
        format_duration_ms(result.duration.as_millis() as u64),
    );
    if let Status::Unavailable(reason) = &result.status {
        line.push_str(&format!(" — {reason}"));
    }
    line
}

/// 给用户看的单行结论：`vet: 2 checks passed (15.5s)`。
pub(super) fn verdict(results: &[CheckResult]) -> String {
    if results.is_empty() {
        return "vet: no checks configured".to_string();
    }

    let failed: Vec<&CheckResult> = results.iter().filter(|r| !r.status.passed()).collect();
    let total_ms: u64 = results.iter().map(|r| r.duration.as_millis() as u64).sum();
    let elapsed = format_duration_ms(total_ms);

    if failed.is_empty() {
        return format!(
            "vet: {} check{} passed ({elapsed})",
            results.len(),
            if results.len() == 1 { "" } else { "s" }
        );
    }

    let blocking: Vec<&CheckResult> = failed
        .iter()
        .copied()
        .filter(|r| r.blocking_failure())
        .collect();
    let names: Vec<String> = failed
        .iter()
        .map(|r| format!("{} ({})", r.name, r.short_status()))
        .collect();

    let lead = if blocking.is_empty() {
        "optional checks failed"
    } else {
        "checks failed"
    };
    format!(
        "vet: {lead} — {} ({elapsed})",
        crate::extensions::util::truncate_chars(&names.join(", "), 200)
    )
}

/// 注入模型的失败报告（作为一条 user 消息落库）。
///
/// `iteration` 是从 1 开始的本轮续跑序号，`max` 是配置上限；两者只用于给模型上下文。
pub(super) fn failure_report(results: &[CheckResult], iteration: usize, max: usize) -> String {
    let mut out = format!(
        "[vet] project checks failed — fix them before finishing (attempt {iteration} of {max})\n"
    );

    let mut budget = TOTAL_REPORT_CHARS;
    for result in results.iter().filter(|r| !r.status.passed()) {
        out.push_str(&format!("\n{}\n", result_line(result)));
        if result.output.is_empty() {
            out.push_str("  (no output)\n");
            continue;
        }

        let body = tail_chars(&result.output, budget.min(PER_CHECK_TAIL_CHARS));
        budget = budget.saturating_sub(body.chars().count());
        if result.truncated || result.output.chars().count() > PER_CHECK_TAIL_CHARS {
            out.push_str(&format!(
                "  {DEF_TRAILING_ELLIPSIS} (earlier output omitted)\n"
            ));
        }
        for line in body.lines() {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
        if budget == 0 {
            out.push_str(&format!(
                "  {DEF_TRAILING_ELLIPSIS} (remaining checks omitted)\n"
            ));
            break;
        }
    }

    out.push_str(
        "\nvet re-runs these checks automatically when you finish, so do not run them yourself. \
         If a failure is clearly unrelated to your change, say so instead of editing unrelated code.\n",
    );
    out
}

/// `/vet run` 的多行明细：一行一条检查 + 失败输出尾部。
pub(super) fn detailed_report(results: &[CheckResult]) -> Vec<String> {
    let mut lines = vec![verdict(results)];
    for result in results {
        lines.push(format!("  {}", result_line(result)));
        if result.status.passed() || result.output.is_empty() {
            continue;
        }
        let body = tail_chars(&result.output, PER_CHECK_TAIL_CHARS.min(2000));
        if result.truncated {
            lines.push(format!(
                "    {DEF_TRAILING_ELLIPSIS} (earlier output omitted)"
            ));
        }
        for line in body.lines() {
            lines.push(format!("    {line}"));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一条结果。
    fn result(name: &str, status: Status, output: &str, optional: bool) -> CheckResult {
        CheckResult {
            name: name.to_string(),
            command: format!("cmd-{name}"),
            optional,
            status,
            exit_code: None,
            output: output.to_string(),
            truncated: false,
            duration: Duration::from_millis(1500),
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_one_captures_stdout_and_reports_success() {
        let dir = tempfile::tempdir().unwrap();
        let checks = vec![Check {
            name: "echo".to_string(),
            command: "echo hello-vet".to_string(),
            optional: false,
        }];
        let results = run_all(dir.path(), &checks, Duration::from_secs(10));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, Status::Passed);
        assert_eq!(results[0].exit_code, Some(0));
        assert!(results[0].output.contains("hello-vet"));
        assert!(!results[0].truncated);
    }

    #[cfg(unix)]
    #[test]
    fn run_one_reports_non_zero_exit_and_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let checks = vec![Check {
            name: "fail".to_string(),
            command: "echo boom >&2; exit 3".to_string(),
            optional: false,
        }];
        let results = run_all(dir.path(), &checks, Duration::from_secs(10));
        assert_eq!(results[0].status, Status::Failed);
        assert_eq!(results[0].exit_code, Some(3));
        assert!(results[0].output.contains("boom"));
    }

    #[cfg(unix)]
    #[test]
    fn run_one_kills_a_check_that_exceeds_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let checks = vec![Check {
            name: "slow".to_string(),
            command: "sleep 30".to_string(),
            optional: false,
        }];
        let started = Instant::now();
        let results = run_all(dir.path(), &checks, Duration::from_millis(300));
        assert_eq!(results[0].status, Status::TimedOut);
        // 必须真的提前返回（不是等满 30s）
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn run_one_keeps_only_the_output_tail() {
        let dir = tempfile::tempdir().unwrap();
        let checks = vec![Check {
            name: "noisy".to_string(),
            command: "seq 1 200000".to_string(),
            optional: false,
        }];
        let results = run_all(dir.path(), &checks, Duration::from_secs(30));
        assert_eq!(results[0].status, Status::Passed);
        assert!(results[0].truncated, "输出超限应标记截断");
        // 尾部保留最后几行
        assert!(results[0].output.contains("200000"));
    }

    #[test]
    fn failure_fingerprint_ignores_passing_checks_and_optional_failures() {
        let green = vec![result("a", Status::Passed, "ok", false)];
        assert_eq!(failure_fingerprint(&green), None);

        let optional = vec![result("a", Status::Failed, "boom", true)];
        assert_eq!(failure_fingerprint(&optional), None);

        let red = vec![result("a", Status::Failed, "boom", false)];
        let again = vec![result("a", Status::Failed, "boom\n", false)];
        assert_eq!(failure_fingerprint(&red), failure_fingerprint(&again));

        let different = vec![result("a", Status::Failed, "other", false)];
        assert_ne!(failure_fingerprint(&red), failure_fingerprint(&different));
    }

    #[test]
    fn verdict_summarizes_all_outcomes() {
        let green = vec![
            result("a", Status::Passed, "", false),
            result("b", Status::Passed, "", false),
        ];
        assert!(verdict(&green).contains("2 checks passed"));

        let red = vec![
            result("a", Status::Passed, "", false),
            result("b", Status::Failed, "boom", false),
        ];
        let text = verdict(&red);
        assert!(text.contains("checks failed"));
        assert!(text.contains("b"));

        let only_optional = vec![result("a", Status::Failed, "boom", true)];
        assert!(verdict(&only_optional).contains("optional checks failed"));

        assert_eq!(verdict(&[]), "vet: no checks configured");
    }

    #[test]
    fn failure_report_lists_failures_and_tells_the_model_not_to_rerun() {
        let results = vec![
            result("test", Status::Failed, "assertion failed\n  left: 1", false),
            result("lint", Status::Passed, "", false),
        ];
        let report = failure_report(&results, 1, 2);
        assert!(report.starts_with("[vet] project checks failed"));
        assert!(report.contains("attempt 1 of 2"));
        assert!(report.contains("assertion failed"));
        assert!(report.contains("do not run them yourself"));
        // 通过的检查不出现在失败报告里
        assert!(!report.contains("lint"));
    }

    #[test]
    fn failure_report_marks_truncated_output_and_keeps_the_tail() {
        let long = "x".repeat(PER_CHECK_TAIL_CHARS + 100);
        let results = vec![result("t", Status::Failed, &long, false)];
        let report = failure_report(&results, 2, 2);
        assert!(report.contains("earlier output omitted"));
        assert!(report.chars().count() < PER_CHECK_TAIL_CHARS + 2000);
    }
}
