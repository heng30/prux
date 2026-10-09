//! 工具注册表与共享类型。分层说明（工具三层设计）：
//!
//! - [`ToolDef`] 负责「描述」：name/description/parameters 只告诉模型工具长什么样；
//! - 各工具文件的 `execute_*` 负责「执行」：真正的行为实现；
//! - 工具间共享的路径规范化等辅助函数以 `pub(crate)` 提供。

use super::read;
use crate::{
    core::provider::Usage,
    error::Error,
    utils::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES},
};
use serde_json::{Value, json};
use std::path::PathBuf;

pub(crate) use crate::utils::paths::relativize_path;

/// 内置工具默认启用 constrained sampling（strict-prefer JSON-schema）。
const BUILTIN_CONSTRAINED_SAMPLING: bool = true;

/// 工具执行模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    /// 同批工具并行执行（默认）
    Parallel,
    /// 顺序执行
    Sequential,
}

/// 工具定义
pub struct ToolDef {
    /// 工具名，作为模型调用与注册表的唯一标识。
    pub name: &'static str,
    /// 面向模型的工具用途说明。
    pub description: String,
    /// Human-readable label（UI 展示；TUI 未消费时为 None）
    pub label: Option<String>,
    /// 工具入参的 JSON Schema。
    pub parameters: Value,
    /// 系统提示 Available tools 段单行片段
    pub snippet: String,
    /// 系统提示 Guidelines 段追加要点
    pub prompt_guidelines: Vec<String>,
    /// provider constrained sampling（false=不请求约束采样）
    pub constrained_sampling: bool,
    /// 渲染外壳（"default" | "self"；TUI 未消费，仅声明）
    pub render_shell: Option<String>,
    /// 单工具执行模式覆盖（None=默认并行）
    pub execution_mode: Option<ToolExecutionMode>,
    /// 工具参数准备（LLM 参数发到执行前先变换）
    pub prepare_arguments: Option<fn(&Value) -> Value>,
    /// 面向程序化调用方（codemode 脚本）的结构化输出 schema；无则为 None。
    pub output_schema: Option<Value>,
}

impl ToolDef {
    /// 简洁构造：name/description/parameters/snippet，其余字段取默认。
    pub fn simple(name: &'static str, description: &str, parameters: Value, snippet: &str) -> Self {
        ToolDef {
            name,
            description: description.to_string(),
            label: None,
            parameters,
            snippet: snippet.to_string(),
            prompt_guidelines: Vec::new(),
            constrained_sampling: false,
            render_shell: None,
            execution_mode: None,
            prepare_arguments: None,
            output_schema: None,
        }
    }
}

/// 全部内置工具名
pub fn all_tool_names() -> Vec<String> {
    let base = ["read", "bash", "edit", "write", "grep", "find", "ls"];

    #[cfg(windows)]
    let names = base.into_iter().chain(["powershell"]);

    #[cfg(not(windows))]
    let names = base.into_iter();

    names.map(|s| s.to_string()).collect()
}

/// 内置工具的一句话说明（子面板的描述列）；未收录的工具名返回空串。
pub fn tool_description(name: &str) -> &'static str {
    match name {
        "read" => "Read files and images",
        "bash" => "Run shell commands",
        "edit" => "Apply targeted edits to files",
        "write" => "Create or overwrite files",
        "grep" => "Search file contents",
        "find" => "Find files by name or glob",
        "ls" => "List directory entries",
        #[cfg(windows)]
        "powershell" => "Run PowerShell commands",
        _ => "",
    }
}

/// 按名称取工具定义（注册表），未命中返回 None
pub fn tool_def(name: &str) -> Option<ToolDef> {
    tool_defs(std::slice::from_ref(&name.to_string()))
        .into_iter()
        .next()
}

/// 工具执行模式覆盖查询（无 ToolDef = 默认并行）
pub fn tool_execution_mode(name: &str) -> Option<ToolExecutionMode> {
    tool_def(name).and_then(|d| d.execution_mode)
}

/// 应用工具参数准备
pub fn apply_prepare_arguments(name: &str, args: &Value) -> Value {
    match tool_def(name) {
        Some(d) => d
            .prepare_arguments
            .map(|f| f(args))
            .unwrap_or_else(|| args.clone()),
        None => args.clone(),
    }
}

