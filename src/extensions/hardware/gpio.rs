//! GPIO 工具：`gpio_read` / `gpio_write` / `hardware_capabilities`。
//!
//! 两条传输通道，按设备配置派发：
//!
//! - **串口**（[`super::serial`]）：行分隔 JSON，对端是跑在板子上的桥接固件
//!   `{"cmd":"gpio_write","params":{"pin":25,"value":1}}` →
//!   `{"ok":true,"data":{"pin":25,"value":1,"state":"HIGH"}}`；
//! - **Uno Q Bridge**（[`super::bridge`]）：本地 TCP 文本行，`gpio_write 13 1` → `OK`。
//!
//! 设备从配置的 `devices` 里选：别名 → 串口路径，或 `{"bridge": "127.0.0.1:9999"}`。
//! 只有一个设备时可以省略 `device`。
//!
//! `hardware_capabilities` 只走串口：它问的是固件自己报告哪些引脚可用。

use super::{DeviceTarget, load_config, tool_labels};
use crate::core::{
    extensions::{ExtensionTool, ToolAnnotations, ToolExposure},
    tools::{ToolError, ToolResult},
};
use serde_json::{Value, json};

/// 工具名：写 GPIO。
pub(super) const TOOL_GPIO_WRITE: &str = "gpio_write";

/// 工具名：读 GPIO。
pub(super) const TOOL_GPIO_READ: &str = "gpio_read";

/// 工具名：查固件报告的引脚能力。
pub(super) const TOOL_CAPABILITIES: &str = "hardware_capabilities";

/// 构造一个 GPIO 工具的公共外壳（延迟工具）。
fn gpio_tool(
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
            destructive: false,
            idempotent: read_only,
        });
    tool.label = Some("GPIO".to_string());
    tool
}

/// `gpio_write` 的工具定义。
pub(super) fn write_tool() -> ExtensionTool {
    gpio_tool(
        TOOL_GPIO_WRITE,
        "Set a GPIO pin HIGH (1) or LOW (0) on a connected board. Works over the serial bridge \
         firmware or the Arduino Uno Q Bridge, depending on how the device is configured.",
        json!({
            "type": "object",
            "properties": {
                "pin": { "type": "integer", "description": "GPIO pin number (board numbering)." },
                "value": { "type": "integer", "enum": [0, 1], "description": "1 = HIGH, 0 = LOW." },
                "device": { "type": "string", "description": "Device alias from config (e.g. `pico0`). Omit when only one is configured." }
            },
            "required": ["pin", "value"]
        }),
        "Set a GPIO pin HIGH/LOW",
        false,
    )
}

/// `gpio_read` 的工具定义。
pub(super) fn read_tool() -> ExtensionTool {
    gpio_tool(
        TOOL_GPIO_READ,
        "Read the current HIGH/LOW state of a GPIO pin on a connected board.",
        json!({
            "type": "object",
            "properties": {
                "pin": { "type": "integer", "description": "GPIO pin number to read (board numbering)." },
                "device": { "type": "string", "description": "Device alias from config (e.g. `pico0`). Omit when only one is configured." }
            },
            "required": ["pin"]
        }),
        "Read a GPIO pin state",
        true,
    )
}

/// `hardware_capabilities` 的工具定义。
pub(super) fn capabilities_tool() -> ExtensionTool {
    gpio_tool(
        TOOL_CAPABILITIES,
        "Ask the serial bridge firmware which GPIO pins it reports (plus the LED pin and any named \
         pin devices). Use it before guessing a pin number.",
        json!({
            "type": "object",
            "properties": {
                "device": { "type": "string", "description": "Device alias from config. Omit when only one serial device is configured." }
            }
        }),
        "Ask the firmware which GPIO pins are available",
        true,
    )
}

/// 必填整数参数（缺参时给一句明确的错误）。
fn required_u64(args: &Value, key: &str) -> std::result::Result<u64, ToolError> {
    args.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| ToolError(format!("missing required parameter: `{key}`")))
}

