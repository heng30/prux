//! ACP 的传输层：stdio 上的 NDJSON + JSON-RPC 2.0。
//!
//! 一条消息一行 JSON，全部走 stdin / stdout。分帧靠换行，不需要 `Content-Length` 头；
//! 用 stdio 是因为编辑器只需 fork 一个子进程加一对管道。

use crate::APP_NAME;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Write,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicI64, Ordering},
    },
};
use tokio::sync::oneshot;

/// JSON 解析失败。
pub(super) const ERROR_PARSE: i64 = -32700;
/// 请求在当前状态下不合法（如已有 prompt 在跑）。
pub(super) const ERROR_INVALID_REQUEST: i64 = -32600;
/// 方法不存在。
pub(super) const ERROR_METHOD_NOT_FOUND: i64 = -32601;
/// 参数缺失或形状不对。
pub(super) const ERROR_INVALID_PARAMS: i64 = -32602;
/// 服务器内部错误。
pub(super) const ERROR_INTERNAL: i64 = -32603;

/// 协商出的协议大版本（整数）。prux 只说 v1。
pub(super) const PROTOCOL_VERSION: u32 = 1;

/// stdout 写锁：事件 sink 与主循环都会写，单行 JSON 不能交错。
static OUT: Mutex<()> = Mutex::new(());

/// 已发出、等客户端答复的请求 id 计数器（从 1 开始，避开通知）。
static NEXT_ID: AtomicI64 = AtomicI64::new(1);

/// 一条入站 JSON-RPC 消息：带 `id` 是请求（必须回一条同 id 的响应），不带是通知；
/// 客户端对我们发起请求的答复则只有 `id` + `result`（或 `error`）。
#[derive(Debug)]
pub(super) struct Incoming {
    /// 请求标识；通知没有它。
    pub id: Option<Value>,
    /// 方法名；客户端对我们发起请求的答复没有它。
    pub method: Option<String>,
    /// 方法参数。
    pub params: Option<Value>,
    /// 客户端响应的结果；错误响应也折叠成 `None`。
    pub result: Option<Value>,
}

impl Incoming {
    /// 从一行 NDJSON 解析；坏 JSON 与非对象（数组、标量）都返回 `None`。
    pub(super) fn parse(line: &str) -> Option<Incoming> {
        let value: Value = serde_json::from_str(line).ok()?;
        if !value.is_object() {
            return None;
        }
        Some(Incoming {
            id: value.get("id").cloned(),
            method: value
                .get("method")
                .and_then(Value::as_str)
                .map(str::to_string),
            params: value.get("params").cloned(),
            result: value.get("result").cloned(),
        })
    }
}

/// 写一行 JSON 并 flush（ACP 客户端按行等待，不 flush 就是死等）。
fn write_line(value: &Value) {
    let _guard = OUT.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{value}").is_ok() {
        _ = out.flush();
    }
}

/// 回一条成功响应。
pub(super) fn respond(id: &Value, result: Value) {
    write_line(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

/// 回一条错误响应。
pub(super) fn respond_error(id: &Value, code: i64, message: &str) {
    write_line(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    }));
}

/// 发一条通知（无 `id`，不等响应）。
pub(super) fn notify(method: &str, params: Value) {
    write_line(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
}

/// 等待客户端答复的请求表：id → 结果发送端。
fn pending() -> &'static Mutex<HashMap<i64, oneshot::Sender<Option<Value>>>> {
    /// 进程级等待表。
    static P: OnceLock<Mutex<HashMap<i64, oneshot::Sender<Option<Value>>>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// 向客户端发一条请求并等它的答复；客户端回错误、答复不合形或连接断开时返回 `None`。
///
/// 答复由 [`complete`] 从 stdin 分发过来，因此调用期间必须还有人读 stdin。
pub(super) async fn request(method: &str, params: Value) -> Option<Value> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, tx);
    write_line(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
    rx.await.ok().flatten()
}

/// 分发客户端答复：命中等待表就唤醒发起方（错误响应折算 `None`），未命中则丢弃。
pub(super) fn complete(id: &Value, result: Option<Value>) {
    let Some(id) = id.as_i64() else {
        return;
    };
    let Some(tx) = pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id)
    else {
        return;
    };
    _ = tx.send(result);
}

/// 组装 `session/update` 的 params：v1 把载荷放在 `update` 下。
pub(super) fn session_update(session_id: &str, update: Value) -> Value {
    json!({"sessionId": session_id, "update": update})
}

/// `initialize` 的响应：能力声明 + 身份。
pub(super) fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "agentCapabilities": {
            // 不支持 session/load：历史已在会话文件里，编辑器侧不需要重放
            "loadSession": false,
            "promptCapabilities": {"image": false, "audio": false, "embeddedContext": false}
        },
        "authMethods": [],
        "agentInfo": {"name": APP_NAME, "version": env!("CARGO_PKG_VERSION")}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests_and_notifications_apart() {
        let request = Incoming::parse(r#"{"jsonrpc":"2.0","id":7,"method":"session/prompt"}"#)
            .expect("请求可解析");
        assert_eq!(request.id, Some(json!(7)));
        assert_eq!(request.method.as_deref(), Some("session/prompt"));

        let notification =
            Incoming::parse(r#"{"jsonrpc":"2.0","method":"session/cancel"}"#).expect("通知可解析");
        assert!(notification.id.is_none(), "通知没有 id");
    }

    #[test]
    fn rejects_non_object_and_broken_json() {
        assert!(Incoming::parse("not json").is_none());
        assert!(Incoming::parse("[]").is_none());
    }

    #[test]
    fn session_update_wraps_the_payload_under_update() {
        let params = session_update("s1", json!({"sessionUpdate": "agent_message_chunk"}));
        assert_eq!(params["sessionId"], json!("s1"));
        assert_eq!(
            params["update"]["sessionUpdate"],
            json!("agent_message_chunk")
        );
    }
}
