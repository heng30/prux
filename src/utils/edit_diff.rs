// 编辑匹配、模糊匹配、统一补丁与显示 diff

use similar::{DiffTag, TextDiff};
use unicode_normalization::UnicodeNormalization;

/// 一次文本编辑请求：待匹配的原文片段与替换后的新文本。
#[derive(Debug, Clone)]
pub struct Edit {
    /// 待匹配并替换的原文片段，须在文件中唯一出现。
    pub old_text: String,
    /// 用于替换命中片段的新文本。
    pub new_text: String,
}

/// 判断内容的行尾风格：首个 `\r\n` 早于首个 `\n` 时返回 `"\r\n"`，否则返回 `"\n"`
/// （无换行也按 LF 处理）。
pub fn detect_line_ending(content: &str) -> &'static str {
    let crlf_idx = content.find("\r\n");
    let lf_idx = content.find('\n');
    match (crlf_idx, lf_idx) {
        (None, _) => "\n",
        (_, None) => "\n",
        (Some(c), Some(l)) => {
            if c < l {
                "\r\n"
            } else {
                "\n"
            }
        }
    }
}

/// 把 `\r\n` 与孤立 `\r` 统一成 `\n`（后续匹配一律在 LF 基准上进行）。
pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 还原行尾：`ending` 为 `"\r\n"` 时把 `\n` 转回 CRLF，否则原样返回。
pub fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

/// 模糊匹配归一化：NFKC + 行尾空白剥离 + Unicode 引号/破折号/空格归一
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    let mut out = String::new();
    for line in nfkc.split('\n') {
        let trimmed = line.trim_end();
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(trimmed);
    }
    // 智能单引号 → '
    let out = out
        .replace(['\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'], "'")
        .replace(['\u{201C}', '\u{201D}', '\u{201E}', '\u{201F}'], "\"")
        .replace(
            [
                '\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{2212}',
            ],
            "-",
        );
    // 特殊空格 → 普通空格
    let mut result = String::with_capacity(out.len());
    for c in out.chars() {
        let cp = c as u32;
        if cp == 0x00A0
            || (0x2002..=0x200A).contains(&cp)
            || cp == 0x202F
            || cp == 0x205F
            || cp == 0x3000
        {
            result.push(' ');
        } else {
            result.push(c);
        }
    }
    result
}

/// 模糊查找结果：命中位置与长度、是否经过归一化，以及可替换的基准内容。
#[derive(Debug)]
pub struct FuzzyMatchResult {
    /// 是否命中（精确匹配或归一化后匹配）。
    pub found: bool,
    /// 命中片段在基准内容中的起始字节偏移；未命中时为 0。
    pub index: usize,
    /// 命中片段的字节长度；未命中时为 0。
    pub match_length: usize,
    /// true 表示精确匹配失败、经归一化后才命中。
    pub used_fuzzy_match: bool,
    /// 实际用于定位和替换的基准内容（模糊命中时为归一化后的文本）。
    pub content_for_replacement: String,
}

/// 先在原文里精确查找，失败再把双方 NFKC/引号/破折号/空白归一后重查。
///
/// 返回命中偏移、长度、是否走了模糊路径，以及实际用于替换的基准内容
/// （模糊命中时是归一化后的文本）；两轮都未命中时 `found = false`。
pub fn fuzzy_find_text(content: &str, old_text: &str) -> FuzzyMatchResult {
    if let Some(idx) = content.find(old_text) {
        return FuzzyMatchResult {
            found: true,
            index: idx,
            match_length: old_text.len(),
            used_fuzzy_match: false,
            content_for_replacement: content.to_string(),
        };
    }
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    if let Some(idx) = fuzzy_content.find(&fuzzy_old_text) {
        return FuzzyMatchResult {
            found: true,
            index: idx,
            match_length: fuzzy_old_text.len(),
            used_fuzzy_match: true,
            content_for_replacement: fuzzy_content,
        };
    }
    FuzzyMatchResult {
        found: false,
        index: 0,
        match_length: 0,
        used_fuzzy_match: false,
        content_for_replacement: content.to_string(),
    }
}

/// 拆出开头的 UTF-8 BOM：返回 `(BOM 前缀, 其余内容)`，无 BOM 时前缀为空串。
pub fn strip_bom(content: &str) -> (String, String) {
    if let Some(rest) = content.strip_prefix('\u{FEFF}') {
        ("\u{FEFF}".to_string(), rest.to_string())
    } else {
        (String::new(), content.to_string())
    }
}