/// 执行 `gpio_write`。
pub(super) async fn run_write(args: &Value) -> std::result::Result<ToolResult, ToolError> {
    let pin = required_u64(args, "pin")?;
    let value = required_u64(args, "value")?;
    if value > 1 {
        return Err(ToolError("`value` must be 0 or 1.".to_string()));
    }

    let config = load_config();
    let (alias, target) = config.device_target(args).map_err(ToolError)?;
    let wanted = if value == 1 { "HIGH" } else { "LOW" };

    match target {
        DeviceTarget::Serial(path) => {
            let command =
                super::serial::Command::new("gpio_write", json!({ "pin": pin, "value": value }));
            match super::serial::send(&path, config.baud, command).await {
                Ok(response) if response.ok => {
                    let state = response
                        .data
                        .get("state")
                        .and_then(Value::as_str)
                        .unwrap_or(wanted);
                    Ok(ToolResult::text(format!(
                        "GPIO {pin} set {state} on {alias} ({path})"
                    )))
                }
                Ok(response) => Ok(ToolResult::error(response.failure())),
                Err(err) => Ok(ToolResult::error(err)),
            }
        }
        DeviceTarget::Bridge(addr) => {
            let argv = [pin.to_string(), value.to_string()];
            match super::bridge::request(&addr, "gpio_write", &argv).await {
                Ok(reply) if !super::bridge::is_error_response(&reply) => Ok(ToolResult::text(
                    format!("GPIO {pin} set {wanted} on {alias} ({addr}): {reply}"),
                )),
                Ok(reply) => Ok(ToolResult::error(reply)),
                Err(err) => Ok(ToolResult::error(err)),
            }
        }
    }
}

/// 执行 `gpio_read`。
pub(super) async fn run_read(args: &Value) -> std::result::Result<ToolResult, ToolError> {
    let pin = required_u64(args, "pin")?;

    let config = load_config();
    let (alias, target) = config.device_target(args).map_err(ToolError)?;

    match target {
        DeviceTarget::Serial(path) => {
            let command = super::serial::Command::new("gpio_read", json!({ "pin": pin }));
            match super::serial::send(&path, config.baud, command).await {
                Ok(response) if response.ok => {
                    let state = response
                        .data
                        .get("state")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            match response.data.get("value").and_then(Value::as_u64) {
                                Some(1) => "HIGH".to_string(),
                                Some(0) => "LOW".to_string(),
                                _ => "unknown".to_string(),
                            }
                        });
                    Ok(ToolResult::text(format!(
                        "GPIO {pin} on {alias} ({path}) is {state}"
                    )))
                }
                Ok(response) => Ok(ToolResult::error(response.failure())),
                Err(err) => Ok(ToolResult::error(err)),
            }
        }
        DeviceTarget::Bridge(addr) => {
            let argv = [pin.to_string()];
            match super::bridge::request(&addr, "gpio_read", &argv).await {
                Ok(reply) if !super::bridge::is_error_response(&reply) => Ok(ToolResult::text(
                    format!("GPIO {pin} on {alias} ({addr}) is {reply}"),
                )),
                Ok(reply) => Ok(ToolResult::error(reply)),
                Err(err) => Ok(ToolResult::error(err)),
            }
        }
    }
}

/// 执行 `hardware_capabilities`：问固件它有哪些引脚、哪个是板载 LED。
pub(super) async fn run_capabilities(args: &Value) -> std::result::Result<ToolResult, ToolError> {
    let config = load_config();
    let (alias, path) = config.serial_target(args).map_err(ToolError)?;

    let command = super::serial::Command::new("capabilities", json!({}));
    match super::serial::send(&path, config.baud, command).await {
        Ok(response) if response.ok => {
            let data = &response.data;
            let mut out = format!("{alias} ({path}) capabilities:");
            if let Some(gpio) = data.get("gpio") {
                out.push_str(&format!("\n  gpio: {gpio}"));
            }
            if let Some(led) = data.get("led_pin") {
                out.push_str(&format!("\n  led_pin: {led}"));
            }
            if let Some(description) = data.get("description").and_then(Value::as_str) {
                out.push_str(&format!("\n  description: {description}"));
            }
            if let Some(devices) = data.get("pin_devices") {
                out.push_str(&format!(
                    "\n  pin_devices: {}",
                    serde_json::to_string_pretty(devices).unwrap_or_else(|_| devices.to_string())
                ));
            }
            Ok(ToolResult::text(out))
        }
        Ok(response) => Ok(ToolResult::error(response.failure())),
        Err(err) => Ok(ToolResult::error(err)),
    }
}

