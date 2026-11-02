//! 树莓派原生 GPIO 与本机信息（`rppal`，仅 Linux）。
//!
//! 与串口 GPIO 的区别：这里**不需要任何固件**——prux 直接操作本机的 BCM 引脚，
//! 因此只在跑在树莓派上的 prux 里有意义（其它 Linux 机器上会报「打不开 GPIO」）。
//!
//! 引脚号一律是 **BCM 编号**（与 `raspi-gpio` / 树莓派文档一致），不是物理引脚号。
//!
//! 非 Linux 平台这些工具仍然存在，但只会返回一句平台不支持的错误——工具面保持一致，
//! 免得模型在 macOS / Windows 上看到时有时无的 `gpio_rpi_*`。

use super::tool_labels;
use crate::core::{
    extensions::{ExtensionTool, ToolAnnotations, ToolExposure},
    tools::{ToolError, ToolResult},
};
use serde_json::{Value, json};

/// 工具名：写引脚。
pub(super) const TOOL_RPI_WRITE: &str = "gpio_rpi_write";

/// 工具名：读引脚。
pub(super) const TOOL_RPI_READ: &str = "gpio_rpi_read";

/// 工具名：闪烁。
pub(super) const TOOL_RPI_BLINK: &str = "gpio_rpi_blink";

/// 工具名：本机信息。
pub(super) const TOOL_RPI_INFO: &str = "rpi_system_info";

/// 闪烁间隔的默认值（毫秒）。
const DEFAULT_BLINK_DELAY_MS: u64 = 200;

/// 闪烁次数的上限：再长就该让用户自己写脚本，而不是占着一个回合。
const MAX_BLINK_TIMES: u64 = 50;

/// 构造一个 RPi 工具的公共外壳（延迟工具）。
fn rpi_tool(
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
            idempotent: read_only,
        });
    tool.label = Some(tool_labels());
    tool
}

/// `gpio_rpi_write` 的工具定义。
pub(super) fn write_tool() -> ExtensionTool {
    rpi_tool(
        TOOL_RPI_WRITE,
        "Drive a Raspberry Pi GPIO pin HIGH or LOW directly (BCM numbering, no firmware needed).",
        json!({
            "type": "object",
            "properties": {
                "pin": { "type": "integer", "description": "BCM GPIO number (not the physical pin)." },
                "value": { "type": "integer", "enum": [0, 1], "description": "1 = HIGH, 0 = LOW." }
            },
            "required": ["pin", "value"]
        }),
        "Drive a Raspberry Pi GPIO pin (BCM numbering)",
        false,
    )
}

/// `gpio_rpi_read` 的工具定义。
pub(super) fn read_tool() -> ExtensionTool {
    rpi_tool(
        TOOL_RPI_READ,
        "Read a Raspberry Pi GPIO pin level as an input (BCM numbering).",
        json!({
            "type": "object",
            "properties": {
                "pin": { "type": "integer", "description": "BCM GPIO number (not the physical pin)." }
            },
            "required": ["pin"]
        }),
        "Read a Raspberry Pi GPIO input (BCM numbering)",
        true,
    )
}

/// `gpio_rpi_blink` 的工具定义。
pub(super) fn blink_tool() -> ExtensionTool {
    rpi_tool(
        TOOL_RPI_BLINK,
        &format!(
            "Blink a Raspberry Pi GPIO pin (BCM numbering): on/off cycles with a delay, capped at {MAX_BLINK_TIMES} cycles."
        ),
        json!({
            "type": "object",
            "properties": {
                "pin": { "type": "integer", "description": "BCM GPIO number." },
                "times": { "type": "integer", "description": format!("Number of on/off cycles (default 3, max {MAX_BLINK_TIMES}).") },
                "delay_ms": { "type": "integer", "description": format!("Delay between transitions in milliseconds (default {DEFAULT_BLINK_DELAY_MS}).") }
            },
            "required": ["pin"]
        }),
        "Blink a Raspberry Pi GPIO pin",
        false,
    )
}

/// `rpi_system_info` 的工具定义。
pub(super) fn info_tool() -> ExtensionTool {
    rpi_tool(
        TOOL_RPI_INFO,
        "Report this machine's model, SoC and kernel (reads `/proc/device-tree/model` and `/proc/cpuinfo`; useful to tell whether this host is a Raspberry Pi).",
        json!({ "type": "object", "properties": {} }),
        "Report the host model / SoC / kernel",
        true,
    )
}

/// 必填引脚号（限制在 BCM 可寻址范围，避免模型给个 400 去开不存在的引脚）。
fn required_pin(args: &Value) -> std::result::Result<u8, ToolError> {
    let pin = args
        .get("pin")
        .and_then(Value::as_u64)
        .ok_or_else(|| ToolError("missing required parameter: `pin`".to_string()))?;
    u8::try_from(pin).map_err(|_| ToolError(format!("`pin` must be 0..=255, got {pin}")))
}

/// 执行 `gpio_rpi_write`。
pub(super) async fn run_write(args: &Value) -> std::result::Result<ToolResult, ToolError> {
    let pin = required_pin(args)?;
    let value = args
        .get("value")
        .and_then(Value::as_u64)
        .ok_or_else(|| ToolError("missing required parameter: `value`".to_string()))?;
    if value > 1 {
        return Err(ToolError("`value` must be 0 or 1.".to_string()));
    }
    drive_pin(pin, value == 1).map(ToolResult::text)
}

/// 执行 `gpio_rpi_read`。
pub(super) async fn run_read(args: &Value) -> std::result::Result<ToolResult, ToolError> {
    let pin = required_pin(args)?;
    read_pin(pin).map(ToolResult::text)
}

