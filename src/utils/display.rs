//! 终端显示宽度与折行/截断

use crate::error::{Error, Result};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// 终端显示宽度：CJK/全角/emoji 为 2 列，控制字符 0 列，其余 1 列
pub fn char_width(c: char) -> usize {
    match c {
        '\t' => 4,
        c if (c as u32) < 32 || (0x7f..=0x9f).contains(&(c as u32)) => 0,
        c => UnicodeWidthChar::width(c).unwrap_or(1).max(1),
    }
}

/// 字符串的终端显示列数（按 [`char_width`] 逐字符累加）。
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// ratatui 落格宽度：与 `Buffer::set_stringn` 口径一致——按 grapheme 切分、
/// 跳过含控制字符的 grapheme，再按整体宽度累加（零宽 grapheme 贡献 0）。
/// 用于背景填充/右对齐：必须与实际绘制宽度一致，否则右侧会少补/多补空格。
pub fn grapheme_width(s: &str) -> usize {
    UnicodeSegmentation::graphemes(s, true)
        .filter(|g| !g.contains(char::is_control))
        .map(UnicodeWidthStr::width)
        .sum()
}

/// 规范化子进程原始输出：剥离 ANSI 转义序列，并把制表符展开为 4 个空格。
/// 工具输出可能带颜色转义 / tab，折行与背景填充都基于显示宽度，
/// 控制/转义字节会让 prux 口径与 ratatui 落格口径漂移（右侧背景缺口）。
pub fn sanitize_output(s: &str) -> String {
    // 快路径：无转义、无 tab（绝大多数工具输出）→ 直接整段拷贝，跳过逐字符状态机
    if !s.contains('\u{1b}') && !s.contains('\t') {
        return s.to_string();
    }

    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek().copied() {
                // CSI: ESC [ 参数 终止字节(0x40..=0x7e)
                Some('[') => {
                    // CSI 终止字节 0x40..=0x7e
                    for n in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC: ESC ] ... BEL 或 ESC \
                Some(']') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\u{7}' {
                            break;
                        }
                        if n == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // 孤立 ESC / 其他双字节转义：丢弃 ESC 与紧随的一个字节
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            '\t' => out.push_str("    "),
            _ => out.push(c),
        }
    }
    out
}

/// 按显示宽度软折行（不切断字符）
pub fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut result = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for c in line.chars() {
        let cw = char_width(c);
        if cw == 0 {
            continue; // 控制字符不显示
        }
        if current_width + cw > width {
            result.push(current);
            current = String::new();
            current_width = 0;
        }
        current.push(c);
        current_width += cw;
    }
    if !current.is_empty() || result.is_empty() {
        result.push(current);
    }
    result
}

/// 按词折行：优先在空白处断行；单个词超过宽度时按显示宽度硬断（CJK 无空格也适用）。
pub fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;

    for word in text.split_whitespace() {
        let w = display_width(word);
        if !cur.is_empty() && cur_w + 1 + w <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_w += 1 + w;
        } else if w <= width {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }

            cur.push_str(word);
            cur_w = w;
        } else {
            // 超长词：硬断，最后一段作为当前行继续拼接后续词
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }

            let mut pieces = wrap_line(word, width);
            cur = pieces.pop().unwrap_or_default();
            cur_w = display_width(&cur);
            out.extend(pieces);
        }
    }

    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// 截取前 n 个显示宽度（用于折叠摘要）
pub fn truncate_display(s: &str, max_width: usize) -> (String, bool) {
    let mut out = String::new();
    let mut w = 0usize;
    let mut truncated = false;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw > max_width {
            truncated = true;
            break;
        }
        w += cw;
        out.push(c);
    }
    (out, truncated)
}

/// 把 `\r\n` 与孤立的 `\r` 统一为 `\n`。
pub fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// 解析 `#RRGGBB` / `RRGGBB` 形式的十六进制颜色（`#` 可省略）。
/// 长度不是 6 位或含非十六进制字符时返回 `None`。
pub fn parse_hex(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.trim_start_matches('#');
    if hex.len() == 6 {
        u8::from_str_radix(&hex[0..2], 16)
            .ok()
            .zip(u8::from_str_radix(&hex[2..4], 16).ok())
            .zip(u8::from_str_radix(&hex[4..6], 16).ok())
            .map(|((r, g), b)| (r, g, b))
    } else {
        None
    }
}

/// 字节数的紧凑展示（B / KiB / MiB / GiB，一位小数）
pub fn format_bytes(n: u64) -> String {
    /// 1024.0，B/KiB/MiB/GiB 逐级换算所用的进制基数。
    const KIB: f64 = 1024.0;
    let n = n as f64;
    if n < KIB {
        return format!("{n:.0} B");
    }
    if n < KIB * KIB {
        return format!("{:.1} KiB", n / KIB);
    }
    if n < KIB * KIB * KIB {
        return format!("{:.1} MiB", n / (KIB * KIB));
    }
    format!("{:.2} GiB", n / (KIB * KIB * KIB))
}

/// 速率的紧凑展示（与 [`format_bytes`] 同后缀，附 `/s`）
pub fn format_rate(bytes_per_s: f64) -> String {
    format!("{}/s", format_bytes(bytes_per_s.max(0.0).round() as u64))
}

