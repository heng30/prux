//! `lsp` 扩展：向语言服务器按需查询定义、引用、符号与诊断。
//!
//! 一个 `lsp` 工具按 `operation` 分派：
//!
//! | operation | 需要 | 返回 |
//! |---|---|---|
//! | `diagnostics` | `path` | 该文件的编译/类型诊断（走 `publishDiagnostics` 推送） |
//! | `definition` | `path` + `line` + `character` | 定义位置（含 `LocationLink` 回退） |
//! | `references` | `path` + `line` + `character` | 全部引用位置 |
//! | `symbols` | `path` 或 `query` | 文件符号大纲 / 工作区符号搜索 |
//!
//! 服务器按 [`servers::SERVERS`] 里的表发现（`$PATH` 探测），进程按
//! 「命令 + 工作区根」常驻复用；扩展被禁用时一并杀掉。诊断要求文件先在服务器里打开
//! （`didOpen` / `didChange`），所以每次调用都会把磁盘上的当前内容同步过去。
//!
//! 行号列号对模型统一是 **1-based、按字符计**；内部转成 LSP 的 0-based UTF-16 列，
//! 非 ASCII 行不会错位。

mod protocol;
mod servers;

use crate::{
    core::{
        extensions::{
            Extension, ExtensionMode, ExtensionTool, ForkProjectInfo, ToolAnnotations, ToolExecCtx,
            ToolExposure,
        },
        settings_manager::agent_dir,
        tools::{ToolError, ToolResult},
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_LSP},
    utils::{
        http::{path_to_uri, uri_to_path},
        paths::resolve_path,
    },
};
use futures_util::future::BoxFuture;
use protocol::{DiagEntry, LspClient};
use serde_json::{Value, json};
use servers::ServerSpec;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use strum_macros::{Display, EnumString, IntoStaticStr};

/// 常驻服务器缓存：键是（命令, 工作区根）。
type ClientCache = HashMap<(String, PathBuf), Arc<LspClient>>;

/// 扩展名（也是 `/extension` 面板中的标识）。
const EXT: &str = "lsp";

/// 工具名（模型可见）。
const TOOL_LSP: &str = "lsp";

/// 单次查询的请求超时；首次打开大项目时服务器要索引，给足时间。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

/// 等 `publishDiagnostics` 推送的超时。
const DIAGNOSTICS_WAIT: Duration = Duration::from_secs(20);

/// 送进 LSP 前允许的最大文件字节数；更大的文件走 `grep` / `read` 而不是语言服务器。
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

/// 诊断最多展示的条数。
const MAX_DIAGNOSTICS: usize = 50;

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static LSP_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_LSP,
    make: || -> Arc<dyn Extension> { Arc::new(Lsp) },
};

/// `lsp` 扩展主体。
pub struct Lsp;

impl Extension for Lsp {
    /// 返回固定扩展名 `lsp`。
    fn name(&self) -> &str {
        EXT
    }

    /// 返回 `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Query a language server for definitions, references, symbols and diagnostics (lsp)."
    }

    /// 声明本扩展移植自 oh-my-pi，供 `/extension` 面板展示来源链接。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "oh-my-pi".to_string(),
            plugin_version: "18.12.2".to_string(),
            url: "https://github.com/can1357/oh-my-pi".to_string(),
        })
    }

    /// 默认关闭：依赖用户机器上装好的语言服务器，按需在 `/extension` 面板开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 只在 `Dev` / `Creator` 扩展模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev]
    }

    /// 提供 `lsp` 工具。
    fn tools(&self) -> Vec<ExtensionTool> {
        vec![lsp_tool()]
    }

    /// 扩展被关闭时杀掉所有常驻服务器，避免留下孤儿进程。
    fn on_enabled_changed(&self, enabled: bool) {
        if !enabled {
            shutdown_clients();
        }
    }

    /// 按工具名分发；未知工具名返回 `ToolError`。
    fn execute_tool_async(
        &self,
        name: String,
        args: Value,
        ctx: ToolExecCtx,
    ) -> BoxFuture<'static, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            match name.as_str() {
                TOOL_LSP => run_lsp(&args, &ctx).await,
                other => Err(ToolError(format!("extension {EXT}: unknown tool: {other}"))),
            }
        })
    }
}

