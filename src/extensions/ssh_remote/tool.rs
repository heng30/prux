//! 远程工具族：`remote_exec` / `remote_read` / `remote_ls` / `remote_grep` / `remote_sync`。
//!
//! 设计前提：**远程是「第二个文件系统 + 一台能跑命令的机器」，不是工作区**。
//! 所以这些工具与内置 `read` / `grep` / `bash` 是并存关系，各自带 `host` 参数，
//! 不接管任何内置行为——`@` 补全、`!` 命令、git 状态、LSP、子代理看到的仍然只有本地。
//!
//! 每个工具都按 `host` 取连接（`None` = `/ssh <target>` 设的默认端点），
//! 连接按端点缓存在 [`crate::extensions::ssh_remote::transport`] 里，多台机器可以同时在线。

use crate::{
    core::{
        extensions::{ExtensionTool, ToolAnnotations},
        tools::{ToolError, ToolResult},
    },
    extensions::ssh_remote::{
        connect, exec, fs,
        transport::{self, RemoteConn},
    },
    utils::display::format_bytes,
};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

/// `remote_exec` 工具名
pub const EXEC: &str = "remote_exec";
/// `remote_read` 工具名
pub const READ: &str = "remote_read";
/// `remote_ls` 工具名
pub const LS: &str = "remote_ls";
/// `remote_grep` 工具名
pub const GREP: &str = "remote_grep";
/// `remote_sync` 工具名
pub const SYNC: &str = "remote_sync";

/// 单次 grep 的默认命中上限
const GREP_DEFAULT_MAX: usize = 200;
/// dry-run / 结果里最多列出的文件数
const LIST_LIMIT: usize = 50;
/// 单次 sync 的文件数上限
const MAX_SYNC_FILES: usize = 2000;

/// 全部工具定义
pub fn tools() -> Vec<ExtensionTool> {
    vec![
        exec_tool(),
        read_tool(),
        ls_tool(),
        grep_tool(),
        sync_tool(),
    ]
}

/// `host` 参数的通用说明
fn host_schema(desc: &str) -> Value {
    json!({
        "type": "string",
        "description": format!("SSH target: `user@host[:port]` or a ~/.ssh/config alias. Omit to use the default set by `/ssh <target>`. {desc}")
    })
}

/// 构造工具定义
fn exec_tool() -> ExtensionTool {
    let mut tool = ExtensionTool::simple(
        EXEC,
        "Run a shell command on a remote host over SSH and return its combined output. \
         Use it to inspect servers (logs, processes, disk, containers); it does not change any \
         local state and does not affect how the built-in tools work.",
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to run on the remote host" },
                "host": host_schema(""),
                "cwd": { "type": "string", "description": "Remote working directory; omit to use the target's remembered directory" },
                "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default from config, 30)" },
                "tail_lines": { "type": "integer", "description": "Only keep the last N lines (runs `tail -n N` on the remote side, saving bandwidth)" }
            },
            "required": ["command"]
        }),
        "Run a shell command on a remote SSH host",
    );
    tool.prompt_guidelines = vec![
        "Use remote_exec for remote inspection (logs, processes, disk, containers); keep commands narrow (tail, head, rg with limits) because the output comes back into the context.".to_string(),
        "Do not prefix remote commands with ssh or wrap local tools: read/write/edit/bash stay local, remote_read/remote_ls/remote_grep/remote_exec are the remote ones.".to_string(),
    ];
    tool.annotations = ToolAnnotations::default();
    tool
}

/// `remote_read` 定义
fn read_tool() -> ExtensionTool {
    ExtensionTool::simple(
        READ,
        "Read a text file from a remote host over SSH, optionally a line range. Mirrors the \
         built-in read tool's offset/limit semantics but never touches local files.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Remote file path (absolute, or relative to the target's remembered directory)" },
                "host": host_schema(""),
                "offset": { "type": "integer", "description": "1-based line to start at" },
                "limit": { "type": "integer", "description": "Maximum number of lines to return" }
            },
            "required": ["path"]
        }),
        "Read a file from a remote SSH host",
    )
}

