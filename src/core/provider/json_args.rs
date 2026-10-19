//! 工具调用参数（模型给出的 JSON 文本）的解析与修复。
//!
//! 模型给出的参数串经常不严格：字符串里带 `\usepackage` 这类非法转义、原始控制字符、或者流式传输到一半被截断。
//! 直接交给 `serde_json` 会整串失败，于是只能退回「最长可解析前缀」——字段与它后面的内容会被静默丢掉。
//!
//! 这里分两条路：
//! - [`parse_streaming_json`]：流式期间用，宽容（修复 → 前缀兜底 → 空对象）；
//! - [`parse_tool_call_arguments`]：调用已定稿时用，严格——绝不接受部分前缀，
//!   失败时返回错误说明（含模型发来的原文），由校验阶段回给模型。

use serde_json::{Value, json};

/// JSON 字符串字面量里的合法转义字符。
///
/// 刻意**不含** `u`：`\u` 只有后接 4 位十六进制才算合法，`\usepackage` 之类按字面文本处理。
const VALID_ESCAPES: [char; 8] = ['"', '\\', '/', 'b', 'f', 'n', 'r', 't'];

/// 把字符串里的一个原始控制字符转义成合法 JSON 转义。
fn escape_control(c: char) -> String {
    match c {
        '\u{08}' => "\\b".to_string(),
        '\u{0c}' => "\\f".to_string(),
        '\n' => "\\n".to_string(),
        '\r' => "\\r".to_string(),
        '\t' => "\\t".to_string(),
        other => format!("\\u{:04x}", other as u32),
    }
}

/// 修复模型给出的畸形 JSON 字符串字面量：
/// - 字符串内的原始控制字符转义成 `\n` / `\uXXXX` 等；
/// - 非法转义（含后接不足 4 位十六进制的 `\u`）前补一个反斜杠，使其按字面文本保留。
///
/// 字符串之外的内容原样保留。拿不到任何需要修复的地方时返回值与输入相同。
pub(crate) fn repair_json(json: &str) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut out = String::with_capacity(json.len());
    let mut in_string = false;
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];

        if !in_string {
            out.push(c);
            if c == '"' {
                in_string = true;
            }
            i += 1;
            continue;
        }

        if c == '"' {
            out.push(c);
            in_string = false;
            i += 1;
            continue;
        }

        if c == '\\' {
            let Some(&next) = chars.get(i + 1) else {
                // 结尾单个反斜杠：补成合法的 `\\`
                out.push_str("\\\\");
                i += 1;
                continue;
            };
            if next == 'u' {
                // 流式截断：剩不到 4 位就无法判定，整段丢掉（pi partial-json 同款）
                if chars.len() - (i + 2) < 4 {
                    break;
                }
                let digits: String = chars[i + 2..i + 6].iter().collect();
                if digits.chars().all(|d| d.is_ascii_hexdigit()) {
                    out.push_str("\\u");
                    out.push_str(&digits);
                    i += 6;
                    continue;
                }
            }
            if VALID_ESCAPES.contains(&next) {
                out.push('\\');
                out.push(next);
                i += 2;
                continue;
            }
            out.push_str("\\\\");
            i += 1;
            continue;
        }

        if (c as u32) <= 0x1f {
            out.push_str(&escape_control(c));
        } else {
            out.push(c);
        }
        i += 1;
    }

    out
}

/// 先直接解析，失败时修复转义再解析；两次都失败返回最后一次的错误文案。
fn parse_json_with_repair(json: &str) -> Result<Value, String> {
    match serde_json::from_str::<Value>(json) {
        Ok(v) => Ok(v),
        Err(err) => {
            let repaired = repair_json(json);
            if repaired != json
                && let Ok(v) = serde_json::from_str::<Value>(&repaired)
            {
                return Ok(v);
            }
            Err(err.to_string())
        }
    }
}

/// 从后往前逐步截断，返回最长的可完整解析 JSON 前缀；没有任何前缀能解析时为 `None`。
fn partial_parse(s: &str) -> Option<Value> {
    let chars: Vec<char> = s.chars().collect();
    for cut in (1..=chars.len()).rev() {
        let candidate: String = chars[..cut].iter().collect();
        if let Ok(v) = serde_json::from_str::<Value>(&candidate) {
            return Some(v);
        }
    }
    None
}

/// 尽量补全不完整的 JSON（流式中途）：先结束未闭合的字符串，再补上缺失的 `}` / `]`。
///
/// 补出来的文本还要能解析才算数（比如 `{"a":` 补成 `{"a":}` 仍非法）——调用方解析失败
/// 就退回空对象。
fn close_incomplete_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    let mut closers: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for c in s.chars() {
        out.push(c);
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => closers.push('}'),
            '[' => closers.push(']'),
            '}' | ']' => {
                closers.pop();
            }
            _ => {}
        }
    }

    if in_string {
        // 结尾是孤立的反斜杠时先去掉，避免补出非法转义
        if escaped {
            out.pop();
        }
        out.push('"');
    }
    while let Some(closer) = closers.pop() {
        out.push(closer);
    }
    out
}

