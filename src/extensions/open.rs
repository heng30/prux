//! `open` 扩展：把文件或 URL 交给系统默认程序打开。
//!
//! 提供 `open` 工具：模型说「把这份报告打开给我看」时，由系统 opener
//! （Linux `xdg-open` / macOS `open` / Windows `cmd /C start`）拉起**用户看得见**的
//! 窗口，而不是自己把内容读进上下文。
//!
//! 与 `read` / `fetch_content` 的分工：那两个把内容取回模型，本工具只把用户引到内容面前。
//! 因此它有副作用（会拉起外部程序），工具结果回显打开了什么、用的是哪个程序。
//!
//! 默认**不启用**（`Dev` / `Creator`）：在用户机器上拉起外部程序属主动行为，按需开启。

use crate::{
    core::{
        extensions::{
            Extension, ExtensionMode, ExtensionTool, ForkProjectInfo, ToolAnnotations, ToolExecCtx,
            ToolExposure,
        },
        tools::{ToolError, ToolResult},
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_OPEN},
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};

/// 扩展名（也是 `/extension` 面板中的标识）。
const EXT: &str = "open";

/// 工具名（模型可见）。
const TOOL_OPEN: &str = "open";

/// 等系统 opener 退出的上限：超时即强杀，避免卡死在不返回的程序上。
const OPENER_TIMEOUT: Duration = Duration::from_secs(10);

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static OPEN_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_OPEN,
    make: || -> Arc<dyn Extension> { Arc::new(Open) },
};

/// `open` 扩展主体。
pub struct Open;

impl Extension for Open {
    /// 返回固定扩展名 `open`。
    fn name(&self) -> &str {
        EXT
    }

    /// 返回 `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Open files or URLs with the system default application (open)."
    }

    /// 声明本扩展移植自 jcode 0.94.0，供 `/extension` 面板展示来源链接。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "jcode".to_string(),
            plugin_version: "0.94.0".to_string(),
            url: "https://github.com/1jehuang/jcode".to_string(),
        })
    }

    /// 默认关闭：会在用户机器上拉起外部程序，由用户在 `/extension` 面板开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 只在 `Dev` / `Creator` 扩展模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 提供 `open` 工具。
    fn tools(&self) -> Vec<ExtensionTool> {
        vec![open_tool()]
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
                TOOL_OPEN => run_open(&args, &ctx).await,
                other => Err(ToolError(format!("extension {EXT}: unknown tool: {other}"))),
            }
        })
    }
}

/// 构造 `open` 工具定义（描述、参数 JSON Schema 与 snippet）。
fn open_tool() -> ExtensionTool {
    ExtensionTool {
        default_active: true,
        exposure: ToolExposure::Direct,
        namespace: None,
        annotations: ToolAnnotations {
            read_only: false,
            destructive: false,
            idempotent: true,
        },
        output_schema: None,
        name: TOOL_OPEN.to_string(),
        description: "Open a file or URL with the system default application on the user's machine (viewer, editor, or browser). Use it to show something to the user; it does not return the file's content.".to_string(),
        label: Some("Open".to_string()),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path (relative paths resolve against the working directory) or an http/https/file URL."
                }
            },
            "required": ["path"]
        }),
        snippet: "Hand a file or URL to the user's default application (opens a visible window); does not read content back.".to_string(),
        prompt_guidelines: vec![
            "`open` launches an external program on the user's machine; use `read` / `fetch_content` when the content itself is what you need.".to_string(),
        ],
        constrained_sampling: false,
        render_shell: None,
        execution_mode: None,
        prepare_arguments: None,
        grammar_sampling: None,
    }
}

/// `open` 工具的执行体：解析参数 → 拼装系统 opener → 拉起进程并等它结束。
///
/// 参数缺失 / 文件不存在 / opener 启动失败都返回 [`ToolError`]（文本直接回给模型）；
/// opener 自身的标准输出丢弃，非空的标准错误拼进结果文本，好让模型看到失败原因。
async fn run_open(args: &Value, ctx: &ToolExecCtx) -> Result<ToolResult, ToolError> {
    let raw = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError("missing required parameter: path".to_string()))?;

    let target = resolve_target(raw, &ctx.cwd).map_err(ToolError)?;
    let (value, kind) = match &target {
        Target::Url(url) => (url.clone(), "URL"),
        Target::Path(path) => (path.clone(), "file"),
    };

    let (program, argv) = opener_command(&value);
    let outcome = spawn_opener(&program, &argv)
        .await
        .map_err(|e| ToolError(format!("failed to launch `{program}`: {e}")))?;

    let mut message = if outcome.success {
        format!("Opened {kind} {value} with `{program}`.")
    } else if let Some(secs) = outcome.timed_out_secs {
        format!(
            "`{program}` did not exit within {secs}s while opening {kind} {value}; it was killed."
        )
    } else {
        format!("`{program}` exited with a non-zero status while opening {kind} {value}.")
    };

    if !outcome.stderr.is_empty() {
        message.push_str(&format!(
            "\n`{program}` wrote to stderr:\n{}",
            outcome.stderr
        ));
    }

    if outcome.success {
        Ok(ToolResult::text(message))
    } else {
        Err(ToolError(message))
    }
}

