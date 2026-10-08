// head/tail 截断逻辑

/// head/tail 截断未显式指定行数上限时的默认值（2000 行）。
pub const DEFAULT_MAX_LINES: usize = 2000;
/// head/tail 截断未显式指定字节上限时的默认值（50KB）。
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// grep 结果单行展示的最大字符数，超出部分会被截断。
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// 标记一次截断由行数、字节数触发，或未发生截断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
    /// 因达到行数上限而截断
    Lines,
    /// 因达到字节上限而截断
    Bytes,
    /// 内容完整，未发生截断
    None,
}

/// 截断后的内容及其统计信息，供工具向模型展示截断状态。
#[derive(Debug, Clone)]
pub struct TruncationResult {
    /// 截断后实际返回的文本（未截断时为原文）
    pub content: String,
    /// 是否发生了截断（true 表示内容不完整）
    pub truncated: bool,
    /// 触发截断的限制类型，未截断为 None
    pub truncated_by: TruncatedBy,
    /// 原始内容总行数（不计末尾换行）
    pub total_lines: usize,
    /// 原始内容总字节数
    pub total_bytes: usize,
    /// 实际保留输出的行数
    pub output_lines: usize,
    /// 实际保留输出的字节数（含行间换行符）
    pub output_bytes: usize,
    /// 末尾行是否只保留了部分内容（tail 截断按字节切分时为 true）
    pub last_line_partial: bool,
    /// 首行是否超过字节上限导致内容被清空（仅 head 截断会出现）
    pub first_line_exceeds_limit: bool,
    /// 本次生效的行数上限
    pub max_lines: usize,
    /// 本次生效的字节上限
    pub max_bytes: usize,
}

pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// 与 TS 的 content.split("\n") + 末尾空行 pop 一致
fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return vec![];
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// 截断头部（保留开头）。用于文件读取。
/// 从不返回部分行；若第一行超过字节限制，返回空内容并标记 first_line_exceeds_limit。
pub fn truncate_head(content: &str, options: (Option<usize>, Option<usize>)) -> TruncationResult {
    let max_lines = options.0.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = options.1.unwrap_or(DEFAULT_MAX_BYTES);

    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
            truncated: false,
            truncated_by: TruncatedBy::None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    let first_line_bytes = lines.first().map(|l| l.len()).unwrap_or(0);
    if first_line_bytes > max_bytes {
        return TruncationResult {
            content: String::new(),
            truncated: true,
            truncated_by: TruncatedBy::Bytes,
            total_lines,
            total_bytes,
            output_lines: 0,
            output_bytes: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
            max_lines,
            max_bytes,
        };
    }

    let mut output_lines_arr: Vec<&str> = Vec::new();
    let mut output_bytes_count = 0usize;
    let mut truncated_by = TruncatedBy::Lines;

    for (i, line) in lines.iter().enumerate() {
        if i >= max_lines {
            break;
        }
        let line_bytes = line.len() + if i > 0 { 1 } else { 0 };
        if output_bytes_count + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        output_lines_arr.push(line);
        output_bytes_count += line_bytes;
    }

    if output_lines_arr.len() >= max_lines && output_bytes_count <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output_content = output_lines_arr.join("\n");
    let final_output_bytes = output_content.len();

    TruncationResult {
        content: output_content,
        truncated: true,
        truncated_by,
        total_lines,
        total_bytes,
        output_lines: output_lines_arr.len(),
        output_bytes: final_output_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// 截断尾部（保留末尾）。用于 bash 输出。
/// 若最后一行超过字节限制，可能返回该行的部分内容。
pub fn truncate_tail(content: &str, options: (Option<usize>, Option<usize>)) -> TruncationResult {
    let max_lines = options.0.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = options.1.unwrap_or(DEFAULT_MAX_BYTES);

    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
            truncated: false,
            truncated_by: TruncatedBy::None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    let mut output_lines_arr: Vec<String> = Vec::new();
    let mut output_bytes_count = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;

    let mut i = lines.len();
    while i > 0 && output_lines_arr.len() < max_lines {
        i -= 1;
        let line = lines[i];
        let line_bytes = line.len() + if !output_lines_arr.is_empty() { 1 } else { 0 };

        if output_bytes_count + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            if output_lines_arr.is_empty() {
                let truncated_line = truncate_string_to_bytes_from_end(line, max_bytes);
                output_lines_arr.insert(0, truncated_line);
                output_bytes_count = output_lines_arr[0].len();
                last_line_partial = true;
            }
            break;
        }

        output_lines_arr.insert(0, line.to_string());
        output_bytes_count += line_bytes;
    }

    if output_lines_arr.len() >= max_lines && output_bytes_count <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }

    let output_content = output_lines_arr.join("\n");
    let final_output_bytes = output_content.len();

    TruncationResult {
        content: output_content,
        truncated: true,
        truncated_by,
        total_lines,
        total_bytes,
        output_lines: output_lines_arr.len(),
        output_bytes: final_output_bytes,
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// 从末尾截断字符串到字节限制。Rust String 为 UTF-8，直接用字节切片。
fn truncate_string_to_bytes_from_end(s: &str, max_bytes: usize) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    let bytes = s.as_bytes();
    let mut used = 0usize;
    let mut out_start = bytes.len();
    let mut pos = bytes.len();
    while pos > 0 {
        let mut char_start = pos - 1;
        while char_start > 0 && (bytes[char_start] & 0xC0) == 0x80 {
            char_start -= 1;
        }
        let char_bytes = pos - char_start;
        if used + char_bytes > max_bytes {
            break;
        }
        used += char_bytes;
        out_start = char_start;
        pos = char_start;
    }
    s[out_start..].to_string()
}

/// 截断单行到最大字符数，追加 [truncated] 后缀（grep 用）
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    if line.chars().count() <= max_chars {
        return (line.to_string(), false);
    }
    let truncated: String = line.chars().take(max_chars).collect();
    (format!("{}... [truncated]", truncated), true)
}

/// 从字符串开头取至多 `max_bytes` 字节（回退到字符边界）。
pub fn head_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 从字符串结尾取至多 `max_bytes` 字节（回退到字符边界）。
pub fn tail_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}