/// 工具支持的查询类型（`strum` 负责与参数字符串互转）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr, Display)]
#[strum(serialize_all = "lowercase")]
enum Operation {
    /// 文件的编译 / 类型诊断。
    Diagnostics,
    /// 光标处的定义位置。
    Definition,
    /// 光标处符号的全部引用。
    References,
    /// 文件符号大纲（有 `path`）或工作区符号搜索（有 `query`）。
    Symbols,
}

/// 取服务器缓存的进程级单例。
fn clients() -> &'static Mutex<ClientCache> {
    /// 服务器缓存的进程级单例。
    static S: OnceLock<Mutex<ClientCache>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取（或新建）某个工作区的服务器连接。
async fn client_for(spec: &'static ServerSpec, root: &Path) -> Result<Arc<LspClient>, String> {
    let key = (spec.command.to_string(), root.to_path_buf());
    if let Some(client) = clients()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
    {
        return Ok(Arc::clone(client));
    }

    let log_path = agent_dir()
        .join("logs")
        .join(format!("lsp-{}.log", spec.command));
    let client = LspClient::spawn(spec, root, &log_path).await?;

    let mut map = clients().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = map.get(&key) {
        return Ok(Arc::clone(existing));
    }
    map.insert(key, Arc::clone(&client));
    Ok(client)
}

/// 丢掉所有连接（扩展禁用时调用）；最后一个 `Arc` 掉落后子进程随之退出。
fn shutdown_clients() {
    clients().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// 构造 `lsp` 工具定义（描述、参数 JSON Schema 与 snippet）。
fn lsp_tool() -> ExtensionTool {
    ExtensionTool {
        exposure: ToolExposure::Direct,
        namespace: None,
        annotations: ToolAnnotations {
            read_only: true,
            destructive: false,
            idempotent: true,
        },
        output_schema: None,
        name: TOOL_LSP.to_string(),
        description: "Ask a language server about a file: diagnostics, definition, references, or symbols. \
                      Unlike grep, this understands types, trait implementations, overloads and macro expansion."
            .to_string(),
        label: Some("Language server".to_string()),
        parameters: json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["diagnostics", "definition", "references", "symbols"],
                    "description": "What to ask the language server."
                },
                "path": {
                    "type": "string",
                    "description": "File path, resolved against the working directory. Required for diagnostics/definition/references and for file symbols."
                },
                "line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based line of the cursor position (definition/references)."
                },
                "character": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based character offset within the line, counted in characters (definition/references)."
                },
                "query": {
                    "type": "string",
                    "description": "Workspace symbol query (symbols without path)."
                }
            },
            "required": ["operation"]
        }),
        snippet: "Use to get compiler diagnostics or to resolve definitions, references and symbols across a codebase."
            .to_string(),
        prompt_guidelines: vec![
            "Run `diagnostics` after editing code instead of shelling out to a compiler: it also covers unsaved edits you have written to disk.".to_string(),
            "`line` and `character` are 1-based and counted in characters; pass the position of the identifier, not the start of the line.".to_string(),
            "The first call in a project may block while the server indexes; if it times out, retry once before falling back to `grep`.".to_string(),
        ],
        constrained_sampling: false,
        render_shell: None,
        execution_mode: None,
        prepare_arguments: None,
        grammar_sampling: None,
    }
}

/// `lsp` 工具的执行体：解析参数 → 确保连接 → 同步文档 → 分派查询。
async fn run_lsp(args: &Value, ctx: &ToolExecCtx) -> Result<ToolResult, ToolError> {
    let operation: Operation = args
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError("missing required parameter: operation".to_string()))?
        .parse()
        .map_err(|_| {
            ToolError(
                "`operation` must be one of: diagnostics, definition, references, symbols"
                    .to_string(),
            )
        })?;

    let path_arg = args.get("path").and_then(Value::as_str).map(str::trim);
    let query = args.get("query").and_then(Value::as_str).map(str::trim);

    match (operation, path_arg, query) {
        (Operation::Symbols, None, Some(query)) if !query.is_empty() => {
            run_workspace_symbols(query, ctx).await
        }
        (Operation::Symbols, None, _) => Err(ToolError(
            "symbols needs either `path` (file outline) or `query` (workspace search)".to_string(),
        )),
        (_, None, _) => Err(ToolError(format!(
            "missing required parameter: path (required for {operation})"
        ))),
        (_, Some(""), _) => Err(ToolError("`path` must not be empty.".to_string())),
        (operation, Some(raw), _) => run_file_query(operation, raw, args, ctx).await,
    }
}

