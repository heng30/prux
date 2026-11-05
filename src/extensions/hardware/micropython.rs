//! MicroPython / CircuitPython 设备上的代码读写与执行（经 `mpremote`）。
//!
//! 对端是一块跑 MicroPython 的板子（Pico、ESP32 等）。这些操作走 `mpremote` CLI，
//! 而不是 [`super::serial`] 的 JSON 协议：`mpremote` 自己知道怎么进入 REPL、
//! 怎么跑完脚本再退出，重写一遍没有意义。
//!
//! | 工具 | 等价命令 |
//! |---|---|
//! | `device_read_code` | `mpremote connect <port> cat :main.py` |
//! | `device_write_code` | `mpremote connect <port> fs cp <tmp> :main.py` + `reset` |
//! | `device_exec` | `mpremote connect <port> run <tmp>` |
//!
//! 约束（AGENTS「外部命令的输出必须有去处」）：子进程 stdin 接 null，stdout/stderr
//! 全部走管道，绝不继承终端；超时到点就杀进程，不让一次卡住的串口把回合拖死。

use super::{load_config, tool_labels};
use crate::{
    core::{
        extensions::{ExtensionTool, ToolAnnotations, ToolExposure},
        tools::{ToolError, ToolResult},
    },
    utils::{process::HideConsole, truncate::truncate_chars_within},
};
use serde_json::{Value, json};
use std::time::Duration;

/// 工具名：读设备上的程序。
pub(super) const TOOL_READ_CODE: &str = "device_read_code";

/// 工具名：写设备上的程序。
pub(super) const TOOL_WRITE_CODE: &str = "device_write_code";

/// 工具名：在设备上执行一段代码。
pub(super) const TOOL_EXEC: &str = "device_exec";

/// 读操作的超时。
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// 写 / 执行操作的超时（要连上去、传输、重启）。
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

/// 返回给模型的最大字符数（串口回显 + traceback 很容易几千字）。
const MAX_OUTPUT_CHARS: usize = 8_000;

/// 构造一个 mpremote 工具（延迟；只有 exec 不算只读）。
fn mp_tool(
    name: &str,
    description: &str,
    parameters: Value,
    snippet: &str,
    read_only: bool,
) -> ExtensionTool {
    let mut tool = ExtensionTool::simple(name, description, parameters, snippet)
        .with_exposure(ToolExposure::Deferred)
        .with_annotations(ToolAnnotations {
            read_only,
            destructive: !read_only,
            idempotent: false,
        });
    tool.label = Some(tool_labels());
    tool
}

/// `device_read_code` 的工具定义。
pub(super) fn read_tool() -> ExtensionTool {
    mp_tool(
        TOOL_READ_CODE,
        "Read the current `main.py` from a MicroPython / CircuitPython board over `mpremote`. \
         Read before writing so you know what is already running.",
        json!({
            "type": "object",
            "properties": {
                "device": { "type": "string", "description": "Device alias from config (e.g. `pico0`). Omit when only one is configured." }
            }
        }),
        "Read main.py from a MicroPython board",
        true,
    )
}

/// `device_write_code` 的工具定义。
fn write_tool_def() -> ExtensionTool {
    mp_tool(
        TOOL_WRITE_CODE,
        "Replace `main.py` on a MicroPython board and reset it so the new program starts running. \
         Always read the current code first.",
        json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "Complete program to write as main.py." },
                "device": { "type": "string", "description": "Device alias from config (e.g. `pico0`)." }
            },
            "required": ["code"]
        }),
        "Write main.py to a MicroPython board and reset it",
        false,
    )
}

/// `device_exec` 的工具定义。
fn exec_tool_def() -> ExtensionTool {
    mp_tool(
        TOOL_EXEC,
        "Run a code snippet on a MicroPython board without changing `main.py` (one-off sensor reads, \
         quick experiments). Output is captured and returned.",
        json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "Code to execute on the board." },
                "device": { "type": "string", "description": "Device alias from config (e.g. `pico0`)." }
            },
            "required": ["code"]
        }),
        "Run a snippet on a MicroPython board",
        false,
    )
}

/// 三个工具一起（扩展注册时用）。
pub(super) fn tools() -> Vec<ExtensionTool> {
    vec![read_tool(), write_tool_def(), exec_tool_def()]
}

/// 跑一次 `mpremote`，返回合并后的输出（失败时带上 stderr）。
async fn run_mpremote(args: &[&str], timeout: Duration) -> std::result::Result<String, String> {
    use tokio::process::Command;

    let mut command = Command::new("mpremote");
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .hide_console()
        .kill_on_drop(true);

    let child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "`mpremote` not found. Install it: pip install mpremote".to_string()
        } else {
            format!("could not run mpremote: {e}")
        }
    })?;

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(format!("mpremote failed: {e}")),
        Err(_) => {
            return Err(format!(
                "mpremote did not finish within {}s (the board may be stuck)",
                timeout.as_secs()
            ));
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() {
        let detail = if stderr.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        return Err(format!("mpremote exited with {}: {detail}", output.status));
    }
    Ok(truncate_chars_within(
        if stdout.trim().is_empty() {
            stderr.trim()
        } else {
            stdout.trim()
        },
        MAX_OUTPUT_CHARS,
        "…",
    ))
}