/// JSON 值的类型名，用于拼校验失败文案。
fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// 单个属性的类型校验：`val` 与 `pschema.type` 不符时返回「expected …, got …」文案。
/// schema 未声明（或声明了未知）`type` 时一律视为通过，返回 `None`。
fn schema_type_mismatch(_key: &str, pschema: &Value, val: &Value) -> Option<String> {
    let ty = pschema.get("type").and_then(|v| v.as_str());
    let ok = match ty {
        Some("string") => val.is_string(),
        Some("number") => val.is_number(),
        Some("integer") => val.is_i64() || val.is_u64(),
        Some("boolean") => val.is_boolean(),
        Some("object") => val.is_object(),
        Some("array") => val.is_array(),
        _ => true,
    };

    if ok {
        None
    } else {
        Some(format!(
            "expected type '{}', got '{}'",
            ty.unwrap_or("any"),
            json_type_name(val)
        ))
    }
}

/// 轻量 JSON Schema 参数校验。覆盖：required 缺失、属性类型、additionalProperties:false 未知键。
/// 成功返回归一化参数（null → 默认空对象）。
pub fn validate_tool_arguments(
    name: &str,
    schema: &Value,
    args: &Value,
) -> std::result::Result<Value, String> {
    let obj = match args {
        Value::Null => json!({}),
        v if v.is_object() => v.clone(),
        _ => {
            return Err(format!(
                "Validation failed for tool \"{name}\":\n  - (root): expected object\n\nReceived arguments:\n{}",
                serde_json::to_string_pretty(args).unwrap_or_default()
            ));
        }
    };

    let mut errors: Vec<(String, String)> = Vec::new();
    if let Some(required) = schema.get("required").and_then(|v| v.as_array()) {
        for r in required {
            if let Some(key) = r.as_str()
                && obj.get(key).is_none()
            {
                errors.push((
                    format!("/{key}"),
                    format!("required property '{key}' is missing"),
                ));
            }
        }
    }

    let props: Vec<(String, Value)> = schema
        .get("properties")
        .and_then(|v| v.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let additional_allowed = schema
        .get("additionalProperties")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    if let Some(obj_map) = obj.as_object() {
        for (key, val) in obj_map {
            match props.iter().find(|(k, _)| k == key) {
                Some((_, pschema)) => {
                    if let Some(msg) = schema_type_mismatch(key, pschema, val) {
                        errors.push((format!("/{key}"), msg));
                    }
                }
                None if !additional_allowed => {
                    errors.push((format!("/{key}"), format!("unexpected property '{key}'")));
                }
                None => {}
            }
        }
    }

    if errors.is_empty() {
        Ok(obj)
    } else {
        let detail: String = errors
            .iter()
            .map(|(p, m)| format!("  - {p}: {m}"))
            .collect::<Vec<_>>()
            .join("\n");

        Err(format!(
            "Validation failed for tool \"{name}\":\n{detail}\n\nReceived arguments:\n{}",
            serde_json::to_string_pretty(args).unwrap_or_default()
        ))
    }
}

/// 工具执行结果：除文本外携带 details/terminate/附件
/// read 工具对图片的附件（read 返回 {type:"image", data, mimeType}）
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResultAttachment {
    /// 附件数据的 base64 编码。
    pub data_base64: String,
    /// 附件 mime 类型（如 `image/png`）。
    pub mime_type: String,
    /// 原始尺寸（缩放/转换时可填写；用于 formatDimensionNote 提示）
    pub original_size: Option<(u32, u32)>,
    /// 是否经过了转换（BMP 等 → PNG）
    pub converted_from: Option<String>,
}

/// 工具执行结果：文本、附加元数据/用量、终止语义与图片附件。
#[derive(Debug, Default)]
pub struct ToolResult {
    /// 返回给模型的文本内容。
    pub text: String,
    /// 附加元数据
    pub details: Option<Value>,
    /// 结构化输出（扩展 `outputSchema` / MCP `structuredContent`）：
    /// 落盘时会并进 `details.structuredContent`，同时以顶层 `structuredContent` 进工具事件。
    pub structured_content: Option<Value>,
    /// 执行消耗
    pub usage: Option<Usage>,
    /// 结果是否带终止语义：batch 执行完后不再请求 LLM
    pub terminate: bool,
    /// 附件（图片）：组装消息时转为 ContentBlock::Image
    pub attachments: Vec<ToolResultAttachment>,
    /// 该结果是否为失败结果（`true` 时模型侧按错误呈现，但结构化结果/附件依然可用）。
    /// 与 `Err(ToolError)` 的分工：纯文本错误走 `Err`，需要同时携带结构化结果时走本字段。
    pub is_error: bool,
}

impl ToolResult {
    /// 构造只含文本的结果：无 details/usage/附件，且不终止 Agent 循环。
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            details: None,
            structured_content: None,
            usage: None,
            terminate: false,
            attachments: Vec::new(),
            is_error: false,
        }
    }

    /// 构造失败结果：`is_error=true`，其余与 [`ToolResult::text`] 相同。
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::text(text)
        }
    }

    /// 把结果标记为失败后返回自身（链式调用）。
    pub fn with_error(mut self) -> Self {
        self.is_error = true;
        self
    }

    /// 设置结构化输出后返回自身（链式调用）。
    pub fn with_structured_content(mut self, value: Value) -> Self {
        self.structured_content = Some(value);
        self
    }

    /// 追加一个图片附件后返回自身（链式调用）。
    pub fn with_attachment(mut self, att: ToolResultAttachment) -> Self {
        self.attachments.push(att);
        self
    }
}