/// `hardware_devices` 的工具定义：列串口与配置的设备。
pub(super) fn devices_tool() -> ExtensionTool {
    let mut tool = ExtensionTool::simple(
        "hardware_devices",
        "List serial ports that look like boards, with USB VID:PID and the board name when known, \
         plus the device aliases configured for this extension. Set `ping` to also probe each port \
         for the serial bridge firmware (slower).",
        json!({
            "type": "object",
            "properties": {
                "ping": {
                    "type": "boolean",
                    "description": "Also send a ping to every listed port and report which ones answer as the serial bridge firmware (adds up to ~1s per port)."
                }
            }
        }),
        "List connected serial boards and configured device aliases",
    )
    .with_exposure(ToolExposure::Deferred)
    .with_annotations(ToolAnnotations {
        read_only: true,
        destructive: false,
        idempotent: true,
    });
    tool.label = Some(tool_labels());
    tool
}

/// 执行 `hardware_devices`。
pub(super) async fn run_devices(args: &Value) -> std::result::Result<ToolResult, ToolError> {
    let config = load_config();
    let ports = super::serial::list_ports();
    let ping = args.get("ping").and_then(Value::as_bool).unwrap_or(false);

    let mut out = String::from("Serial ports:\n");
    if ports.is_empty() {
        out.push_str(&format!(
            "  (none; allowed prefixes are {})\n",
            super::serial::allowed_prefixes()
        ));
    } else {
        for (path, detail) in &ports {
            let firmware = if ping && super::serial::ping(path, config.baud).await {
                "  [serial bridge firmware]"
            } else {
                ""
            };
            if detail.is_empty() {
                out.push_str(&format!("  {path}{firmware}\n"));
            } else {
                out.push_str(&format!("  {path}  {detail}{firmware}\n"));
            }
        }
    }

    out.push_str("\nConfigured devices:\n");
    if config.devices.is_empty() {
        out.push_str(
            "  (none; add `devices: {\"pico0\": \"/dev/ttyACM0\"}` to the extension config)\n",
        );
    } else {
        for (alias, target) in &config.devices {
            out.push_str(&format!("  {alias} → {target}\n"));
        }
    }
    Ok(ToolResult::text(out))
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

    /// 单线程 runtime（工具是 async 的）。
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn missing_parameters_are_reported_by_name() {
        assert!(required_u64(&json!({}), "pin").is_err());
        assert_eq!(required_u64(&json!({ "pin": 25 }), "pin").unwrap(), 25);
    }

    #[test]
    fn devices_lists_the_allowlist_when_nothing_is_configured() {
        let (_g, _ad) = guard();
        let out = runtime().block_on(run_devices(&json!({}))).unwrap();
        assert!(out.text.contains("Serial ports:"), "{}", out.text);
        assert!(out.text.contains("Configured devices:"), "{}", out.text);
    }

    #[test]
    fn gpio_write_rejects_a_value_other_than_0_or_1() {
        let (_g, _ad) = guard();
        let err = runtime()
            .block_on(run_write(&json!({ "pin": 25, "value": 2 })))
            .unwrap_err();
        assert!(err.0.contains("0 or 1"), "{}", err.0);
    }

    #[test]
    fn gpio_write_without_a_configured_device_explains_the_config() {
        let (_g, _ad) = guard();
        let err = runtime()
            .block_on(run_write(&json!({ "pin": 25, "value": 1 })))
            .unwrap_err();
        assert!(err.0.contains("devices"), "{}", err.0);
    }

    #[test]
    fn bridge_devices_have_no_serial_path() {
        let (_g, _ad) = guard();
        let err = runtime()
            .block_on(run_capabilities(&json!({})))
            .unwrap_err();
        assert!(err.0.contains("devices"), "{}", err.0);
    }
}
