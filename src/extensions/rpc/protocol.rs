//! RPC 的传输层：stdout 上「一行一条 JSON」，stdin 上同样一行一条命令。
//!
//! 分帧只认 LF（客户端必须按字节流切开，别用会认 `U+2028` 的行读取器）：
//! 事件、命令响应、扩展 UI 记录都写 stdout，诊断一律走 stderr。

use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    io::Write,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::oneshot;

/// stdout 写锁：agent 事件 sink、命令响应与扩展 UI 记录都会写，单行 JSON 不能交错。
static OUT: Mutex<()> = Mutex::new(());

/// 扩展 UI 请求 id 计数器；只做区分，客户端不该解析它的格式。
static NEXT_UI: AtomicU64 = AtomicU64::new(1);

/// 等客户端答复的扩展 UI 请求：id → 唤醒通道。
fn pending() -> &'static Mutex<HashMap<String, oneshot::Sender<Value>>> {
    /// 扩展 UI 等待表（叶级锁：只在插入与取走时持有）。
    static P: OnceLock<Mutex<HashMap<String, oneshot::Sender<Value>>>> = OnceLock::new();
    P.get_or_init(Default::default)
}

/// 写一行 JSON 并 flush（客户端按行等待，不 flush 就是死等）。
pub(super) fn emit(value: &Value) {
    let _guard = OUT.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{value}").is_ok() {
        _ = out.flush();
    }
}

/// 成功响应；`data` 为 `None` 时不带该字段
pub(super) fn success(id: Option<&Value>, command: &str, data: Option<Value>) -> Value {
    let mut map = Map::new();
    if let Some(id) = id {
        map.insert("id".into(), id.clone());
    }
    map.insert("type".into(), json!("response"));
    map.insert("command".into(), json!(command));
    map.insert("success".into(), json!(true));
    if let Some(data) = data {
        map.insert("data".into(), data);
    }
    Value::Object(map)
}

/// 失败响应：命令处理出错时唯一的一行结论，错误正文原样给客户端。
pub(super) fn failure(id: Option<&Value>, command: &str, error: &str) -> Value {
    let mut map = Map::new();
    if let Some(id) = id {
        map.insert("id".into(), id.clone());
    }
    map.insert("type".into(), json!("response"));
    map.insert("command".into(), json!(command));
    map.insert("success".into(), json!(false));
    map.insert("error".into(), json!(error));
    Value::Object(map)
}

/// 发一条不需答复的扩展 UI 记录（notify / setStatus / setTitle / set_editor_text）。
pub(super) fn fire(method: &str, fields: Value) {
    emit(&ui_record(method, &next_ui_id(), fields));
}

/// 发一条要答复的扩展 UI 请求并等客户端回答；stdin 关闭或请求被丢弃时返回 `None`。
///
/// 返回的是客户端那一行 `extension_ui_response` 原文：调用方按 `value` /
/// `confirmed` / `cancelled` 自行解释。
pub(super) async fn ask(method: &str, fields: Value) -> Option<Value> {
    let id = next_ui_id();
    let (tx, rx) = oneshot::channel();
    pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.clone(), tx);
    emit(&ui_record(method, &id, fields));
    rx.await.ok()
}

/// 分发客户端对扩展 UI 请求的答复；未知 id（早已超时/取消）直接丢弃。
pub(super) fn complete(id: &str, response: Value) {
    let sender = pending()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(id);
    if let Some(tx) = sender {
        _ = tx.send(response);
    }
}

/// 分配一个新的扩展 UI 请求 id。
fn next_ui_id() -> String {
    format!("ui-{}", NEXT_UI.fetch_add(1, Ordering::Relaxed))
}

/// 组装一条 `extension_ui_request` 记录。
fn ui_record(method: &str, id: &str, fields: Value) -> Value {
    let mut map = match fields {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    map.insert("type".into(), json!("extension_ui_request"));
    map.insert("id".into(), json!(id));
    map.insert("method".into(), json!(method));
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 无数据成功响应不带 `data` 键；有 id 时原样回传（客户端按 id 对齐）。
    #[test]
    fn responses_echo_the_id_and_omit_empty_data() {
        let id = json!("req-1");
        let ok = success(Some(&id), "abort", None);
        assert_eq!(ok["id"], json!("req-1"));
        assert_eq!(ok["success"], json!(true));
        assert!(ok.get("data").is_none());

        let err = failure(None, "parse", "boom");
        assert!(err.get("id").is_none());
        assert_eq!(err["command"], json!("parse"));
        assert_eq!(err["error"], json!("boom"));
    }

    /// UI 记录的公共字段先写、方法自有字段后写，`method` 不会被载荷顶掉。
    #[test]
    fn ui_records_carry_type_id_and_method() {
        let record = ui_record(
            "select",
            "ui-7",
            json!({"title": "Pick", "method": "hijack"}),
        );
        assert_eq!(record["type"], json!("extension_ui_request"));
        assert_eq!(record["id"], json!("ui-7"));
        assert_eq!(record["method"], json!("select"));
        assert_eq!(record["title"], json!("Pick"));
    }
}
