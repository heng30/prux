//! `remote_exec` 的实现：跑一条远端命令，把结果整理成模型好读的文本。
//!
//! 输出按 [`Limits`] 与每轮预算双重截断：远端日志动辄几十万行，一次 `tail` 就能
//! 吃掉整轮上下文。截断时明确告诉模型「还有多少没显示」，而不是静默丢内容。

use crate::extensions::ssh_remote::transport::{self, RemoteConn};
use std::{sync::Arc, time::Duration};

/// 一次 `remote_exec` 的请求
pub struct ExecRequest {
    /// 要执行的远端命令
    pub command: String,
    /// 远端工作目录；`None` = 端点记住的默认目录
    pub cwd: Option<String>,
    /// 超时（秒）；`None` = 配置里的默认值
    pub timeout_secs: Option<u32>,
    /// 只保留最后多少行（等价于 `| tail -n N`，在远端执行，省带宽）
    pub tail_lines: Option<usize>,
}

/// 执行并把结果渲染成文本（含退出码与截断说明）
pub async fn run(conn: &Arc<RemoteConn>, req: &ExecRequest) -> Result<String, String> {
    let limits = transport::limits();
    let command = match req.tail_lines {
        Some(n) if n > 0 => format!("{} 2>&1 | tail -n {n}", req.command),
        _ => req.command.clone(),
    };
    let timeout = req.timeout_secs.map(|s| {
        Duration::from_secs(
            s.clamp(1, crate::extensions::ssh_remote::state::MAX_TIMEOUT_SECONDS) as u64,
        )
    });
    // 先按 exec 上限抓，还剩多少预算就只给多少，避免一轮里堆满远端输出
    let wanted = limits.exec_max_bytes.min(limits.turn_max_bytes);
    let granted = transport::take_turn_budget(wanted);
    if granted == 0 {
        return Ok(format!(
            "Output budget for this turn is exhausted; `{}` was not run. Narrow the next command \
             (grep/head) or start a new turn.",
            req.command
        ));
    }
    let out = transport::exec(conn, &command, req.cwd.as_deref(), timeout, granted).await?;

    let mut body = out.combined();
    let total_lines = body.lines().count();
    let mut notes: Vec<String> = Vec::new();
    if total_lines > limits.exec_max_lines {
        let skip = total_lines - limits.exec_max_lines;
        body = body.lines().skip(skip).collect::<Vec<_>>().join("\n");
        notes.push(format!(
            "kept the last {} of {total_lines} lines",
            limits.exec_max_lines
        ));
    }
    if out.truncated {
        notes.push(format!(
            "output hit the {} byte cap (the command was still running when it was cut)",
            granted
        ));
    }
    if body.trim().is_empty() {
        body = "(no output)".to_string();
    }
    let exit = match out.exit_code {
        Some(code) => format!("exit {code}"),
        None => "exit unknown (the command was cut off)".to_string(),
    };
    let mut header = format!(
        "{} · {}",
        conn.endpoint.display(),
        req.cwd.clone().unwrap_or_else(|| conn.endpoint.cwd.clone())
    );
    if !notes.is_empty() {
        header.push_str(" · ");
        header.push_str(&notes.join("; "));
    }
    Ok(format!(
        "{header}\n$ {}\n{}\n({exit})",
        req.command,
        body.trim_end()
    ))
}

#[cfg(test)]
mod tests {

    /// 超时参数会被夹到允许区间（0 与超大值都不能直接下发）
    #[test]
    fn timeout_is_clamped() {
        let clamp = |s: u32| s.clamp(1, crate::extensions::ssh_remote::state::MAX_TIMEOUT_SECONDS);
        assert_eq!(clamp(0), 1);
        assert_eq!(
            clamp(u32::MAX),
            crate::extensions::ssh_remote::state::MAX_TIMEOUT_SECONDS
        );
    }
}
