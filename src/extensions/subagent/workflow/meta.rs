//! 工作流脚本的 `meta` 块：提取与校验。
//!
//! 脚本以 `export const meta = { … }` 开头，但脚本体要作为函数体执行，`export` 在
//! 沙箱里是语法错误；而且**在任何 agent 跑之前**就要拿到 `meta.phases` 以便进度树
//! 从第一帧就有分组。扫描到对象字面量的配对 `}`，只在空上下文里**求值那一段字面量**
//! （纯字面量无需任何全局，任何引用/调用都会抛错）。
//!
//! - 扫描器感知字符串 / 模板 / 注释 / 正则，避免 `{`/`}` 误判；
//! - 拒绝模板插值 `${…}`（`` `a${1+1}b` `` 无需全局也能求值，光靠空上下文拦不住）；
//! - 用独立的 rquickjs `Context`（无任何全局）求值该片段，并加 100ms 中断上界
//!   （`(() => { while (true); })()` 无需全局也能求值，必须能中断）。
//!
//! `body` 只把 `export ` 六个字符替换成空格，**保持其后所有字节偏移不变**。

use rquickjs::{Context, Runtime};
use serde_json::Value;
use std::time::{Duration, Instant};

/// `meta` 求值的墙钟上限（超时即判定为非纯字面量）。
const META_EVAL_TIMEOUT_MS: u64 = 100;

/// 出错时的统一提示：`meta` 必须是纯字面量。
const PURE_LITERAL_HINT: &str = "The `meta` object must be a PURE LITERAL — no variables, function calls, spreads, or template interpolation.";

/// 一个前置声明的阶段（进度树第一帧就要显示）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkflowPhaseMeta {
    /// 阶段标题。
    pub title: String,
    /// 一行的阶段说明（可空）。
    pub detail: Option<String>,
    /// 该阶段钉住的模型（仅展示；运行期不读）。
    pub model: Option<String>,
}

/// 工作流 `meta`。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowMeta {
    /// 工作流名（必填）。
    pub name: String,
    /// 用途简述（必填）。
    pub description: String,
    /// 用于具名工作流列表（运行期不读）。
    pub when_to_use: Option<String>,
    /// 前置声明的阶段（None = 无阶段）。
    pub phases: Option<Vec<WorkflowPhaseMeta>>,
}

/// 提取结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaExtraction {
    /// 解析出的 `meta`。
    pub meta: WorkflowMeta,
    /// 已剥掉 `export `（替换为空格）的脚本体。
    pub body: String,
}

/// `meta` 提取/校验失败
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaError(pub String);

impl std::error::Error for MetaError {}