/// 工具执行错误（统一编码为消息，不打断 Agent 循环）
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ToolError(pub String);

impl From<ToolError> for Error {
    /// 把工具错误包成通用 [`Error::Message`]，保留原始文案（工具错误不打断 Agent 循环）。
    fn from(e: ToolError) -> Self {
        Error::Message(e.0)
    }
}

/// 按名称生成工具定义列表（注册表）
pub fn tool_defs(names: &[String]) -> Vec<ToolDef> {
    let mut defs = Vec::new();
    for name in names {
        match name.as_str() {
            "read" => defs.push(read_def()),
            "write" => defs.push(write_def()),
            "edit" => defs.push(edit_def()),
            "bash" => defs.push(bash_def()),
            "grep" => defs.push(grep_def()),
            "find" => defs.push(find_def()),
            "ls" => defs.push(ls_def()),
            #[cfg(windows)]
            "powershell" => defs.push(powershell_def()),
            _ => {}
        }
    }
    defs
}

/// 内置 `read` 工具定义：参数 path/offset/limit，描述里的截断上限随常量自动更新。
fn read_def() -> ToolDef {
    ToolDef {
        name: "read",
        label: None,
        prompt_guidelines: Vec::new(),
        constrained_sampling: BUILTIN_CONSTRAINED_SAMPLING,
        render_shell: None,
        description: format!(
            "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to {} lines or {}KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
            DEFAULT_MAX_LINES,
            DEFAULT_MAX_BYTES / 1024
        ),
        snippet: "Read the contents of a file".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
                "offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
                "limit": { "type": "number", "description": "Maximum number of lines to read" }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        execution_mode: None,
        prepare_arguments: None,
        output_schema: Some(read::output_schema()),
    }
}

/// 内置 `write` 工具定义：参数 path/content，覆盖写并自动建父目录。
fn write_def() -> ToolDef {
    ToolDef {
        name: "write",
        label: None,
        prompt_guidelines: Vec::new(),
        constrained_sampling: BUILTIN_CONSTRAINED_SAMPLING,
        render_shell: None,
        description: "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".to_string(),
        snippet: "Write content to a file".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
                "content": { "type": "string", "description": "Content to write to the file" }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }),
    execution_mode: None,
    prepare_arguments: None,
    output_schema: None,
    }
}

/// 内置 `edit` 工具定义：参数 path/edits（oldText/newText 精确替换），
/// 并挂上参数归一化钩子 [`super::edit::normalize_edit_args`]。
fn edit_def() -> ToolDef {
    ToolDef {
        name: "edit",
        label: None,
        prompt_guidelines: Vec::new(),
        constrained_sampling: BUILTIN_CONSTRAINED_SAMPLING,
        render_shell: None,
        description: "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.".to_string(),
        snippet: "Edit a single file using exact text replacement".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
                "edits": {
                    "type": "array",
                    "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": { "type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call." },
                            "newText": { "type": "string", "description": "Replacement text for this targeted edit." }
                        },
                        "required": ["oldText", "newText"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path", "edits"],
            "additionalProperties": false
        }),
    execution_mode: None,
    prepare_arguments: Some(super::edit::normalize_edit_args),
    output_schema: None,
    }
}

