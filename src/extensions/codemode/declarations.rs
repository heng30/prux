//! 工具声明渲染：JSON Schema → TypeScript 声明文本。

use serde_json::Value;

/// 多行对象的缩进（两个空格）。
const INDENT: &str = "  ";

/// 单次 schema 展开里最多展开多少个 `$ref`（防止共享定义撑爆输出）。
const MAX_REF_EXPANSIONS: usize = 32;

/// 渲染出的输入类型超过这个字符数就退化成 `unknown`
pub const DEFAULT_INPUT_SCHEMA_MAX_CHARS: usize = 16_000;

/// 取值是数字或字符串、并以 `@keyword value` 形式写进属性注释的 JSON Schema 关键字。
const TAG_KEYWORDS: [&str; 11] = [
    "minimum",
    "exclusiveMinimum",
    "maximum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "minItems",
    "maxItems",
];

/// 下界关键字：多个值同时成立时只有最大的那个有效。
const LOWER_BOUNDS: [&str; 4] = ["minimum", "exclusiveMinimum", "minLength", "minItems"];

/// 上界关键字：多个值同时成立时只有最小的那个有效。
const UPPER_BOUNDS: [&str; 4] = ["maximum", "exclusiveMaximum", "maxLength", "maxItems"];

/// 单个标签的最大长度：更长的（大段默认值、长 pattern）直接舍弃。
const MAX_TAG_CHARS: usize = 80;

/// 脚本用的工具标识符：不合法的字符变成 `_`（`my-tool` → `my_tool`）。
pub fn to_codemode_identifier(name: &str) -> String {
    let mut identifier = String::new();
    for ch in name.chars() {
        let valid = if identifier.is_empty() {
            ch.is_ascii_alphabetic() || ch == '_' || ch == '$'
        } else {
            ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'
        };
        identifier.push(if valid { ch } else { '_' });
    }

    if identifier.is_empty() {
        "_".to_string()
    } else {
        identifier
    }
}

/// JSON Schema → TypeScript 类型表达式；超过 `max_chars` 时先退一步去掉约束标签，还超就返回 `unknown`。
pub fn schema_to_type(schema: &Value, max_chars: Option<usize>) -> String {
    let render = |tags: bool| {
        let mut ctx = SchemaContext::new(schema);
        ctx.tags = tags;
        to_type(schema, &mut ctx)
    };

    let ty = render(true);
    let Some(max) = max_chars else {
        return ty;
    };
    if ty.chars().count() <= max {
        return ty;
    }

    let untagged = render(false);
    if untagged.chars().count() <= max {
        untagged
    } else {
        "unknown".to_string()
    }
}

/// 工具作为 `tools` 成员的一行签名：`read(args: T): Promise<R>;`。
pub fn render_tool_signature(name: &str, input: Option<&Value>, output: Option<&Value>) -> String {
    let input = match input {
        Some(schema) => schema_to_type(schema, Some(DEFAULT_INPUT_SCHEMA_MAX_CHARS)),
        None => "unknown".to_string(),
    };
    let output = match output {
        Some(schema) => schema_to_type(schema, None),
        None => "unknown".to_string(),
    };
    format!(
        "{}(args: {}): Promise<{}>;",
        to_codemode_identifier(name),
        input,
        output
    )
}

/// 工具样例：描述 + prompt guidelines + 声明（`ALL_TOOLS` 条目与 describeTool() 的返回值）。
///
/// `guidelines` 是系统提示里只对「已声明」工具展示的 prompt guidelines（见 [`crate::core::system_prompt`]）；
/// 脚本侧也把它拼在描述后面，让 `ALL_TOOLS` / `describeTool()` 看到同样多的信息。空行与空条目直接跳过。
pub fn render_tool_sample(
    name: &str,
    description: &str,
    guidelines: &[String],
    input: Option<&Value>,
    output: Option<&Value>,
) -> String {
    let declaration = format!(
        "declare const tools: {{ {} }};",
        render_tool_signature(name, input, output)
    );
    let bullets: Vec<String> = guidelines
        .iter()
        .filter(|guideline| !guideline.trim().is_empty())
        .map(|guideline| format!("- {}", guideline.trim()))
        .collect();
    let guidelines = if bullets.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", bullets.join("\n"))
    };
    format!(
        "{}{guidelines}\n\ncodemode tool declaration:\n```ts\n{declaration}\n```",
        description.trim(),
    )
}