impl std::fmt::Display for MetaError {
    /// 直接输出错误说明文本。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 用说明文本构造 [`MetaError`]。
fn fail(message: impl Into<String>) -> MetaError {
    MetaError(message.into())
}

/// 源码是否**声称**是工作流脚本（廉价判定，用于区分目录里的普通 `.js`）。
pub fn has_meta_declaration(source: &str) -> bool {
    find_declaration(source).is_some()
}

/// 定位 `export const meta =`（允许内部任意空白），返回其结束偏移。
fn find_declaration(source: &str) -> Option<usize> {
    let mut from = 0;
    let b = source.as_bytes();

    while let Some(rel) = source[from..].find("export") {
        let idx = from + rel;
        let line_start = source[..idx].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let indent_ok = source[line_start..idx]
            .bytes()
            .all(|c| c == b' ' || c == b'\t');

        if indent_ok {
            let mut i = idx + "export".len();
            let skip_ws = |i: &mut usize| {
                while *i < b.len() && (b[*i] as char).is_ascii_whitespace() {
                    *i += 1;
                }
            };

            skip_ws(&mut i);

            if source[i..].starts_with("const") {
                i += "const".len();
                skip_ws(&mut i);

                if source[i..].starts_with("meta") {
                    i += "meta".len();
                    skip_ws(&mut i);

                    if i < b.len() && b[i] == b'=' {
                        return Some(i + 1);
                    }
                }
            }
        }
        from = idx + "export".len();
    }

    None
}

/// 对象字面量扫描结果：配对 `}` 之后的下标与是否出现模板插值。
struct ScanResult {
    /// 字面量收尾 `}` 之后的下标；未配平为 None。
    end: Option<usize>,
    /// 模板字面量里出现过 `${` 插值。
    saw_interpolation: bool,
}

/// 从 `open`（`{` 的下标）起找配对的 `}`，感知字符串/模板/注释/正则。
fn scan_object_literal(src: &[u8], open: usize) -> ScanResult {
    /// 扫描器当前所处的词法上下文（代码 / 字符串 / 模板 / 注释 / 正则）。
    #[derive(PartialEq)]
    enum Mode {
        /// 普通代码上下文（默认态）。
        Code,
        /// `//` 行注释，遇换行回到代码态。
        LineComment,
        /// `/* */` 块注释，遇 `*/` 回到代码态。
        BlockComment,
        /// 单引号字符串字面量。
        Single,
        /// 双引号字符串字面量。
        Double,
        /// 反引号模板字符串，`${` 进入插值。
        Template,
        /// 正则字面量，遇未转义 `/` 结束。
        Regex,
    }

    let mut depth: i64 = 0;
    let mut i = open;
    let mut saw_interpolation = false;
    let mut mode = Mode::Code;
    let mut template_stack: Vec<i64> = Vec::new();

    let is_regex_position = |idx: usize| -> bool {
        let mut j = idx;
        while j > 0 && (src[j - 1] as char).is_ascii_whitespace() {
            j -= 1;
        }

        if j == 0 {
            return true;
        }

        let prev = src[j - 1];
        !(prev.is_ascii_alphanumeric()
            || prev == b'_'
            || prev == b'$'
            || prev == b')'
            || prev == b']')
    };

    while i < src.len() {
        let c = src[i];
        let next = src.get(i + 1).copied();
        match mode {
            Mode::LineComment => {
                if c == b'\n' {
                    mode = Mode::Code;
                }
                i += 1;
                continue;
            }
            Mode::BlockComment => {
                if c == b'*' && next == Some(b'/') {
                    mode = Mode::Code;
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            Mode::Single | Mode::Double | Mode::Regex => {
                if c == b'\\' {
                    i += 2;
                    continue;
                }
                match mode {
                    Mode::Single if c == b'\'' => mode = Mode::Code,
                    Mode::Double if c == b'"' => mode = Mode::Code,
                    Mode::Regex if c == b'/' => mode = Mode::Code,
                    // 未终结的字符串/正则不可能跨行；回到 code，避免误判的正则吞掉全文
                    _ if c == b'\n' && mode != Mode::Double => mode = Mode::Code,
                    _ => {}
                }
                i += 1;
                continue;
            }
            Mode::Template => {
                if c == b'\\' {
                    i += 2;
                    continue;
                }
                if c == b'`' {
                    mode = Mode::Code;
                    i += 1;
                    continue;
                }
                if c == b'$' && next == Some(b'{') {
                    saw_interpolation = true;
                    template_stack.push(depth);
                    depth += 1;
                    mode = Mode::Code;
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }
            Mode::Code => {}
        }

        if c == b'/' && next == Some(b'/') {
            mode = Mode::LineComment;
            i += 2;
            continue;
        }
        if c == b'/' && next == Some(b'*') {
            mode = Mode::BlockComment;
            i += 2;
            continue;
        }
        if c == b'\'' {
            mode = Mode::Single;
            i += 1;
            continue;
        }
        if c == b'"' {
            mode = Mode::Double;
            i += 1;
            continue;
        }
        if c == b'`' {
            mode = Mode::Template;
            i += 1;
            continue;
        }
        if c == b'/' && is_regex_position(i) {
            mode = Mode::Regex;
            i += 1;
            continue;
        }
        if c == b'{' {
            depth += 1;
            i += 1;
            continue;
        }
        if c == b'}' {
            depth -= 1;
            i += 1;
            if let Some(&top) = template_stack.last()
                && depth == top
            {
                template_stack.pop();
                mode = Mode::Template;
                continue;
            }
            if depth == 0 {
                return ScanResult {
                    end: Some(i),
                    saw_interpolation,
                };
            }
            continue;
        }
        i += 1;
    }

    ScanResult {
        end: None,
        saw_interpolation,
    }
}

/// 在空上下文里求值字面量片段（带 100ms 中断上界）。
fn eval_literal(fragment: &str) -> Result<Value, String> {
    let rt = Runtime::new().map_err(|e| format!("runtime: {e}"))?;
    rt.set_interrupt_handler(Some(Box::new({
        let deadline = Instant::now() + Duration::from_millis(META_EVAL_TIMEOUT_MS);
        move || Instant::now() > deadline
    })));

    let ctx = Context::full(&rt).map_err(|e| format!("context: {e}"))?;
    let source = format!("({fragment})");

    let json: String = ctx
        .with(|ctx| {
            let val: rquickjs::Value = ctx.eval(source.as_bytes()).map_err(|e| format!("{e}"))?;

            // 用 rquickjs 自带的 JSON.stringify 序列化，再由 Rust 解析；这天然拒绝函数/undefined/循环引用
            let json_obj: rquickjs::Object =
                ctx.globals().get("JSON").map_err(|e| format!("{e}"))?;
            let stringify: rquickjs::Function =
                json_obj.get("stringify").map_err(|e| format!("{e}"))?;
            let out: rquickjs::Value = stringify.call((val,)).map_err(|e| format!("{e}"))?;

            out.as_string()
                .map(|s| s.to_string().map_err(|e| format!("{e}")))
                .unwrap_or_else(|| Ok("undefined".to_string()))
        })
        .map_err(|e: String| e)?;

    if json == "undefined" {
        return Err("`meta` must be an object literal.".to_string());
    }

    serde_json::from_str(&json).map_err(|e| format!("{e}"))
}

/// 解析 `meta.phases` 数组为阶段元数据。
///
/// `phases` 缺失时返回 `Ok(None)`；不是数组、元素不是对象、`title` 缺失或为空、
/// `detail`/`model` 非字符串时返回错误。
fn parse_phases(value: &Value) -> Result<Option<Vec<WorkflowPhaseMeta>>, MetaError> {
    let Some(arr) = value.get("phases") else {
        return Ok(None);
    };
    let Some(arr) = arr.as_array() else {
        return Err(fail(
            "`meta.phases` must be an array of { title, detail?, model? } objects.",
        ));
    };

    let mut out = Vec::with_capacity(arr.len());
    for (index, entry) in arr.iter().enumerate() {
        let Some(obj) = entry.as_object() else {
            return Err(fail(format!(
                "`meta.phases[{index}]` must be an object with a `title`."
            )));
        };
        let title = obj.get("title").and_then(|v| v.as_str()).unwrap_or("");

        if title.trim().is_empty() {
            return Err(fail(format!(
                "`meta.phases[{index}].title` must be a non-empty string."
            )));
        }

        let detail = match obj.get("detail") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => Some(
                v.as_str()
                    .ok_or_else(|| {
                        fail(format!("`meta.phases[{index}].detail` must be a string."))
                    })?
                    .to_string(),
            ),
        };
        let model = match obj.get("model") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => Some(
                v.as_str()
                    .ok_or_else(|| fail(format!("`meta.phases[{index}].model` must be a string.")))?
                    .to_string(),
            ),
        };

        out.push(WorkflowPhaseMeta {
            title: title.to_string(),
            detail,
            model,
        });
    }
    Ok(Some(out))
}

/// 从脚本开头提取 `meta` 并返回可执行 body。
pub fn extract_meta(source: &str) -> Result<MetaExtraction, MetaError> {
    let (after_eq, open, close) = locate_meta_literal(source)?;
    let fragment = &source[open..close];
    let value = eval_literal(fragment).map_err(eval_error)?;
    let (name, description, when_to_use) = validate_meta(&value)?;
    let phases = parse_phases(&value)?;
    let body = strip_export(source, after_eq)?;

    Ok(MetaExtraction {
        meta: WorkflowMeta {
            name,
            description,
            when_to_use,
            phases,
        },
        body,
    })
}

/// 定位 `meta` 对象字面量，返回 `(声明结束偏移, `{` 偏移, 配对 `}` 偏移)`。
///
/// 依次校验：有 `export const meta` 声明 → 赋的是对象字面量 → 花括号配平 → 没有模板插值。
fn locate_meta_literal(source: &str) -> Result<(usize, usize, usize), MetaError> {
    let Some(after_eq) = find_declaration(source) else {
        return Err(fail(format!(
            "A workflow script must begin with `export const meta = {{ name, description }}`.\n{PURE_LITERAL_HINT}"
        )));
    };
    let Some(open_rel) = source[after_eq..].find('{') else {
        return Err(fail(format!(
            "`export const meta` must be assigned an object literal.\n{PURE_LITERAL_HINT}"
        )));
    };
    let open = after_eq + open_rel;

    let scan = scan_object_literal(source.as_bytes(), open);
    let Some(close) = scan.end else {
        return Err(fail(
            "`meta` object literal is never closed — check for an unbalanced `{`.",
        ));
    };

    if scan.saw_interpolation {
        return Err(fail(format!(
            "`meta` must not use template interpolation (`${{...}}`).\n{PURE_LITERAL_HINT}"
        )));
    }

    Ok((after_eq, open, close))
}

/// `eval_literal` 的失败措辞：超时（说明不是纯字面量）与其它分开。
fn eval_error(e: String) -> MetaError {
    if e.contains("interrupted") || e.contains("Interrupted") {
        fail(format!(
            "`meta` did not finish evaluating within {META_EVAL_TIMEOUT_MS}ms — it must be a literal, not a computation.\n{PURE_LITERAL_HINT}"
        ))
    } else {
        fail(format!(
            "`meta` could not be evaluated: {e}\n{PURE_LITERAL_HINT}"
        ))
    }
}

/// 校验 `meta` 对象并取出 `(name, description, whenToUse)`。
fn validate_meta(value: &serde_json::Value) -> Result<(String, String, Option<String>), MetaError> {
    let Some(obj) = value.as_object() else {
        return Err(fail(format!(
            "`meta` must be an object literal.\n{PURE_LITERAL_HINT}"
        )));
    };

    let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name.trim().is_empty() {
        return Err(fail(
            "`meta.name` is required and must be a non-empty string.",
        ));
    }

    let description = obj
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if description.trim().is_empty() {
        return Err(fail(
            "`meta.description` is required and must be a non-empty string.",
        ));
    }

    let when_to_use = match obj.get("whenToUse") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| fail("`meta.whenToUse` must be a string."))?
                .to_string(),
        ),
    };

    Ok((name.to_string(), description.to_string(), when_to_use))
}