/// 执行 `gpio_rpi_blink`。
pub(super) async fn run_blink(args: &Value) -> std::result::Result<ToolResult, ToolError> {
    let pin = required_pin(args)?;
    let times = args
        .get("times")
        .and_then(Value::as_u64)
        .unwrap_or(3)
        .clamp(1, MAX_BLINK_TIMES);
    let delay = args
        .get("delay_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_BLINK_DELAY_MS)
        .clamp(1, 5_000);

    blink_pin(pin, times, delay).map(ToolResult::text)
}

/// 执行 `rpi_system_info`。
pub(super) async fn run_info() -> std::result::Result<ToolResult, ToolError> {
    Ok(ToolResult::text(system_info()))
}

#[cfg(target_os = "linux")]
fn drive_pin(pin: u8, high: bool) -> std::result::Result<String, ToolError> {
    use rppal::gpio::Gpio;

    let gpio = Gpio::new().map_err(|e| ToolError(format!("could not open /dev/gpiochip: {e}")))?;
    let mut output = gpio
        .get(pin)
        .map_err(|e| ToolError(format!("GPIO {pin} is not available: {e}")))?
        .into_output();
    if high {
        output.set_high();
    } else {
        output.set_low();
    }
    Ok(format!(
        "GPIO {pin} set {}",
        if high { "HIGH" } else { "LOW" }
    ))
}

#[cfg(not(target_os = "linux"))]
fn drive_pin(pin: u8, _high: bool) -> std::result::Result<String, ToolError> {
    Err(ToolError(format!(
        "native GPIO is only supported on Linux (Raspberry Pi); cannot drive BCM pin {pin} on this platform"
    )))
}

#[cfg(target_os = "linux")]
fn read_pin(pin: u8) -> std::result::Result<String, ToolError> {
    use rppal::gpio::Gpio;

    let gpio = Gpio::new().map_err(|e| ToolError(format!("could not open /dev/gpiochip: {e}")))?;
    let input = gpio
        .get(pin)
        .map_err(|e| ToolError(format!("GPIO {pin} is not available: {e}")))?
        .into_input();
    let level = if input.is_high() { "HIGH" } else { "LOW" };
    Ok(format!("GPIO {pin} is {level}"))
}

#[cfg(not(target_os = "linux"))]
fn read_pin(pin: u8) -> std::result::Result<String, ToolError> {
    Err(ToolError(format!(
        "native GPIO is only supported on Linux (Raspberry Pi); cannot read BCM pin {pin} on this platform"
    )))
}

#[cfg(target_os = "linux")]
fn blink_pin(pin: u8, times: u64, delay_ms: u64) -> std::result::Result<String, ToolError> {
    use rppal::gpio::Gpio;
    use std::time::Duration;

    let gpio = Gpio::new().map_err(|e| ToolError(format!("could not open /dev/gpiochip: {e}")))?;
    let mut output = gpio
        .get(pin)
        .map_err(|e| ToolError(format!("GPIO {pin} is not available: {e}")))?
        .into_output();

    let delay = Duration::from_millis(delay_ms);
    for _ in 0..times {
        output.set_high();
        std::thread::sleep(delay);
        output.set_low();
        std::thread::sleep(delay);
    }
    Ok(format!(
        "GPIO {pin} blinked {times} time(s) at {delay_ms}ms"
    ))
}

#[cfg(not(target_os = "linux"))]
fn blink_pin(pin: u8, _times: u64, _delay_ms: u64) -> std::result::Result<String, ToolError> {
    Err(ToolError(format!(
        "native GPIO is only supported on Linux (Raspberry Pi); cannot blink BCM pin {pin} on this platform"
    )))
}

/// 本机型号 / SoC / 内核：优先 `/proc/device-tree/model`，退到 `/proc/cpuinfo`。
fn system_info() -> String {
    let model = std::fs::read_to_string("/proc/device-tree/model")
        .ok()
        .map(|s| s.trim_end_matches('\0').trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/proc/cpuinfo")
                .ok()
                .and_then(|text| {
                    text.lines()
                        .find(|line| line.to_lowercase().starts_with("model name"))
                        .and_then(|line| line.split_once(':'))
                        .map(|(_, v)| v.trim().to_string())
                })
        })
        .unwrap_or_else(|| "unknown".to_string());

    let soc = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.to_lowercase().starts_with("hardware"))
                .and_then(|line| line.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| std::env::consts::OS.to_string());

    format!(
        "Model: {model}\nSoC: {soc}\nKernel: {kernel}\nArch: {}",
        std::env::consts::ARCH
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_outside_the_bcm_range_are_rejected() {
        assert_eq!(required_pin(&json!({ "pin": 17 })).unwrap(), 17);
        assert!(required_pin(&json!({ "pin": 300 })).is_err());
        assert!(required_pin(&json!({})).is_err());
    }

    #[test]
    fn system_info_reports_three_labelled_lines() {
        let info = system_info();
        for label in ["Model: ", "SoC: ", "Kernel: ", "Arch: "] {
            assert!(info.contains(label), "{label} 缺失: {info}");
        }
    }

    #[test]
    fn blink_rejects_an_impossible_pin_without_touching_hardware() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime
            .block_on(run_blink(&json!({ "pin": 300 })))
            .unwrap_err();
        assert!(err.0.contains("0..=255"), "{}", err.0);
    }

    #[test]
    fn write_rejects_values_other_than_0_or_1() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime
            .block_on(run_write(&json!({ "pin": 17, "value": 7 })))
            .unwrap_err();
        assert!(err.0.contains("0 or 1"), "{}", err.0);
    }
}
