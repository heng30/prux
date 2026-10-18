//! `/giff` 的只读数据来源：跑一次 `git diff`，把输出解析成左右两侧的行序列。
//!
//! 只读——不写工作树、不 rebase、不提交；失败（非 git 仓库等）以 [`Error::Message`] 返回，
//! 由命令层提示给用户。

use crate::error::{Error, Result};
use regex::Regex;
use std::collections::HashMap;
use std::process::Command;
use std::sync::LazyLock;

/// 一行改动：(源文件里的行号, 行文本)；行号为 `0` 表示左右对齐时补的占位空行。
pub type LineChange = (usize, String);

/// 一个文件的改动：左侧 `base`（`-` 行与上下文行）、右侧 `head`（`+` 行与上下文行）。
pub type FileChanges = HashMap<String, (Vec<LineChange>, Vec<LineChange>)>;

/// `diff --git a/<旧路径> b/<新路径>` 的文件头；取第二个捕获组（`b/` 侧路径）作为文件名。
static DIFF_FILE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^diff --git a/(.+) b/(.+)$").unwrap());

/// hunk 头 `@@ -<base 起始行>[,行数] +<head 起始行>[,行数] @@`，用于恢复两侧行号。
static HUNK_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^@@ -(\d+),?\d* \+(\d+),?\d* @@").unwrap());