/// 在模糊归一后的内容里数 `old_text` 的出现次数（空 `old_text` 记 0），
/// 用于判定待替换文本是否唯一。
fn count_occurrences(content: &str, old_text: &str) -> usize {
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old_text = normalize_for_fuzzy_match(old_text);
    if fuzzy_old_text.is_empty() {
        return 0;
    }
    fuzzy_content.match_indices(&fuzzy_old_text).count()
}

/// 编辑应用结果：替换前的基准内容与替换后的内容，供上层生成 diff。
#[derive(Debug)]
pub struct AppliedEditsResult {
    /// 归一化后的替换前基准内容。
    pub base_content: String,
    /// 应用全部编辑后的新内容。
    pub new_content: String,
}

/// 生成“找不到待替换文本”的错误消息；单次编辑与多次编辑（带 `edits[i]` 定位）措辞不同。
fn get_not_found_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "Could not find the exact text in {}. The old text must match exactly including all whitespace and newlines.",
            path
        )
    } else {
        format!(
            "Could not find edits[{}] in {}. The oldText must match exactly including all whitespace and newlines.",
            edit_index, path
        )
    }
}

/// 生成“命中不唯一”的错误消息（`occurrences` 为实际命中次数），提示补充上下文使其唯一。
fn get_duplicate_error(
    path: &str,
    edit_index: usize,
    total_edits: usize,
    occurrences: usize,
) -> String {
    if total_edits == 1 {
        format!(
            "Found {} occurrences of the text in {}. The text must be unique. Please provide more context to make it unique.",
            occurrences, path
        )
    } else {
        format!(
            "Found {} occurrences of edits[{}] in {}. Each oldText must be unique. Please provide more context to make it unique.",
            occurrences, edit_index, path
        )
    }
}

/// 生成 `oldText` 为空的错误消息（单次 / 多次编辑两种措辞）。
fn get_empty_old_text_error(path: &str, edit_index: usize, total_edits: usize) -> String {
    if total_edits == 1 {
        format!("oldText must not be empty in {}.", path)
    } else {
        format!(
            "edits[{}].oldText must not be empty in {}.",
            edit_index, path
        )
    }
}

/// 生成“替换后内容与原文相同”的错误消息（单次编辑额外提示可能是特殊字符或文本不符预期）。
fn get_no_change_error(path: &str, total_edits: usize) -> String {
    if total_edits == 1 {
        format!(
            "No changes made to {}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected.",
            path
        )
    } else {
        format!(
            "No changes made to {}. The replacements produced identical content.",
            path
        )
    }
}

/// 已定位的编辑：记录它在基准内容中的偏移、匹配长度与替换文本。
#[derive(Debug, Clone)]
struct MatchedEdit {
    /// 对应输入 edits 数组中的下标，用于报错定位。
    edit_index: usize,
    /// 在基准内容中的起始字节偏移。
    match_index: usize,
    /// 被替换片段的字节长度。
    match_length: usize,
    /// 替换进去的文本。
    new_text: String,
}

/// 区间替换操作的别名，供 apply_replacements 系列按偏移执行替换。
type TextReplacement = MatchedEdit;

/// 一行在基准内容中的字节区间，用于把替换区间映射成整行范围。
struct LineSpan {
    /// 该行在内容中的起始字节偏移。
    start: usize,
    /// 该行的结束字节偏移（含行尾换行符）。
    end: usize,
}

/// 按行切分且保留行尾换行符（等价 JS `/[^\n]*\n|[^\n]+/g`），末行无换行时也作为一行。
fn split_lines_with_endings(content: &str) -> Vec<&str> {
    // 与 JS regex /[^\n]*\n|[^\n]+/g 等价
    let mut result = Vec::new();
    let mut rest = content;
    while !rest.is_empty() {
        if let Some(pos) = rest.find('\n') {
            result.push(&rest[..=pos]);
            rest = &rest[pos + 1..];
        } else {
            result.push(rest);
            break;
        }
    }
    result
}

/// 计算每行在内容中的字节区间（`end` 含行尾换行符），供替换区间映射到整行范围。
fn get_line_spans(content: &str) -> Vec<LineSpan> {
    let mut offset = 0;
    split_lines_with_endings(content)
        .iter()
        .map(|line| {
            let span = LineSpan {
                start: offset,
                end: offset + line.len(),
            };
            offset = span.end;
            span
        })
        .collect()
}