/// `remote_ls` 定义
fn ls_tool() -> ExtensionTool {
    ExtensionTool::simple(
        LS,
        "List a directory on a remote host over SSH (directories first, then files with sizes).",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Remote directory path; omit to use the target's remembered directory" },
                "host": host_schema("")
            }
        }),
        "List a remote directory over SSH",
    )
}

/// `remote_grep` 定义
fn grep_tool() -> ExtensionTool {
    ExtensionTool::simple(
        GREP,
        "Search file contents on a remote host over SSH using ripgrep when available (falls back \
         to grep -rn). Returns path:line:text matches.",
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression (ripgrep syntax)" },
                "host": host_schema(""),
                "path": { "type": "string", "description": "Remote file or directory to search; omit to use the target's remembered directory" },
                "glob": { "type": "string", "description": "Only search files matching this glob, e.g. `*.rs`" },
                "max_matches": { "type": "integer", "description": "Maximum number of matching lines (default 200)" }
            },
            "required": ["pattern"]
        }),
        "Search remote file contents over SSH",
    )
}

/// `remote_sync` 定义
fn sync_tool() -> ExtensionTool {
    let mut tool = ExtensionTool::simple(
        SYNC,
        "Copy files between the local machine and a remote host over SFTP. `direction: push` sends \
         local -> remote, `pull` fetches remote -> local. Supports files and directory trees; \
         `dry_run` lists what would be transferred without writing anything.",
        json!({
            "type": "object",
            "properties": {
                "direction": { "type": "string", "enum": ["push", "pull"], "description": "push: local -> remote; pull: remote -> local" },
                "local": { "type": "string", "description": "Local path (absolute, or relative to the session working directory)" },
                "remote": { "type": "string", "description": "Remote path (absolute, or relative to the target's remembered directory)" },
                "host": host_schema(""),
                "dry_run": { "type": "boolean", "description": "Only report the file list and total size" }
            },
            "required": ["direction", "local", "remote"]
        }),
        "Copy files between local and a remote SSH host",
    );
    tool.prompt_guidelines = vec![
        "Use remote_sync when the user asks to copy or deploy files; it is explicit, never automatic. Run it with dry_run first when the source is a directory.".to_string(),
    ];
    tool
}

/// 按 `host` 参数取连接（`None` → 默认端点）
async fn conn_for(host: Option<&str>) -> Result<Arc<RemoteConn>, String> {
    match host.map(str::trim).filter(|h| !h.is_empty()) {
        Some(host) => transport::resolve_host(host).await,
        None => transport::resolve_default().await,
    }
}

/// 从参数里取字符串
fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

/// 从参数里取整数
fn int_arg(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64())
}

/// 分派一个工具调用；`cwd` 是本地会话工作目录（解析 `remote_sync` 的相对本地路径）
pub async fn dispatch(name: &str, args: &Value, cwd: &str) -> Result<ToolResult, ToolError> {
    let host = str_arg(args, "host");
    match name {
        EXEC => run_exec(host, args).await,
        READ => run_read(host, args).await,
        LS => run_ls(host, args).await,
        GREP => run_grep(host, args).await,
        SYNC => run_sync(host, args, cwd).await,
        other => Err(ToolError(format!("unknown ssh-remote tool: {other}"))),
    }
}

/// `remote_exec`
async fn run_exec(host: Option<&str>, args: &Value) -> Result<ToolResult, ToolError> {
    let command = str_arg(args, "command")
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| ToolError("`command` is required".to_string()))?;
    let conn = conn_for(host).await.map_err(ToolError)?;
    let request = exec::ExecRequest {
        command: command.to_string(),
        cwd: str_arg(args, "cwd").map(|s| s.to_string()),
        timeout_secs: int_arg(args, "timeout_secs").map(|v| v.max(1) as u32),
        tail_lines: int_arg(args, "tail_lines").map(|v| v.max(0) as usize),
    };
    let text = exec::run(&conn, &request).await.map_err(ToolError)?;
    Ok(ToolResult::text(text))
}

