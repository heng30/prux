//! 测试用假 CDP 服务：一个 `/json/version` HTTP 端点 + 一个浏览器级 WebSocket 端点。
//!
//! 只回放本扩展真正用到的命令，并把收到的命令记进 [`FakeBrowser::commands`] 供断言。
//! 全程只监听 `127.0.0.1` 的随机端口，不联网。

use super::cdp::{Cdp, discover};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_tungstenite::tungstenite::Message;

/// 假浏览器：两个监听端口 + 命令记录。
pub(crate) struct FakeBrowser {
    /// `/json/version` 的 HTTP 端口。
    http_port: u16,
    /// 收到的全部命令（`method` + `params`，按到达顺序）。
    commands: Arc<Mutex<Vec<(String, Value)>>>,
    /// 两个监听任务的句柄（drop 时 abort）。
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl FakeBrowser {
    /// 起两个监听任务并返回句柄。
    pub(crate) async fn start() -> Self {
        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_port = ws_listener.local_addr().unwrap().port();
        let ws_url = format!("ws://127.0.0.1:{ws_port}/devtools/browser/fake");

        let commands: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let ws_commands = Arc::clone(&commands);
        let ws_handle = tokio::spawn(async move {
            while let Ok((stream, _)) = ws_listener.accept().await {
                let commands = Arc::clone(&ws_commands);
                tokio::spawn(async move { serve_ws(stream, commands).await });
            }
        });

        let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = http_listener.local_addr().unwrap().port();
        let body =
            json!({ "Browser": "FakeBrowser/1.0", "webSocketDebuggerUrl": ws_url }).to_string();
        let http_handle = tokio::spawn(async move {
            while let Ok((mut socket, _)) = http_listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        Self {
            http_port,
            commands,
            handles: vec![ws_handle, http_handle],
        }
    }

    /// `/json/version` 的 HTTP 端口（测试里当「已在监听的配置端口」用）。
    pub(crate) fn http_port(&self) -> u16 {
        self.http_port
    }

    /// 走一遍发现 + 握手，返回连上假浏览器的 [`Cdp`]（等价于扩展的 `ensure_connection`）。
    pub(crate) async fn connect(&self) -> Cdp {
        let endpoint = discover("127.0.0.1", self.http_port, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(endpoint.browser, "FakeBrowser/1.0");
        Cdp::connect(endpoint, Duration::from_secs(5))
            .await
            .unwrap()
    }

    /// 是否收到过某个方法。
    pub(crate) fn saw(&self, method: &str) -> bool {
        self.commands().iter().any(|(name, _)| name == method)
    }

    /// 某个方法的全部参数（按到达顺序）。
    pub(crate) fn params_of(&self, method: &str) -> Vec<Value> {
        self.commands()
            .iter()
            .filter(|(name, _)| name == method)
            .map(|(_, params)| params.clone())
            .collect()
    }

    /// 是否有某个 `Runtime.evaluate` 表达式包含 `needle`。
    pub(crate) fn saw_expression(&self, needle: &str) -> bool {
        self.params_of("Runtime.evaluate").iter().any(|params| {
            params
                .get("expression")
                .and_then(Value::as_str)
                .is_some_and(|expression| expression.contains(needle))
        })
    }

    /// 收到的命令快照（断言失败时打印用）。
    pub(crate) fn commands(&self) -> Vec<(String, Value)> {
        self.commands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Drop for FakeBrowser {
    /// 测试结束时停掉两个监听任务。
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}

/// 回放一条 WebSocket 连接上的全部命令。
async fn serve_ws(stream: TcpStream, commands: Arc<Mutex<Vec<(String, Value)>>>) {
    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };

    while let Some(Ok(message)) = ws.next().await {
        let Ok(text) = message.into_text() else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        commands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((method.clone(), params.clone()));

        let id = value.get("id").cloned().unwrap_or(Value::Null);
        let response = match reply(&method, &params) {
            Ok(result) => json!({ "id": id, "result": result }),
            Err(message) => json!({ "id": id, "error": { "code": -32601, "message": message } }),
        };
        if ws.send(Message::text(response.to_string())).await.is_err() {
            break;
        }
    }
}

/// 按方法回放一条结果；未实现的方法返回 CDP 风格的错误。
fn reply(method: &str, params: &Value) -> Result<Value, String> {
    Ok(match method {
        "Target.getTargets" => json!({
            "targetInfos": [
                { "targetId": "T1", "type": "page", "title": "Fake Page", "url": "https://example.test/" },
                { "targetId": "T9", "type": "service_worker", "title": "", "url": "https://example.test/sw.js" }
            ]
        }),
        "Target.attachToTarget" => json!({ "sessionId": "S1" }),
        "Target.createTarget" => json!({ "targetId": "T2" }),
        "Target.getTargetInfo" => json!({
            "targetInfo": { "targetId": "T2", "type": "page", "title": "New Tab", "url": "about:blank" }
        }),
        "Target.activateTarget"
        | "Page.navigate"
        | "DOM.setFileInputFiles"
        | "Input.dispatchMouseEvent"
        | "Input.insertText"
        | "Input.dispatchKeyEvent" => json!({}),
        "Page.getLayoutMetrics" => json!({ "cssContentSize": { "width": 800, "height": 1200 } }),
        "Page.captureScreenshot" => json!({ "data": png_base64() }),
        "Runtime.evaluate" => {
            // 表达式带这个标记时回一个「会话失效」错误，用来验证缓存清理
            if expression_contains(params, "__stale_session__") {
                return Err("Session with given id not found".to_string());
            }
            evaluate_reply(params)
        }
        other => return Err(format!("FakeBrowser has no handler for {other}")),
    })
}

/// `Runtime.evaluate` 的表达式里是否包含 `needle`。
fn expression_contains(params: &Value, needle: &str) -> bool {
    params
        .get("expression")
        .and_then(Value::as_str)
        .is_some_and(|expression| expression.contains(needle))
}

/// 按表达式内容回放 `Runtime.evaluate` 的结果。
///
/// 顺序即优先级：快照 JS 里也含 `document.title` / `innerText`，所以它排在最前。
fn evaluate_reply(params: &Value) -> Value {
    let expression = params
        .get("expression")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let return_by_value = params
        .get("returnByValue")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let result = if expression.starts_with("!!") {
        // `wait` 的条件表达式：`__never__` 恒不满足，其余恒满足
        json!({ "type": "boolean", "value": !expression.contains("__never__") })
    } else if expression.contains("const interactable") {
        json!({ "type": "object", "value": {
            "title": "Fake Page",
            "url": "https://example.test/",
            "refs": ["e1 <button> \"Go\""],
            "total": 1,
            "text": "Hello world"
        }})
    } else if expression.contains("readyState") {
        json!({ "type": "string", "value": "complete" })
    } else if expression.contains("document.title") {
        json!({ "type": "object", "value": { "title": "Fake Page", "url": "https://example.test/" } })
    } else if expression.contains("outerHTML") {
        json!({ "type": "string", "value": "<html><body>Hello world</body></html>" })
    } else if expression.contains("innerText") {
        json!({ "type": "string", "value": "Hello world" })
    } else if expression.contains("getBoundingClientRect") {
        json!({ "type": "object", "value": { "x": 10.0, "y": 20.0, "width": 100.0, "height": 40.0 } })
    } else if expression.contains("return el || null") {
        if return_by_value {
            json!({ "type": "object", "subtype": "null", "value": Value::Null })
        } else {
            json!({ "type": "object", "className": "HTMLInputElement", "objectId": "OBJ1" })
        }
    } else {
        json!({ "type": "object", "value": { "ok": true } })
    };

    json!({ "result": result })
}

/// 一张 2×2 的合法 PNG（base64），让 `prepare_image` 真的能解码。
fn png_base64() -> String {
    let image = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]));
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes)
}