/// 递归渲染的上下文：根 schema（`$ref` 解析基准）、在当前路径上的引用、已展开次数。
struct SchemaContext<'a> {
    /// `$ref` 解析的基准 schema，本地引用（`#/...`）都相对它查找
    root: &'a Value,
    /// 当前递归路径上已进入但尚未展开完的 `$ref`，命中其中一项即判定为自引用
    resolving: Vec<String>,
    /// 本次渲染已展开的 `$ref` 累计次数，达到 `MAX_REF_EXPANSIONS` 后不再展开
    expansions: usize,
    /// 属性注释里是否带约束标签（输出超长时会不带标签重渲一次）
    tags: bool,
}

impl<'a> SchemaContext<'a> {
    /// 以 `root` 为 `$ref` 解析基准开一份空上下文（默认带约束标签）。
    fn new(root: &'a Value) -> Self {
        SchemaContext {
            root,
            resolving: Vec::new(),
            expansions: 0,
            tags: true,
        }
    }
}

/// `$ref` 是否是指向根 schema 内部的本地引用。
fn resolve_ref<'a>(reference: &str, root: &'a Value) -> Option<&'a Value> {
    if reference != "#" && !reference.starts_with("#/") {
        return None;
    }

    let mut current = root;
    for segment in reference[2..].split('/').filter(|s| !s.is_empty()) {
        let key = segment.replace("~1", "/").replace("~0", "~");
        current = current.as_object()?.get(&key)?;
    }

    Some(current)
}

/// 去重后的联合类型；`unknown` 吞掉其它分支，空集合是 `never`。
fn union(types: Vec<String>) -> String {
    let mut unique: Vec<String> = Vec::new();
    for t in types {
        if !unique.contains(&t) {
            unique.push(t);
        }
    }

    if unique.iter().any(|t| t == "unknown") {
        return "unknown".to_string();
    }

    if unique.is_empty() {
        return "never".to_string();
    }
    unique.join(" | ")
}

/// 对象的属性名：合法标识符原样，否则 JSON 引号包裹。
fn property_key(name: &str) -> String {
    let mut chars = name.chars();
    let valid = match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        }
        _ => false,
    };

    if valid {
        name.to_string()
    } else {
        Value::String(name.to_string()).to_string()
    }
}