/// 千分位格式化（如：14,985）
pub fn format_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// 成本格式化：保留 6 位小数后去掉尾随 0（`0.029497` / `0.1` / `0`）。
pub fn format_cost(cost: f64) -> String {
    let s = format!("{cost:.6}");
    let s = s.trim_end_matches('0');
    let s = s.trim_end_matches('.');

    if s.is_empty() {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// 成本格式化：不小于 $0.01 时保留 2 位小数，
/// 更小则取 2 位有效数字（分类器单次调用常在分以下），带 `$` 前缀。
pub fn format_cost_usd(cost: f64) -> String {
    if cost >= 0.01 {
        return format!("${cost:.2}");
    }
    format!("${}", two_significant_digits(cost))
}

/// 指数不低于 -6 时用定点记法（`0.0012`），更小用 `1.2e-7` 形式的指数记法。
fn two_significant_digits(value: f64) -> String {
    if !value.is_finite() || value == 0.0 {
        return "0".to_string();
    }

    let exponent = value.abs().log10().floor() as i32;
    if exponent < -6 {
        let mantissa = value / 10f64.powi(exponent);
        return format!("{mantissa:.1}e{exponent}");
    }

    let decimals = (1 - exponent).max(0) as usize;
    format!("{value:.decimals$}")
}

/// 按宽度软折行，并把行数封顶在 `max_lines`。
///
/// 文本里的换行会被当作段落边界（每段独立折行），空白过多时会被丢弃。
/// 超过 `max_lines` 时只保留前 `max_lines` 行，并在末行末尾追加 `...`（预留 3 列，不会被宽度截掉）。
pub fn wrap_capped(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();

    for logical in text.lines() {
        let trimmed = logical.trim();
        if trimmed.is_empty() {
            continue;
        }
        lines.extend(wrap_words(trimmed, width));
    }

    if lines.is_empty() {
        lines.push(String::new());
    }

    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            let (kept, _) = truncate_display(last, width.saturating_sub(3));
            *last = format!("{kept}...");
        }
    }

    lines
}

/// 按显示宽度裁剪到 `width`，并**保留前导空白**（右栏 prompt 的缩进靠它；
/// `truncate_chars` 会 `trim()`，不能用在带缩进的行上）；裁掉了才补 `…`。
pub fn fit_display(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }

    let (kept, _) = truncate_display(text, width.saturating_sub(1));
    format!("{kept}…")
}

/// 压平为单行供渲染：换行/制表/控制字符 → 空格，连续空白归一，首尾 trim。
///
/// 必须压平：多行 prompt 里的 `\n` 若留在 `Line` 里会把面板行数与高度算错；
/// 控制字符则会让显示宽度失真、截断位置偏移。
pub fn flatten_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() || c.is_control() {
            pending_space = !out.is_empty();
            continue;
        }

        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    out
}

/// 生成随机 UUID v4（版本位与 RFC 4122 变体位已置位）。
/// 系统随机源不可用时返回错误。
pub fn random_uuid_v4() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|source| Error::msg(format!("getrandom: {source}")))?;

    // 版本位 4 / 变体位 RFC 4122
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();

    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

/// 是否为 8-4-4-4-12 形式的 UUID 字符串
pub fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(i, b)| match i {
        8 | 13 | 18 | 23 => *b == b'-',
        _ => b.is_ascii_hexdigit(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes_and_rates() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1024 * 1024 * 3 / 2), "1.5 MiB");
        assert_eq!(format_rate(2048.0), "2.0 KiB/s");
        assert_eq!(format_rate(0.0), "0 B/s");
    }

    #[test]
    fn format_cost_trims_trailing_zeros() {
        assert_eq!(format_cost(0.123456), "0.123456");
        assert_eq!(format_cost(0.100000), "0.1");
        assert_eq!(format_cost(0.02949725), "0.029497");
        assert_eq!(format_cost(0.0), "0");
    }

    /// 成本格式化对齐 pi `formatCost`：不小于 $0.01 保留两位小数，更小取两位有效数字。
    #[test]
    fn format_cost_usd_matches_pi() {
        assert_eq!(format_cost_usd(0.0), "$0");
        assert_eq!(format_cost_usd(0.0012), "$0.0012");
        assert_eq!(format_cost_usd(0.002), "$0.0020");
        assert_eq!(format_cost_usd(0.0123), "$0.01");
        assert_eq!(format_cost_usd(1.5), "$1.50");
        assert_eq!(format_cost_usd(0.0000012), "$0.0000012");
        assert_eq!(format_cost_usd(0.00000012), "$1.2e-7");
    }

    #[test]
    fn wrap_words_breaks_on_word_boundary() {
        let text = "alpha beta gamma delta epsilon";
        let lines = wrap_words(text, 12);
        assert_eq!(lines.join(" "), text, "不切断单词: {lines:?}");
        for l in &lines {
            assert!(display_width(l) <= 12, "不超宽度: {l:?}");
        }
    }

    #[test]
    fn wrap_words_hard_breaks_long_word_and_cjk() {
        let lines = wrap_words("abcdefghijklmnop", 6);
        assert_eq!(lines, vec!["abcdef", "ghijkl", "mnop"]);

        // CJK 无空格：按显示宽度硬断，不丢字
        let cjk = "用于研究复杂问题的通用代理";
        let lines = wrap_words(cjk, 10);
        assert_eq!(lines.concat(), cjk);
        for l in &lines {
            assert!(display_width(l) <= 10, "不超宽度: {l:?}");
        }
    }
}