/// 带 `path` 的查询：诊断 / 定义 / 引用 / 文件符号。
async fn run_file_query(
    operation: Operation,
    raw_path: &str,
    args: &Value,
    ctx: &ToolExecCtx,
) -> Result<ToolResult, ToolError> {
    let path = resolve_path(raw_path, &ctx.cwd);
    if !path.is_file() {
        return Err(ToolError(format!("not a file: {}", path.display())));
    }
    let language_id = servers::language_id_for(&path).ok_or_else(|| {
        ToolError(format!(
            "unsupported file type for the language server: {} (supported: rust, c/cpp, typescript, \
             javascript, python, go, java, ruby, lua, csharp, php)",
            path.display()
        ))
    })?;
    let spec = servers::server_for(language_id)
        .ok_or_else(|| ToolError(format!("no language server configured for `{language_id}`")))?;

    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|e| ToolError(format!("cannot stat {}: {e}", path.display())))?;
    if metadata.len() > MAX_FILE_BYTES {
        return Err(ToolError(format!(
            "{} is larger than {} MiB; use `read` or `grep` instead of the language server",
            path.display(),
            MAX_FILE_BYTES / (1024 * 1024)
        )));
    }

    let text = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| ToolError(format!("cannot read {}: {e}", path.display())))?;
    let absolute = path.canonicalize().unwrap_or_else(|_| path.clone());
    let root = servers::workspace_root(&absolute, spec);
    let client = client_for(spec, &root).await.map_err(ToolError)?;

    let uri = path_to_uri(&absolute);
    if uri.is_empty() {
        return Err(ToolError(format!(
            "cannot convert {} to a file URI",
            absolute.display()
        )));
    }

    let synced_at = Instant::now();
    let version = client
        .sync_document(&uri, language_id, &text)
        .await
        .map_err(ToolError)?;

    let body = match operation {
        Operation::Diagnostics => {
            let entry = client
                .wait_diagnostics(&uri, synced_at, version, DIAGNOSTICS_WAIT)
                .await;
            format_diagnostics(&absolute, entry)
        }
        Operation::Symbols => {
            let params = json!({ "textDocument": { "uri": uri } });
            let result = client
                .request("textDocument/documentSymbol", params, REQUEST_TIMEOUT)
                .await
                .map_err(ToolError)?;
            format_document_symbols(&absolute, &result)
        }
        Operation::Definition | Operation::References => {
            let (line, character) = cursor_position(args, &text)?;
            let position = json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            });
            if operation == Operation::Definition {
                let result = client
                    .request("textDocument/definition", position, REQUEST_TIMEOUT)
                    .await
                    .map_err(ToolError)?;
                format_locations(&absolute, line, character, "definition", &result)
            } else {
                let params = json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character },
                    "context": { "includeDeclaration": true }
                });
                let result = client
                    .request("textDocument/references", params, REQUEST_TIMEOUT)
                    .await
                    .map_err(ToolError)?;
                format_locations(&absolute, line, character, "reference", &result)
            }
        }
    };

    Ok(ToolResult::text(body))
}

