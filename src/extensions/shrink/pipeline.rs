//! 工具结果压缩管道：内容类型检测与四个确定性压缩器。
//!
//! 全部是纯函数（文本进、文本出），不读盘、不碰全局状态，便于脱离扩展单测。
//! 调用方 [`super`] 负责门槛（最小字节数、按类型开关）、原文落盘与尾部标记。
//!
//! 四条硬约束，改压缩器之前先读：
//! - **绝不放大**：压完不比原文短就由调用方原样返还原文，压缩器不必自己判断；
//! - **绝不切坏多字节字符**：所有长度按 `char` 计，切片一律走字符边界；
//! - **确定性**：同一输入永远同一输出（不依赖时间、随机数、环境）；
//! - **不猜**：识别不出内容类型就 [`Kind::Plain`]，调用方直接放弃压缩。

use serde_json::Value;
use std::collections::HashMap;

/// 搜索压缩器里每个文件保留的命中行数上限（超出计数）。
const SEARCH_PER_FILE: usize = 6;
/// 搜索压缩器最多渲染的文件数（超出只计数）。
const SEARCH_FILES: usize = 20;
/// 日志压缩器保留的首部行数。
const LOG_HEAD: usize = 3;
/// 日志压缩器保留的尾部行数。
const LOG_TAIL: usize = 6;
/// diff 里连续未变更行的折叠阈值。
const DIFF_CONTEXT: usize = 3;
/// JSON 表格化的列数上限。
const JSON_COLS: usize = 6;
/// JSON 单元格的字符上限。
const JSON_CELL: usize = 40;
/// JSON 超过该行数时保留首尾并省略中间。
const JSON_ROWS: usize = 40;

/// 日志压缩器认定的「要紧」关键词（全小写比较）。
const LOG_KEYWORDS: [&str; 10] = [
    "error",
    "warning",
    "fail",
    "panic",
    "cannot",
    "refused",
    "denied",
    "abort",
    "fatal",
    "not found",
];

/// 识别出的内容类型（决定用哪个压缩器）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum_macros::IntoStaticStr)]
pub enum Kind {
    /// `grep` 一类的 `path:line:body` 命中列表。
    #[strum(serialize = "search")]
    Search,
    /// 命令输出、构建与测试日志。
    #[strum(serialize = "log")]
    Log,
    /// 统一 diff。
    #[strum(serialize = "diff")]
    Diff,
    /// 顶层是数组的 JSON。
    #[strum(serialize = "json")]
    Json,
    /// 认不出来：不做任何压缩。
    #[strum(serialize = "plain")]
    Plain,
}

/// 识别内容类型：工具名先验优先，其后按结构启发式逐个试。
///
/// `tool` 是工具名（用于先验：`grep` 恒为搜索命中、只有命令类工具才按日志压）；
/// `text` 是工具结果原文。
///
/// 日志只对 `bash` / `powershell` 生效：其它工具的长正文（文档、源码、长段落）
/// 偶尔也「像日志」，按日志压它们只会把内容藏起来。diff / JSON / 搜索命中
/// 三类靠结构识别，任何工具都认。
pub fn detect(tool: &str, text: &str) -> Kind {
    if text.trim().is_empty() {
        return Kind::Plain;
    }
    if tool == "grep" {
        return Kind::Search;
    }
    if is_diff(text) {
        return Kind::Diff;
    }
    if is_search(text) {
        return Kind::Search;
    }
    if is_json_array(text) {
        return Kind::Json;
    }
    if matches!(tool, "bash" | "powershell") && is_log(text) {
        return Kind::Log;
    }
    Kind::Plain
}

/// 按类型压缩；`None` 表示这类内容压不动（调用方原样返还文本）。
pub fn compress(kind: Kind, text: &str) -> Option<String> {
    match kind {
        Kind::Search => compress_search(text),
        Kind::Log => compress_log(text),
        Kind::Diff => compress_diff(text),
        Kind::Json => compress_json(text),
        Kind::Plain => None,
    }
}

