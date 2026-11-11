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

/// glob 相对 `path`（与输出路径同一形态），不是相对 agent 的启动目录（pi #11807）。
#[tokio::test]
async fn grep_glob_is_relative_to_path() {
    let dir = temp_cwd();
    std::fs::create_dir_all(dir.path().join("repo/migrations/main")).unwrap();
    std::fs::create_dir_all(dir.path().join("repo/other")).unwrap();
    std::fs::write(dir.path().join("repo/migrations/main/a.js"), "needle\n").unwrap();
    std::fs::write(dir.path().join("repo/other/b.js"), "needle\n").unwrap();

    let result = execute_grep(
        "needle",
        Some("repo"),
        Some("migrations/main/*.js"),
        false,
        true,
        None,
        None,
        cwd_str(&dir),
    )
    .await
    .unwrap();
    let ToolResult { text: t, .. } = result;
    assert!(t.contains("a.js"), "{t}");
    assert!(!t.contains("b.js"), "{t}");

    // 前导 `./` 被忽略
    let result = execute_grep(
        "needle",
        Some("repo"),
        Some("./migrations/main/*.js"),
        false,
        true,
        None,
        None,
        cwd_str(&dir),
    )
    .await
    .unwrap();
    assert!(result.text.contains("a.js"), "{}", result.text);
}

/// 上下文行由 ripgrep 产出：匹配行 `file:line:`、上下文行 `file-line-`（pi #9276）。
#[tokio::test]
async fn grep_context_lines_come_from_ripgrep() {
    let dir = temp_cwd();
    std::fs::write(dir.path().join("f.txt"), "one\ntwo\nneedle\nfour\nfive\n").unwrap();
    let result = execute_grep(
        "needle",
        Some("f.txt"),
        None,
        false,
        true,
        Some(1),
        None,
        cwd_str(&dir),
    )
    .await
    .unwrap();
    let ToolResult { text: t, .. } = result;
    assert!(t.contains("f.txt-2- two"), "{t}");
    assert!(t.contains("f.txt:3: needle"), "{t}");
    assert!(t.contains("f.txt-4- four"), "{t}");
    assert!(!t.contains("five"), "{t}");
}

/// 结果数**恰好**等于 limit 时不提示 limit reached，多一条才提示（pi #11423）。
#[tokio::test]
async fn grep_and_find_limit_notice_needs_extra_result() {
    let dir = temp_cwd();
    std::fs::write(dir.path().join("two.txt"), "hit\nhit\n").unwrap();
    let result = execute_grep(
        "hit",
        Some("two.txt"),
        None,
        false,
        true,
        None,
        Some(2),
        cwd_str(&dir),
    )
    .await
    .unwrap();
    let ToolResult { text: t, .. } = result;
    assert!(!t.contains("limit reached"), "{t}");

    std::fs::write(dir.path().join("three.txt"), "hit\nhit\nhit\n").unwrap();
    let result = execute_grep(
        "hit",
        Some("three.txt"),
        None,
        false,
        true,
        None,
        Some(2),
        cwd_str(&dir),
    )
    .await
    .unwrap();
    assert!(result.text.contains("limit reached"), "{}", result.text);

    // find：恰好 limit 个文件同样不提示
    for name in ["x.md", "y.md"] {
        std::fs::write(dir.path().join(name), "x").unwrap();
    }
    let result = execute_find("*.md", None, Some(2), cwd_str(&dir))
        .await
        .unwrap();
    assert!(!result.text.contains("limit reached"), "{}", result.text);
    std::fs::write(dir.path().join("z.md"), "x").unwrap();
    let result = execute_find("*.md", None, Some(2), cwd_str(&dir))
        .await
        .unwrap();
    assert!(result.text.contains("limit reached"), "{}", result.text);
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

/// Windows 分隔符的路径模式与 `src/**/*.ts` 都要按路径语义匹配（pi #9262）。
#[tokio::test]
async fn find_path_patterns_normalize_separators() {
    let dir = temp_cwd();
    std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
    std::fs::write(dir.path().join("src/a.ts"), "x").unwrap();
    std::fs::write(dir.path().join("src/deep/b.ts"), "x").unwrap();
    std::fs::write(dir.path().join("src/notes.md"), "x").unwrap();

    // 反斜杠写法等价于正斜杠
    let result = execute_find(r"src\*.ts", None, None, cwd_str(&dir))
        .await
        .unwrap();
    let ToolResult { text: t, .. } = result;
    assert!(t.contains("a.ts"), "{t}");
    assert!(!t.contains("notes.md"), "{t}");

    // `src/*.ts` 不匹配子目录；`src/**/*.ts` 两种层级都匹配
    let result = execute_find("src/*.ts", None, None, cwd_str(&dir))
        .await
        .unwrap();
    assert!(!result.text.contains("deep/b.ts"), "{}", result.text);
    let result = execute_find("src/**/*.ts", None, None, cwd_str(&dir))
        .await
        .unwrap();
    assert!(result.text.contains("a.ts"), "{}", result.text);
    assert!(result.text.contains("b.ts"), "{}", result.text);
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
