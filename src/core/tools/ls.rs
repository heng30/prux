//! ls 工具：目录列表。

use crate::{
    core::tools::{
        index::{ToolError, ToolResult, resolve_tool_path},
        operations::{DEFAULT_TOOL_OPS, ToolOperations},
    },
    utils::truncate::{self, DEFAULT_MAX_BYTES},
};

/// ls 未显式传 limit 时的默认条目数上限。
const LS_DEFAULT_LIMIT: usize = 500;

/// 用默认后端列出目录条目，`limit` 为条目数上限（None 时用默认 500）。
pub async fn execute_ls(
    path: Option<&str>,
    limit: Option<i64>,
    cwd: &str,
) -> Result<ToolResult, ToolError> {
    execute_ls_with_ops(path, limit, cwd, &DEFAULT_TOOL_OPS).await
}

/// 带后端注入的 ls
pub async fn execute_ls_with_ops(
    path: Option<&str>,
    limit: Option<i64>,
    cwd: &str,
    ops: &dyn ToolOperations,
) -> Result<ToolResult, ToolError> {
    let dir_path = resolve_tool_path(path.unwrap_or("."), cwd);
    let dir = dir_path.to_string_lossy().to_string();
    if !ops.exists(&dir) {
        return Err(ToolError(format!("Path not found: {}", dir_path.display())));
    }
    if !ops.stat(&dir).map(|s| s.is_dir).unwrap_or(false) {
        return Err(ToolError(format!(
            "Not a directory: {}",
            dir_path.display()
        )));
    }
    let effective_limit = limit.unwrap_or(LS_DEFAULT_LIMIT as i64).max(1) as usize;

    let mut entries: Vec<String> = match ops.read_dir(&dir) {
        Ok(v) => v,
        Err(e) => return Err(ToolError(format!("Cannot read directory: {}", e))),
    };

    // 小写排序
    entries.sort_by_key(|a| a.to_lowercase());

    let mut results: Vec<String> = Vec::new();
    let mut entry_limit_reached = false;
    for entry in entries {
        if results.len() >= effective_limit {
            entry_limit_reached = true;
            break;
        }
        let full_path = dir_path.join(&entry);
        let full = full_path.to_string_lossy().to_string();
        let suffix = match ops.stat(&full) {
            Some(m) if m.is_dir => "/",
            _ => "",
        };
        results.push(format!("{}{}", entry, suffix));
    }

    if results.is_empty() {
        return Ok(ToolResult::text("(empty directory)".to_string()));
    }

    let raw_output = results.join("\n");
    let truncation = truncate::truncate_head(&raw_output, (Some(usize::MAX), None));
    let mut output = truncation.content;
    let mut notices: Vec<String> = Vec::new();
    if entry_limit_reached {
        notices.push(format!(
            "{} entries limit reached. Use limit={} for more",
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
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok(ToolResult::text(output))
}