/// 系统 opener 的一次执行结果：标准输出丢弃，只保留标准错误与退出状态。
#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenerOutcome {
    /// 进程是否以成功状态退出；非零退出码、被信号杀死、超时强杀都是 `false`。
    success: bool,
    /// 进程写到标准错误的全部内容（首尾空白已裁剪）；空串表示它什么都没写。
    stderr: String,
    /// 超时强杀时为 `Some(等待上限的秒数)`，正常退出为 `None`。
    timed_out_secs: Option<u64>,
}

/// 工具参数解析结果：URL 原样透传，本地路径已按 cwd 展开并校验存在。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// `http` / `https` / `file` URL，不做存在性检查。
    Url(String),
    /// 本地文件路径，相对路径已按 cwd 展开。
    Path(String),
}

/// 参数是否是交给系统 opener 直开的 URL（只认这三种 scheme）。
fn is_url(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("file://")
}

/// 把工具参数解析成 [`Target`]。
///
/// 空参数与不存在的本地路径都返回 `Err`（文本可直接展示给模型）；
/// URL 不校验可达性——连不连得上由用户那边的浏览器决定。
fn resolve_target(raw: &str, cwd: &str) -> Result<Target, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("`path` must not be empty.".to_string());
    }

    if is_url(raw) {
        return Ok(Target::Url(raw.to_string()));
    }

    let path = Path::new(raw);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(cwd).join(path)
    };

    if !resolved.exists() {
        return Err(format!("path not found: {}", resolved.display()));
    }

    Ok(Target::Path(resolved.to_string_lossy().to_string()))
}

/// 按平台给出系统 opener 的命令行：程序名 + 参数列表。
///
/// Windows 的 `start` 是 `cmd` 内建命令，其第一个引号参数被当作窗口标题，
/// 因此必须补一个空串占位，否则目标会被当成标题而打不开。
fn opener_command(target: &str) -> (String, Vec<String>) {
    if cfg!(target_os = "macos") {
        ("open".to_string(), vec![target.to_string()])
    } else if cfg!(target_os = "windows") {
        (
            "cmd".to_string(),
            vec![
                "/C".to_string(),
                "start".to_string(),
                String::new(),
                target.to_string(),
            ],
        )
    } else {
        ("xdg-open".to_string(), vec![target.to_string()])
    }
}

