//! `codemode` 源码格式：JavaScript，前面可选一行 `// @options: {...}`。
//!
//! 有**首行**算选项行，解析出的代码把选项行换成空行（保持行号不变）

use serde_json::Value;

/// 选项行的前缀（首行 trim 后以此开头才当选项行）
pub const OPTIONS_PREFIX: &str = "// @options:";

/// 支持 grammar 约束采样的模型收到的源码语法。
/// 只约束首行选项行的形状；选项 JSON 与代码本身仍由 [`parse_source`] 校验。
pub const CODEMODE_SOURCE_GRAMMAR: &str = r"
start: options_source | plain_source
options_source: OPTIONS_LINE NEWLINE SOURCE
plain_source: SOURCE

OPTIONS_LINE: /[ \t]*\/\/ @options:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
";

/// `setTimeout` 能表达的最大延时：`timeout_ms` 的上界
const MAX_TIMEOUT_MS: u64 = 2_147_483_647;

/// 源码里 `// @options:` 行声明的执行选项。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceOptions {
    /// 脚本输出的 token 预算；None = 用调用方默认值（10000）。
    pub max_output_tokens: Option<u64>,
    /// 整个脚本（含工具调用）的硬超时毫秒数；None = 不限时。
    pub timeout_ms: Option<u64>,
}

/// 解析结果：真正求值的代码 + 选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSource {
    /// 首行声明的执行选项。
    pub options: SourceOptions,
    /// 选项行已替换为**空行**的脚本源码（行号与用户输入一致，报错行号才对得上）。
    pub code: String,
}

/// 把 `// @options:` 的值解析成 [`SourceOptions`]。
///
/// 空串、非法 JSON、非对象、未知字段、类型不符一律返回 `Err`（英文文案）。
fn parse_options(directive: &str) -> Result<SourceOptions, String> {
    const FIELDS: &str = "`max_output_tokens` and `timeout_ms`";
    if directive.is_empty() {
        return Err(format!(
            "@options must be a JSON object with supported fields {FIELDS}"
        ));
    }

    let value: Value = serde_json::from_str(directive)
        .map_err(|e| format!("@options must be valid JSON with supported fields {FIELDS}: {e}"))?;

    let Some(fields) = value.as_object() else {
        return Err(format!(
            "@options must be a JSON object with supported fields {FIELDS}"
        ));
    };

    for key in fields.keys() {
        if key != "max_output_tokens" && key != "timeout_ms" {
            return Err(format!("@options only supports {FIELDS}; got `{key}`"));
        }
    }

    let mut options = SourceOptions::default();
    if let Some(v) = fields.get("max_output_tokens") {
        options.max_output_tokens = Some(v.as_u64().ok_or(
            "@options field `max_output_tokens` must be a non-negative safe integer".to_string(),
        )?);
    }

    if let Some(v) = fields.get("timeout_ms") {
        let ms = v.as_u64().ok_or(format!(
            "@options field `timeout_ms` must be a positive integer up to {MAX_TIMEOUT_MS}"
        ))?;

        if ms == 0 || ms > MAX_TIMEOUT_MS {
            return Err(format!(
                "@options field `timeout_ms` must be a positive integer up to {MAX_TIMEOUT_MS}"
            ));
        }
        options.timeout_ms = Some(ms);
    }

    Ok(options)
}