/// shell 类工具（bash / powershell）面向程序化调用方的结构化输出 schema：
/// 至多 1 MiB 的完整输出 + 退出码 + 墙钟耗时（模型侧仍是 50 KiB / 2000 行的文本）。
fn shell_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "output": { "type": "string", "description": "Combined stdout and stderr, up to 1 MiB. Longer output keeps its first and last 512 KiB around an omission marker." },
            "truncated": { "type": "boolean", "description": "Whether `output` omits part of the command output" },
            "full_output_path": { "type": "string", "description": "Temp file with the full output, when truncated" },
            "exit_code": { "type": "number" },
            "wall_time_seconds": { "type": "number" }
        },
        "required": ["output", "truncated", "exit_code", "wall_time_seconds"]
    })
}

/// 内置 `bash` 工具定义：参数 command/timeout，附带 PI_* 环境变量的 prompt guideline。
fn bash_def() -> ToolDef {
    ToolDef {
        name: "bash",
        label: None,
        prompt_guidelines: vec![
            "You can inspect PI_* environment variables for current model and session details."
                .to_string(),
        ],
        constrained_sampling: BUILTIN_CONSTRAINED_SAMPLING,
        render_shell: None,
        description: format!(
            "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last {} lines or {}KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.",
            DEFAULT_MAX_LINES,
            DEFAULT_MAX_BYTES / 1024
        ),
        snippet: "Execute bash commands (ls, grep, find, etc.)".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Bash command to execute" },
                "timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" }
            },
            "required": ["command"],
            "additionalProperties": false
        }),
        execution_mode: None,
        prepare_arguments: None,
        output_schema: Some(shell_output_schema()),
    }
}

/// 内置 `grep` 工具定义：参数 pattern/path/glob/ignoreCase/literal/context/limit；
/// 正则表达式自由度高，不启用 constrained sampling。
fn grep_def() -> ToolDef {
    ToolDef {
        name: "grep",
        label: None,
        prompt_guidelines: Vec::new(),
        constrained_sampling: false,
        render_shell: None,
        description: format!(
            "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or {}KB (whichever is hit first). Long lines are truncated to 500 chars.",
            DEFAULT_MAX_BYTES / 1024
        ),
        snippet: "Search file contents for patterns (respects .gitignore)".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Search pattern (regex or literal string)" },
                "path": { "type": "string", "description": "Directory or file to search (default: current directory)" },
                "glob": { "type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'" },
                "ignoreCase": { "type": "boolean", "description": "Case-insensitive search (default: false)" },
                "literal": { "type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)" },
                "context": { "type": "number", "description": "Number of lines to show before and after each match (default: 0)" },
                "limit": { "type": "number", "description": "Maximum number of matches to return (default: 100)" }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
        execution_mode: None,
        prepare_arguments: None,
        output_schema: None,
    }
}

/// 内置 `find` 工具定义：参数 pattern/path/limit；glob 自由度高，不启用 constrained sampling。
fn find_def() -> ToolDef {
    ToolDef {
        name: "find",
        label: None,
        prompt_guidelines: Vec::new(),
        constrained_sampling: false,
        render_shell: None,
        description: format!(
            "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to 1000 results or {}KB (whichever is hit first).",
            DEFAULT_MAX_BYTES / 1024
        ),
        snippet: "Find files by glob pattern (respects .gitignore)".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'" },
                "path": { "type": "string", "description": "Directory to search in (default: current directory)" },
                "limit": { "type": "number", "description": "Maximum number of results (default: 1000)" }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
        execution_mode: None,
        prepare_arguments: None,
        output_schema: None,
    }
}

/// 内置 `ls` 工具定义：参数 path/limit，无必填参数，不启用 constrained sampling。
fn ls_def() -> ToolDef {
    ToolDef {
        name: "ls",
        label: None,
        prompt_guidelines: Vec::new(),
        constrained_sampling: false,
        render_shell: None,
        description: format!(
            "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or {}KB (whichever is hit first).",
            DEFAULT_MAX_BYTES / 1024
        ),
        snippet: "List directory contents".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list (default: current directory)" },
                "limit": { "type": "number", "description": "Maximum number of entries to return (default: 500)" }
            },
            "required": [],
            "additionalProperties": false
        }),
        execution_mode: None,
        prepare_arguments: None,
        output_schema: None,
    }
}