/// 文本是否像统一 diff。
fn is_diff(text: &str) -> bool {
    text.starts_with("diff --git ")
        || text.starts_with("@@ ")
        || text.contains("\n@@ ")
        || (text.contains("\n--- ") && text.contains("\n+++ "))
}

/// 文本是否像 `path:line:body` 的搜索命中列表（至少 3 行可解析）。
fn is_search(text: &str) -> bool {
    text.lines()
        .take(400)
        .filter(|l| parse_hit(l).is_some())
        .count()
        >= 3
}

/// 文本是否是顶层为数组的 JSON（解析成功且非空）。
fn is_json_array(text: &str) -> bool {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('[') || text.len() > 512 * 1024 {
        return false;
    }
    matches!(serde_json::from_str::<Vec<Value>>(text), Ok(v) if !v.is_empty())
}

/// 文本是否像命令输出 / 构建日志（行数够多才值得压缩）。
fn is_log(text: &str) -> bool {
    text.lines().count() >= 30
}

/// 解析一行命中，返回 `(path, 行号, 正文)`。
///
/// 认两种形状：`path:line:body`（命中行）与 `path-line-body`（上下文行，
/// 见 `crate::core::tools::grep`）。路径含空格的行不认——那类项目走不压缩的原路。
fn parse_hit(line: &str) -> Option<(&str, &str, &str)> {
    for sep in [':', '-'] {
        let Some((path, rest)) = line.split_once(sep) else {
            continue;
        };
        if path.is_empty() || path.contains(' ') {
            continue;
        }
        let Some((num, body)) = rest.split_once(sep) else {
            continue;
        };
        if !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) {
            return Some((path, num, body));
        }
    }
    None
}

