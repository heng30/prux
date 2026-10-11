//! CDP（Chrome DevTools Protocol）客户端：发现端点、维持一条 WebSocket、发命令收响应。
//!
//! 连接挂在**浏览器级** WebSocket 上（`/json/version` 的 `webSocketDebuggerUrl`），
//! 页面命令经 `Target.attachToTarget { flatten: true }` 拿到的 `sessionId` 走同一条连接
//! （flat session 模式），因此一个连接就能管多标签页。
//!
//! 事件一律丢弃：本扩展的动作都是请求/响应式的（等页面加载用轮询 `document.readyState`，
//! 不等 `Page.loadEventFired`），所以不需要事件路由，也不需要 `*.enable`。

use crate::utils::net;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{
    mpsc::{self, UnboundedSender},
    oneshot,
};
use tokio_tungstenite::tungstenite::Message;

/// 在途请求表：CDP 命令 id → 响应回调。
type Pending = Arc<StdMutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// 浏览器级端点信息（`GET /json/version` 的结果）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// 浏览器标识，如 `Chrome/140.0.7339.80`；端点没给时为 `unknown`。
    pub browser: String,
    /// 浏览器级 WebSocket 地址（`webSocketDebuggerUrl`）。
    pub ws_url: String,
}

/// 一个 `type == "page"` 的标签页目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageTarget {
    /// CDP 目标 id（`Target.attachToTarget` / `Target.activateTarget` 用）。
    pub target_id: String,
    /// 页面标题（端点未提供时为空串）。
    pub title: String,
    /// 页面 URL。
    pub url: String,
}

/// 通过 `GET http://<host>:<port>/json/version` 发现浏览器级 WebSocket 地址。
///
/// 强制直连（不走环境代理）：调试端点是本机回环，代理只会把它变成超时。
/// 主机非回环、端点不可达、响应不是 DevTools 端点、WebSocket 非回环都返回 `Err`（面向用户的中文提示）。
pub async fn discover(host: &str, port: u16, timeout: Duration) -> Result<Endpoint, String> {
    if !net::is_loopback_host(host) {
        return Err(format!(
            "browser: refusing to connect to non-loopback host `{host}`; the CDP endpoint has no authentication"
        ));
    }

    let url = format!("http://{host}:{port}/json/version");
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .no_proxy()
        .build()
        .map_err(|e| format!("browser: cannot build an HTTP client: {e}"))?;
    let response = client.get(&url).send().await.map_err(|e| {
        format!(
            "browser: cannot reach the browser debug endpoint at {url} ({e}). \
             Start Chrome/Chromium/Edge/Brave with --remote-debugging-port={port} and try again."
        )
    })?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "browser: {url} returned HTTP {status}; is this a browser DevTools endpoint?"
        ));
    }

    let value: Value = response
        .json()
        .await
        .map_err(|e| format!("browser: invalid response from {url}: {e}"))?;
    let ws_url = value
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    if ws_url.is_empty() {
        return Err(format!(
            "browser: {url} has no webSocketDebuggerUrl; is this a browser DevTools endpoint?"
        ));
    }

    if !net::url_is_loopback(&ws_url) {
        return Err(format!(
            "browser: refusing to connect to non-loopback WebSocket `{ws_url}`"
        ));
    }

    Ok(Endpoint {
        browser: value
            .get("Browser")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        ws_url,
    })
}

/// 页面会话表：已 attach 的 target 与当前活动标签页。
#[derive(Default)]
struct Sessions {
    /// targetId → sessionId（同一 target 只 attach 一次）。
    attached: HashMap<String, String>,
    /// 当前活动标签页；`None` = 还没 attach 过任何页面。
    active: Option<PageTarget>,
}

