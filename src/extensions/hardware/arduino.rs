//! Arduino 上传：`arduino_upload`（写临时 sketch → compile → upload）。
//!
//! 走 `arduino-cli`（不是自制串口协议）：模型给一份完整的 `.ino` 代码，这里把它落成
//! 临时 sketch 目录，先 `compile` 再 `upload -p <port>`。编译/上传都可能很慢，
//! 因此各有超时；子进程输出全部走管道，绝不继承终端。
//!
//! `fqbn`（板型标识）默认按配置的板卡名推导，也可由调用方显式给：
//! `arduino-uno` → `arduino:avr:uno`，`arduino-mega` → `arduino:avr:mega`。

use super::{config::load_config, tool_labels};
use crate::{
    core::{
        extensions::{ExtensionTool, ToolAnnotations, ToolExposure},
        tools::{ToolError, ToolResult},
    },
    utils::{process::HideConsole, truncate::truncate_chars_within},
};
use serde_json::{Value, json};
use std::time::Duration;
use tempfile::TempDir;
use tokio::process::Command;

/// 工具名。
pub(super) const TOOL_UPLOAD: &str = "arduino_upload";

/// sketch 目录名（`arduino-cli` 要求目录名与 `.ino` 同名）。
const SKETCH_NAME: &str = "prux_sketch";

/// 编译超时：首次编译要展开 core，慢是正常的。
const COMPILE_TIMEOUT: Duration = Duration::from_secs(180);

/// 上传超时。
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// 返回给模型的最大字符数（编译器诊断能刷几百行）。
const MAX_OUTPUT_CHARS: usize = 8_000;

/// `arduino_upload` 的工具定义。
pub(super) fn upload_tool() -> ExtensionTool {
    let mut tool = ExtensionTool::simple(
        TOOL_UPLOAD,
        "Compile and upload a complete Arduino sketch to the connected board via `arduino-cli`. \
         Write the full `.ino` (setup + loop); Uno pin 13 is the built-in LED.",
        json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "Full Arduino sketch (complete .ino file content)." },
                "port": { "type": "string", "description": "Serial port (e.g. `/dev/ttyACM0`, `COM3`). Omit to use the configured device." },
                "fqbn": { "type": "string", "description": "Board identifier for arduino-cli (e.g. `arduino:avr:uno`). Omit to derive it from the configured board." },
                "board": { "type": "string", "description": "Board name from the datasheet table, used to derive `fqbn` when it is not given." },
                "device": { "type": "string", "description": "Device alias used to resolve `port` when `port` is omitted." }
            },
            "required": ["code"]
        }),
        "Compile and upload an Arduino sketch",
    )
    .with_exposure(ToolExposure::Deferred)
    .with_annotations(ToolAnnotations {
        read_only: false,
        destructive: true,
        idempotent: false,
    });
    tool.label = Some(tool_labels());
    tool
}

/// 板卡名 → arduino-cli 的 fqbn。
fn fqbn_for_board(board: &str) -> Option<&'static str> {
    match board {
        "arduino-uno" | "arduino-uno-q" => Some("arduino:avr:uno"),
        "arduino-mega" => Some("arduino:avr:mega"),
        _ => None,
    }
}