/// 把 `export ` 替换成 6 个空格，保持其后所有偏移不变。
fn strip_export(source: &str, after_eq: usize) -> Result<String, MetaError> {
    let export_at = source[..after_eq]
        .rfind("export")
        .ok_or_else(|| fail("internal: declaration offset lost"))?;

    let mut body = String::with_capacity(source.len());
    body.push_str(&source[..export_at]);
    body.push_str("      ");
    body.push_str(&source[export_at + "export".len()..]);

    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_of(src: &str) -> WorkflowMeta {
        extract_meta(src).unwrap().meta
    }

    #[test]
    fn extracts_required_fields_and_body_offsets() {
        let src = "export const meta = {\n  name: 'demo',\n  description: 'd',\n}\nphase('X')\n";
        let out = extract_meta(src).unwrap();
        assert_eq!(out.meta.name, "demo");
        assert_eq!(out.meta.description, "d");
        // body 行数与原文一致，且 export 被空格替换
        assert_eq!(out.body.lines().count(), src.lines().count());
        assert!(out.body.contains("phase('X')"));
        assert!(!out.body.contains("export"));
    }

    #[test]
    fn phases_parse_and_validate() {
        let m = meta_of(
            "export const meta = { name: 'n', description: 'd', phases: [\n  { title: 'Scan', detail: 'a}b' },\n  { title: 'Fix', model: 'm' },\n] }",
        );
        let phases = m.phases.unwrap();
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].detail.as_deref(), Some("a}b"));
        assert_eq!(phases[1].model.as_deref(), Some("m"));
    }

    #[test]
    fn rejects_missing_or_impure_meta() {
        assert!(extract_meta("const x = 1").is_err());
        assert!(extract_meta("export const meta = { name: 'n' }").is_err());
        // 变量引用（非纯字面量）：空上下文里求值抛错
        assert!(extract_meta("export const meta = { name: N, description: 'd' }").is_err());
        // 模板插值被显式拒绝
        let err = extract_meta("export const meta = { name: `a${1}b`, description: 'd' }")
            .unwrap_err()
            .0;
        assert!(err.contains("interpolation"), "{err}");
        // 阶段缺 title
        assert!(
            extract_meta("export const meta = { name: 'n', description: 'd', phases: [{}] }")
                .is_err()
        );
    }

    #[test]
    fn braces_inside_strings_and_comments_do_not_end_the_scan() {
        let m = meta_of(
            "export const meta = { name: 'n', description: 'has } brace', // }\n  whenToUse: 'x' }",
        );
        assert_eq!(m.when_to_use.as_deref(), Some("x"));
    }

    #[test]
    fn has_meta_declaration_is_cheap_and_accurate() {
        assert!(has_meta_declaration("export const meta = {}"));
        assert!(!has_meta_declaration("const meta = {}"));
        assert!(!has_meta_declaration("xexport const meta = {}"));
    }
}