/// 写一份临时脚本（保持到命令跑完）。
fn temp_script(code: &str) -> std::result::Result<tempfile::NamedTempFile, ToolError> {
    let file = tempfile::Builder::new()
        .prefix("prux_device_")
        .suffix(".py")
        .tempfile()
        .map_err(|e| ToolError(format!("could not create a temp script: {e}")))?;
    std::fs::write(file.path(), code)
        .map_err(|e| ToolError(format!("could not write the temp script: {e}")))?;
    Ok(file)
}

/// 执行 `device_read_code`。
pub(super) async fn run_read(args: &Value) -> std::result::Result<ToolResult, ToolError> {
    let config = load_config();
    let (alias, port) = config.serial_target(args).map_err(ToolError)?;

    match run_mpremote(&["connect", &port, "cat", ":main.py"], READ_TIMEOUT).await {
        Ok(output) if output.trim().is_empty() => Ok(ToolResult::text(format!(
            "main.py on {alias} ({port}) is empty or missing."
        ))),
        Ok(output) => Ok(ToolResult::text(format!(
            "Current main.py on {alias} ({port}):\n\n```python\n{output}\n```"
        ))),
        Err(err) => Ok(ToolResult::error(err)),
    }
}

/// 执行 `device_write_code`。
pub(super) async fn run_write(args: &Value) -> std::result::Result<ToolResult, ToolError> {
    let code = args
        .get("code")
        .and_then(Value::as_str)
        .filter(|code| !code.trim().is_empty())
        .ok_or_else(|| ToolError("missing or empty parameter: `code`".to_string()))?;

    let config = load_config();
    let (alias, port) = config.serial_target(args).map_err(ToolError)?;

    let script = temp_script(code)?;
    let remote = ":main.py";
    let local = script
        .path()
        .to_str()
        .ok_or_else(|| ToolError("temp script path is not valid UTF-8".to_string()))?;

    run_mpremote(
        &["connect", &port, "fs", "cp", local, remote],
        WRITE_TIMEOUT,
    )
    .await
    .map_err(ToolError)?;
    run_mpremote(&["connect", &port, "reset"], WRITE_TIMEOUT)
        .await
        .map_err(ToolError)?;

    Ok(ToolResult::text(format!(
        "Wrote {} bytes to main.py on {alias} ({port}) and reset the board.",
        code.len()
    )))
}

/// 执行 `device_exec`。
pub(super) async fn run_exec(args: &Value) -> std::result::Result<ToolResult, ToolError> {
    let code = args
        .get("code")
        .and_then(Value::as_str)
        .filter(|code| !code.trim().is_empty())
        .ok_or_else(|| ToolError("missing or empty parameter: `code`".to_string()))?;

    let config = load_config();
    let (alias, port) = config.serial_target(args).map_err(ToolError)?;

    let script = temp_script(code)?;
    let local = script
        .path()
        .to_str()
        .ok_or_else(|| ToolError("temp script path is not valid UTF-8".to_string()))?;

    match run_mpremote(&["connect", &port, "run", local], WRITE_TIMEOUT).await {
        Ok(output) => Ok(ToolResult::text(format!(
            "Output from {alias} ({port}):\n{}",
            if output.trim().is_empty() {
                "(no output)"
            } else {
                output.as_str()
            }
        ))),
        Err(err) => Ok(ToolResult::error(err)),
    }
}

/// 该路径是否是 `.py`（测试用它验证临时脚本的扩展名）。
#[cfg(test)]
fn is_python(path: &std::path::Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("py")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{AUTH_TEST_LOCK, AgentDirGuard};
    use std::sync::MutexGuard;

    /// 串行锁 + 临时 agent_dir。
    fn guard() -> (MutexGuard<'static, ()>, AgentDirGuard) {
        let lock = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        (lock, AgentDirGuard::temp())
    }

    #[test]
    fn empty_code_is_rejected_before_calling_mpremote() {
        let (_g, _ad) = guard();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for outcome in [
            runtime.block_on(run_write(&json!({ "code": "  " }))),
            runtime.block_on(run_exec(&json!({ "code": "" }))),
            runtime.block_on(run_exec(&json!({}))),
        ] {
            assert!(outcome.is_err(), "{outcome:?}");
        }
    }

    #[test]
    fn temp_scripts_are_python_files() {
        let script = temp_script("print('hi')").unwrap();
        assert!(is_python(script.path()));
        assert_eq!(
            std::fs::read_to_string(script.path()).unwrap(),
            "print('hi')"
        );
    }

    #[test]
    fn missing_device_is_reported_as_a_tool_error() {
        let (_g, _ad) = guard();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime.block_on(run_read(&json!({}))).unwrap_err();
        assert!(err.0.contains("devices"), "{}", err.0);
    }

    #[test]
    fn all_three_tools_are_named_and_deferred() {
        let names: Vec<String> = tools().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec!["device_read_code", "device_write_code", "device_exec"]
        );
        for tool in tools() {
            assert_eq!(tool.exposure, ToolExposure::Deferred, "{}", tool.name);
        }
    }
}
