//! powershell 工具（仅 Windows 启用）。

use crate::{
    core::tools::{
        OnChunkCallback,
        bash::{
            BashOutcome, BashOutputCollector, BashStructuredOutput, STRUCTURED_OUTPUT_MAX_BYTES,
        },
        index::{ToolError, ToolResult},
    },
    utils::truncate::{self, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES},
};
use serde_json::json;
use std::{
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, process::Command};

/// powershell 解析顺序（pwsh 优先，回退 powershell）
pub fn find_powershell() -> Option<PathBuf> {
    for name in ["pwsh", "powershell"] {
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                let candidate = dir.join(if cfg!(windows) {
                    format!("{}.exe", name)
                } else {
                    name.to_string()
                });

                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// 执行 powershell 命令（Windows 语义：-NoProfile -NonInteractive -Command）
pub async fn execute_powershell(
    command: &str,
    timeout: Option<f64>,
    cwd: &str,
    on_chunk: Option<OnChunkCallback>,
) -> BashOutcome {
    let fail = |text: String| BashOutcome {
        text,
        exit_code: None,
        cancelled: false,
        timed_out: false,
        truncated: false,
        full_output_path: None,
        structured: None,
    };

    let ps = match find_powershell() {
        Some(p) => p,
        None => {
            return fail(
                "powershell executable not found: searched $PATH for pwsh and powershell."
                    .to_string(),
            );
        }
    };

    let cwd_path = PathBuf::from(cwd);
    if !cwd_path.exists() {
        return fail(format!(
            "Working directory does not exist: {}\nCannot execute powershell commands.",
            cwd
        ));
    }

    let mut cmd = Command::new(ps);
    cmd.args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(command)
        .current_dir(&cwd_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return fail(format!("Failed to execute command: {}", e)),
    };
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut collector = BashOutputCollector::new();
    let started_at = Instant::now();

    let collect = async {
        let mut bufs: Vec<Box<dyn tokio::io::AsyncRead + Unpin + Send>> = Vec::new();
        if let Some(o) = stdout.take() {
            bufs.push(Box::new(o));
        }
        if let Some(e) = stderr.take() {
            bufs.push(Box::new(e));
        }

        // 每个输出流单独一个读取任务（与 bash 同理：轮流读会在 stdout 写满管道、
        // stderr 无数据但未关闭时互等死锁），经 channel 汇总回本任务顺序收集。
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        for mut b in bufs {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                loop {
                    match b.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            let text = String::from_utf8_lossy(&buf[..n]).to_string();
                            if tx.send(text).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
        drop(tx);

        while let Some(text) = rx.recv().await {
            if let Some(cb) = &on_chunk {
                cb.lock().await(&text);
            }

            collector.push(&text);
        }
    };

    let mut timed_out = false;
    let status = match timeout {
        Some(t) => match tokio::time::timeout(Duration::from_secs_f64(t), child.wait()).await {
            Ok(s) => s.ok(),
            Err(_) => {
                timed_out = true;
                _ = child.kill().await;
                _ = child.wait().await;
                None
            }
        },
        None => child.wait().await.ok(),
    };
    collect.await;
    let exit_code = status.and_then(|s| s.code());

    // 输出收集：末段 + 截断（复用 bash 收集器的构建逻辑）
    let full = collector
        .full_output
        .as_ref()
        .map(|f| f.path().to_string_lossy().to_string());
    let total_lines = collector.completed_lines + if collector.has_open_line { 1 } else { 0 };
    let truncated = total_lines > DEFAULT_MAX_LINES || collector.total_bytes > DEFAULT_MAX_BYTES;
    let mut output_text = if truncated {
        let tail = truncate::truncate_tail(
            &collector.tail,
            (Some(DEFAULT_MAX_LINES), Some(DEFAULT_MAX_BYTES)),
        );
        let mut t = tail.content;
        t.push_str(&format!(
            "\n\n[Output truncated: showing last {} lines / {}KB]",
            DEFAULT_MAX_LINES,
            DEFAULT_MAX_BYTES / 1024
        ));
        t
    } else {
        collector.tail.clone()
    };

    let cancelled = false;
    if timed_out {
        output_text = format!(
            "{}\n\nCommand timed out after {} seconds",
            output_text,
            timeout.unwrap_or(0.0) as i64
        );
    } else if let Some(code) = exit_code.filter(|c| *c != 0) {
        output_text = format!("{}\n\nCommand exited with code {}", output_text, code);
    }

    if output_text.is_empty() {
        output_text = "(no output)".to_string();
    }

    let wall_time_seconds = (started_at.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    let structured = exit_code.map(|code| {
        let (full, over_limit) = collector.read_full_output(STRUCTURED_OUTPUT_MAX_BYTES);
        json!(BashStructuredOutput {
            output: full.clone(),
            truncated: over_limit,
            full_output_path: if over_limit { Some(full) } else { None },
            exit_code: code,
            wall_time_seconds,
        })
    });

    BashOutcome {
        text: output_text,
        exit_code,
        cancelled,
        timed_out,
        truncated,
        full_output_path: full,
        structured,
    }
}

/// execute_powershell 的 ToolResult 包装（非零退出/超时 → 错误结果）
pub async fn execute_powershell_result(
    command: &str,
    timeout: Option<f64>,
    cwd: &str,
    on_chunk: Option<OnChunkCallback>,
) -> Result<ToolResult, ToolError> {
    let out = execute_powershell(command, timeout, cwd, on_chunk).await;
    if out.cancelled || out.timed_out {
        return Err(ToolError(out.text));
    }
    let mut result = if out.exit_code.is_some_and(|c| c != 0) {
        ToolResult::error(out.text)
    } else {
        ToolResult::text(out.text)
    };
    result.structured_content = out.structured;
    Ok(result)
}