/// 跑一次 `arduino-cli`，失败时把 stderr 一起带回来。
async fn run_cli(args: &[String], timeout: Duration) -> Result<String, String> {
    let mut command = Command::new("arduino-cli");
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .hide_console()
        .kill_on_drop(true);

    let child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "`arduino-cli` not found. Install it: https://arduino.github.io/arduino-cli/"
                .to_string()
        } else {
            format!("could not run arduino-cli: {e}")
        }
    })?;

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(format!("arduino-cli failed: {e}")),
        Err(_) => {
            return Err(format!(
                "arduino-cli did not finish within {}s",
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
        return Err(format!(
            "arduino-cli exited with {}: {detail}",
            output.status
        ));
    }
    Ok(truncate_chars_within(
        format!("{stdout}\n{stderr}").trim(),
        MAX_OUTPUT_CHARS,
        "…",
    ))
}

/// 把 sketch 写进临时目录（目录名必须与 `.ino` 同名，否则 arduino-cli 不认）。
fn write_sketch(code: &str) -> Result<TempDir, ToolError> {
    let dir = tempfile::Builder::new()
        .prefix("prux_arduino_")
        .tempdir()
        .map_err(|e| ToolError(format!("could not create a sketch directory: {e}")))?;
    let sketch_dir = dir.path().join(SKETCH_NAME);
    std::fs::create_dir_all(&sketch_dir)
        .map_err(|e| ToolError(format!("could not create the sketch directory: {e}")))?;
    std::fs::write(sketch_dir.join(format!("{SKETCH_NAME}.ino")), code)
        .map_err(|e| ToolError(format!("could not write the sketch: {e}")))?;
    Ok(dir)
}

/// 执行 `arduino_upload`。
pub(super) async fn run_upload(args: &Value) -> Result<ToolResult, ToolError> {
    let code = args
        .get("code")
        .and_then(Value::as_str)
        .filter(|code| !code.trim().is_empty())
        .ok_or_else(|| ToolError("missing or empty parameter: `code`".to_string()))?;

    let config = load_config();

    let board = args
        .get("board")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| config.default_board().map(str::to_string));
    let fqbn = match (
        args.get("fqbn").and_then(Value::as_str),
        board.as_deref().and_then(|board| {
            config
                .fqbn_for(board)
                .or_else(|| fqbn_for_board(board).map(str::to_string))
        }),
    ) {
        (Some(fqbn), _) => fqbn.to_string(),
        (None, Some(fqbn)) => fqbn.to_string(),
        (None, None) => {
            return Ok(ToolResult::error(
                "No `fqbn`. Pass one (e.g. `arduino:avr:uno`) or configure an Arduino board."
                    .to_string(),
            ));
        }
    };

    let port = match args.get("port").and_then(Value::as_str) {
        Some(port) => port.to_string(),
        None => config.serial_target(args).map_err(ToolError)?.1,
    };

    let sketch = write_sketch(code)?;
    let sketch_dir = sketch.path().join(SKETCH_NAME);
    let sketch_arg = sketch_dir.to_string_lossy().to_string();

    run_cli(
        &[
            "compile".to_string(),
            "--fqbn".to_string(),
            fqbn.clone(),
            sketch_arg.clone(),
        ],
        COMPILE_TIMEOUT,
    )
    .await
    .map_err(ToolError)?;

    let upload = run_cli(
        &[
            "upload".to_string(),
            "-p".to_string(),
            port.clone(),
            "--fqbn".to_string(),
            fqbn.clone(),
            sketch_arg,
        ],
        UPLOAD_TIMEOUT,
    )
    .await
    .map_err(ToolError)?;

    Ok(ToolResult::text(format!(
        "Compiled ({fqbn}) and uploaded to {port}.\n\n{upload}"
    )))
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
    fn known_boards_map_to_fqbns() {
        assert_eq!(fqbn_for_board("arduino-uno"), Some("arduino:avr:uno"));
        assert_eq!(fqbn_for_board("arduino-mega"), Some("arduino:avr:mega"));
        assert_eq!(fqbn_for_board("esp32"), None);
    }

    #[test]
    fn sketch_directory_matches_the_ino_name() {
        let dir = write_sketch("void setup() {}").unwrap();
        let sketch_dir = dir.path().join(SKETCH_NAME);
        assert!(sketch_dir.is_dir());
        let ino = sketch_dir.join(format!("{SKETCH_NAME}.ino"));
        assert_eq!(std::fs::read_to_string(ino).unwrap(), "void setup() {}");
    }

    #[test]
    fn empty_code_is_rejected() {
        let (_g, _ad) = guard();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(
            runtime
                .block_on(run_upload(&json!({ "code": " " })))
                .is_err()
        );
        assert!(runtime.block_on(run_upload(&json!({}))).is_err());
    }

    #[test]
    fn missing_fqbn_is_explained() {
        let (_g, _ad) = guard();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = runtime
            .block_on(run_upload(&json!({ "code": "void setup(){}" })))
            .unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("fqbn"), "{}", out.text);
    }
}
