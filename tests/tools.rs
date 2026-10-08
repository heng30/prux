//! core::tools 集成测试：在临时目录真实执行七个内置工具。

use prux::core::tools::{
    ToolResult, execute_bash, execute_edit, execute_find, execute_grep, execute_ls, execute_read,
    execute_write, tool_defs,
};
use std::path::Path;

/// 构建临时工作目录
fn temp_cwd() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    // 预置一个测试文件
    std::fs::write(dir.path().join("hello.txt"), "line1\nline2\nline3\n").unwrap();
    dir
}

fn cwd_str(dir: &tempfile::TempDir) -> &str {
    dir.path().to_str().unwrap()
}

/// 在部分文件系统（tmpfs/overlayfs）上，刚写完的假工具脚本立刻 exec 会短暂返回
/// `ETXTBSY`（Text file busy）。这不是被测代码的问题——生产里工具只下载一次、之后才
/// 执行——但会让下面两个「本地 bin 副本」测试随机失败。重试到成功（上限约 2s）。
async fn retry_text_file_busy<T, F, Fut>(mut f: F) -> Result<T, prux::core::tools::ToolError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, prux::core::tools::ToolError>>,
{
    for _ in 0..200 {
        match f().await {
            Err(e) if e.0.contains("Text file busy") => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            other => return other,
        }
    }
    f().await
}

#[test]
fn tool_defs_registry() {
    let defs = tool_defs(&[
        "read".into(),
        "write".into(),
        "edit".into(),
        "bash".into(),
        "grep".into(),
        "find".into(),
        "ls".into(),
        "unknown-tool".into(),
    ]);
    assert_eq!(defs.len(), 7, "unknown tools should be skipped");
    assert_eq!(defs[0].name, "read");
    assert!(!defs[0].description.is_empty());
    assert!(defs[0].parameters.get("properties").is_some());
}

#[tokio::test]
async fn read_file_basic() {
    let dir = temp_cwd();
    let result = execute_read("hello.txt", None, None, cwd_str(&dir))
        .await
        .unwrap();
    let ToolResult { text: t, .. } = result;
    assert!(t.contains("line1"));
    assert!(t.contains("line3"));
}

#[tokio::test]
async fn read_file_with_offset_limit() {
    let dir = temp_cwd();
    let result = execute_read("hello.txt", Some(2), Some(1), cwd_str(&dir))
        .await
        .unwrap();
    let ToolResult { text: t, .. } = result;
    assert!(t.contains("line2"));
    assert!(!t.contains("line1"));
    assert!(!t.contains("line3"));
}

#[tokio::test]
async fn read_missing_file_errors() {
    let dir = temp_cwd();
    let err = execute_read("nope.txt", None, None, cwd_str(&dir))
        .await
        .unwrap_err();
    assert!(err.0.contains("Error reading file"));
}

#[tokio::test]
async fn read_binary_file_lossy_replaces_invalid_utf8() {
    // 对齐 pi TextDecoder().decode：无效字节以 U+FFFD 替换继续显示文本
    let dir = temp_cwd();
    std::fs::write(dir.path().join("bin.dat"), b"hello \xff\xfe world").unwrap();
    let result = execute_read("bin.dat", None, None, cwd_str(&dir))
        .await
        .unwrap();
    let ToolResult { text: t, .. } = result;
    assert!(!t.contains("Binary file"), "{}", t);
    assert!(t.contains("hello \u{FFFD}\u{FFFD} world"), "{t:?}");
}

#[tokio::test]
async fn write_creates_file_and_dirs() {
    let dir = temp_cwd();
    let result = execute_write("nested/a/b.txt", "content", cwd_str(&dir))
        .await
        .unwrap();
    let ToolResult { text: t, .. } = result;
    assert!(t.contains("Successfully wrote to"));
    assert!(!t.contains("7 bytes"), "不应报告误导字节计数: {t}");
    let content = std::fs::read_to_string(dir.path().join("nested/a/b.txt")).unwrap();
    assert_eq!(content, "content");
}