/// 把一个替换区间映射成覆盖它的整行下标范围 `[start, end)`。
/// 起点或终点落在基准内容之外时返回 `Err`。
fn get_replacement_line_range(
    lines: &[LineSpan],
    replacement: &TextReplacement,
) -> Result<(usize, usize), String> {
    let replacement_start = replacement.match_index;
    let replacement_end = replacement.match_index + replacement.match_length;

    let mut start_line = -1i64;
    for (i, line) in lines.iter().enumerate() {
        if replacement_start >= line.start && replacement_start < line.end {
            start_line = i as i64;
            break;
        }
    }
    if start_line == -1 {
        return Err("Replacement range is outside the base content.".to_string());
    }

    let mut end_line = start_line as usize;
    while end_line < lines.len() && lines[end_line].end < replacement_end {
        end_line += 1;
    }
    if end_line >= lines.len() {
        return Err("Replacement range is outside the base content.".to_string());
    }

    Ok((start_line as usize, end_line + 1))
}

/// 按偏移在 `content` 上从后往前执行替换（倒序保证前面的偏移不受影响）；
/// `offset` 是这些偏移的基准起点。
fn apply_replacements(content: &str, replacements: &[TextReplacement], offset: usize) -> String {
    let mut result = content.to_string();
    for replacement in replacements.iter().rev() {
        let match_index = replacement.match_index - offset;
        let before = &result[..match_index];
        let after = &result[match_index + replacement.match_length..];
        result = format!("{}{}{}", before, replacement.new_text, after);
    }
    result
}

/// 在归一化 base 上匹配替换，但保留未改行的原始字节
fn apply_replacements_preserving_unchanged_lines(
    original_content: &str,
    base_content: &str,
    replacements: &[TextReplacement],
) -> Result<String, String> {
    let original_lines = split_lines_with_endings(original_content);
    let base_lines = get_line_spans(base_content);
    if original_lines.len() != base_lines.len() {
        return Err(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .to_string(),
        );
    }

    let mut sorted: Vec<TextReplacement> = replacements.to_vec();
    sorted.sort_by_key(|r| r.match_index);

    let mut groups: Vec<(usize, usize, Vec<TextReplacement>)> = Vec::new();
    for replacement in sorted {
        let (start_line, end_line) = get_replacement_line_range(&base_lines, &replacement)?;
        if let Some(last) = groups.last_mut()
            && start_line < last.1
        {
            last.1 = last.1.max(end_line);
            last.2.push(replacement.clone());
            continue;
        }
        groups.push((start_line, end_line, vec![replacement.clone()]));
    }

    let mut original_line_index = 0;
    let mut result = String::new();
    for (group_start, group_end, group_replacements) in &groups {
        result.push_str(&original_lines[original_line_index..*group_start].join(""));

        let group_start_offset = base_lines[*group_start].start;
        let group_end_offset = base_lines[*group_end - 1].end;
        let slice = &base_content[group_start_offset..group_end_offset];
        result.push_str(&apply_replacements(
            slice,
            group_replacements,
            group_start_offset,
        ));
        original_line_index = *group_end;
    }
    result.push_str(&original_lines[original_line_index..].join(""));

    Ok(result)
}

/// 在 LF 归一后的内容上应用一批编辑，返回替换前基准内容与替换后内容供上层出 diff。
///
/// 依次做：空 `oldText` 检查 → 精确/模糊匹配 → 唯一性检查 → 重叠检查 → 应用替换。
/// 找不到文本、命中多次、区间重叠、替换后无变化均以面向用户的 `Err(String)` 返回。
pub fn apply_edits_to_normalized_content(
    normalized_content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<AppliedEditsResult, String> {
    let normalized_edits: Vec<Edit> = edits
        .iter()
        .map(|e| Edit {
            old_text: normalize_to_lf(&e.old_text),
            new_text: normalize_to_lf(&e.new_text),
        })
        .collect();

    for (i, edit) in normalized_edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(get_empty_old_text_error(path, i, normalized_edits.len()));
        }
    }

    let initial_matches: Vec<FuzzyMatchResult> = normalized_edits
        .iter()
        .map(|e| fuzzy_find_text(normalized_content, &e.old_text))
        .collect();
    let used_fuzzy_match = initial_matches.iter().any(|m| m.used_fuzzy_match);
    let replacement_base_content = if used_fuzzy_match {
        normalize_for_fuzzy_match(normalized_content)
    } else {
        normalized_content.to_string()
    };

    let mut matched_edits: Vec<MatchedEdit> = Vec::new();
    for (i, edit) in normalized_edits.iter().enumerate() {
        let match_result = fuzzy_find_text(&replacement_base_content, &edit.old_text);
        if !match_result.found {
            return Err(get_not_found_error(path, i, normalized_edits.len()));
        }
        let occurrences = count_occurrences(&replacement_base_content, &edit.old_text);
        if occurrences > 1 {
            return Err(get_duplicate_error(
                path,
                i,
                normalized_edits.len(),
                occurrences,
            ));
        }
        matched_edits.push(MatchedEdit {
            edit_index: i,
            match_index: match_result.index,
            match_length: match_result.match_length,
            new_text: edit.new_text.clone(),
        });
    }

    matched_edits.sort_by_key(|m| m.match_index);
    for i in 1..matched_edits.len() {
        let previous = &matched_edits[i - 1];
        let current = &matched_edits[i];
        if previous.match_index + previous.match_length > current.match_index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {}. Merge them into one edit or target disjoint regions.",
                previous.edit_index, current.edit_index, path
            ));
        }
    }

    let base_content = normalized_content.to_string();
    let new_content = if used_fuzzy_match {
        apply_replacements_preserving_unchanged_lines(
            normalized_content,
            &replacement_base_content,
            &matched_edits,
        )?
    } else {
        apply_replacements(&replacement_base_content, &matched_edits, 0)
    };

    if base_content == new_content {
        return Err(get_no_change_error(path, normalized_edits.len()));
    }

    Ok(AppliedEditsResult {
        base_content,
        new_content,
    })
}