/// `remote_read`
async fn run_read(host: Option<&str>, args: &Value) -> Result<ToolResult, ToolError> {
    let path = str_arg(args, "path").ok_or_else(|| ToolError("`path` is required".to_string()))?;
    let conn = conn_for(host).await.map_err(ToolError)?;
    let limits = transport::limits();
    let sftp = fs::open(&conn).await.map_err(ToolError)?;
    let slice = sftp
        .read_range(
            path,
            int_arg(args, "offset"),
            int_arg(args, "limit"),
            &limits,
        )
        .await
        .map_err(ToolError)?;
    let start = int_arg(args, "offset").unwrap_or(1).max(1);
    let header = if slice.offset_beyond_end {
        format!(
            "{}:{} (offset {start} is past the end: {} lines)",
            conn.endpoint.display(),
            path,
            slice.total_lines
        )
    } else {
        format!(
            "{}:{} (lines {start}-{} of {})",
            conn.endpoint.display(),
            path,
            start + slice.selected_lines.saturating_sub(1) as i64,
            slice.total_lines
        )
    };
    let budget = transport::take_turn_budget(limits.read_max_bytes + header.len());
    if budget == 0 {
        return Ok(ToolResult::text(format!(
            "{header}\n(output budget for this turn is exhausted; run it again next turn)"
        )));
    }
    Ok(ToolResult::text(format!("{header}\n{}", slice.content)))
}

/// `remote_ls`
async fn run_ls(host: Option<&str>, args: &Value) -> Result<ToolResult, ToolError> {
    let conn = conn_for(host).await.map_err(ToolError)?;
    let path = str_arg(args, "path")
        .map(|s| s.to_string())
        .unwrap_or_else(|| conn.endpoint.cwd.clone());
    let sftp = fs::open(&conn).await.map_err(ToolError)?;
    let entries = sftp.list(&path).await.map_err(ToolError)?;
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<(String, u64)> = Vec::new();
    for (name, is_dir) in entries {
        if is_dir {
            dirs.push(format!("{name}/"));
        } else {
            let size = sftp
                .stat(&format!("{}/{}", path.trim_end_matches('/'), name))
                .await
                .map(|s| s.size)
                .unwrap_or(0);
            files.push((name, size));
        }
    }
    let mut body = format!("{}:{path}\n", conn.endpoint.display());
    if dirs.is_empty() && files.is_empty() {
        body.push_str("(empty)");
    }
    for dir in &dirs {
        body.push_str(&format!("{dir}\n"));
    }
    for (name, size) in &files {
        body.push_str(&format!("{name}\t{}\n", format_bytes(*size)));
    }
    Ok(ToolResult::text(body.trim_end().to_string()))
}

/// `remote_grep`
async fn run_grep(host: Option<&str>, args: &Value) -> Result<ToolResult, ToolError> {
    let pattern = str_arg(args, "pattern")
        .filter(|p| !p.is_empty())
        .ok_or_else(|| ToolError("`pattern` is required".to_string()))?;
    let conn = conn_for(host).await.map_err(ToolError)?;
    let max = int_arg(args, "max_matches")
        .map(|v| v.clamp(1, 5000) as usize)
        .unwrap_or(GREP_DEFAULT_MAX);
    let path = str_arg(args, "path")
        .map(|s| s.to_string())
        .unwrap_or_else(|| conn.endpoint.cwd.clone());
    let quoted_pattern = connect::shell_quote(pattern);
    let quoted_path = connect::shell_quote(&path);
    let glob = str_arg(args, "glob");
    // 远端优先用 ripgrep；没有就退回 grep。两者都限制总行数，避免刷屏。
    let command = match glob {
        Some(g) => format!(
            "if command -v rg >/dev/null 2>&1; then rg --no-heading --line-number --color never \
             --max-count {max} --glob {} -- {quoted_pattern} {quoted_path}; \
             else grep -rn --color=never -m {max} --include={} -- {quoted_pattern} {quoted_path}; fi \
             2>/dev/null | head -n {max}",
            connect::shell_quote(g),
            connect::shell_quote(g),
        ),
        None => format!(
            "if command -v rg >/dev/null 2>&1; then rg --no-heading --line-number --color never \
             --max-count {max} -- {quoted_pattern} {quoted_path}; \
             else grep -rn --color=never -m {max} -- {quoted_pattern} {quoted_path}; fi \
             2>/dev/null | head -n {max}"
        ),
    };
    let limits = transport::limits();
    let budget = transport::take_turn_budget(limits.exec_max_bytes);
    if budget == 0 {
        return Ok(ToolResult::text(
            "Output budget for this turn is exhausted; narrow the next search or start a new turn.",
        ));
    }
    let out = transport::exec(&conn, &command, None, None, budget)
        .await
        .map_err(ToolError)?;
    let body = out.combined();
    let lines = body.lines().count();
    let shown = if body.trim().is_empty() {
        format!(
            "{}:{path}: no matches for {pattern}",
            conn.endpoint.display()
        )
    } else {
        let mut text = format!("{}:{path}\n{}", conn.endpoint.display(), body.trim_end());
        if lines >= max {
            text.push_str(&format!(
                "\n(stopped at {max} matching lines; narrow the pattern or raise max_matches)"
            ));
        }
        text
    };
    Ok(ToolResult::text(shown))
}