#[tokio::test]
async fn edit_replaces_block() {
    let dir = temp_cwd();
    let edits = serde_json::json!([{ "oldText": "line2", "newText": "REPLACED" }]);
    let (result, detail, _first_line) = execute_edit("hello.txt", &edits, cwd_str(&dir))
        .await
        .unwrap();
    let ToolResult { text: t, .. } = result;
    assert!(t.contains("Successfully replaced 1 block"));
    assert!(detail.contains("PATCH:"));
    let content = std::fs::read_to_string(dir.path().join("hello.txt")).unwrap();
    assert!(content.contains("REPLACED"));
    assert!(!content.contains("line2"));
}

#[tokio::test]
async fn edit_rejects_bad_path() {
    let dir = temp_cwd();
    let edits = serde_json::json!([{ "oldText": "a", "newText": "b" }]);
    let err = execute_edit("missing.txt", &edits, cwd_str(&dir))
        .await
        .unwrap_err();
    assert!(err.0.contains("not a file"));
}

#[tokio::test]
async fn bash_echo() {
    let dir = temp_cwd();
    let result = execute_bash("echo hello-bash", None, cwd_str(&dir))
        .await
        .unwrap();
    let ToolResult { text: t, .. } = result;
    assert_eq!(t.trim(), "hello-bash");
}

#[tokio::test]
async fn bash_exit_code_error() {
    let dir = temp_cwd();
    let err = execute_bash("exit 3", None, cwd_str(&dir))
        .await
        .unwrap_err();
    assert!(err.0.contains("exited with code 3"));
}

#[tokio::test]
async fn bash_timeout() {
    let dir = temp_cwd();
    let err = execute_bash("sleep 5", Some(0.2), cwd_str(&dir))
        .await
        .unwrap_err();
    assert!(err.0.contains("timed out"));
}

#[tokio::test]
async fn bash_invalid_timeout() {
    let dir = temp_cwd();
    let err = execute_bash("echo x", Some(-1.0), cwd_str(&dir))
        .await
        .unwrap_err();
    assert!(err.0.contains("Invalid timeout"));
}

#[tokio::test]
async fn ls_lists_entries() {
    let dir = temp_cwd();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let result = execute_ls(None, None, cwd_str(&dir)).await.unwrap();
    let ToolResult { text: t, .. } = result;
    assert!(t.contains("hello.txt"));
    assert!(t.contains("sub/"), "dirs should keep the / suffix");
}

#[tokio::test]
async fn ls_missing_dir_errors() {
    let dir = temp_cwd();
    let err = execute_ls(Some("nope"), None, cwd_str(&dir))
        .await
        .unwrap_err();
    assert!(err.0.contains("Path not found"));
}

#[tokio::test]
async fn grep_finds_matches() {
    let dir = temp_cwd();
    std::fs::write(
        dir.path().join("src.rs"),
        "fn main() {}\n// comment\nfn helper() {}\n",
    )
    .unwrap();
    let result = execute_grep(
        "fn ",
        Some("."),
        None,
        false,
        false,
        None,
        None,
        cwd_str(&dir),
    )
    .await
    .unwrap();
    let ToolResult { text: t, .. } = result;
    assert!(t.contains("src.rs"));
    assert!(t.contains("fn main"));
    assert!(!t.contains("comment"));
}

#[tokio::test]
async fn grep_literal_and_ignore_case() {
    let dir = temp_cwd();
    std::fs::write(dir.path().join("a.txt"), "Foo\nbar\n").unwrap();
    let result = execute_grep(
        "FOO",
        Some("."),
        None,
        true,
        true,
        None,
        None,
        cwd_str(&dir),
    )
    .await
    .unwrap();
    let ToolResult { text: t, .. } = result;
    assert!(t.contains("Foo"));
}