/// 按文件分组，每文件留前 [`SEARCH_PER_FILE`] 条命中，其余计数。
fn compress_search(text: &str) -> Option<String> {
    let mut order: Vec<&str> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut groups: Vec<(Vec<&str>, usize)> = Vec::new();

    for line in text.lines() {
        let Some((path, _, _)) = parse_hit(line) else {
            continue;
        };
        let idx = match index.get(path) {
            Some(idx) => *idx,
            None => {
                let idx = order.len();
                order.push(path);
                index.insert(path, idx);
                groups.push((Vec::new(), 0));
                idx
            }
        };
        let group = &mut groups[idx];
        if group.0.len() < SEARCH_PER_FILE {
            group.0.push(line);
        } else {
            group.1 += 1;
        }
    }

    if order.is_empty() {
        return None;
    }

    let total: usize = groups.iter().map(|(kept, more)| kept.len() + more).sum();
    let mut out = String::new();
    out.push_str(&format!("{total} hits in {} files\n", order.len()));
    for (idx, path) in order.iter().take(SEARCH_FILES).enumerate() {
        let (kept, more) = &groups[idx];
        out.push_str(&format!("{path} ({} hits)\n", kept.len() + more));
        for line in kept {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
        if *more > 0 {
            out.push_str(&format!("  [+{more} more]\n"));
        }
    }
    if order.len() > SEARCH_FILES {
        out.push_str(&format!("[+{} more files]\n", order.len() - SEARCH_FILES));
    }
    Some(out)
}

/// 保留首尾与含关键词的行，其余连续区段折叠成 `<… N lines …>`。
fn compress_log(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    if n <= LOG_HEAD + LOG_TAIL {
        return None;
    }

    let keep: Vec<bool> = lines
        .iter()
        .enumerate()
        .map(|(idx, line)| idx < LOG_HEAD || idx + LOG_TAIL >= n || is_interesting(line))
        .collect();

    let mut out = String::new();
    let mut skipped = 0usize;
    for (idx, line) in lines.iter().enumerate() {
        if keep[idx] {
            if skipped > 0 {
                out.push_str(&format!("<… {skipped} lines …>\n"));
                skipped = 0;
            }
            out.push_str(line);
            out.push('\n');
        } else {
            skipped += 1;
        }
    }
    if skipped > 0 {
        out.push_str(&format!("<… {skipped} lines …>\n"));
    }
    Some(out)
}

/// 一行日志是否要保留（含错误/警告关键词）。
fn is_interesting(line: &str) -> bool {
    let lower = line.to_lowercase();
    LOG_KEYWORDS.iter().any(|kw| lower.contains(kw))
}

/// 保留全部变更行与 hunk 头，连续未变更行折叠成 `<… N unchanged …>`。
fn compress_diff(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut skipped = 0usize;

    for line in text.lines() {
        if line.starts_with(' ') {
            skipped += 1;
            if skipped < DIFF_CONTEXT {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }
        if skipped >= DIFF_CONTEXT {
            out.push_str(&format!("<… {} unchanged …>\n", skipped - DIFF_CONTEXT + 1));
        }
        skipped = 0;
        out.push_str(line);
        out.push('\n');
    }
    if skipped >= DIFF_CONTEXT {
        out.push_str(&format!("<… {} unchanged …>\n", skipped - DIFF_CONTEXT + 1));
    }
    Some(out)
}

/// 对象数组渲染成紧凑表格；超长时保留首尾。
fn compress_json(text: &str) -> Option<String> {
    let value: Vec<Value> = serde_json::from_str(text).ok()?;
    if value.len() <= JSON_ROWS / 2 {
        return None;
    }
    let rows: Vec<&serde_json::Map<String, Value>> =
        value.iter().map(|v| v.as_object()).collect::<Option<_>>()?;

    // 列集合按首次出现顺序取前 JSON_COLS 个键
    let mut cols: Vec<&str> = Vec::new();
    for row in &rows {
        for key in row.keys() {
            if cols.len() >= JSON_COLS {
                break;
            }
            if !cols.contains(&key.as_str()) {
                cols.push(key);
            }
        }
    }
    if cols.is_empty() {
        return None;
    }

    let mut out = format!(
        "{} rows × {} cols (of {})\n",
        rows.len(),
        cols.len(),
        value.len()
    );
    out.push_str(&cols.join(" | "));
    out.push('\n');
    out.push_str(&cols.iter().map(|_| "---").collect::<Vec<_>>().join(" | "));
    out.push('\n');

    let head = JSON_ROWS / 2;
    let tail = 5;
    for (idx, row) in rows.iter().enumerate() {
        if idx == head && rows.len() > head + tail {
            out.push_str(&format!("<… {} rows …>\n", rows.len() - head - tail));
        }
        if idx >= head && idx + tail < rows.len() {
            continue;
        }
        let cells: Vec<String> = cols.iter().map(|key| cell(row.get(*key))).collect();
        out.push_str(&cells.join(" | "));
        out.push('\n');
    }
    Some(out)
}

/// 一个 JSON 值渲染成单元格（字符串不加引号，其余走紧凑序列化）。
fn cell(value: Option<&Value>) -> String {
    let text = match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.replace('\n', " "),
        Some(other) => other.to_string(),
    };
    if text.chars().count() <= JSON_CELL {
        return text;
    }
    let mut out: String = text.chars().take(JSON_CELL).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 搜索命中按文件分组、每文件留够数、其余只计数。
    #[test]
    fn groups_search_hits_by_file() {
        let mut text = String::new();
        for i in 0..20 {
            text.push_str(&format!("src/a.rs:{i}:alpha {i}\n"));
        }
        for i in 0..3 {
            text.push_str(&format!("src/b.rs:{i}:beta {i}\n"));
        }
        let out = compress_search(&text).expect("压得动");
        assert!(out.starts_with("23 hits in 2 files"), "{out}");
        assert!(out.contains("src/a.rs (20 hits)"));
        assert!(out.contains("[+14 more]"), "{out}");
        assert!(out.contains("src/b.rs:0:beta 0"));
        assert!(out.len() < text.len());
    }

    /// 上下文行（`path-line-`）也算命中，路径带空格的行不算。
    #[test]
    fn parses_context_lines_and_skips_noise() {
        let hit = parse_hit("src/a.rs-12-  body").expect("上下文行");
        assert_eq!(hit, ("src/a.rs", "12", "  body"));
        assert!(parse_hit("some prose: not a hit").is_none());
        assert!(parse_hit("/a b/c.rs:1:x").is_none());
    }

    /// 日志保留首尾与错误行，中间大段折叠。
    #[test]
    fn keeps_head_tail_and_errors() {
        let mut text = String::from("$ cargo build\nCompiling a\nCompiling b\n");
        for i in 0..50 {
            text.push_str(&format!("noise line {i}\n"));
        }
        text.push_str("error[E0308]: mismatched types\n");
        text.push_str("warning: unused\n");
        for i in 0..6 {
            text.push_str(&format!("tail {i}\n"));
        }
        let out = compress_log(&text).expect("压得动");
        assert!(out.contains("error[E0308]"), "{out}");
        assert!(out.contains("warning: unused"), "{out}");
        assert!(out.contains("lines …>"), "{out}");
        assert!(out.len() < text.len());
    }

    /// diff 只留 hunk 头与变更行，连续未变更折成一行。
    #[test]
    fn collapses_unchanged_runs() {
        let mut text = String::from("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,10 +1,10 @@\n");
        for i in 0..8 {
            text.push_str(&format!(" context {i}\n"));
        }
        text.push_str("+added\n-removed\n");
        let out = compress_diff(&text).expect("压得动");
        assert!(out.contains("@@ -1,10 +1,10 @@"));
        assert!(out.contains("+added") && out.contains("-removed"));
        assert!(out.contains("unchanged …>"), "{out}");
        assert!(!out.contains("context 7"));
    }

    /// 对象数组渲染成表格，超长保留首尾。
    #[test]
    fn renders_json_array_as_table() {
        let rows: Vec<serde_json::Value> = (0..100)
            .map(|i| serde_json::json!({ "id": i, "name": format!("row {i}") }))
            .collect();
        let text = serde_json::to_string(&rows).unwrap();
        let out = compress_json(&text).expect("压得动");
        assert!(out.starts_with("100 rows × 2 cols"), "{out}");
        assert!(out.contains("id | name"));
        assert!(out.contains("row 99"));
        assert!(out.contains("rows …>"), "{out}");
        assert!(out.len() < text.len());
    }

    /// 认不出的内容一律 Plain，不做任何压缩；非命令类工具的长段落也不按日志压。
    #[test]
    fn unknown_text_stays_plain() {
        assert_eq!(detect("read", "just a sentence\n"), Kind::Plain);
        assert_eq!(detect("bash", "a\nb\nc\n"), Kind::Plain);
        let prose = "一句话。\n".repeat(400);
        assert_eq!(detect("read", &prose), Kind::Plain);
        assert_eq!(compress(Kind::Plain, "anything"), None);
    }

    /// 检测：grep 恒为搜索；diff / json / 长日志按结构识别。
    #[test]
    fn detects_by_tool_prior_and_structure() {
        assert_eq!(detect("grep", "whatever"), Kind::Search);
        assert_eq!(detect("bash", "diff --git a/x b/x\n+1\n"), Kind::Diff);
        assert_eq!(detect("read", "[{\"a\":1},{\"a\":2}]"), Kind::Json);
        let log: String = (0..40).map(|i| format!("line {i}\n")).collect();
        assert_eq!(detect("bash", &log), Kind::Log);
    }

    /// 多字节字符不被切坏。
    #[test]
    fn never_splits_multibyte() {
        let rows: Vec<serde_json::Value> = (0..60)
            .map(|i| serde_json::json!({ "名称": "中文值".repeat(20), "i": i }))
            .collect();
        let text = serde_json::to_string(&rows).unwrap();
        let out = compress_json(&text).expect("压得动");
        assert!(out.chars().all(|c| c != '\u{FFFD}'));
        assert_eq!(out, compress_json(&text).unwrap(), "输出必须确定");
    }
}
