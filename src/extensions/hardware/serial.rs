//! 串口通道：行分隔 JSON 协议（`{cmd, params}` → `{ok, data, error}`）。
//!
//! 板子上的固件（Pico / ESP32 / Arduino 上跑的桥接程序）用这套协议响应主机命令。
//! 命令名与参数由各工具构造，本模块只负责「写一行、读一行、超时」。
//!
//! 两条硬约束：
//! - **只允许白名单里的设备路径**（`/dev/ttyACM*` / `/dev/ttyUSB*` / USB modem 等）：
//!   模型给什么路径就开什么，等于把任意设备交给它；
//! - **每次调用重新打开端口**：既然 prux 是一次一进程的 TUI，没有长驻连接要维护，
//!   打开-写-读-关 也顺带避免半死连接把后续命令拖死。

use super::boards;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_serial::SerialPortBuilderExt;

/// 设备路径白名单前缀
const ALLOWED_PREFIXES: &[&str] = &[
    "/dev/ttyACM",
    "/dev/ttyUSB",
    "/dev/tty.usbmodem",
    "/dev/cu.usbmodem",
    "/dev/tty.usbserial",
    "/dev/cu.usbserial",
    "COM",
];

/// 默认波特率。
pub(super) const DEFAULT_BAUD: u32 = 115_200;

/// 单次「写一行 + 读一行」的超时。
const TIMEOUT: Duration = Duration::from_secs(5);

/// 主机 → 设备的命令。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct Command {
    /// 命令名（`ping` / `gpio_read` / `gpio_write` / `capabilities` …）。
    pub cmd: String,
    /// 命令参数，形状随命令而定。
    #[serde(default)]
    pub params: Value,
}

impl Command {
    /// 构造一条带参数的命令。
    pub(super) fn new(cmd: &str, params: Value) -> Self {
        Command {
            cmd: cmd.to_string(),
            params,
        }
    }
}

/// 设备 → 主机的响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(super) struct Response {
    /// 命令是否成功。
    pub ok: bool,
    /// 响应载荷，形状随命令而定。
    #[serde(default)]
    pub data: Value,
    /// 失败原因（成功时固件通常不返回该字段）。
    #[serde(default)]
    pub error: Option<String>,
}

impl Response {
    /// 失败原因：固件给的 `error`，没有就给一句通用文案。
    pub(super) fn failure(&self) -> String {
        self.error
            .clone()
            .unwrap_or_else(|| "device returned ok:false".to_string())
    }
}

/// 路径是否在允许的串口前缀里。
pub(super) fn is_allowed_path(path: &str) -> bool {
    ALLOWED_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

/// 允许前缀的提示串（配置文件写错时给用户看）。
pub(super) fn allowed_prefixes() -> String {
    ALLOWED_PREFIXES
        .iter()
        .map(|prefix| format!("{prefix}*"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// 枚举当前可用的串口路径（只列白名单内的）。
pub(super) fn list_ports() -> Vec<(String, String)> {
    let Ok(ports) = tokio_serial::available_ports() else {
        return Vec::new();
    };
    ports
        .into_iter()
        .filter(|port| is_allowed_path(&port.port_name))
        .map(|port| {
            let detail = match &port.port_type {
                tokio_serial::SerialPortType::UsbPort(usb) => {
                    let board = boards::lookup_usb(usb.vid, usb.pid);
                    match board {
                        Some(spec) => format!("{:04x}:{:04x} {}", usb.vid, usb.pid, spec.board),
                        None => format!("{:04x}:{:04x}", usb.vid, usb.pid),
                    }
                }
                _ => String::new(),
            };
            (port.port_name, detail)
        })
        .collect()
}

/// 发一条命令并解析响应。
pub(super) async fn send(path: &str, baud: u32, cmd: Command) -> Result<Response, String> {
    if !is_allowed_path(path) {
        return Err(format!(
            "serial path `{path}` is not allowed; expected one of {}",
            allowed_prefixes()
        ));
    }

    let mut line =
        serde_json::to_string(&cmd).map_err(|e| format!("could not serialize the command: {e}"))?;
    line.push('\n');

    let port = tokio_serial::new(path, baud)
        .open_native_async()
        .map_err(|e| format!("could not open `{path}` at {baud} baud: {e}"))?;

    let mut reader = BufReader::new(port);
    let exchange = async {
        reader
            .get_mut()
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("could not write to `{path}`: {e}"))?;
        reader
            .get_mut()
            .flush()
            .await
            .map_err(|e| format!("could not flush `{path}`: {e}"))?;

        let mut response = String::new();
        reader
            .read_line(&mut response)
            .await
            .map_err(|e| format!("could not read from `{path}`: {e}"))?;
        Ok::<String, String>(response)
    };

    let response = match tokio::time::timeout(TIMEOUT, exchange).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(format!(
                "`{path}` did not answer within {}s (is the firmware running?)",
                TIMEOUT.as_secs()
            ));
        }
    };

    if response.trim().is_empty() {
        return Err(format!("`{path}` closed the connection without a response"));
    }
    serde_json::from_str::<Response>(response.trim()).map_err(|e| {
        format!(
            "`{path}` returned a non-JSON line ({e}): {}",
            response.trim()
        )
    })
}

/// `ping` 握手：确认对端是这套固件（而不是随便一个串口设备）。
///
/// 用比普通命令短得多的超时：设备发现会逐个端口试，等 5s × N 个端口没人受得了。
pub(super) async fn ping(path: &str, baud: u32) -> bool {
    let command = Command::new("ping", json!({}));
    match tokio::time::timeout(Duration::from_millis(800), send(path, baud, command)).await {
        Ok(Ok(response)) => response.ok,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_serial_prefixes_are_allowed() {
        for path in [
            "/dev/ttyACM0",
            "/dev/ttyUSB1",
            "/dev/tty.usbmodem1101",
            "/dev/cu.usbmodem1101",
            "COM3",
        ] {
            assert!(is_allowed_path(path), "{path} 应被允许");
        }
        for path in ["/dev/sda", "/etc/passwd", "/tmp/zc-sim-0", ""] {
            assert!(!is_allowed_path(path), "{path} 不该被允许");
        }
    }

    #[test]
    fn commands_serialize_to_one_json_line() {
        let cmd = Command::new("gpio_write", json!({ "pin": 25, "value": 1 }));
        let line = serde_json::to_string(&cmd).unwrap();
        assert!(!line.contains('\n'));
        assert!(line.contains("\"cmd\":\"gpio_write\""));
        assert!(line.contains("\"params\":{"));
    }

    #[test]
    fn responses_parse_with_or_without_optional_fields() {
        let ok: Response = serde_json::from_str(r#"{"ok":true,"data":{"state":"HIGH"}}"#).unwrap();
        assert!(ok.ok);
        assert_eq!(ok.data["state"], "HIGH");
        assert!(ok.error.is_none());

        let bare: Response = serde_json::from_str(r#"{"ok":true}"#).unwrap();
        assert!(bare.data.is_null());

        let failed: Response = serde_json::from_str(r#"{"ok":false,"error":"pin busy"}"#).unwrap();
        assert_eq!(failed.failure(), "pin busy");
    }

    #[test]
    fn a_path_outside_the_allowlist_is_rejected_before_opening() {
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(send(
                "/etc/passwd",
                115_200,
                Command::new("ping", json!({})),
            ))
            .unwrap_err();
        assert!(err.contains("not allowed"), "{err}");
    }
}
