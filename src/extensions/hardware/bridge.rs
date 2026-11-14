//! Arduino Uno Q Bridge：走本地 TCP 的文本行协议。
//!
//! Uno Q 上的 Bridge app（Python + MCU）在本机 `127.0.0.1:9999` 暴露
//! `digitalWrite` / `digitalRead`，协议极简：
//!
//! ```text
//! → gpio_write 13 1\n
//! ← OK
//! → gpio_read 13\n
//! ← 1
//! ```
//!
//! 与 [`super::serial`] 的区别只是传输层：命令语义一样，因此 [`super::gpio`] 按设备
//! 配置派发到这两条通道之一。

use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

/// Bridge app 的默认监听地址。
pub(super) const DEFAULT_ADDR: &str = "127.0.0.1:9999";

/// 建立连接的超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 等一次响应的超时。
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(3);

/// 单次响应最多读这么多字节（协议只有 `OK` / 数字 / 一行错误）。
const MAX_RESPONSE: usize = 256;

/// 发一条 Bridge 命令并返回响应文本。
///
/// 以 `error:` 开头的响应会被调用方当成失败处理（这里原样返回，不抛错——错误文案要
/// 原封不动地交给模型）。
pub(super) async fn request(addr: &str, command: &str, args: &[String]) -> Result<String, String> {
    let mut stream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            return Err(format!(
                "could not connect to the Uno Q Bridge at {addr}: {e} (is the Bridge app running?)"
            ));
        }
        Err(_) => {
            return Err(format!(
                "connecting to the Uno Q Bridge at {addr} timed out after {}s",
                CONNECT_TIMEOUT.as_secs()
            ));
        }
    };

    let line = if args.is_empty() {
        format!("{command}\n")
    } else {
        format!("{command} {}\n", args.join(" "))
    };
    stream
        .write_all(line.as_bytes())
        .await
        .map_err(|e| format!("could not write to the Uno Q Bridge: {e}"))?;
    stream
        .flush()
        .await
        .map_err(|e| format!("could not flush to the Uno Q Bridge: {e}"))?;

    let mut buffer = vec![0u8; MAX_RESPONSE];
    let read = tokio::time::timeout(RESPONSE_TIMEOUT, stream.read(&mut buffer)).await;
    match read {
        Ok(Ok(0)) => Err("the Uno Q Bridge closed the connection without a response".to_string()),
        Ok(Ok(n)) => Ok(String::from_utf8_lossy(&buffer[..n]).trim().to_string()),
        Ok(Err(e)) => Err(format!("could not read from the Uno Q Bridge: {e}")),
        Err(_) => Err(format!(
            "the Uno Q Bridge did not answer within {}s",
            RESPONSE_TIMEOUT.as_secs()
        )),
    }
}

/// Bridge 响应是否是错误（协议约定 `error:` 前缀）。
pub(super) fn is_error_response(response: &str) -> bool {
    response.trim_start().starts_with("error:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// 起一个一次性的假 Bridge：记录收到的行，回一句固定响应。
    async fn fake_bridge(reply: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 256];
            let n = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..n]).trim().to_string();
            socket.write_all(reply.as_bytes()).await.unwrap();
            request
        });
        (addr, handle)
    }

    #[test]
    fn writes_the_documented_line_and_returns_the_reply() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (addr, handle) = fake_bridge("1\n").await;
            let reply = request(&addr, "gpio_read", &["13".to_string()])
                .await
                .unwrap();
            assert_eq!(reply, "1");
            assert_eq!(handle.await.unwrap(), "gpio_read 13");
        });
    }

    #[test]
    fn write_commands_carry_both_arguments() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (addr, handle) = fake_bridge("OK\n").await;
            let reply = request(&addr, "gpio_write", &["13".to_string(), "1".to_string()])
                .await
                .unwrap();
            assert_eq!(reply, "OK");
            assert_eq!(handle.await.unwrap(), "gpio_write 13 1");
        });
    }

    #[test]
    fn error_prefixes_are_recognized() {
        assert!(is_error_response("error: pin not available"));
        assert!(is_error_response("  error: busy"));
        assert!(!is_error_response("OK"));
        assert!(!is_error_response("1"));
    }

    #[test]
    fn a_dead_address_reports_how_to_fix_it() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime
            .block_on(request("127.0.0.1:1", "gpio_read", &["1".to_string()]))
            .unwrap_err();
        assert!(err.contains("Bridge app"), "{err}");
    }
}