/// 流式（可能不完整）JSON 解析：修复后解析 → 最长可解析前缀 → 补全未闭合结构 → 空对象。
///
/// 任何输入都返回一个 JSON 值（对象），调用方无需处理错误。
pub(crate) fn parse_streaming_json(s: &str) -> Value {
    if s.trim().is_empty() {
        return json!({});
    }
    if let Ok(v) = parse_json_with_repair(s) {
        return v;
    }
    if let Some(v) = partial_parse(s) {
        return v;
    }
    let repaired = repair_json(s);
    if let Some(v) = partial_parse(&repaired) {
        return v;
    }
    if let Ok(v) = serde_json::from_str::<Value>(&close_incomplete_json(&repaired)) {
        return v;
    }
    json!({})
}

/// 已定稿的工具调用参数解析：**绝不**保留部分解析结果。
///
/// 返回 `(arguments, arguments_error)`：
/// - 解析成功且是对象 → `(对象, None)`；
/// - 参数为 `None` / 空串 → `({}, None)`（模型没给参数）；
/// - 非法 JSON，或结果不是对象（数组 / `null` / 标量）→ `({}, Some(说明))`，
///   说明里带上模型发来的原文，供校验阶段回给模型。
pub(crate) fn parse_tool_call_arguments(json: Option<&str>) -> (Value, Option<String>) {
    let Some(json) = json else {
        return (json!({}), None);
    };

    if json.trim().is_empty() {
        return (json!({}), None);
    }

    match parse_json_with_repair(json) {
        Ok(v @ Value::Object(_)) => (v, None),
        Ok(_) => (
            json!({}),
            Some(format!(
                "Tool arguments must be a JSON object\n\nReceived arguments:\n{json}"
            )),
        ),
        Err(err) => (
            json!({}),
            Some(format!("{err}\n\nReceived arguments:\n{json}")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 非法 `\u` 转义按字面保留，字段不再被丢掉（pi #10868）。
    #[test]
    fn repairs_invalid_unicode_escapes_without_dropping_fields() {
        assert_eq!(
            parse_streaming_json(r#"{"text":"\url{x}"}"#),
            json!({ "text": "\\url{x}" })
        );
        assert_eq!(
            parse_streaming_json(r#"{"path":"a.tex","content":"\usepackage{x}","mode":"w"}"#),
            json!({ "path": "a.tex", "content": "\\usepackage{x}", "mode": "w" })
        );
        // 合法的 \u 转义仍然解码
        assert_eq!(
            parse_streaming_json(r#"{"text":"caf\u00e9 \url"}"#),
            json!({ "text": "café \\url" })
        );
    }

    /// 流式中途被截断的 `\u` 转义被丢弃，前面的字段仍然拿得到（对齐 pi partial-json）。
    #[test]
    fn truncated_unicode_escape_keeps_earlier_fields() {
        assert_eq!(
            parse_streaming_json(r#"{"text":"ab\u00"#),
            json!({ "text": "ab" })
        );
        assert_eq!(
            parse_streaming_json(r#"{"text":"ok","tail":"ab\u00"#),
            json!({ "text": "ok", "tail": "ab" })
        );
    }

    /// 原始控制字符被转义，而不是让整串解析失败。
    #[test]
    fn escapes_raw_control_characters() {
        assert_eq!(
            parse_streaming_json("{\"text\":\"a\nb\"}"),
            json!({ "text": "a\nb" })
        );
    }

    /// 定稿参数：非法 JSON → 空对象 + 错误说明（含原文），不再吞成部分前缀。
    #[test]
    fn finalized_arguments_never_keep_a_partial_prefix() {
        let (args, err) = parse_tool_call_arguments(Some(r#"{"path":"a.rs","limit":}"#));
        assert_eq!(args, json!({}));
        let err = err.expect("malformed arguments must report an error");
        assert!(err.contains("Received arguments:"), "{err}");
        assert!(err.contains(r#""path":"a.rs""#), "{err}");
    }

    /// 定稿参数：结果不是对象时同样报错（数组 / null / 标量）。
    #[test]
    fn finalized_arguments_require_an_object() {
        for raw in ["[1,2]", "null", "42", r#""text""#] {
            let (args, err) = parse_tool_call_arguments(Some(raw));
            assert_eq!(args, json!({}), "{raw}");
            assert!(err.is_some(), "{raw}");
        }
        // 非法 `\u` 修复后仍是合法对象 → 不报错
        let (args, err) = parse_tool_call_arguments(Some(r#"{"content":"\usepackage{x}"}"#));
        assert_eq!(err, None);
        assert_eq!(args, json!({ "content": "\\usepackage{x}" }));
        // 没给参数 → 空对象、无错误
        assert_eq!(parse_tool_call_arguments(None), (json!({}), None));
        assert_eq!(parse_tool_call_arguments(Some("  ")), (json!({}), None));
    }
}