/// 属性的 `description`（trim 后）；没有则返回空串。
fn description_of(property: &Value) -> String {
    property
        .as_object()
        .and_then(|o| o.get("description"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// 数组类型：`items` 单 schema → `Array<T>`；元组（`prefixItems` / 数组式 `items`）→ `[T, U]`。
fn array_type(schema: &serde_json::Map<String, Value>, ctx: &mut SchemaContext<'_>) -> String {
    if let Some(items) = schema.get("items")
        && !items.is_array()
    {
        return format!("Array<{}>", to_type(items, ctx));
    }

    let tuple: &[Value] = match schema.get("prefixItems").and_then(|v| v.as_array()) {
        Some(t) => t,
        None => match schema.get("items").and_then(|v| v.as_array()) {
            Some(t) => t,
            None => &[],
        },
    };

    if tuple.is_empty() {
        return "unknown[]".to_string();
    }

    let parts: Vec<String> = tuple.iter().map(|item| to_type(item, ctx)).collect();
    format!("[{}]", parts.join(", "))
}

/// 属性的 TS 类型表达不了的约束，渲染成 JSDoc 风格标签：
/// `@integer @minimum 1 @maximum 100 @default 20`；没有约束时返回空串。
///
/// 可空字段（如 `int | None`）把约束放在非 null 分支：分支与属性自身的约束同时生效。
fn constraint_tags(schema: &Value) -> String {
    let Some(property) = schema.as_object() else {
        return String::new();
    };

    // 可空联合取唯一的非 null 分支（`null` 本身不携带约束）
    let variants = property
        .get("anyOf")
        .and_then(Value::as_array)
        .or_else(|| property.get("oneOf").and_then(Value::as_array));
    let non_null: Vec<&Value> = variants
        .map(|list| {
            list.iter()
                .filter(|variant| variant.get("type").and_then(Value::as_str) != Some("null"))
                .collect()
        })
        .unwrap_or_default();

    let mut branches: Vec<&serde_json::Map<String, Value>> = vec![property];
    if non_null.len() == 1
        && let Some(branch) = non_null[0].as_object()
    {
        branches.push(branch);
    }

    // Draft-04 / OpenAPI 3.0：布尔 `exclusiveMinimum` / `exclusiveMaximum` 把 `minimum` / `maximum` 改成排他的
    let normalized: Vec<serde_json::Map<String, Value>> = branches
        .iter()
        .map(|branch| {
            let mut out = (*branch).clone();
            if out.get("exclusiveMinimum") == Some(&Value::Bool(true))
                && let Some(minimum) = out.get("minimum").cloned()
            {
                out.insert("exclusiveMinimum".to_string(), minimum);
                out.remove("minimum");
            }
            if out.get("exclusiveMaximum") == Some(&Value::Bool(true))
                && let Some(maximum) = out.get("maximum").cloned()
            {
                out.insert("exclusiveMaximum".to_string(), maximum);
                out.remove("maximum");
            }
            out
        })
        .collect();

    let mut tags: Vec<String> = Vec::new();
    let is_integer = |kind: Option<&Value>| match kind {
        Some(Value::String(s)) => s == "integer",
        Some(Value::Array(types)) => types.iter().any(|t| t.as_str() == Some("integer")),
        _ => false,
    };
    if normalized
        .iter()
        .any(|branch| is_integer(branch.get("type")))
    {
        tags.push("@integer".to_string());
    }

    for keyword in TAG_KEYWORDS {
        let values: Vec<&Value> = normalized.iter().filter_map(|b| b.get(keyword)).collect();
        if LOWER_BOUNDS.contains(&keyword) || UPPER_BOUNDS.contains(&keyword) {
            let numbers: Vec<f64> = values.iter().filter_map(|v| v.as_f64()).collect();
            if numbers.is_empty() {
                continue;
            }
            // 下界取最大、上界取最小：同时成立时只有那个有效
            let bound = if LOWER_BOUNDS.contains(&keyword) {
                numbers.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            } else {
                numbers.iter().copied().fold(f64::INFINITY, f64::min)
            };
            tags.push(format!("@{keyword} {}", format_bound(bound)));
            continue;
        }

        // 其余关键字（pattern / format / multipleOf 等）：不同取值各出一个标签
        let mut seen: Vec<&Value> = Vec::new();
        for value in values {
            if !matches!(value, Value::Number(_) | Value::String(_)) || seen.contains(&value) {
                continue;
            }
            seen.push(value);
            tags.push(format!("@{keyword} {}", format_tag_value(value)));
        }
    }

    if normalized
        .iter()
        .any(|branch| branch.get("uniqueItems") == Some(&Value::Bool(true)))
    {
        tags.push("@uniqueItems".to_string());
    }
    // 注解类只从属性自身取（不跟非 null 分支合并）
    if let Some(default) = property.get("default").filter(|v| !v.is_null()) {
        tags.push(format!("@default {default}"));
    }
    if property.get("deprecated") == Some(&Value::Bool(true)) {
        tags.push("@deprecated".to_string());
    }

    tags.retain(|tag| tag.chars().count() <= MAX_TAG_CHARS);
    tags.iter()
        .map(|tag| escape_line_terminators(tag))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 标签里的数字：整数值去掉小数点，非整数保留小数。
fn format_bound(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// 标签里的取值：数字原样，字符串不加引号（`@format date-时间` 这类约定）。
fn format_tag_value(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `//` 注释行在换行符处会被截断：`\r` / `\n` / U+2028 / U+2029 转成 `\uXXXX`。
fn escape_line_terminators(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\r' | '\n' | '\u{2028}' | '\u{2029}' => out.push_str(&format!("\\u{:04x}", ch as u32)),
            other => out.push(other),
        }
    }
    out
}

/// 对象类型：单行（属性既无描述也无约束）或逐属性一行 + `//` 注释（描述与约束标签）。
fn object_type(schema: &serde_json::Map<String, Value>, ctx: &mut SchemaContext<'_>) -> String {
    let empty = serde_json::Map::new();
    let properties = schema
        .get("properties")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let mut names: Vec<&String> = properties.keys().collect();
    names.sort();

    let mut members: Vec<String> = Vec::new();
    let mut typed_names: Vec<&String> = Vec::new();
    for name in &names {
        let optional = if required.contains(&name.as_str()) {
            ""
        } else {
            "?"
        };
        members.push(format!(
            "{}{optional}: {};",
            property_key(name),
            to_type(&properties[*name], ctx)
        ));
        typed_names.push(name);
    }

    match schema.get("additionalProperties") {
        Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) => members.push("[key: string]: unknown;".to_string()),
        Some(other) => members.push(format!("[key: string]: {};", to_type(other, ctx))),
        None if names.is_empty() => members.push("[key: string]: unknown;".to_string()),
        None => {}
    }

    if members.is_empty() {
        return "{}".to_string();
    }

    let has_comments = typed_names.iter().any(|n| {
        !description_of(&properties[*n]).is_empty()
            || (ctx.tags && !constraint_tags(&properties[*n]).is_empty())
    });
    if !has_comments {
        return format!("{{ {} }}", members.join(" "));
    }

    let mut lines = vec!["{".to_string()];
    for (index, name) in typed_names.iter().enumerate() {
        for line in description_of(&properties[*name]).split(['\r', '\n', '\u{2028}', '\u{2029}']) {
            if !line.trim().is_empty() {
                lines.push(format!("{INDENT}// {}", line.trim()));
            }
        }
        let tags = if ctx.tags {
            constraint_tags(&properties[*name])
        } else {
            String::new()
        };
        if !tags.is_empty() {
            lines.push(format!("{INDENT}// {tags}"));
        }
        lines.push(format!("{INDENT}{}", members[index]));
    }

    for member in members.iter().skip(typed_names.len()) {
        lines.push(format!("{INDENT}{member}"));
    }

    lines.push("}".to_string());
    lines.join("\n")
}

/// 递归渲染：布尔 schema、`$ref`、`const`/`enum`、组合器、`type` 分支。
fn to_type(schema: &Value, ctx: &mut SchemaContext<'_>) -> String {
    match schema {
        Value::Bool(true) => return "unknown".to_string(),
        Value::Bool(false) => return "never".to_string(),
        Value::Object(_) => {}
        _ => return "unknown".to_string(),
    }

    let obj = schema.as_object().expect("handled above");

    if let Some(reference) = obj.get("$ref").and_then(|v| v.as_str()) {
        if ctx.resolving.iter().any(|r| r == reference) || ctx.expansions >= MAX_REF_EXPANSIONS {
            return "unknown".to_string();
        }

        let Some(target) = resolve_ref(reference, ctx.root) else {
            return "unknown".to_string();
        };

        ctx.expansions += 1;
        ctx.resolving.push(reference.to_string());
        let rendered = to_type(&target.clone(), ctx);
        ctx.resolving.pop();
        return rendered;
    }

    if let Some(value) = obj.get("const") {
        return value.to_string();
    }

    if let Some(values) = obj.get("enum").and_then(|v| v.as_array()) {
        return union(values.iter().map(|v| v.to_string()).collect());
    }

    let variants = obj
        .get("anyOf")
        .and_then(|v| v.as_array())
        .or_else(|| obj.get("oneOf").and_then(|v| v.as_array()));

    if let Some(variants) = variants {
        let parts: Vec<String> = variants.iter().map(|v| to_type(v, ctx)).collect();
        return union(parts);
    }

    if let Some(parts) = obj.get("allOf").and_then(|v| v.as_array()) {
        let rendered: Vec<String> = parts
            .iter()
            .map(|p| to_type(p, ctx))
            .filter(|p| p != "unknown")
            .collect();

        if rendered.is_empty() {
            return "unknown".to_string();
        }

        return rendered
            .iter()
            .map(|p| {
                if p.contains(" | ") {
                    format!("({p})")
                } else {
                    p.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" & ");
    }

    match obj.get("type") {
        Some(Value::Array(types)) => {
            let parts: Vec<String> = types
                .iter()
                .map(|t| {
                    let mut copy = obj.clone();
                    copy.insert("type".to_string(), t.clone());
                    to_type(&Value::Object(copy), ctx)
                })
                .collect();
            union(parts)
        }
        Some(Value::String(t)) => match t.as_str() {
            "string" => "string".to_string(),
            "number" | "integer" => "number".to_string(),
            "boolean" => "boolean".to_string(),
            "null" => "null".to_string(),
            "array" => array_type(obj, ctx),
            "object" => object_type(obj, ctx),
            _ => "unknown".to_string(),
        },
        _ => {
            if obj.contains_key("properties")
                || obj.contains_key("additionalProperties")
                || obj.contains_key("required")
            {
                object_type(obj, ctx)
            } else if obj.contains_key("items") || obj.contains_key("prefixItems") {
                array_type(obj, ctx)
            } else {
                "unknown".to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 标识符归一：非法字符变 `_`，空名变 `_`。
    #[test]
    fn identifier_normalization() {
        assert_eq!(
            to_codemode_identifier("mcp__docs__search"),
            "mcp__docs__search"
        );
        assert_eq!(to_codemode_identifier("my-tool"), "my_tool");
        assert_eq!(to_codemode_identifier("9lives"), "_lives");
        assert_eq!(to_codemode_identifier(""), "_");
    }

    /// 基础类型、字面量、联合与组合器。
    #[test]
    fn renders_primitives_literals_and_unions() {
        assert_eq!(schema_to_type(&json!({"type": "string"}), None), "string");
        assert_eq!(schema_to_type(&json!({"type": "integer"}), None), "number");
        assert_eq!(
            schema_to_type(&json!({"type": ["string", "null"]}), None),
            "string | null"
        );
        assert_eq!(schema_to_type(&json!({"const": "a"}), None), "\"a\"");
        assert_eq!(
            schema_to_type(&json!({"enum": ["a", 1, null]}), None),
            "\"a\" | 1 | null"
        );
        assert_eq!(
            schema_to_type(
                &json!({"anyOf": [{"type": "string"}, {"type": "number"}]}),
                None
            ),
            "string | number"
        );
        assert_eq!(
            schema_to_type(&json!({"anyOf": [{"type": "string"}, {}]}), None),
            "unknown"
        );
        assert_eq!(
            schema_to_type(
                &json!({"allOf": [{"anyOf": [{"type": "string"}, {"type": "number"}]}, {"const": 1}]}),
                None
            ),
            "(string | number) & 1"
        );
        assert_eq!(
            schema_to_type(&json!({"$ref": "#/defs/x"}), None),
            "unknown"
        );
        assert_eq!(schema_to_type(&json!(true), None), "unknown");
        assert_eq!(schema_to_type(&json!(false), None), "never");
    }

    /// 对象：属性名按字典序、单行渲染、`additionalProperties` 各分支。
    #[test]
    fn renders_objects_on_one_line_sorted() {
        assert_eq!(
            schema_to_type(
                &json!({
                    "type": "object",
                    "properties": {"city": {"type": "string"}, "max-lines": {"type": "number"}},
                    "required": ["city"],
                    "additionalProperties": false
                }),
                None
            ),
            "{ city: string; \"max-lines\"?: number; }"
        );
        assert_eq!(
            schema_to_type(
                &json!({"type": "object", "additionalProperties": {"type": "number"}}),
                None
            ),
            "{ [key: string]: number; }"
        );
        assert_eq!(
            schema_to_type(&json!({"type": "object"}), None),
            "{ [key: string]: unknown; }"
        );
        assert_eq!(
            schema_to_type(
                &json!({"type": "object", "properties": {}, "additionalProperties": false}),
                None
            ),
            "{}"
        );
    }

    /// 属性带 description 时逐属性一行 + `//` 注释。
    #[test]
    fn puts_property_descriptions_on_comment_lines() {
        assert_eq!(
            schema_to_type(
                &json!({
                    "type": "object",
                    "properties": {
                        "weather": {
                            "type": "array",
                            "description": "look up weather for a given list of locations",
                            "items": {"type": "object", "properties": {"location": {"type": "string"}}, "required": ["location"]}
                        }
                    },
                    "required": ["weather"]
                }),
                None
            ),
            "{\n  // look up weather for a given list of locations\n  weather: Array<{ location: string; }>;\n}"
        );
    }

    /// 数组、元组与 `$ref` 展开 / 递归截断。
    #[test]
    fn renders_arrays_tuples_and_refs() {
        assert_eq!(
            schema_to_type(&json!({"type": "array", "items": {"type": "string"}}), None),
            "Array<string>"
        );
        assert_eq!(
            schema_to_type(
                &json!({"type": "array", "prefixItems": [{"type": "string"}, {"type": "number"}]}),
                None
            ),
            "[string, number]"
        );
        assert_eq!(schema_to_type(&json!({"type": "array"}), None), "unknown[]");
        assert_eq!(
            schema_to_type(
                &json!({
                    "$defs": {"name": {"type": "string"}},
                    "type": "object",
                    "properties": {"n": {"$ref": "#/$defs/name"}}
                }),
                None
            ),
            "{ n?: string; }"
        );
        // 自引用：递归处退化为 unknown，不无限展开。
        assert_eq!(
            schema_to_type(
                &json!({"$defs": {"n": {"properties": {"next": {"$ref": "#/$defs/n"}}}}, "$ref": "#/$defs/n"}),
                None
            ),
            "{ next?: unknown; }"
        );
        // 远端引用不解析。
        assert_eq!(
            schema_to_type(&json!({"$ref": "https://example.com/x"}), None),
            "unknown"
        );
    }

    /// 超过 `max_chars` 的输入类型退化成 `unknown`。
    #[test]
    fn long_input_type_becomes_unknown() {
        let schema = json!({"type": "string"});
        assert_eq!(schema_to_type(&schema, Some(3)), "unknown");
        assert_eq!(schema_to_type(&schema, Some(100)), "string");
    }

    /// 属性的 type 表达不了的约束写成注释行上的 `@` 标签（对齐 pi 的格式与顺序）。
    #[test]
    fn puts_constraints_the_type_cannot_express_on_comment_lines() {
        let schema = json!({
            "type": "object",
            "properties": {
                "timeoutMs": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 30000,
                    "default": 0,
                    "description": "Wait in milliseconds."
                },
                "id": {"type": "string", "pattern": "^[a-z]+$", "maxLength": 8},
                "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 5, "uniqueItems": true, "deprecated": true},
                "name": {"type": "string"}
            },
            "required": ["id"]
        });
        assert_eq!(
            schema_to_type(&schema, None),
            [
                "{",
                "  // @maxLength 8 @pattern ^[a-z]+$",
                "  id: string;",
                "  name?: string;",
                "  // @maxItems 5 @uniqueItems @deprecated",
                "  tags?: Array<string>;",
                "  // Wait in milliseconds.",
                "  // @integer @minimum 0 @maximum 30000 @default 0",
                "  timeoutMs?: number;",
                "}",
            ]
            .join("\n")
        );
    }

    /// 可空联合（`int | None`）的约束写在非 null 分支上，与属性自身的约束同时生效。
    #[test]
    fn reads_constraints_of_optional_fields_from_the_non_null_branch() {
        let schema = json!({
            "type": "object",
            "properties": {
                "limit": {
                    "anyOf": [{"type": "integer", "minimum": 1, "maximum": 100}, {"type": "null"}],
                    "default": null,
                    "title": "Limit"
                }
            }
        });
        assert_eq!(
            schema_to_type(&schema, None),
            "{\n  // @integer @minimum 1 @maximum 100\n  limit?: number | null;\n}"
        );

        let schema = json!({
            "type": "object",
            "properties": {
                "n": {
                    "anyOf": [{"type": "integer", "minimum": 10, "maximum": 50, "format": "int32"}, {"type": "null"}],
                    "minimum": 0,
                    "maximum": 20,
                    "format": "port"
                }
            }
        });
        assert_eq!(
            schema_to_type(&schema, None),
            "{\n  // @integer @minimum 10 @maximum 20 @format port @format int32\n  n?: number | null;\n}"
        );
    }

    /// Draft-04 / OpenAPI 3.0 的布尔 `exclusiveMinimum` / `exclusiveMaximum`。
    #[test]
    fn reads_draft04_boolean_exclusive_bounds() {
        let schema = json!({
            "type": "object",
            "properties": {
                "n": {"type": "integer", "minimum": 0, "exclusiveMinimum": true, "maximum": 10, "exclusiveMaximum": false},
                "m": {"anyOf": [{"type": "number", "maximum": 5, "exclusiveMaximum": true}, {"type": "null"}], "maximum": 8}
            }
        });
        assert_eq!(
            schema_to_type(&schema, None),
            [
                "{",
                "  // @maximum 8 @exclusiveMaximum 5",
                "  m?: number | null;",
                "  // @integer @exclusiveMinimum 0 @maximum 10",
                "  n?: number;",
                "}",
            ]
            .join("\n")
        );
    }

    /// 过长的标签舍弃；标签与描述里的换行符转成 `\uXXXX`（否则 `//` 注释会被截断）。
    #[test]
    fn leaves_out_long_tags_and_escapes_line_terminators() {
        // default 太长 → 整个标签被丢弃（因此不需要多行渲染）
        let schema = json!({
            "type": "object",
            "properties": {"config": {"type": "object", "default": {"text": "x".repeat(100)}}}
        });
        assert_eq!(
            schema_to_type(&schema, None),
            "{ config?: { [key: string]: unknown; }; }"
        );

        let schema = json!({
            "type": "object",
            "properties": {
                "sep": {"type": "string", "pattern": "^[\r\n\u{2028}\u{2029}]$", "default": "\u{2028}", "description": "a\u{2028}b"}
            }
        });
        assert_eq!(
            schema_to_type(&schema, None),
            [
                "{",
                "  // a",
                "  // b",
                "  // @pattern ^[\\u000d\\u000a\\u2028\\u2029]$ @default \"\\u2028\"",
                "  sep?: string;",
                "}",
            ]
            .join("\n")
        );
    }

    /// 超预算时先去标签，仍然超才退成 `unknown`。
    #[test]
    fn drops_constraint_tags_before_rendering_over_the_budget_as_unknown() {
        let schema =
            json!({"type": "object", "properties": {"n": {"type": "integer", "minimum": 1}}});
        assert_eq!(
            schema_to_type(&schema, None),
            "{\n  // @integer @minimum 1\n  n?: number;\n}"
        );
        assert_eq!(schema_to_type(&schema, Some(20)), "{ n?: number; }");
        assert_eq!(schema_to_type(&schema, Some(10)), "unknown");
    }

    /// 签名与样例：标识符归一 + 输出类型 + `declare const tools` 包裹。
    #[test]
    fn renders_signature_and_sample() {
        assert_eq!(
            render_tool_signature(
                "my-tool",
                Some(&json!({"type": "object", "properties": {"a": {"type": "string"}}})),
                Some(&json!({"type": "boolean"}))
            ),
            "my_tool(args: { a?: string; }): Promise<boolean>;"
        );
        assert_eq!(
            render_tool_signature("bare", None, None),
            "bare(args: unknown): Promise<unknown>;"
        );
        assert_eq!(
            render_tool_sample(
                "read",
                "Read a file",
                &[],
                Some(&json!({ "type": "object" })),
                None
            ),
            "Read a file\n\ncodemode tool declaration:\n```ts\ndeclare const tools: { read(args: { [key: string]: unknown; }): Promise<unknown>; };\n```"
        );

        // 2.22：prompt guidelines 以 `- ` 项目符号拼在描述之后（空条目跳过）
        assert_eq!(
            render_tool_sample(
                "bash",
                "Run a command",
                &[
                    "Inspect PI_* environment variables.".to_string(),
                    "   ".to_string(),
                    "  Prefer short commands.  ".to_string(),
                ],
                Some(&json!({ "type": "object" })),
                None
            ),
            "Run a command\n\n- Inspect PI_* environment variables.\n- Prefer short commands.\n\ncodemode tool declaration:\n```ts\ndeclare const tools: { bash(args: { [key: string]: unknown; }): Promise<unknown>; };\n```"
        );
    }
}
