//! find 工具：基于 fd 的 glob 搜索。

use crate::{
    core::{
        tools::{
            index::{ToolError, ToolResult, relativize_path, resolve_tool_path},
            operations::{DEFAULT_TOOL_OPS, ToolOperations},
        },
        tools_manager,
    },
    utils::truncate::{self, DEFAULT_MAX_BYTES},
};

/// find 未显式传 limit 时的默认返回条数上限。
const FIND_DEFAULT_LIMIT: usize = 1000;

/// 用默认后端按 glob 模式搜索文件，`limit` 为返回条数上限（None 时用默认 1000）。
pub async fn execute_find(
    pattern: &str,
    path: Option<&str>,
    limit: Option<i64>,
    cwd: &str,
) -> Result<ToolResult, ToolError> {
    execute_find_with_ops(pattern, path, limit, cwd, &DEFAULT_TOOL_OPS).await
}

/// 带后端注入的 find
pub async fn execute_find_with_ops(
    pattern: &str,
    path: Option<&str>,
    limit: Option<i64>,
    cwd: &str,
    ops: &dyn ToolOperations,
) -> Result<ToolResult, ToolError> {
    let search_path = resolve_tool_path(path.unwrap_or("."), cwd);
    let search = search_path.to_string_lossy().to_string();
    if !ops.exists(&search) {
        return Err(ToolError(format!(
            "Path not found: {}",
            search_path.display()
        )));
    }
    let effective_limit = limit.unwrap_or(FIND_DEFAULT_LIMIT as i64).max(1) as usize;

    let mut args: Vec<String> = vec!["--glob".into(), "--color=never".into(), "--hidden".into()];

    // git 仓库检测
    let mut inside_git = false;
    let mut cur = search_path.clone();
    loop {
        if ops.exists(cur.join(".git").to_string_lossy().as_ref()) {
            inside_git = true;
            break;
        }
        match cur.parent() {
            Some(p) if p != cur => cur = p.to_path_buf(),
            _ => break,
        }
    }
    if !inside_git {
        args.push("--no-require-git".into());
    }
    args.push("--max-results".into());
    args.push(effective_limit.to_string());

    let mut effective_pattern = pattern.to_string();
    if pattern.contains('/') {
        args.push("--full-path".into());
        if !pattern.starts_with('/') && !pattern.starts_with("**/") && pattern != "**" {
            effective_pattern = format!("**/{}", pattern);
        }
    }
    args.push("--".into());
    args.push(effective_pattern);
    args.push(search_path.to_string_lossy().to_string());

    let fd_bin =
        tools_manager::fd_path().ok_or_else(|| ToolError(tools_manager::install_hint("fd")))?;

    let mut child = match tokio::process::Command::new(&fd_bin)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return Err(ToolError(format!("failed to run fd ({fd_bin}): {e}")));
        }
    };
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut r = stderr;
        let _ = r.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).to_string()
    });
    let stdout_task = tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let reader = tokio::io::BufReader::new(stdout);
        let mut lines = reader.lines();
        let mut out = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            out.push(line);
        }
        out
    });

    let status = child.wait().await.ok();
    let lines = stdout_task.await.unwrap_or_default();
    let stderr_text = stderr_task.await.unwrap_or_default();
    let code = status.and_then(|s| s.code());

    let relativized: Vec<String> = lines
        .iter()
        .map(|l| l.trim_end_matches('\r').trim().to_string())
        .filter(|l| !l.is_empty())
        .map(|l| relativize_path(&l, &search_path))
        .collect();

    if let Some(code) = code
        && code != 0
    {
        let err_msg = if stderr_text.trim().is_empty() {
            format!("fd exited with code {}", code)
        } else {
            stderr_text.trim().to_string()
        };
        if relativized.is_empty() {
            return Err(ToolError(err_msg));
        }
    }
    if relativized.is_empty() {
        return Ok(ToolResult::text(
            "No files found matching pattern".to_string(),
        ));
    }

    let result_limit_reached = relativized.len() >= effective_limit;
    let raw_output = relativized.join("\n");
    let truncation = truncate::truncate_head(&raw_output, (Some(usize::MAX), None));
    let mut result_output = truncation.content;
    let mut notices: Vec<String> = Vec::new();
    if result_limit_reached {
        notices.push(format!(
            "{} results limit reached. Use limit={} for more, or refine pattern",
            effective_limit,
            effective_limit * 2
        ));
    }
    if truncation.truncated {
        notices.push(format!(
            "{} limit reached",
            truncate::format_size(DEFAULT_MAX_BYTES)
        ));
    }
    if !notices.is_empty() {
        result_output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok(ToolResult::text(result_output))
}