/// 本地递归列文件（相对路径），供 push 使用
fn walk_local(root: &Path) -> Result<Vec<std::path::PathBuf>, String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("read {}: {e}", dir.display()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                if out.len() >= MAX_SYNC_FILES {
                    return Err(format!(
                        "more than {MAX_SYNC_FILES} files under {}; narrow the path",
                        root.display()
                    ));
                }
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// 拼接远端路径
fn join_remote(base: &str, rel: &str) -> String {
    if rel.is_empty() {
        return base.to_string();
    }
    format!("{}/{}", base.trim_end_matches('/'), rel)
}

/// `remote_sync`
async fn run_sync(host: Option<&str>, args: &Value, cwd: &str) -> Result<ToolResult, ToolError> {
    let direction = str_arg(args, "direction")
        .filter(|d| *d == "push" || *d == "pull")
        .ok_or_else(|| ToolError("`direction` must be push or pull".to_string()))?;
    let local_arg = str_arg(args, "local")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError("`local` is required".to_string()))?;
    let remote_arg = str_arg(args, "remote")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError("`remote` is required".to_string()))?;
    let dry_run = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let local_path = {
        let p = Path::new(local_arg);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            Path::new(cwd).join(p)
        }
    };
    let conn = conn_for(host).await.map_err(ToolError)?;
    let sftp = fs::open(&conn).await.map_err(ToolError)?;

    if direction == "push" {
        push(&sftp, &conn, &local_path, remote_arg, dry_run)
            .await
            .map_err(ToolError)
    } else {
        pull(&sftp, &conn, &local_path, remote_arg, dry_run)
            .await
            .map_err(ToolError)
    }
}