/// 生成统一补丁（对齐 jsdiff createTwoFilesPatch，context=4, FILE_HEADERS_ONLY）
pub fn generate_unified_patch(
    path: &str,
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> String {
    let diff = TextDiff::from_lines(old_content, new_content);
    diff.unified_diff()
        .header(path, path)
        .context_radius(context_lines)
        .to_string()
}

/// 生成带行号的显示用 diff
pub fn generate_diff_string(
    old_content: &str,
    new_content: &str,
    context_lines: usize,
) -> (String, Option<usize>) {
    let diff = TextDiff::from_lines(old_content, new_content);
    let old_lines: Vec<&str> = old_content.split('\n').collect();
    let new_lines: Vec<&str> = new_content.split('\n').collect();
    let max_line_num = old_lines.len().max(new_lines.len());
    let line_num_width = max_line_num.to_string().len();

    // 先把每个 op 展开成 part（每行带前缀）
    let mut parts: Vec<Vec<(char, String)>> = Vec::new();
    for op in diff.ops() {
        let mut raw: Vec<String> = Vec::new();
        let mut is_change = false;
        match op.tag() {
            DiffTag::Equal => {
                for i in op.old_range() {
                    if let Some(line) = diff.old_slice(i) {
                        raw.push(line.strip_suffix('\n').unwrap_or(line).to_string());
                    }
                }
            }
            DiffTag::Delete => {
                is_change = true;
                for i in op.old_range() {
                    if let Some(line) = diff.old_slice(i) {
                        raw.push(line.strip_suffix('\n').unwrap_or(line).to_string());
                    }
                }
            }
            DiffTag::Insert => {
                is_change = true;
                for i in op.new_range() {
                    if let Some(line) = diff.new_slice(i) {
                        raw.push(line.strip_suffix('\n').unwrap_or(line).to_string());
                    }
                }
            }
            DiffTag::Replace => {
                is_change = true;
                for i in op.old_range() {
                    if let Some(line) = diff.old_slice(i) {
                        raw.push(line.strip_suffix('\n').unwrap_or(line).to_string());
                    }
                }
                for i in op.new_range() {
                    if let Some(line) = diff.new_slice(i) {
                        raw.push(line.strip_suffix('\n').unwrap_or(line).to_string());
                    }
                }
            }
        }

        // 给行加前缀
        let mut prefixed: Vec<(char, String)> = Vec::new();
        if is_change {
            let old_count = match op.tag() {
                DiffTag::Equal | DiffTag::Delete | DiffTag::Replace => op.old_range().len(),
                _ => 0,
            };
            for (idx, line) in raw.iter().enumerate() {
                let prefix = match op.tag() {
                    DiffTag::Insert => '+',
                    DiffTag::Equal => ' ',
                    _ => {
                        if idx < old_count {
                            '-'
                        } else {
                            '+'
                        }
                    }
                };
                prefixed.push((prefix, line.clone()));
            }
        } else {
            for line in raw {
                prefixed.push((' ', line));
            }
        }
        parts.push(prefixed);
    }

    // 相邻的变更 part 合并（jsdiff 中 added/removed 相邻会合并成同一个变更显示区）
    let mut merged: Vec<(bool, Vec<(char, String)>)> = Vec::new();
    for part in parts {
        let is_change = part.iter().any(|(c, _)| *c != ' ');
        if let Some(last) = merged.last_mut()
            && last.0 == is_change
        {
            last.1.extend(part);
            continue;
        }
        merged.push((is_change, part));
    }

    let mut output: Vec<String> = Vec::new();
    let mut old_line_num = 1usize;
    let mut new_line_num = 1usize;
    let mut last_was_change = false;
    let mut first_changed_line: Option<usize> = None;

    let n = merged.len();
    for (i, (is_change, lines)) in merged.iter().enumerate() {
        if *is_change {
            if first_changed_line.is_none() {
                first_changed_line = Some(new_line_num);
            }
            for (prefix, line) in lines {
                let padded = format!(
                    "{:>width$}",
                    old_line_num.min(new_line_num),
                    width = line_num_width
                );
                if *prefix == '+' {
                    let line_num = format!("{}", new_line_num);
                    let padded = format!("{:>width$}", line_num, width = line_num_width);
                    output.push(format!("+{} {}", padded, line));
                    new_line_num += 1;
                } else if *prefix == '-' {
                    let line_num = format!("{}", old_line_num);
                    let padded = format!("{:>width$}", line_num, width = line_num_width);
                    output.push(format!("-{} {}", padded, line));
                    old_line_num += 1;
                } else {
                    let _ = padded;
                    let line_num = format!("{}", old_line_num);
                    let padded = format!("{:>width$}", line_num, width = line_num_width);
                    output.push(format!(" {} {}", padded, line));
                    old_line_num += 1;
                    new_line_num += 1;
                }
            }
            last_was_change = true;
        } else {
            let next_part_is_change = i < n - 1 && merged[i + 1].0;
            let has_leading_change = last_was_change;
            let has_trailing_change = next_part_is_change;
            let raw = lines;

            if has_leading_change && has_trailing_change {
                if raw.len() <= context_lines * 2 {
                    for (_, line) in raw {
                        let line_num = format!("{}", old_line_num);
                        let padded = format!("{:>width$}", line_num, width = line_num_width);
                        output.push(format!(" {} {}", padded, line));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                } else {
                    let leading_lines = &raw[..context_lines.min(raw.len())];
                    let trailing_start = raw.len().saturating_sub(context_lines);
                    let trailing_lines = &raw[trailing_start..];
                    let skipped_lines = raw.len() - leading_lines.len() - trailing_lines.len();

                    for (_, line) in leading_lines {
                        let line_num = format!("{}", old_line_num);
                        let padded = format!("{:>width$}", line_num, width = line_num_width);
                        output.push(format!(" {} {}", padded, line));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                    output.push(format!(" {} ...", "".pad_left(line_num_width)));
                    old_line_num += skipped_lines;
                    new_line_num += skipped_lines;
                    for (_, line) in trailing_lines {
                        let line_num = format!("{}", old_line_num);
                        let padded = format!("{:>width$}", line_num, width = line_num_width);
                        output.push(format!(" {} {}", padded, line));
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                }
            } else if has_leading_change {
                let shown = raw.len().min(context_lines);
                for (_, line) in &raw[..shown] {
                    let line_num = format!("{}", old_line_num);
                    let padded = format!("{:>width$}", line_num, width = line_num_width);
                    output.push(format!(" {} {}", padded, line));
                    old_line_num += 1;
                    new_line_num += 1;
                }
                let skipped = raw.len() - shown;
                if skipped > 0 {
                    output.push(format!(" {} ...", "".pad_left(line_num_width)));
                    old_line_num += skipped;
                    new_line_num += skipped;
                }
            } else if has_trailing_change {
                let skipped = raw.len().saturating_sub(context_lines);
                if skipped > 0 {
                    output.push(format!(" {} ...", "".pad_left(line_num_width)));
                    old_line_num += skipped;
                    new_line_num += skipped;
                }
                for (_, line) in &raw[skipped..] {
                    let line_num = format!("{}", old_line_num);
                    let padded = format!("{:>width$}", line_num, width = line_num_width);
                    output.push(format!(" {} {}", padded, line));
                    old_line_num += 1;
                    new_line_num += 1;
                }
            } else {
                old_line_num += raw.len();
                new_line_num += raw.len();
            }
            last_was_change = false;
        }
    }

    (output.join("\n"), first_changed_line)
}

trait PadLeft {
    /// 把字符串左填充到至少 `width` 个字符（足够宽则原样返回）。
    fn pad_left(&self, width: usize) -> String;
}

impl PadLeft for str {
    /// 用空格把左侧补齐到 `width`；已达标时返回原串副本（按字节长度判断）。
    fn pad_left(&self, width: usize) -> String {
        if self.len() >= width {
            self.to_string()
        } else {
            format!("{}{}", " ".repeat(width - self.len()), self)
        }
    }
}