/// 一条 CDP 连接。
///
/// 生命周期由调用方掌握：`Arc` 的最后一份被丢弃时，写任务随之结束并关闭 WebSocket。
/// 命令超时不会关闭连接（页面卡住不等于连接坏了），连接真的断了则由 [`Cdp::is_closed`] 反映。
pub struct Cdp {
    /// 出站消息队列（写任务独占 WebSocket 的写半边）。
    tx: UnboundedSender<Message>,
    /// 在途请求表。
    pending: Pending,
    /// 下一个命令 id（从 1 开始递增）。
    next_id: AtomicU64,
    /// 端点信息（状态展示用）。
    endpoint: Endpoint,
    /// 单条命令的响应超时。
    timeout: Duration,
    /// 页面会话表。
    sessions: StdMutex<Sessions>,
    /// 读任务是否已结束（连接已断）。
    closed: Arc<AtomicBool>,
}

impl Cdp {
    /// 连上浏览器级 WebSocket，并起写/读两个后台任务。
    ///
    /// 握手失败返回 `Err`（面向用户的提示）。调用方需保证在 tokio 运行时内调用。
    pub async fn connect(endpoint: Endpoint, timeout: Duration) -> Result<Self, String> {
        let (stream, _) = tokio_tungstenite::connect_async(endpoint.ws_url.as_str())
            .await
            .map_err(|e| {
                format!(
                    "browser: WebSocket handshake with {} failed: {e}",
                    endpoint.ws_url
                )
            })?;
        let (mut sink, mut source) = stream.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        let pending: Pending = Arc::new(StdMutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));

        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }
            _ = sink.close().await;
        });

        let reader_pending = Arc::clone(&pending);
        let reader_closed = Arc::clone(&closed);
        tokio::spawn(async move {
            while let Some(message) = source.next().await {
                match message {
                    Ok(Message::Text(text)) => route(&reader_pending, &text),
                    Ok(Message::Binary(data)) => {
                        if let Ok(text) = std::str::from_utf8(&data) {
                            route(&reader_pending, text);
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            reader_closed.store(true, Ordering::Relaxed);
            fail_all(&reader_pending, "the CDP connection was closed");
        });

        Ok(Self {
            tx,
            pending,
            next_id: AtomicU64::new(1),
            endpoint,
            timeout,
            sessions: StdMutex::new(Sessions::default()),
            closed,
        })
    }

    /// 连接是否已断（读任务已结束）。
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// 浏览器标识（如 `Chrome/140.0.7339.80`）。
    pub fn browser(&self) -> &str {
        &self.endpoint.browser
    }

    /// 浏览器级 WebSocket 地址。
    pub fn ws_url(&self) -> &str {
        &self.endpoint.ws_url
    }

    /// 发一条浏览器级命令（不带 `sessionId`）。
    pub async fn send(&self, method: &str, params: Value) -> Result<Value, String> {
        self.send_with(None, method, params).await
    }

    /// 发一条页面命令（带 `sessionId`，flat session 模式）。
    pub async fn send_session(
        &self,
        session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        self.send_with(Some(session_id), method, params).await
    }

    /// 发命令并等响应：登记回调 → 投递 → 等 oneshot（带超时）。
    ///
    /// 超时时把该 id 从在途表摘掉（迟到响应会被当作未知 id 丢弃），并返回超时错误。
    async fn send_with(
        &self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, sender);

        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session_id) = session_id {
            message["sessionId"] = json!(session_id);
        }
        if self.tx.send(Message::text(message.to_string())).is_err() {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err("browser: the CDP connection is closed".to_string());
        }

        match tokio::time::timeout(self.timeout, receiver).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => {
                self.drop_stale_session(session_id, &error);
                Err(error)
            }
            Ok(Err(_)) => {
                Err("browser: the CDP connection closed before the response arrived".to_string())
            }
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                Err(format!(
                    "browser: `{method}` timed out after {}s",
                    self.timeout.as_secs()
                ))
            }
        }
    }

    /// 页面会话失效时清掉缓存。
    ///
    /// 用户关掉标签页（或目标被销毁）后，旧 `sessionId` 会报
    /// `Session with given id not found`；不清缓存的话本扩展会永久卡在死会话上
    /// （每个动作都报同一句错）。清掉后下一次动作会重新 attach 当前标签页。
    fn drop_stale_session(&self, session_id: Option<&str>, error: &str) {
        let Some(session_id) = session_id else {
            return;
        };
        if !is_stale_session_error(error) {
            return;
        }

        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.attached.retain(|_, value| value != session_id);
        let stale_active = sessions
            .active
            .as_ref()
            .is_some_and(|target| !sessions.attached.contains_key(&target.target_id));
        if stale_active {
            sessions.active = None;
        }
    }

    /// 列出全部标签页（`type == "page"`，且排除 `devtools://` 这类内部页）。
    pub async fn pages(&self) -> Result<Vec<PageTarget>, String> {
        let result = self.send("Target.getTargets", json!({})).await?;
        let infos = result
            .get("targetInfos")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(infos
            .iter()
            .filter(|info| info.get("type").and_then(Value::as_str) == Some("page"))
            .filter(|info| {
                !info
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .starts_with("devtools://")
            })
            .map(page_target_from)
            .collect())
    }

    /// 新建一个标签页，返回它的目标信息。
    pub async fn create_target(&self, url: &str) -> Result<PageTarget, String> {
        let result = self
            .send("Target.createTarget", json!({ "url": url }))
            .await?;
        let target_id = result
            .get("targetId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if target_id.is_empty() {
            return Err("browser: Target.createTarget returned no targetId".to_string());
        }

        let info = self
            .send("Target.getTargetInfo", json!({ "targetId": target_id }))
            .await
            .unwrap_or(Value::Null);
        match info.get("targetInfo") {
            Some(target) => Ok(page_target_from(target)),
            None => Ok(PageTarget {
                target_id,
                title: String::new(),
                url: url.to_string(),
            }),
        }
    }

    /// attach 到目标并把它设为活动标签页，返回 `sessionId`。
    ///
    /// 同一目标只 attach 一次（结果缓存）；`activate` 为 true 时先把它切到前台
    /// （输入事件只对活动标签页生效）。
    pub async fn attach(&self, target: &PageTarget, activate: bool) -> Result<String, String> {
        if activate {
            self.send(
                "Target.activateTarget",
                json!({ "targetId": target.target_id }),
            )
            .await?;
        }

        let cached = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .attached
            .get(&target.target_id)
            .cloned();

        let session_id = match cached {
            Some(session_id) => session_id,
            None => {
                let result = self
                    .send(
                        "Target.attachToTarget",
                        json!({ "targetId": target.target_id, "flatten": true }),
                    )
                    .await?;
                let session_id = result
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if session_id.is_empty() {
                    return Err(format!(
                        "browser: Target.attachToTarget returned no sessionId for {}",
                        target.target_id
                    ));
                }
                self.sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .attached
                    .insert(target.target_id.clone(), session_id.clone());
                session_id
            }
        };

        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active = Some(target.clone());
        Ok(session_id)
    }

    /// 当前活动标签页。
    pub fn active(&self) -> Option<PageTarget> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active
            .clone()
    }

    /// 当前活动标签页的 `sessionId`（没 attach 过时为 `None`）。
    pub fn active_session(&self) -> Option<String> {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let target = sessions.active.as_ref()?;
        sessions.attached.get(&target.target_id).cloned()
    }

    /// 在页面上求值，返回 CDP 的 `RemoteObject`。
    ///
    /// 页面抛异常时返回 `Err`（文案取异常的 `description` / `text`），
    /// 因此调用方不用自己检查 `exceptionDetails`。
    pub async fn evaluate(
        &self,
        session_id: &str,
        expression: &str,
        return_by_value: bool,
    ) -> Result<Value, String> {
        self.evaluate_with(session_id, expression, return_by_value, false)
            .await
    }

    /// [`Cdp::evaluate`] 的完整版：可要求等待 Promise 落地。
    pub async fn evaluate_with(
        &self,
        session_id: &str,
        expression: &str,
        return_by_value: bool,
        await_promise: bool,
    ) -> Result<Value, String> {
        let result = self
            .send_session(
                session_id,
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": return_by_value,
                    "awaitPromise": await_promise,
                }),
            )
            .await?;

        if let Some(details) = result.get("exceptionDetails") {
            return Err(format!(
                "browser: the page threw: {}",
                exception_text(details)
            ));
        }

        Ok(result.get("result").cloned().unwrap_or(Value::Null))
    }
}