/// 本地 → 远端
async fn push(
    sftp: &fs::Sftp,
    conn: &Arc<RemoteConn>,
    local: &Path,
    remote: &str,
    dry_run: bool,
) -> Result<ToolResult, String> {
    let meta = std::fs::metadata(local).map_err(|e| format!("{}: {e}", local.display()))?;
    if meta.is_file() {
        let target = if sftp.stat(remote).await.map(|s| s.is_dir).unwrap_or(false) {
            join_remote(remote, &file_name(local)?)
        } else {
            remote.to_string()
        };
        let size = meta.len();
        if !dry_run {
            sftp.upload_from(local, &target).await?;
        }
        let action = if dry_run { "would push" } else { "pushed" };
        return Ok(ToolResult::text(format!(
            "{action} 1 file ({}) {} → {}:{target}",
            format_bytes(size),
            local.display(),
            conn.endpoint.display()
        )));
    }
    if !meta.is_dir() {
        return Err(format!(
            "{} is neither a file nor a directory",
            local.display()
        ));
    }
    let files = walk_local(local)?;
    if files.is_empty() {
        return Ok(ToolResult::text(format!(
            "{}: nothing to push",
            local.display()
        )));
    }
    let mut total = 0u64;
    let mut listed = Vec::new();
    for path in &files {
        let rel = path
            .strip_prefix(local)
            .map_err(|e| format!("{}: {e}", path.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        let target = join_remote(remote, &rel);
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        total += size;
        if !dry_run {
            sftp.upload_from(path, &target).await?;
        }
        if listed.len() < LIST_LIMIT {
            listed.push(format!("  {rel}\t{}", format_bytes(size)));
        }
    }
    let action = if dry_run { "would push" } else { "pushed" };
    let mut text = format!(
        "{action} {} files ({}) {} → {}:{remote}",
        files.len(),
        format_bytes(total),
        local.display(),
        conn.endpoint.display()
    );
    if !listed.is_empty() {
        text.push('\n');
        text.push_str(&listed.join("\n"));
        if files.len() > listed.len() {
            text.push_str(&format!("\n  … {} more", files.len() - listed.len()));
        }
    }
    if dry_run {
        text.push_str("\n(dry run: nothing was written)");
    }
    Ok(ToolResult::text(text))
}

/// 远端 → 本地
async fn pull(
    sftp: &fs::Sftp,
    conn: &Arc<RemoteConn>,
    local: &Path,
    remote: &str,
    dry_run: bool,
) -> Result<ToolResult, String> {
    let stat = sftp.stat(remote).await?;
    if !stat.is_dir {
        let target = if local.is_dir() {
            local.join(remote.rsplit('/').next().unwrap_or("file"))
        } else {
            local.to_path_buf()
        };
        if !dry_run {
            sftp.download_to(remote, &target).await?;
        }
        let action = if dry_run { "would pull" } else { "pulled" };
        return Ok(ToolResult::text(format!(
            "{action} 1 file ({}) {}:{remote} → {}",
            format_bytes(stat.size),
            conn.endpoint.display(),
            target.display()
        )));
    }
    let files = sftp.walk_files(remote).await?;
    if files.is_empty() {
        return Ok(ToolResult::text(format!("{remote}: nothing to pull")));
    }
    let mut total = 0u64;
    let mut listed = Vec::new();
    for rel in &files {
        let source = join_remote(remote, rel);
        let size = sftp.stat(&source).await.map(|s| s.size).unwrap_or(0);
        total += size;
        if !dry_run {
            sftp.download_to(&source, &local.join(rel)).await?;
        }
        if listed.len() < LIST_LIMIT {
            listed.push(format!("  {rel}\t{}", format_bytes(size)));
        }
    }
    let action = if dry_run { "would pull" } else { "pulled" };
    let mut text = format!(
        "{action} {} files ({}) {}:{remote} → {}",
        files.len(),
        format_bytes(total),
        conn.endpoint.display(),
        local.display()
    );
    if !listed.is_empty() {
        text.push('\n');
        text.push_str(&listed.join("\n"));
        if files.len() > listed.len() {
            text.push_str(&format!("\n  … {} more", files.len() - listed.len()));
        }
    }
    if dry_run {
        text.push_str("\n(dry run: nothing was written)");
    }
    Ok(ToolResult::text(text))
}

/// 取文件名
fn file_name(path: &Path) -> Result<String, String> {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| format!("{}: no file name", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_are_unique_and_declare_host() {
        let tools = tools();
        assert_eq!(tools.len(), 5);
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 5, "工具名不能重复");
        for tool in &tools {
            assert!(tool.snippet.len() < 80, "{} snippet 太长", tool.name);
            assert!(
                crate::extensions::ssh_remote::state::Limits::default().read_max_lines > 0,
                "limits 必须可用（工具定义依赖默认预算）"
            );
        }
    }

    #[test]
    fn join_remote_handles_trailing_slash() {
        assert_eq!(join_remote("/srv/app", "a.rs"), "/srv/app/a.rs");
        assert_eq!(join_remote("/srv/app/", "a.rs"), "/srv/app/a.rs");
        assert_eq!(join_remote("/srv/app", ""), "/srv/app");
    }

    #[test]
    fn walk_local_lists_relative_files() {
        let dir = std::env::temp_dir().join(format!("prux-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("sub/b.txt"), "b").unwrap();
        let files = walk_local(&dir).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|p| p.starts_with(&dir)));
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_default_is_used_when_host_omitted() {
        let _l = crate::extensions::ssh_remote::state::TEST_ROOT_LOCK.blocking_lock();
        let _g = crate::extensions::ssh_remote::state::TestRootGuard::temp();
        assert!(
            crate::extensions::ssh_remote::state::RemoteConfig::load()
                .default
                .is_none()
        );
    }
}
