//! 文本查找与片段定位。
//!
//! 在给定文本中按 [`FindMode`] 查找一个或多个查询串，输出带上下文的可读片段与命中统计。

use strum_macros::{EnumString, IntoStaticStr};
use unicode_normalization::UnicodeNormalization;

/// 查找模式（对应 `findMode` 参数）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum FindMode {
    /// 区分大小写的字面匹配
    Exact,
    /// 不区分大小写的字面匹配（默认）
    #[default]
    #[strum(serialize = "case-insensitive")]
    CaseInsensitive,
    /// 按段落 + token 的编辑距离模糊匹配
    Fuzzy,
}

impl FindMode {
    /// 解析 `findMode` 取值；非法的取值返回 `None`。
    pub fn parse(value: &str) -> Option<Self> {
        value.parse().ok()
    }

    /// 与工具参数一致的字符串形式（用于错误信息与 details 输出）
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

impl std::fmt::Display for FindMode {
    /// 输出与工具参数一致的字符串形式（同 [`FindMode::as_str`]）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 单次命中
#[derive(Debug, Clone)]
struct Match {
    /// 产生该命中的查询串
    query: String,
    /// 命中在文本中的起始字节偏移
    start: usize,
    /// 命中在文本中的结束字节偏移（不含）
    end: usize,
}

/// 合并后的片段区间
#[derive(Debug, Clone)]
struct Range {
    /// 区间起始字节偏移（含前文上下文）
    start: usize,
    /// 区间结束字节偏移（含后文上下文，不含）
    end: usize,
    /// 落在该区间内的全部命中
    matches: Vec<Match>,
}

/// 检索结果
#[derive(Debug, Clone)]
pub struct FindResult {
    /// 格式化后的可读文本（含片段、计数与页脚说明）
    pub text: String,
    /// 命中总数（截断前）
    pub match_count: usize,
    /// 实际写入 `text` 的命中数（受 `MAX_OUTPUT_CHARS` 限制）
    pub returned_matches: usize,
    /// 每个查询的命中数（多查询时用于细节展示）
    #[allow(dead_code)]
    pub query_results: Vec<(String, usize)>,
}

/// 在文本中查找 `queries`（exact / case-insensitive / fuzzy）。
///
/// `context_chars`：每个命中点前后各保留多少字符作为上下文片段；
/// `max_output_chars`：生成文本的总长度上限（字符数），超出后丢弃剩余片段与页脚。
pub fn find_content(
    text: &str,
    queries: &[String],
    mode: FindMode,
    context_chars: usize,
    max_output_chars: usize,
) -> FindResult {
    let mut normalized: Vec<String> = Vec::new();
    for query in queries {
        let trimmed = query.trim();
        if !trimmed.is_empty() && !normalized.iter().any(|q| q == trimmed) {
            normalized.push(trimmed.to_string());
        }
    }

    let mut matches: Vec<Match> = Vec::new();
    for query in &normalized {
        match mode {
            FindMode::Fuzzy => matches.extend(fuzzy_matches(text, query)),
            FindMode::Exact => matches.extend(literal_matches(text, query, false)),
            FindMode::CaseInsensitive => matches.extend(literal_matches(text, query, true)),
        }
    }
    let query_results: Vec<(String, usize)> = normalized
        .iter()
        .map(|query| {
            let count = matches.iter().filter(|m| &m.query == query).count();
            (query.clone(), count)
        })
        .collect();

    let heading = if matches.is_empty() {
        format!("Text matches ({mode}): no matches")
    } else {
        format!("Text matches ({mode})")
    };
    let mut sections = vec![heading.clone()];
    let mut formatted_length = heading.len();
    let mut returned_matches = 0usize;

    for range in merge_ranges(text.len(), &matches, context_chars) {
        let prefix = if range.start > 0 { "…" } else { "" };
        let suffix = if range.end < text.len() { "…" } else { "" };
        let slice = char_slice(text, range.start, range.end);
        let snippet = format!(
            "{prefix}{}{suffix}",
            slice.split_whitespace().collect::<Vec<_>>().join(" ")
        );
        let mut counts: Vec<String> = Vec::new();
        for query in unique_queries(&range.matches) {
            let count = range.matches.iter().filter(|m| m.query == query).count();
            counts.push(format!("\"{query}\" ×{count}"));
        }
        let section = format!("{}. {}\n{snippet}", sections.len(), counts.join(", "));
        if formatted_length + 2 + section.len() > max_output_chars {
            break;
        }
        formatted_length += 2 + section.len();
        sections.push(section);
        returned_matches += range.matches.len();
    }

    let missing: Vec<String> = query_results
        .iter()
        .filter(|(_, count)| *count == 0)
        .map(|(query, _)| format!("\"{query}\""))
        .collect();
    let mut footer = Vec::new();
    if !missing.is_empty() {
        footer.push(format!("No matches: {}", missing.join(", ")));
    }
    if returned_matches < matches.len() {
        footer.push(format!(
            "Showing {returned_matches} of {} matches.",
            matches.len()
        ));
    }
    for section in footer {
        if formatted_length + 2 + section.len() > max_output_chars {
            break;
        }
        formatted_length += 2 + section.len();
        sections.push(section);
    }

    FindResult {
        text: sections.join("\n\n"),
        match_count: matches.len(),
        returned_matches,
        query_results,
    }
}

/// 按出现顺序去重，保留 `matches` 中首次出现的查询串。
fn unique_queries(matches: &[Match]) -> Vec<String> {
    let mut out = Vec::new();
    for m in matches {
        if !out.contains(&m.query) {
            out.push(m.query.clone());
        }
    }
    out
}

/// 字面匹配：返回 `query` 在 `text` 中的全部出现位置。
/// `case_insensitive` 为真时先对两侧做小写折叠（偏移按折叠后字节长度计）。
fn literal_matches(text: &str, query: &str, case_insensitive: bool) -> Vec<Match> {
    let haystack = if case_insensitive {
        text.to_lowercase()
    } else {
        text.to_string()
    };
    let needle = if case_insensitive {
        query.to_lowercase()
    } else {
        query.to_string()
    };
    let mut matches = Vec::new();
    if needle.is_empty() {
        return matches;
    }
    let mut search_from = 0usize;
    while let Some(found) = haystack[search_from..].find(&needle) {
        let start = search_from + found;
        matches.push(Match {
            query: query.to_string(),
            start,
            end: start + needle.len(),
        });
        search_from = start + needle.len().max(1);
        if search_from > haystack.len() {
            break;
        }
    }
    matches
}

/// NFD 去音标 + 小写（近似原版 `normalize`）
fn normalize(value: &str) -> String {
    value
        .nfd()
        .filter(|c| !is_combining(*c))
        .collect::<String>()
        .to_lowercase()
}

/// 是否为 Unicode 组合用记号（音标/变音符号），用于 NFD 后剔除。
fn is_combining(c: char) -> bool {
    matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

/// 模糊匹配：按段落切分，查询被切成 token；
/// 单 token 查询需命中 1 个，多 token 查询需命中过半，命中段落取首个命中 token 的位置。
fn fuzzy_matches(text: &str, query: &str) -> Vec<Match> {
    let query_tokens: Vec<String> = tokenize(&normalize(query));
    if query_tokens.is_empty() {
        return Vec::new();
    }
    let mut matches = Vec::new();
    for (paragraph_start, paragraph) in paragraphs(text) {
        if paragraph.trim().is_empty() {
            continue;
        }
        let tokens = tokenize_with_offsets(&paragraph);
        let normalized_tokens: Vec<(String, usize, usize)> = tokens
            .iter()
            .map(|(token, start, end)| (normalize(token), *start, *end))
            .collect();
        let mut matched = 0usize;
        for query_token in &query_tokens {
            let threshold = fuzzy_threshold(query_token.len());
            if normalized_tokens
                .iter()
                .any(|(candidate, _, _)| edit_distance_within(query_token, candidate, threshold))
            {
                matched += 1;
            }
        }
        let required = if query_tokens.len() == 1 {
            1
        } else {
            query_tokens.len().div_ceil(2).max(1)
        };
        if matched < required {
            continue;
        }
        if let Some((_, start, end)) = normalized_tokens.iter().find(|(candidate, _, _)| {
            query_tokens
                .iter()
                .any(|q| edit_distance_within(q, candidate, fuzzy_threshold(q.len())))
        }) {
            matches.push(Match {
                query: query.to_string(),
                start: paragraph_start + start,
                end: paragraph_start + end,
            });
        }
    }
    matches
}

/// 按 token 长度决定允许的编辑距离：≥ 9 允许 2，≥ 5 允许 1，否则必须精确（0）。
fn fuzzy_threshold(len: usize) -> usize {
    if len >= 9 {
        2
    } else if len >= 5 {
        1
    } else {
        0
    }
}

/// 按 Unicode 字母/数字切分出 token（丢弃标点与空白）。
fn tokenize(text: &str) -> Vec<String> {
    tokenize_with_offsets(text)
        .into_iter()
        .map(|(token, _, _)| token)
        .collect()
}

/// 同 [`tokenize`]，但返回每个 token 及其在 `text` 中的字节区间。
fn tokenize_with_offsets(text: &str) -> Vec<(String, usize, usize)> {
    let mut tokens = Vec::new();
    let mut current_start: Option<usize> = None;
    for (index, ch) in text.char_indices() {
        if ch.is_alphanumeric() {
            if current_start.is_none() {
                current_start = Some(index);
            }
        } else if let Some(start) = current_start.take() {
            tokens.push((text[start..index].to_string(), start, index));
        }
    }
    if let Some(start) = current_start {
        tokens.push((text[start..].to_string(), start, text.len()));
    }
    tokens
}

/// 段落切分：连续非空行算一段
fn paragraphs(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut current = String::new();
    let mut current_start = 0usize;
    for line in text.split_inclusive('\n') {
        if line.trim().is_empty() {
            if !current.is_empty() {
                out.push((current_start, std::mem::take(&mut current)));
            }
            start += line.len();
            continue;
        }
        if current.is_empty() {
            current_start = start;
        }
        current.push_str(line);
        start += line.len();
    }
    if !current.is_empty() {
        out.push((current_start, current));
    }
    out
}

/// 计算两个 token 的编辑距离是否不超过 `maximum`（带提前剪枝）。
fn edit_distance_within(left: &str, right: &str, maximum: usize) -> bool {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    if left.len().abs_diff(right.len()) > maximum {
        return false;
    }
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for i in 1..=left.len() {
        let mut current = vec![i];
        let mut row_minimum = i;
        for j in 1..=right.len() {
            let value = (previous[j] + 1)
                .min(current[j - 1] + 1)
                .min(previous[j - 1] + usize::from(left[i - 1] != right[j - 1]));
            current.push(value);
            row_minimum = row_minimum.min(value);
        }
        if row_minimum > maximum {
            return false;
        }
        previous = current;
    }
    previous[right.len()] <= maximum
}

/// 将命中按位置合并为区间：每个命中向两侧扩张 `context_chars`，
/// 相邻/重叠的区间（`start <= 前区间 end`）合并为一个，避免片段重复。
fn merge_ranges(text_length: usize, matches: &[Match], context_chars: usize) -> Vec<Range> {
    let mut sorted: Vec<Match> = matches.to_vec();
    sorted.sort_by_key(|m| m.start);
    let mut ranges: Vec<Range> = Vec::new();
    for m in sorted {
        let start = m.start.saturating_sub(context_chars);
        let end = (m.end + context_chars).min(text_length);
        if let Some(previous) = ranges.last_mut()
            && start <= previous.end
        {
            previous.end = previous.end.max(end);
            previous.matches.push(m);
            continue;
        }
        ranges.push(Range {
            start,
            end,
            matches: vec![m],
        });
    }
    ranges
}

/// 按字节边界取子串（调用方保证落在字符边界上：start/end 来自 find 或 token 边界）
fn char_slice(text: &str, start: usize, end: usize) -> String {
    let start = floor_char_boundary(text, start.min(text.len()));
    let end = ceil_char_boundary(text, end.min(text.len()));
    text.get(start..end).unwrap_or("").to_string()
}

/// 向下取整到最近的字符边界（`index` 超出范围时截断到 `text.len()`）。
fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// 向上取整到最近的字符边界。
fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用限制值（与 `web_access::store` 的默认值一致）
    const CONTEXT_CHARS: usize = 400;
    const MAX_OUTPUT_CHARS: usize = 20_000;

    /// 以默认限制调用 [`find_content`] 的测试封装
    fn find(text: &str, queries: &[&str], mode: FindMode) -> FindResult {
        let queries: Vec<String> = queries.iter().map(|q| q.to_string()).collect();
        find_content(text, &queries, mode, CONTEXT_CHARS, MAX_OUTPUT_CHARS)
    }

    #[test]
    fn find_mode_parses_known_values_only() {
        assert_eq!(FindMode::parse("exact"), Some(FindMode::Exact));
        assert_eq!(
            FindMode::parse("case-insensitive"),
            Some(FindMode::CaseInsensitive)
        );
        assert_eq!(FindMode::parse("fuzzy"), Some(FindMode::Fuzzy));
        assert_eq!(FindMode::parse("Fuzzy"), None);
        assert_eq!(FindMode::parse(""), None);
        assert_eq!(FindMode::default(), FindMode::CaseInsensitive);
    }

    #[test]
    fn find_literal_and_case_insensitive() {
        let text = "Install the tool. Then install again. INSTALL everywhere.";
        let exact = find(text, &["install"], FindMode::Exact);
        assert_eq!(exact.match_count, 1);
        let ci = find(text, &["install"], FindMode::CaseInsensitive);
        assert_eq!(ci.match_count, 3);
        assert!(ci.text.contains("Text matches"));
        let none = find(text, &["absent"], FindMode::CaseInsensitive);
        assert!(none.text.contains("no matches"));
        assert!(none.text.contains("No matches: \"absent\""));
    }

    #[test]
    fn find_fuzzy_tolerates_typos() {
        let text = "The configuration file controls timeouts.\n\nAnother paragraph here.";
        let result = find(text, &["configration"], FindMode::Fuzzy);
        assert_eq!(result.match_count, 1, "{}", result.text);
    }

    #[test]
    fn find_is_utf8_safe() {
        let text = "中文测试内容，包含关键词搜索。再次搜索。";
        let result = find(text, &["搜索"], FindMode::CaseInsensitive);
        assert_eq!(result.match_count, 2);
        // 不应 panic
        assert!(result.text.contains("搜索"));
    }
}