/// 不带 `path` 的符号搜索：从 `cwd` 向上找第一个能识别的项目根，用它的服务器查。
async fn run_workspace_symbols(query: &str, ctx: &ToolExecCtx) -> Result<ToolResult, ToolError> {
    let (spec, root) = servers::detect_workspace(Path::new(&ctx.cwd)).ok_or_else(|| {
        ToolError(format!(
            "cannot tell which language this workspace is in; pass `path` to a file whose \
             language has a server (cwd: {})",
            ctx.cwd
        ))
    })?;

    let client = client_for(spec, &root).await.map_err(ToolError)?;
    let result = client
        .request(
            "workspace/symbol",
            json!({ "query": query }),
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(ToolError)?;
    Ok(ToolResult::text(format_workspace_symbols(query, &result)))
}

/// 解析并校验 `line` / `character`，返回 LSP 的 0-based 行与 UTF-16 列。
fn cursor_position(args: &Value, text: &str) -> Result<(u32, u32), ToolError> {
    let line = args
        .get("line")
        .and_then(Value::as_u64)
        .ok_or_else(|| ToolError("missing required parameter: line".to_string()))?;
    let character = args
        .get("character")
        .and_then(Value::as_u64)
        .ok_or_else(|| ToolError("missing required parameter: character".to_string()))?;
    if line == 0 || character == 0 {
        return Err(ToolError(
            "`line` and `character` are 1-based and must be >= 1".to_string(),
        ));
    }

    let line_text = text
        .lines()
        .nth(line as usize - 1)
        .ok_or_else(|| ToolError(format!("line {line} is past the end of the file")))?;

    let char_index = character as usize - 1;
    if char_index > line_text.chars().count() {
        return Err(ToolError(format!(
            "character {character} is past the end of line {line}"
        )));
    }

    Ok(((line - 1) as u32, utf16_column(line_text, char_index)))
}

/// 把「行内第几个字符」（0-based，char 计）转成 UTF-16 列。
fn utf16_column(line: &str, char_index: usize) -> u32 {
    line.chars()
        .take(char_index)
        .map(|c| c.len_utf16() as u32)
        .sum()
}

/// 把 UTF-16 列转回「行内第几个字符」（0-based，char 计）。
fn char_column(line: &str, utf16: u64) -> usize {
    let mut units = 0u64;
    for (index, c) in line.chars().enumerate() {
        if units >= utf16 {
            return index;
        }
        units += c.len_utf16() as u64;
    }
    line.chars().count()
}

/// 诊断条目 → 面向模型的文本。
fn format_diagnostics(path: &Path, entry: Option<DiagEntry>) -> String {
    let Some(entry) = entry else {
        return format!(
            "{}: the language server did not report diagnostics in time (it may still be indexing). \
             Retry in a moment.",
            path.display()
        );
    };
    if entry.items.is_empty() {
        return format!("{}: no diagnostics.", path.display());
    }

    // 只读一次源文件：每条诊断都要把 UTF-16 列换算回字符列。
    let source = std::fs::read_to_string(path).unwrap_or_default();
    let source_lines: Vec<&str> = source.lines().collect();

    let mut lines = vec![format!(
        "{} — {} diagnostic{}:",
        path.display(),
        entry.items.len(),
        if entry.items.len() == 1 { "" } else { "s" }
    )];
    for item in entry.items.iter().take(MAX_DIAGNOSTICS) {
        lines.push(format_diagnostic(&source_lines, item));
    }
    if entry.items.len() > MAX_DIAGNOSTICS {
        lines.push(format!(
            "  … and {} more",
            entry.items.len() - MAX_DIAGNOSTICS
        ));
    }
    lines.join("\n")
}

/// 单条诊断：`12:5  error: message (code) [source]`。
fn format_diagnostic(source_lines: &[&str], item: &Value) -> String {
    let start = item
        .get("range")
        .and_then(|r| r.get("start"))
        .cloned()
        .unwrap_or(Value::Null);
    let line = start.get("line").and_then(Value::as_u64).unwrap_or(0) + 1;
    let character = start
        .get("character")
        .and_then(Value::as_u64)
        .map(|c| {
            source_lines
                .get(line as usize - 1)
                .map_or(1, |line_text| char_column(line_text, c) + 1)
        })
        .unwrap_or(1);

    let severity = match item.get("severity").and_then(Value::as_u64) {
        Some(1) => "error",
        Some(2) => "warning",
        Some(3) => "info",
        Some(4) => "hint",
        _ => "diagnostic",
    };
    let message = item
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .replace('\n', " ");
    let mut suffix = String::new();
    if let Some(code) = item.get("code") {
        let code = match code {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if !code.is_empty() {
            suffix.push_str(&format!(" ({code})"));
        }
    }
    if let Some(source) = item.get("source").and_then(Value::as_str)
        && !source.is_empty()
    {
        suffix.push_str(&format!(" [{source}]"));
    }

    format!("  {line}:{character}  {severity}: {message}{suffix}")
}

/// 定义 / 引用响应 → 位置列表。
fn format_locations(path: &Path, line: u32, character: u32, noun: &str, result: &Value) -> String {
    let locations = collect_locations(result);
    if locations.is_empty() {
        return format!(
            "No {noun} found at {}:{}:{}.",
            path.display(),
            line + 1,
            character + 1
        );
    }

    let mut lines = vec![format!(
        "{} {noun}{}:",
        locations.len(),
        if locations.len() == 1 { "" } else { "s" }
    )];
    for (path, line, character) in locations {
        lines.push(format!("  {}:{line}:{character}", path.display()));
    }
    lines.join("\n")
}

/// 从 `Location` / `Location[]` / `LocationLink[]` 里抽出 `(路径, 1-based 行, 1-based 列)`。
fn collect_locations(value: &Value) -> Vec<(PathBuf, u64, u64)> {
    let mut out = Vec::new();
    let items: Vec<&Value> = match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items.iter().collect(),
        other => vec![other],
    };
    for item in items {
        let uri = item
            .get("uri")
            .or_else(|| item.get("targetUri"))
            .and_then(Value::as_str);
        let range = item
            .get("range")
            .or_else(|| item.get("targetSelectionRange"))
            .or_else(|| item.get("targetRange"));
        let (Some(uri), Some(range)) = (uri, range) else {
            continue;
        };
        let Some(path) = uri_to_path(uri) else {
            continue;
        };
        let start = range.get("start").cloned().unwrap_or(Value::Null);
        let line = start.get("line").and_then(Value::as_u64).unwrap_or(0) + 1;
        let character = start.get("character").and_then(Value::as_u64).unwrap_or(0) + 1;
        out.push((path, line, character));
    }
    out
}

/// 文件符号响应 → 缩进大纲。
fn format_document_symbols(path: &Path, result: &Value) -> String {
    let Some(items) = result.as_array() else {
        return format!("{}: no symbols found.", path.display());
    };
    if items.is_empty() {
        return format!("{}: no symbols found.", path.display());
    }

    let mut lines = vec![format!("{} — {} symbol(s):", path.display(), items.len())];
    let mut count = 0usize;
    for item in items {
        push_document_symbol(item, 0, &mut lines, &mut count);
    }
    lines.join("\n")
}

/// 递归追加一个文档符号及其子符号。
fn push_document_symbol(item: &Value, depth: usize, lines: &mut Vec<String>, count: &mut usize) {
    let Some(name) = item.get("name").and_then(Value::as_str) else {
        return;
    };
    let kind = symbol_kind(item.get("kind").and_then(Value::as_u64));
    let start = item
        .get("selectionRange")
        .or_else(|| item.get("range"))
        .or_else(|| {
            item.get("location")
                .and_then(|location| location.get("range"))
        })
        .and_then(|range| range.get("start"));
    let line = start
        .and_then(|s| s.get("line"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
        + 1;
    let character = start
        .and_then(|s| s.get("character"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
        + 1;

    *count += 1;
    lines.push(format!(
        "  {}{line}:{character}  {kind} {name}",
        "  ".repeat(depth)
    ));

    if let Some(children) = item.get("children").and_then(Value::as_array) {
        for child in children {
            push_document_symbol(child, depth + 1, lines, count);
        }
    }
}

/// 工作区符号响应 → 带文件位置的列表。
fn format_workspace_symbols(query: &str, result: &Value) -> String {
    let Some(items) = result.as_array() else {
        return format!("No symbols matching `{query}`.");
    };
    if items.is_empty() {
        return format!("No symbols matching `{query}`.");
    }

    let mut lines = vec![format!("{} symbol(s) matching `{query}`:", items.len())];
    for item in items {
        let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
        let kind = symbol_kind(item.get("kind").and_then(Value::as_u64));
        let location = item.get("location").cloned().unwrap_or(Value::Null);
        let path = location
            .get("uri")
            .and_then(Value::as_str)
            .and_then(uri_to_path)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "?".to_string());
        let line = location
            .get("range")
            .and_then(|r| r.get("start"))
            .and_then(|s| s.get("line"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            + 1;
        let character = location
            .get("range")
            .and_then(|r| r.get("start"))
            .and_then(|s| s.get("character"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            + 1;
        lines.push(format!("  {path}:{line}:{character}  {kind} {name}"));
    }
    lines.join("\n")
}

/// LSP `SymbolKind` 数字 → 可读名字。
fn symbol_kind(kind: Option<u64>) -> &'static str {
    match kind {
        Some(1) => "file",
        Some(2) => "module",
        Some(3) => "namespace",
        Some(4) => "package",
        Some(5) => "class",
        Some(6) => "method",
        Some(7) => "property",
        Some(8) => "field",
        Some(9) => "constructor",
        Some(10) => "enum",
        Some(11) => "interface",
        Some(12) => "function",
        Some(13) => "variable",
        Some(14) => "const",
        Some(15) => "string",
        Some(16) => "number",
        Some(17) => "boolean",
        Some(18) => "array",
        Some(19) => "object",
        Some(20) => "key",
        Some(21) => "null",
        Some(22) => "enum member",
        Some(23) => "struct",
        Some(24) => "event",
        Some(25) => "operator",
        Some(26) => "type parameter",
        _ => "symbol",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;

    #[test]
    fn utf16_columns_round_trip_for_ascii() {
        let line = "let value = compute();";
        assert_eq!(utf16_column(line, 4), 4);
        assert_eq!(char_column(line, 4), 4);
    }

    #[test]
    fn utf16_columns_account_for_non_bmp() {
        let line = "a😀b";
        assert_eq!(utf16_column(line, 2), 3);
        assert_eq!(char_column(line, 3), 2);
        assert_eq!(char_column(line, 4), 3);
    }

    #[test]
    fn cursor_position_rejects_out_of_range() {
        let text = "fn main() {}\n";
        let args = json!({ "line": 2, "character": 1 });
        assert!(cursor_position(&args, text).is_err());

        let args = json!({ "line": 1, "character": 99 });
        assert!(cursor_position(&args, text).is_err());

        let args = json!({ "line": 1, "character": 4 });
        assert_eq!(cursor_position(&args, text).unwrap(), (0, 3));
    }

    #[test]
    fn collect_locations_handles_all_three_shapes() {
        let single = json!({
            "uri": "file:///tmp/a.rs",
            "range": { "start": { "line": 3, "character": 4 }, "end": { "line": 3, "character": 5 } }
        });
        assert_eq!(collect_locations(&single).len(), 1);

        let links = json!([{
            "targetUri": "file:///tmp/b.rs",
            "targetRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
            "targetSelectionRange": { "start": { "line": 1, "character": 2 }, "end": { "line": 1, "character": 3 } }
        }]);
        let locations = collect_locations(&links);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].1, 2);
        assert_eq!(locations[0].2, 3);

        assert!(collect_locations(&Value::Null).is_empty());
    }

    #[test]
    fn formats_diagnostics_with_severity_and_code() {
        let _guard = AgentDirGuard::temp();
        let path = std::env::temp_dir().join("prux-lsp-diag.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let entry = DiagEntry {
            items: vec![json!({
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
                "severity": 1,
                "message": "mismatched types",
                "code": "E0308",
                "source": "rust-analyzer"
            })],
            version: Some(1),
            received: Instant::now(),
        };
        let text = format_diagnostics(&path, Some(entry));
        assert!(text.contains("1 diagnostic:"), "{text}");
        assert!(
            text.contains("1:1  error: mismatched types (E0308) [rust-analyzer]"),
            "{text}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn document_symbols_are_indented_by_depth() {
        let result = json!([{
            "name": "Foo",
            "kind": 23,
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 9, "character": 0 } },
            "selectionRange": { "start": { "line": 0, "character": 7 }, "end": { "line": 0, "character": 10 } },
            "children": [{
                "name": "new",
                "kind": 6,
                "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 4, "character": 0 } },
                "selectionRange": { "start": { "line": 2, "character": 7 }, "end": { "line": 2, "character": 10 } }
            }]
        }]);
        let text = format_document_symbols(Path::new("/tmp/lib.rs"), &result);
        assert!(text.contains("1:8  struct Foo"), "{text}");
        assert!(text.contains("    3:8  method new"), "{text}");
    }

    #[test]
    fn operation_parses_from_string() {
        assert_eq!(
            "diagnostics".parse::<Operation>().unwrap(),
            Operation::Diagnostics
        );
        assert!("bogus".parse::<Operation>().is_err());
    }
}
