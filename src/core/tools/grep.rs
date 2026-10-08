//! grep 工具：基于 ripgrep --json 的流式搜索。

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
use serde_json::Value;
use std::{
    path::Path,
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
};

/// grep 未指定 limit 时默认返回的最大匹配条数。
const GREP_DEFAULT_LIMIT: usize = 100;
/// 单条匹配行截断的最大字符数，超出部分被省略。
const GREP_MAX_LINE_LENGTH: usize = 500;

/// 以 ripgrep 的 JSON 输出执行流式 grep 搜索（使用默认后端 `DEFAULT_TOOL_OPS`）。
///
/// `context` 是匹配行前后附带的上下文行数（≤0 视为 0），`limit` 是最大匹配条数（默认 100、至少 1）。
/// 搜索路径不存在或执行失败时返回 `ToolError`。
#[allow(clippy::too_many_arguments)]
pub async fn execute_grep(
    pattern: &str,
    path: Option<&str>,
    glob: Option<&str>,
    ignore_case: bool,
    literal: bool,
    context: Option<i64>,
    limit: Option<i64>,
    cwd: &str,
) -> Result<ToolResult, ToolError> {
    execute_grep_with_ops(
        pattern,
        path,
        glob,
        ignore_case,
        literal,
        context,
        limit,
        cwd,
        &DEFAULT_TOOL_OPS,
    )
    .await
}

/// 带后端注入的 grep
#[allow(clippy::too_many_arguments)]
pub async fn execute_grep_with_ops(
    pattern: &str,
    path: Option<&str>,
    glob: Option<&str>,
    ignore_case: bool,
    literal: bool,
    context: Option<i64>,
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
    let is_directory = ops.stat(&search).map(|s| s.is_dir).unwrap_or(false);
    let context_value = context.filter(|c| *c > 0).unwrap_or(0) as usize;
    let effective_limit = (limit.unwrap_or(GREP_DEFAULT_LIMIT as i64)).max(1) as usize;

    let mut args: Vec<String> = vec![
        "--json".into(),
        "--line-number".into(),
        "--color=never".into(),
        "--hidden".into(),
    ];
    if ignore_case {
        args.push("--ignore-case".into());
    }
    if literal {
        args.push("--fixed-strings".into());
    }
    if let Some(g) = glob {
        args.push("--glob".into());
        args.push(g.into());
    }
    args.push("--".into());
    args.push(pattern.into());
    args.push(search_path.to_string_lossy().to_string());

    let rg_bin =
        tools_manager::rg_path().ok_or_else(|| ToolError(tools_manager::install_hint("rg")))?;

    let mut child = match tokio::process::Command::new(&rg_bin)
        .args(&args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return Err(ToolError(format!("failed to run rg ({rg_bin}): {e}")));
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

    // 收集匹配（流式解析 JSON 行）
    let mut matches: Vec<(String, usize, String)> = Vec::new(); // (file, line, text)
    let mut match_count = 0usize;
    let mut match_limit_reached = false;

    // 达 limit 时 kill rg
    let kill_flag = Arc::new(AtomicBool::new(false));
    let kill_flag_parse = kill_flag.clone();

    let parse_task = tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let reader = tokio::io::BufReader::new(stdout);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            if match_count >= effective_limit {
                kill_flag_parse.store(true, Ordering::SeqCst);
                break;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("type").and_then(|t| t.as_str()) == Some("match") {
                match_count += 1;
                let file = v
                    .pointer("/data/path/text")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let line_num = v
                    .pointer("/data/line_number")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0) as usize;
                let line_text = v
                    .pointer("/data/lines/text")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if !file.is_empty() && line_num > 0 {
                    matches.push((file, line_num, line_text));
                }
                if match_count >= effective_limit {
                    match_limit_reached = true;
                    kill_flag_parse.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }
        (matches, match_limit_reached)
    });

    // limit 已达：立即杀掉 rg（否则大仓库上会跑到自然结束）
    if kill_flag.load(Ordering::SeqCst) {
        _ = child.kill().await;
    }

    let status = child.wait().await.ok();
    let (matches, match_limit_reached) = parse_task.await.unwrap_or_default();
    let stderr_text = stderr_task.await.unwrap_or_default();

    if let Some(status) = status {
        let code = status.code().unwrap_or(-1);
        if code != 0 && code != 1 && !match_limit_reached {
            let err_msg = if stderr_text.trim().is_empty() {
                format!("ripgrep exited with code {}", code)
            } else {
                stderr_text.trim().to_string()
            };
            return Err(ToolError(err_msg));
        }
    }

    if matches.is_empty() {
        return Ok(ToolResult::text("No matches found".to_string()));
    }

    // 格式化输出
    let mut output_lines: Vec<String> = Vec::new();
    let mut lines_truncated = false;
    if context_value == 0 {
        for (file, line_num, line_text) in &matches {
            let rel = format_grep_path(file, &search_path, is_directory);
            let sanitized = line_text
                .replace("\r\n", "\n")
                .replace('\r', "")
                .trim_end_matches('\n')
                .to_string();
            let (truncated, was) = truncate::truncate_line(&sanitized, GREP_MAX_LINE_LENGTH);
            if was {
                lines_truncated = true;
            }
            output_lines.push(format!("{}:{}: {}", rel, line_num, truncated));
        }
    } else {
        for (file, line_num, _) in &matches {
            let rel = format_grep_path(file, &search_path, is_directory);
            let file_lines = read_file_lines(file).await;
            if file_lines.is_empty() {
                output_lines.push(format!("{}:{}: (unable to read file)", rel, line_num));
                continue;
            }
            let start = line_num.saturating_sub(context_value).max(1);
            let end = (line_num + context_value).min(file_lines.len());
            for current in start..=end {
                let line_text = file_lines.get(current - 1).cloned().unwrap_or_default();
                let sanitized = line_text.replace('\r', "");
                let (truncated, was) = truncate::truncate_line(&sanitized, GREP_MAX_LINE_LENGTH);
                if was {
                    lines_truncated = true;
                }
                if current == *line_num {
                    output_lines.push(format!("{}:{}: {}", rel, current, truncated));
                } else {
                    output_lines.push(format!("{}-{}- {}", rel, current, truncated));
                }
            }
        }
    }

    let raw_output = output_lines.join("\n");
    let truncation = truncate::truncate_head(&raw_output, (Some(usize::MAX), None));
    let mut output = truncation.content;
    let mut notices: Vec<String> = Vec::new();
    if match_limit_reached {
        notices.push(format!(
            "{} matches limit reached. Use limit={} for more, or refine pattern",
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
    if lines_truncated {
        notices.push(format!(
            "Some lines truncated to {} chars. Use read tool to see full lines",
            GREP_MAX_LINE_LENGTH
        ));
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok(ToolResult::text(output))
}

/// 读取文本文件并按行拆分（统一为 LF、去掉 CR）；读取失败时返回空 `Vec`。
async fn read_file_lines(file_path: &str) -> Vec<String> {
    match tokio::fs::read_to_string(file_path).await {
        Ok(content) => content
            .replace("\r\n", "\n")
            .replace('\r', "")
            .split('\n')
            .map(|s| s.to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// 输出路径：目录搜索时相对路径，否则 basename
pub(crate) fn format_grep_path(file_path: &str, search_path: &Path, is_directory: bool) -> String {
    if is_directory {
        let rel = relativize_path(file_path, search_path);
        if rel != file_path && !rel.starts_with("..") {
            return rel;
        }
    }
    Path::new(file_path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| file_path.to_string())
}