/// ANSI 颜色转义：已加 `--no-color`，但用户 `diff.color` 配置可能仍强制上色，这里兜底剥掉。
static ANSI_ESCAPE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[.*?m").unwrap());

/// 采集未提交改动（等价 `git diff`：HEAD ↔ 工作树）。
///
/// 返回 `(改动, 左侧标签, 右侧标签)`；标签用于界面 header 与两栏标题。
pub fn collect_uncommitted() -> Result<(FileChanges, String, String)> {
    let output = git_diff()?;
    Ok((
        parse(&output),
        "HEAD".to_string(),
        "Working Tree".to_string(),
    ))
}

/// 执行 `git diff --no-color` 并返回标准输出文本。
///
/// `git` 无法启动（未安装）或命令失败（非 git 仓库）时返回可展示的错误，
/// 错误里带 `git` 自己写的 stderr。
fn git_diff() -> Result<String> {
    let output = Command::new("git")
        .args(["diff", "--no-color"])
        .output()
        .map_err(|e| Error::Message(format!("failed to run git: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Message(format!(
            "git diff failed: {}",
            stderr.trim()
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 解析 `git diff` 输出。
///
/// 逐行走：文件头切分文件、hunk 头恢复两侧起始行号、`-`/`+` 分别推进左右、其余行
/// 作为上下文同时进两侧。`\ No newline at end of file` 与各类元数据行丢弃（不是内容）。
pub fn parse(diff_output: &str) -> FileChanges {
    let mut file_changes = FileChanges::new();
    let mut current_file = String::new();
    let mut base_lines: Vec<LineChange> = Vec::new();
    let mut head_lines: Vec<LineChange> = Vec::new();
    let mut base_line_number = 1usize;
    let mut head_line_number = 1usize;

    for raw_line in diff_output.lines() {
        let trimmed = raw_line.trim_end();

        // "no newline" 标记不是内容，既不渲染也不占行号
        if trimmed.starts_with("\\ ") {
            continue;
        }

        let line = ANSI_ESCAPE_RE.replace_all(trimmed, "");

        if let Some(caps) = DIFF_FILE_RE.captures(line.as_ref()) {
            if !current_file.is_empty() {
                file_changes.insert(
                    std::mem::take(&mut current_file),
                    (
                        std::mem::take(&mut base_lines),
                        std::mem::take(&mut head_lines),
                    ),
                );
            }
            current_file = match caps.get(2) {
                Some(m) => m.as_str().to_string(),
                None => continue,
            };
            base_line_number = 1;
            head_line_number = 1;
            continue;
        }

        if let Some(caps) = HUNK_HEADER_RE.captures(line.as_ref()) {
            base_line_number = caps
                .get(1)
                .and_then(|m| m.as_str().parse::<usize>().ok())
                .unwrap_or(1);
            head_line_number = caps
                .get(2)
                .and_then(|m| m.as_str().parse::<usize>().ok())
                .unwrap_or(1);
            continue;
        }

        if is_metadata(line.as_ref()) {
            continue;
        }

        if let Some(rest) = line.strip_prefix('-') {
            base_lines.push((base_line_number, format!("-{rest}")));
            base_line_number += 1;
        } else if let Some(rest) = line.strip_prefix('+') {
            head_lines.push((head_line_number, format!("+{rest}")));
            head_line_number += 1;
        } else {
            base_lines.push((base_line_number, line.to_string()));
            head_lines.push((head_line_number, line.to_string()));
            base_line_number += 1;
            head_line_number += 1;
        }
    }

    if !current_file.is_empty() {
        file_changes.insert(current_file, (base_lines, head_lines));
    }

    file_changes
}

/// 是否为 diff 的元数据行（文件模式、重命名、二进制标记等，不属于任何一侧的内容）。
fn is_metadata(line: &str) -> bool {
    [
        "index",
        "---",
        "+++",
        "@@",
        "new file mode",
        "new mode",
        "old mode",
        "deleted file mode",
        "rename from",
        "rename to",
        "copy from",
        "copy to",
        "similarity index",
        "dissimilarity index",
        "Binary files",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一段两文件的 diff 输出。
    const SAMPLE: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 1111111..2222222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,4 +1,4 @@
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
     // keep
 }
diff --git a/src/b.rs b/src/b.rs
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/src/b.rs
@@ -0,0 +1,2 @@
+line one
+line two
";

    /// 文件按 `b/` 侧路径切分，上下文行同时进两侧。
    #[test]
    fn parse_splits_files_and_context() {
        let changes = parse(SAMPLE);
        assert_eq!(changes.len(), 2);

        let (base, head) = &changes["src/a.rs"];
        assert_eq!(base.len(), 4, "上下文 2 + 删除 1 + 上下文 1");
        assert_eq!(head.len(), 4);
        assert_eq!(base[0], (1, " fn main() {".to_string()));
        assert_eq!(base[1], (2, "-    println!(\"old\");".to_string()));
        assert_eq!(head[1], (2, "+    println!(\"new\");".to_string()));
        assert_eq!(base[3].0, 4, "删除行之后的行号继续推进");

        let (base, head) = &changes["src/b.rs"];
        assert!(base.is_empty(), "新增文件没有左侧内容");
        assert_eq!(head.len(), 2);
        assert_eq!(head[0], (1, "+line one".to_string()));
    }

    /// hunk 头恢复行号：第二个 hunk 从它自己的起始行继续，而不是接着上一个 hunk 数。
    #[test]
    fn hunk_header_resets_line_numbers() {
        let diff = "\
diff --git a/f.rs b/f.rs
--- a/f.rs
+++ b/f.rs
@@ -1,1 +1,1 @@
-a
+b
@@ -10,1 +10,1 @@
-c
+d
";
        let (base, head) = &parse(diff)["f.rs"];
        assert_eq!(base[0], (1, "-a".to_string()));
        assert_eq!(base[1], (10, "-c".to_string()));
        assert_eq!(head[1], (10, "+d".to_string()));
    }

    /// 元数据行与 `\ No newline` 标记不进内容。
    #[test]
    fn metadata_and_no_newline_marker_are_skipped() {
        let diff = "\
diff --git a/f.rs b/f.rs
old mode 100644
new mode 100755
rename from f.rs
rename to g.rs
Binary files a/x.bin and b/x.bin differ
diff --git a/h.rs b/h.rs
--- a/h.rs
+++ b/h.rs
@@ -1,1 +1,1 @@
-a
\\ No newline at end of file
+b
\\ No newline at end of file
";
        let changes = parse(diff);
        let (base, head) = &changes["f.rs"];
        assert!(
            base.is_empty() && head.is_empty(),
            "纯元数据的文件两侧都没有内容"
        );
        let (base, head) = &changes["h.rs"];
        assert_eq!(base.len(), 1);
        assert_eq!(head.len(), 1);
    }

    /// 非 diff 输出（空串）解析为空表，调用方据此提示「没有改动」。
    #[test]
    fn empty_input_yields_no_changes() {
        assert!(parse("").is_empty());
    }
}
