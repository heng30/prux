//! write 工具

use crate::core::tools::index::{ToolError, ToolResult, resolve_tool_path};

/// 将 `content` 写入 `path`（相对路径按 `cwd` 解析），必要时创建缺失的父目录；
/// 写入失败时返回错误。
pub async fn execute_write(path: &str, content: &str, cwd: &str) -> Result<ToolResult, ToolError> {
    let absolute_path = resolve_tool_path(path, cwd);
    if let Some(parent) = absolute_path.parent()
        && !parent.as_os_str().is_empty()
    {
        _ = std::fs::create_dir_all(parent);
    }

    match std::fs::write(&absolute_path, content) {
        Ok(_) => Ok(ToolResult::text(format!("Successfully wrote to {}", path))),
        Err(e) => Err(ToolError(format!(
            "Could not write file: {}. Error: {}",
            path, e
        ))),
    }
}