/// powershell 工具定义
#[cfg(windows)]
fn powershell_def() -> ToolDef {
    ToolDef {
        name: "powershell",
        label: None,
        prompt_guidelines: Vec::new(),
        constrained_sampling: BUILTIN_CONSTRAINED_SAMPLING,
        render_shell: None,
        description: "Execute a PowerShell command in the current working directory. Returns stdout and stderr. Optionally provide a timeout in seconds.".to_string(),
        snippet: "Execute a PowerShell command in the current working directory".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "PowerShell command to execute" },
                "timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" }
            },
            "required": ["command"],
            "additionalProperties": false
        }),
        execution_mode: None,
        prepare_arguments: None,
        output_schema: Some(shell_output_schema()),
    }
}

/// 规范化工具参数中的路径：替换特殊空白字符、去掉 @ 前缀
pub(crate) fn normalize_tool_path(path: &str) -> String {
    let normalized: String = path
        .chars()
        .map(|c| match c as u32 {
            0x00A0 | 0x2000..=0x200A | 0x202F | 0x205F | 0x3000 => ' ',
            _ => c,
        })
        .collect();
    if let Some(rest) = normalized.strip_prefix('@') {
        rest.to_string()
    } else {
        normalized
    }
}

/// 解析工具路径（相对路径基于 cwd）
pub fn resolve_tool_path(path: &str, cwd: &str) -> PathBuf {
    let normalized = normalize_tool_path(path);
    let p = PathBuf::from(&normalized);
    if p.is_absolute() {
        p
    } else {
        PathBuf::from(cwd).join(p)
    }
}

/// 简易图片魔数检测（read 工具内部使用）
pub(crate) fn detect_supported_image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    crate::utils::mime::detect_image_mime(bytes)
}

/// io::Error 的 errno 字符串（edit/write 错误信息用）
pub(crate) fn err_code(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "ENOENT".to_string(),
        std::io::ErrorKind::PermissionDenied => "EACCES".to_string(),
        _ => "EIO".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tools::validate_tool_arguments;

    #[test]
    fn validate_missing_required_reports_pi_style_error() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
            "additionalProperties": false
        });
        let err = validate_tool_arguments("read", &schema, &json!({})).unwrap_err();
        assert!(err.contains("Validation failed for tool \"read\""), "{err}");
        assert!(err.contains("required property 'path' is missing"), "{err}");
    }

    #[test]
    fn validate_type_mismatch_and_additional_properties() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
            "additionalProperties": false
        });
        let err = validate_tool_arguments("read", &schema, &json!({ "path": 42 })).unwrap_err();
        assert!(
            err.contains("expected type 'string', got 'number'"),
            "{err}"
        );

        let err = validate_tool_arguments("read", &schema, &json!({ "path": "a", "bogus": 1 }))
            .unwrap_err();
        assert!(err.contains("unexpected property 'bogus'"), "{err}");
    }

    #[test]
    fn validate_null_normalizes_to_empty_object() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": [],
            "additionalProperties": false
        });
        assert_eq!(
            validate_tool_arguments("ls", &schema, &Value::Null).unwrap(),
            json!({})
        );
    }

    #[test]
    fn validate_passes_with_valid_args() {
        let schema = json!({
            "type": "object",
            "properties": { "path": { "type": "string" }, "limit": { "type": "number" } },
            "required": ["path"],
            "additionalProperties": false
        });
        let out =
            validate_tool_arguments("read", &schema, &json!({ "path": "x", "limit": 3 })).unwrap();
        assert_eq!(out["path"], "x");
    }

    #[test]
    fn execution_mode_tool_def_queries() {
        // task 已移出内置（子代理走 subagent 扩展）；内置默认并行
        assert_eq!(tool_execution_mode("read"), None);
        assert_eq!(tool_execution_mode("no-such-tool"), None);
        // all_tool_names 不含 task
        assert!(!all_tool_names().contains(&"task".to_string()));
    }

    #[test]
    fn prepare_arguments_edit_string_edits_array() {
        // edit 工具把字符串形式的 edits 归一化为数组
        let args = json!({ "path": "a.rs", "edits": "[{\"oldText\":\"a\",\"newText\":\"b\"}]" });
        let prepared = apply_prepare_arguments("edit", &args);
        assert!(prepared["edits"].is_array(), "{prepared}");
        assert_eq!(
            apply_prepare_arguments("read", &json!({ "path": "x" })),
            json!({ "path": "x" })
        );
    }
}