/// 拆出首行可选的 `// @options:` 行；输入整体为空、选项非法、选项行后无代码都返回 `Err`。
pub fn parse_source(input: &str) -> Result<ParsedSource, String> {
    if input.trim().is_empty() {
        return Err(
            "Expected JavaScript source text (non-empty). Provide JS only, optionally with a first line `// @options: {\"max_output_tokens\": 1000}`."
                .to_string(),
        );
    }

    let newline = input.find('\n');
    let first_line = match newline {
        Some(i) => input[..i].strip_suffix('\r').unwrap_or(&input[..i]),
        None => input,
    };

    let trimmed = first_line.trim_start();
    if !trimmed.starts_with(OPTIONS_PREFIX) {
        return Ok(ParsedSource {
            code: input.to_string(),
            options: SourceOptions::default(),
        });
    }

    let code = match newline {
        Some(i) => &input[i..],
        None => "",
    };

    if code.trim().is_empty() {
        return Err(
            "The @options line must be followed by JavaScript source on subsequent lines"
                .to_string(),
        );
    }

    Ok(ParsedSource {
        code: code.to_string(),
        options: parse_options(trimmed[OPTIONS_PREFIX.len()..].trim())?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 纯代码（无选项行）原样返回。
    #[test]
    fn plain_code_is_unchanged() {
        assert_eq!(
            parse_source("text('hi')").unwrap(),
            ParsedSource {
                code: "text('hi')".to_string(),
                options: SourceOptions::default()
            }
        );
        assert_eq!(
            parse_source("// just a comment\nreturn 1").unwrap(),
            ParsedSource {
                code: "// just a comment\nreturn 1".to_string(),
                options: SourceOptions::default()
            }
        );
    }

    /// 选项行被解析并把自身替换成空行（行号不变）。
    #[test]
    fn parses_options_line_and_keeps_line_numbers() {
        let p = parse_source("// @options: {\"timeout_ms\": 10}\nconst a = 1;\ntext(a)").unwrap();
        assert_eq!(p.code, "\nconst a = 1;\ntext(a)");
        assert_eq!(
            p.options,
            SourceOptions {
                max_output_tokens: None,
                timeout_ms: Some(10)
            }
        );

        let p =
            parse_source("  // @options:{\"max_output_tokens\":0,\"timeout_ms\":1500}\r\ntext(1)")
                .unwrap();
        assert_eq!(
            p.options,
            SourceOptions {
                max_output_tokens: Some(0),
                timeout_ms: Some(1500)
            }
        );

        assert_eq!(
            parse_source("// @options: {}\ntext(1)").unwrap().code,
            "\ntext(1)"
        );
    }

    /// 只有首行算选项行；前缀不完全匹配的注释按普通代码处理。
    #[test]
    fn only_first_line_is_an_options_line() {
        let input = "text(1)\n// @options: {\"timeout_ms\": 1}";
        assert_eq!(parse_source(input).unwrap().code, input);
        assert_eq!(
            parse_source("// @optionsx {}\ntext(1)").unwrap().options,
            SourceOptions::default()
        );
    }

    /// 空输入与非法选项一律报错，且文案与 pi 一致。
    #[test]
    fn rejects_empty_input_and_invalid_options() {
        let cases: &[(&str, &str)] = &[
            ("", "Expected JavaScript source text (non-empty)"),
            ("  \n", "Expected JavaScript source text (non-empty)"),
            (
                "// @options:\ntext(1)",
                "@options must be a JSON object with supported fields",
            ),
            (
                "// @options: {timeout_ms: 1}\ntext(1)",
                "@options must be valid JSON with supported fields",
            ),
            (
                "// @options: [1]\ntext(1)",
                "@options must be a JSON object with supported fields",
            ),
            (
                "// @options: {\"yield\": 1}\ntext(1)",
                "@options only supports `max_output_tokens` and `timeout_ms`; got `yield`",
            ),
            (
                "// @options: {\"max_output_tokens\": 1.5}\ntext(1)",
                "@options field `max_output_tokens` must be a non-negative safe integer",
            ),
            (
                "// @options: {\"timeout_ms\": 0}\ntext(1)",
                "@options field `timeout_ms` must be a positive integer",
            ),
            (
                "// @options: {\"timeout_ms\": 1}",
                "The @options line must be followed by JavaScript source on subsequent lines",
            ),
            (
                "// @options: {\"timeout_ms\": 1}\n  \n",
                "The @options line must be followed by JavaScript source on subsequent lines",
            ),
        ];

        for (input, message) in cases {
            let err = parse_source(input).unwrap_err();
            assert!(err.contains(message), "input {input:?} → {err:?}");
        }
    }
}