#[tokio::test]
async fn grep_no_matches() {
    let dir = temp_cwd();
    let result = execute_grep(
        "zzz_nothing",
        Some("."),
        None,
        false,
        true,
        None,
        None,
        cwd_str(&dir),
    )
    .await
    .unwrap();
    let ToolResult { text: t, .. } = result;
    assert!(t.contains("No matches"));
}

#[tokio::test]
async fn find_by_glob() {
    let dir = temp_cwd();
    std::fs::write(dir.path().join("a.md"), "x").unwrap();
    std::fs::write(dir.path().join("b.txt"), "x").unwrap();
    let result = execute_find("*.md", None, None, cwd_str(&dir))
        .await
        .unwrap();
    let ToolResult { text: t, .. } = result;
    assert!(t.contains("a.md"));
    assert!(!t.contains("b.txt"));
}

#[tokio::test]
async fn find_no_results() {
    let dir = temp_cwd();
    let result = execute_find("*.zzz", None, None, cwd_str(&dir))
        .await
        .unwrap();
    let ToolResult { text: t, .. } = result;
    assert!(t.contains("No files found"));
}

#[test]
fn tool_path_normalization() {
    // resolve_tool_path 处理 @ 前缀与特殊空白（内部函数，间接验证路径解析）
    let dir = temp_cwd();
    let p = cwd_str(&dir);
    assert!(Path::new(p).is_dir());
}

/// 本地 `<agent_dir>/bin` 副本不在 PATH 上时，grep 也必须用解析出的**完整路径**调起。
/// 用一个忽略参数的假 rg 脚本验证：输出只可能来自本地副本（PATH 里没有它）。
#[cfg(unix)]
#[tokio::test]
async fn grep_uses_local_bin_copy_not_path() {
    use std::os::unix::fs::PermissionsExt;

    let _agent_dir = prux::test_support::AgentDirGuard::temp();
    let bin = prux::core::tools_manager::bin_dir();
    std::fs::create_dir_all(&bin).unwrap();
    let rg = bin.join("rg");
    std::fs::write(
        &rg,
        "#!/bin/sh\nprintf '%s\\n' \
         '{\"type\":\"match\",\"data\":{\"path\":{\"text\":\"LOCAL_RG_OK.rs\"},\"line_number\":1,\"lines\":{\"text\":\"matched\\n\"}}}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&rg, std::fs::Permissions::from_mode(0o755)).unwrap();

    let dir = temp_cwd();
    let cwd = cwd_str(&dir).to_string();
    let ToolResult { text: t, .. } = retry_text_file_busy(|| {
        execute_grep("anything", Some("."), None, false, false, None, None, &cwd)
    })
    .await
    .unwrap();

    assert!(
        t.contains("LOCAL_RG_OK.rs"),
        "应调用本地副本，实际输出：{t}"
    );
}

/// find 同 grep：本地副本可直接调起
#[cfg(unix)]
#[tokio::test]
async fn find_uses_local_bin_copy_not_path() {
    use std::os::unix::fs::PermissionsExt;

    let _agent_dir = prux::test_support::AgentDirGuard::temp();
    let bin = prux::core::tools_manager::bin_dir();
    std::fs::create_dir_all(&bin).unwrap();
    let fd = bin.join("fd");
    std::fs::write(&fd, "#!/bin/sh\necho LOCAL_FD_OK.md\n").unwrap();
    std::fs::set_permissions(&fd, std::fs::Permissions::from_mode(0o755)).unwrap();

    let dir = temp_cwd();
    let cwd = cwd_str(&dir).to_string();
    let ToolResult { text: t, .. } =
        retry_text_file_busy(|| execute_find("*.md", None, None, &cwd))
            .await
            .unwrap();

    assert!(
        t.contains("LOCAL_FD_OK.md"),
        "应调用本地副本，实际输出：{t}"
    );
}