/// 把 `Target.getTargets` / `Target.getTargetInfo` 的一条记录转成 [`PageTarget`]。
fn page_target_from(info: &Value) -> PageTarget {
    PageTarget {
        target_id: info
            .get("targetId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: info
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        url: info
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

/// 错误文案是否表示页面会话已失效（标签页被关 / 目标被销毁）。
fn is_stale_session_error(error: &str) -> bool {
    error.to_ascii_lowercase().contains("session with given id")
}

/// 从 `exceptionDetails` 里挑一句可读的异常文案。
fn exception_text(details: &Value) -> String {
    if let Some(description) = details
        .get("exception")
        .and_then(|e| e.get("description"))
        .and_then(Value::as_str)
    {
        return description.to_string();
    }
    if let Some(text) = details.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    details.to_string()
}

/// 按 id 把响应分发给等待者；事件（无 `id`）一律忽略。
fn route(pending: &Pending, text: &str) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let Some(id) = value.get("id").and_then(Value::as_u64) else {
        return;
    };
    let Some(sender) = pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id)
    else {
        return;
    };

    let outcome = match value.get("error") {
        Some(error) => Err(format!(
            "browser: CDP error {}: {}",
            error.get("code").and_then(Value::as_i64).unwrap_or(0),
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
        )),
        None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
    };
    _ = sender.send(outcome);
}

/// 连接断开时让所有在途请求立即失败，而不是各自等到超时。
fn fail_all(pending: &Pending, reason: &str) {
    let mut table = pending.lock().unwrap_or_else(|e| e.into_inner());
    for (_, sender) in table.drain() {
        _ = sender.send(Err(format!("browser: {reason}")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 非回环主机在发现阶段就被拒绝（不发任何请求）。
    #[tokio::test]
    async fn discover_rejects_non_loopback_host() {
        let error = discover("example.com", 9222, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(error.contains("non-loopback"), "{error}");
    }

    /// CDP 错误响应被转成可读文案，成功响应取 `result`。
    #[test]
    fn route_dispatches_results_and_errors() {
        let pending: Pending = Arc::new(StdMutex::new(HashMap::new()));

        let (ok_sender, ok_receiver) = oneshot::channel();
        pending.lock().unwrap().insert(1, ok_sender);
        route(&pending, r#"{"id":1,"result":{"sessionId":"S1"}}"#);
        assert_eq!(
            ok_receiver.blocking_recv().unwrap().unwrap(),
            json!({ "sessionId": "S1" })
        );

        let (err_sender, err_receiver) = oneshot::channel();
        pending.lock().unwrap().insert(2, err_sender);
        route(
            &pending,
            r#"{"id":2,"error":{"code":-32601,"message":"method not found"}}"#,
        );
        let error = err_receiver.blocking_recv().unwrap().unwrap_err();
        assert!(error.contains("-32601") && error.contains("method not found"));

        // 事件（无 id）与未知 id 都不应 panic，也不该留下悬挂项
        route(&pending, r#"{"method":"Page.loadEventFired"}"#);
        route(&pending, r#"{"id":99,"result":{}}"#);
        assert!(pending.lock().unwrap().is_empty());
    }

    /// 断开时在途请求立即拿到错误，而不是等超时。
    #[test]
    fn fail_all_releases_every_pending_call() {
        let pending: Pending = Arc::new(StdMutex::new(HashMap::new()));
        let (sender, receiver) = oneshot::channel();
        pending.lock().unwrap().insert(7, sender);
        fail_all(&pending, "the CDP connection was closed");
        let error = receiver.blocking_recv().unwrap().unwrap_err();
        assert!(error.contains("connection was closed"), "{error}");
    }

    /// 只有「会话失效」类错误才判为需要清缓存。
    #[test]
    fn stale_session_errors_are_recognised() {
        assert!(is_stale_session_error(
            "browser: CDP error -32001: Session with given id not found."
        ));
        assert!(!is_stale_session_error(
            "browser: CDP error -32000: Cannot navigate to invalid URL"
        ));
    }
}