/// 拉起系统 opener 并等它结束：标准输出直接丢弃，标准错误捕获后交给调用方。
///
/// 必须等进程退出才读得到 stderr，因此这里不再「启动即返回」；超过 [`OPENER_TIMEOUT`]
/// 未退出就强杀（`kill_on_drop` 保证超时后不留孤儿进程），并把标准输入也关掉，
/// 免得 opener 等待输入而挂住。只有进程起不来（如 `xdg-open` 不在 `$PATH`）才返回 `Err`。
async fn spawn_opener(program: &str, args: &[String]) -> std::io::Result<OpenerOutcome> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn()?;

    let output = match tokio::time::timeout(OPENER_TIMEOUT, child.wait_with_output()).await {
        Ok(result) => result?,
        Err(_) => {
            return Ok(OpenerOutcome {
                success: false,
                stderr: String::new(),
                timed_out_secs: Some(OPENER_TIMEOUT.as_secs()),
            });
        }
    };

    Ok(OpenerOutcome {
        success: output.status.success(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        timed_out_secs: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::ToolExecCtx;
    use serde_json::json;

    /// 只填 cwd 的测试上下文：`open` 不碰子代理与嵌套工具，其余字段给不可用占位。
    fn ctx(cwd: &str) -> ToolExecCtx {
        ToolExecCtx {
            cwd: cwd.to_string(),
            make_sub_agent: Arc::new(|_spec| Err("subagents unavailable in tests".to_string())),
            parent_abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_id: None,
            depth: 0,
            parent_model: None,
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            script_tools: Default::default(),
            session_branch_entries: Default::default(),
            script_call: false,
        }
    }

    #[test]
    fn recognizes_only_supported_url_schemes() {
        assert!(is_url("http://example.com"));
        assert!(is_url("HTTPS://example.com"));
        assert!(is_url("file:///tmp/a.txt"));
        assert!(!is_url("ftp://example.com"));
        assert!(!is_url("./local.txt"));
        assert!(!is_url("example.com"));
    }

    #[test]
    fn resolve_passes_urls_through_without_touching_disk() {
        assert_eq!(
            resolve_target("https://example.com/a?b=1", "/nonexistent"),
            Ok(Target::Url("https://example.com/a?b=1".to_string()))
        );
    }

    #[test]
    fn resolve_expands_relative_paths_against_cwd() {
        let dir = std::env::temp_dir().join(format!("prux-open-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("note.txt");
        std::fs::write(&file, "hi").unwrap();

        assert_eq!(
            resolve_target("note.txt", &dir.to_string_lossy()),
            Ok(Target::Path(file.to_string_lossy().to_string()))
        );
        assert_eq!(
            resolve_target(&file.to_string_lossy(), "/nonexistent"),
            Ok(Target::Path(file.to_string_lossy().to_string())),
            "绝对路径不看 cwd"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_rejects_empty_and_missing_paths() {
        assert!(resolve_target("   ", "/tmp").is_err());
        let err = resolve_target("definitely-missing.txt", "/tmp").unwrap_err();
        assert!(err.contains("path not found"), "{err}");
    }

    #[test]
    fn opener_command_matches_platform() {
        let (program, argv) = opener_command("/tmp/a.txt");
        if cfg!(target_os = "macos") {
            assert_eq!(program, "open");
            assert_eq!(argv, vec!["/tmp/a.txt"]);
        } else if cfg!(target_os = "windows") {
            assert_eq!(program, "cmd");
            assert_eq!(argv, vec!["/C", "start", "", "/tmp/a.txt"]);
        } else {
            assert_eq!(program, "xdg-open");
            assert_eq!(argv, vec!["/tmp/a.txt"]);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_opener_captures_stderr_and_drops_stdout() {
        let outcome = spawn_opener(
            "sh",
            &[
                "-c".to_string(),
                "printf out; printf 'boom\\n' >&2".to_string(),
            ],
        )
        .await
        .unwrap();
        assert!(outcome.success);
        assert_eq!(outcome.stderr, "boom");
        assert_eq!(outcome.timed_out_secs, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_opener_reports_non_zero_exit_with_stderr() {
        let outcome = spawn_opener(
            "sh",
            &["-c".to_string(), "printf 'nope\\n' >&2; exit 3".to_string()],
        )
        .await
        .unwrap();
        assert!(!outcome.success);
        assert_eq!(outcome.stderr, "nope");
    }

    #[tokio::test]
    async fn spawn_opener_rejects_missing_program() {
        assert!(spawn_opener("prux-no-such-opener", &[]).await.is_err());
    }

    #[test]
    fn fork_project_reports_jcode_source() {
        let info = Open.fork_project().expect("open 应声明 fork 来源");
        assert_eq!(info.plugin_name, "jcode");
        assert_eq!(info.plugin_version, "0.94.0");
        assert_eq!(info.url, "https://github.com/1jehuang/jcode");
    }

    #[test]
    fn tool_declares_required_path_parameter() {
        let tool = open_tool();
        assert_eq!(tool.name, TOOL_OPEN);
        assert_eq!(tool.parameters["required"], json!(["path"]));
        assert_eq!(tool.parameters["properties"]["path"]["type"], "string");
        assert!(tool.annotations.idempotent);
        assert!(!tool.annotations.read_only);
    }

    #[tokio::test]
    async fn missing_parameter_is_an_error() {
        let err = run_open(&json!({}), &ctx("/tmp")).await.unwrap_err();
        assert!(err.0.contains("missing required parameter"), "{}", err.0);
    }

    #[tokio::test]
    async fn missing_file_is_an_error() {
        let err = run_open(&json!({ "path": "no-such-file.txt" }), &ctx("/tmp"))
            .await
            .unwrap_err();
        assert!(err.0.contains("path not found"), "{}", err.0);
    }

    #[tokio::test]
    async fn unknown_tool_name_is_rejected() {
        let err = Open
            .execute_tool_async("nope".to_string(), json!({}), ctx("/tmp"))
            .await
            .unwrap_err();
        assert!(err.0.contains("unknown tool: nope"), "{}", err.0);
    }
}
