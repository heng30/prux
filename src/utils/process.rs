//! 进程辅助：执行外部命令时的平台差异、输出收集与回收。
//!
//! 六件事：
//! - [`shell_command`]：按平台构造「执行一条命令字符串」的 shell 调用；
//! - [`run_stdout`]：跑一条现成的命令并取 stdout，失败时把 stderr 带进错误；
//! - [`drain_tail`]：持续读空管道而只保留尾部（限量，且不会因为管道写满把子进程卡死）；
//! - [`kill_process_tree`]：连同子进程一起强杀（Unix 进程组 / Windows `taskkill /T`）；
//! - [`blocking`]：在同步接口里安全地等一个阻塞动作；
//! - [`spawn_blocking`]：`async fn` 里把阻塞动作丢到阻塞线程池。
//!
//! 这些是「跑一条用户配置的命令」所需的最小公共面：内置 `bash` 工具（异步流式）与
//! 需要同步等待结果的扩展（如 `vet`）共用同一套进程回收语义，避免各写一份。

use std::{
    io::Read,
    process::{Command, Stdio},
};

/// 按平台构造执行一条命令字符串的 shell 调用。
///
/// Unix 用 `sh -c`，Windows 用 `cmd /C`——两者都是该平台上必然存在的解释器。
/// 刻意不复用内置 `bash` 工具的 shell 选择（`shellPath` / `/bin/bash` 兜底）：那是给
/// **模型**执行命令用的，这里的命令来自用户配置，只需要「能跑起来」的最小保证。
pub fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", command]);
        cmd
    }

    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", command]);
        cmd
    }
}

/// 持续读空管道，只保留尾部 `limit` 字节。返回 `(文本, 是否丢弃过前缀)`。
///
/// 必须持续读到 EOF：只要不读，子进程写满管道就会阻塞，表现为「命令卡住」而不是
/// 「输出太多被截断」。因此这里的取舍是**读全部、留尾部**，超过 `2 * limit` 时滚动
/// 丢弃前缀（摊还 O(n)，不必每读一次就搬动整个缓冲）。
pub fn drain_tail(mut reader: impl Read, limit: usize) -> (String, bool) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;

    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > limit * 2 {
                    let drop = buf.len() - limit;
                    buf.drain(..drop);
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }

    if buf.len() > limit {
        let drop = buf.len() - limit;
        buf.drain(..drop);
        truncated = true;
    }

    let text = String::from_utf8_lossy(&buf).to_string();
    (text, truncated)
}

/// 跑一条命令并取它的 stdout；失败时把 stderr 带进错误。
///
/// `cmd` 的 stdin 强制接到 null、stdout/stderr 收成管道（调用方预先设好的重定向会被覆盖）。
/// `what` 只用于拼错误前缀，写“在做什么”（如 `swaymsg -t get_tree`）而不是命令本身。
/// 非零退出码返回错误，带状态码与 stderr 末段（stderr 为空时给一句可读说明）；
/// 进程起不来（可执行文件不在 PATH 上等）也返回错误。
pub fn run_stdout(mut cmd: Command, what: &str) -> Result<Vec<u8>, String> {
    let output = cmd
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .map_err(|e| format!("{what}: {e}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "{what} failed ({}): {}",
            output.status,
            if err.is_empty() { "no stderr" } else { &err }
        ));
    }
    Ok(output.stdout)
}

/// 强杀进程树：Unix 用 `killpg`（子进程需以 `process_group(0)` 起），
/// Windows 用 `taskkill /F /T` 连同子进程一起终止，其它平台只杀直接子进程。
///
/// 调用方仍应同时 `Child::kill()`：本函数按 pid 定位进程组，若子进程已经退出、
/// pid 被复用，单靠它可能落空。
pub fn kill_process_tree(pid: u32) {
    #[cfg(unix)]
    {
        _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }

    #[cfg(windows)]
    {
        _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
    }
}

/// 在当前运行时里安全地执行阻塞工作。
///
/// 同步接口（如扩展的边界钩子）里等一个分钟级的子进程时，必须让出 tokio 的 worker：
/// 多线程运行时走 `block_in_place`（运行时另起线程接管调度），单线程运行时直接执行
/// （那里没有别的任务需要调度），不在运行时内（普通线程 / 单元测试）同样直接执行。
pub fn blocking<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(f)
        }
        _ => f(),
    }
}

/// 在 `async fn` 里把阻塞动作丢到 tokio 阻塞线程池并等它结束。
///
/// 与同步版 [`blocking`] 的分工：本函数给「有 `await` 可以交还控制权」的调用方（截图、
/// 输入注入、剪贴板命令等秒级动作），[`blocking`] 给「本来就在同步接口里」的调用方。
/// 任务 panic 或被取消时把 [`tokio::task::JoinError`] 原样交回，文案由调用方决定——
/// `utils` 不依赖 `core`，这里不能拼 `ToolError`。
pub async fn spawn_blocking<T, F>(f: F) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn drain_tail_keeps_everything_below_the_limit() {
        let (text, truncated) = drain_tail(Cursor::new(b"hello".to_vec()), 64);
        assert_eq!(text, "hello");
        assert!(!truncated);
    }

    #[test]
    fn drain_tail_keeps_only_the_tail_and_flags_it() {
        let input = "x".repeat(1000);
        let (text, truncated) = drain_tail(Cursor::new(input.into_bytes()), 100);
        assert!(truncated);
        assert_eq!(text.chars().count(), 100);
    }

    #[test]
    fn drain_tail_rolls_over_multiple_rounds_of_output() {
        // 远大于 2*limit：验证滚动丢弃（而不是只裁一次）
        let (text, truncated) = drain_tail(Cursor::new(vec![b'a'; 100_000]), 128);
        assert!(truncated);
        assert_eq!(text.len(), 128);
    }

    #[cfg(unix)]
    #[test]
    fn shell_command_runs_the_command_string() {
        let output = shell_command("echo shell-ok").output().unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("shell-ok"));
    }

    #[cfg(unix)]
    #[test]
    fn kill_process_tree_terminates_a_group_quickly() {
        use std::{
            os::unix::process::CommandExt as _,
            process::Stdio,
            time::{Duration, Instant},
        };

        let mut cmd = shell_command("sleep 30 & wait");
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        cmd.process_group(0);
        let mut child = cmd.spawn().unwrap();

        let pid = child.id();
        let started = Instant::now();
        kill_process_tree(pid);
        _ = child.kill();
        _ = child.wait();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "进程组应被强杀，而不是等满 30s"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_stdout_returns_stdout_on_success() {
        let out = run_stdout(shell_command("echo run-ok"), "echo").unwrap();
        assert!(String::from_utf8_lossy(&out).contains("run-ok"));
    }

    #[cfg(unix)]
    #[test]
    fn run_stdout_reports_stderr_on_failure() {
        let err = run_stdout(shell_command("echo boom >&2; exit 3"), "boom").unwrap_err();
        assert!(err.contains("boom failed"), "{err}");
        assert!(err.contains("boom"), "{err}");
    }

    #[test]
    fn blocking_runs_without_a_runtime() {
        assert_eq!(blocking(|| 41 + 1), 42);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_runs_inside_a_multi_thread_runtime() {
        assert_eq!(blocking(|| 6 * 7), 42);
    }
}
