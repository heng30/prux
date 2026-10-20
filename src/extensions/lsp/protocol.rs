//! 最小 LSP 客户端：`Content-Length` 帧读写、请求/响应配对、诊断通知缓存。
//!
//! 只实现 prux 需要的这一侧：发请求、收响应、把 `textDocument/publishDiagnostics`
//! 这类服务器推送存起来。不引 `lsp-types`（那套类型会把 union 响应再包一层），
//! 参数与结果都直接按 JSON 处理，由 [`super`] 里的格式化函数解释。
//!
//! 两条硬性约束：
//!
//! - **帧格式是 `Content-Length: <n>\r\n\r\n<body>`**，不是行分隔 JSON。本仓 MCP 的
//!   stdio 传输是 newline-delimited，两者不能互相套用。
//! - 服务器会反向发请求（`workspace/configuration`、`client/registerCapability`、
//!   `window/workDoneProgress/create`），**必须回响应**，否则服务器在握手或首次请求时挂住。

use super::servers::ServerSpec;
use crate::{
    extensions::util::content_hash,
    utils::{http::path_to_uri, paths::find_in_path},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex as AsyncMutex, oneshot},
};

/// 单条 header 行的长度上限；超过视为服务器输出异常，直接放弃连接。
const MAX_HEADER_LINE: usize = 8 * 1024;

/// 一次 `publishDiagnostics` 推送的内容。
#[derive(Debug, Clone)]
pub struct DiagEntry {
    /// 服务器给的诊断数组（原始 JSON，字段含义由调用方解释）。
    pub items: Vec<Value>,
    /// 推送里带的文档版本；服务器不带版本时为 `None`。
    pub version: Option<i64>,
    /// 收到这条推送的时刻，用于「只要 didOpen 之后的推送」。
    pub received: Instant,
}

/// 一个常驻的语言服务器连接。
pub struct LspClient {
    /// 子进程句柄，只用来在 [`Drop`] 时杀进程。
    child: Mutex<Child>,
    /// 写端；读端已交给后台任务。
    stdin: Arc<AsyncMutex<ChildStdin>>,
    /// 下一个请求 id。
    next_id: AtomicI64,
    /// 已发出、尚未收到响应的请求。
    pending: Arc<AsyncMutex<HashMap<i64, oneshot::Sender<Value>>>>,
    /// 服务器推送的诊断，按文档 URI 缓存。
    diagnostics: Arc<Mutex<HashMap<String, DiagEntry>>>,
    /// 已打开文档的 URI →（当前版本, 上次同步内容的哈希）。
    opened: AsyncMutex<HashMap<String, (i64, u64)>>,
    /// 错误信息里标识是哪个服务器（`rust-analyzer @ /path`）。
    label: String,
}

impl LspClient {
    /// 启动服务器并完成 `initialize` / `initialized` 握手。
    ///
    /// `log_path` 是子进程 stderr 的落点（服务器日志绝不能进 TUI 滚动区）；
    /// `root` 是工作区根，会同时作为 `rootUri` 与 `workspaceFolders[0]`。
    pub async fn spawn(
        spec: &ServerSpec,
        root: &Path,
        log_path: &Path,
    ) -> Result<Arc<Self>, String> {
        let exe = find_in_path(spec.command).ok_or_else(|| {
            format!(
                "language server `{}` is not installed or not on PATH. Install it with: {}",
                spec.command, spec.install_hint
            )
        })?;

        if let Some(parent) = log_path.parent() {
            _ = std::fs::create_dir_all(parent);
        }

        let stderr = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
        {
            Ok(file) => Stdio::from(file),
            Err(_) => Stdio::null(),
        };

        let mut cmd = Command::new(&exe);
        cmd.args(spec.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to start `{}`: {e}", spec.command))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("`{}` has no stdin", spec.command))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("`{}` has no stdout", spec.command))?;

        let client = Arc::new(Self {
            child: Mutex::new(child),
            stdin: Arc::new(AsyncMutex::new(stdin)),
            next_id: AtomicI64::new(1),
            pending: Arc::new(AsyncMutex::new(HashMap::new())),
            diagnostics: Arc::new(Mutex::new(HashMap::new())),
            opened: AsyncMutex::new(HashMap::new()),
            label: format!("{} @ {}", spec.command, root.display()),
        });

        spawn_reader(&client, stdout);
        client
            .request(
                "initialize",
                initialize_params(root),
                Duration::from_secs(30),
            )
            .await?;
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    /// 发一个请求并等响应；`error` 字段与超时都转成 `Err`。
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(error) = self.write(message).await {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => match response.get("error") {
                Some(error) => Err(format!(
                    "{method} failed ({}): {}",
                    self.label,
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                )),
                None => Ok(response.get("result").cloned().unwrap_or(Value::Null)),
            },
            Ok(Err(_)) => Err(format!(
                "{method} failed: the language server exited ({})",
                self.label
            )),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(format!(
                    "{method} timed out after {}s ({}). The server may still be indexing; \
                     retry in a moment or check the server log.",
                    timeout.as_secs(),
                    self.label
                ))
            }
        }
    }

    /// 发一条通知（不期望响应）。
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.write(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    /// 同步文档内容：首次 `didOpen`，内容变化时全量 `didChange` 并递增版本。
    ///
    /// 内容与上次同步一致时**不发任何消息**（避免每次查询都触发服务器重新分析）。
    /// 返回当前版本号；调用方拿它配合 [`Self::wait_diagnostics`] 判断推送是否够新。
    pub async fn sync_document(
        &self,
        uri: &str,
        language_id: &str,
        text: &str,
    ) -> Result<i64, String> {
        let hash = content_hash(text);
        let mut opened = self.opened.lock().await;

        let next = match opened.get(uri) {
            Some((version, old_hash)) => {
                if *old_hash == hash {
                    return Ok(*version);
                }
                version + 1
            }
            None => 1,
        };
        opened.insert(uri.to_string(), (next, hash));

        if next == 1 {
            self.write(json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id,
                        "version": 1,
                        "text": text,
                    }
                }
            }))
            .await?;
        } else {
            self.write(json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": uri, "version": next },
                    "contentChanges": [{ "text": text }]
                }
            }))
            .await?;
        }
        Ok(next)
    }

    /// 等 `since` 之后、版本不早于 `version` 的第一条针对 `uri` 的诊断推送；超时返回 `None`。
    ///
    /// 服务器不带版本（`None`）时只看收到时间——`rust-analyzer` 与 `clangd` 都不带，
    /// 严格按版本比会把它们全部滤掉。
    pub async fn wait_diagnostics(
        &self,
        uri: &str,
        since: Instant,
        version: i64,
        wait: Duration,
    ) -> Option<DiagEntry> {
        let deadline = Instant::now() + wait;
        loop {
            {
                let cache = self.diagnostics.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = cache.get(uri)
                    && entry.received >= since
                    && entry.version.is_none_or(|pushed| pushed >= version)
                {
                    return Some(entry.clone());
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// 写一整帧。
    async fn write(&self, message: Value) -> Result<(), String> {
        let body = serde_json::to_vec(&message).map_err(|e| e.to_string())?;
        let mut stdin = self.stdin.lock().await;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        stdin
            .write_all(header.as_bytes())
            .await
            .map_err(|e| format!("write to {} failed: {e}", self.label))?;
        stdin
            .write_all(&body)
            .await
            .map_err(|e| format!("write to {} failed: {e}", self.label))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("flush to {} failed: {e}", self.label))
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.start_kill();
        }
    }
}

/// 起后台任务读服务器输出：响应投递给等待者，推送存进诊断缓存，反向请求现场回响应。
fn spawn_reader(client: &Arc<LspClient>, stdout: tokio::process::ChildStdout) {
    let pending = Arc::clone(&client.pending);
    let diagnostics = Arc::clone(&client.diagnostics);
    let stdin = Arc::clone(&client.stdin);
    let label = client.label.clone();

    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_frame(&mut reader).await {
                Ok(Some(message)) => {
                    handle_message(message, &pending, &diagnostics, &stdin, &label).await;
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = error;
                    break;
                }
            }
        }
        // 服务器退出：让所有等待中的请求立刻失败，而不是各自耗到超时。
        pending.lock().await.clear();
    });
}

/// 处理一条服务器消息。
async fn handle_message(
    message: Value,
    pending: &Arc<AsyncMutex<HashMap<i64, oneshot::Sender<Value>>>>,
    diagnostics: &Arc<Mutex<HashMap<String, DiagEntry>>>,
    stdin: &Arc<AsyncMutex<ChildStdin>>,
    label: &str,
) {
    if let Some(method) = message.get("method").and_then(Value::as_str) {
        // 服务器发来的请求（带 id）必须回，否则它会卡在等响应上。
        if let Some(id) = message.get("id").cloned() {
            let result = match method {
                "workspace/configuration" => {
                    let count = message
                        .get("params")
                        .and_then(|p| p.get("items"))
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or(0);
                    Value::Array(vec![Value::Null; count])
                }
                _ => Value::Null,
            };
            let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            let _ = write_shared(stdin, reply, label).await;
            return;
        }

        if method == "textDocument/publishDiagnostics"
            && let Some(params) = message.get("params")
            && let Some(uri) = params.get("uri").and_then(Value::as_str)
        {
            let entry = DiagEntry {
                items: params
                    .get("diagnostics")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                version: params.get("version").and_then(Value::as_i64),
                received: Instant::now(),
            };
            diagnostics
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(uri.to_string(), entry);
        }
        return;
    }

    // 响应：按 id 唤醒等待者。
    if let Some(id) = message.get("id").and_then(Value::as_i64)
        && let Some(tx) = pending.lock().await.remove(&id)
    {
        let _ = tx.send(message);
    }
}

/// 写帧（后台任务与前台请求共用写端）。
async fn write_shared(
    stdin: &Arc<AsyncMutex<ChildStdin>>,
    message: Value,
    label: &str,
) -> Result<(), String> {
    let body = serde_json::to_vec(&message).map_err(|e| e.to_string())?;
    let mut stdin = stdin.lock().await;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    stdin
        .write_all(header.as_bytes())
        .await
        .map_err(|e| format!("write to {label} failed: {e}"))?;
    stdin
        .write_all(&body)
        .await
        .map_err(|e| format!("write to {label} failed: {e}"))?;
    stdin
        .flush()
        .await
        .map_err(|e| format!("flush to {label} failed: {e}"))
}

/// 读一整帧；对端关闭时返回 `Ok(None)`。
///
/// 泛型化到 `AsyncBufRead` 是为了能用内存管道直接测帧解析。
async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Value>, String> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("read from language server failed: {e}"))?;
        if read == 0 {
            return Ok(None);
        }
        if line.len() > MAX_HEADER_LINE {
            return Err("language server sent an oversized header line".to_string());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = value.trim().parse().ok();
        }
    }

    let length = content_length
        .ok_or_else(|| "language server sent a frame without Content-Length".to_string())?;
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| format!("read body from language server failed: {e}"))?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| format!("language server sent invalid JSON: {e}"))
}

/// `initialize` 的固定参数：给足 prux 实际会用的能力，其余一律不声明。
fn initialize_params(root: &Path) -> Value {
    let root_uri = path_to_uri(root);
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");
    json!({
        "processId": std::process::id(),
        "clientInfo": { "name": "prux", "version": env!("CARGO_PKG_VERSION") },
        "rootUri": root_uri,
        "workspaceFolders": [{ "uri": root_uri, "name": name }],
        "capabilities": {
            "textDocument": {
                "synchronization": { "dynamicRegistration": false, "didSave": false, "willSave": false },
                "definition": { "linkSupport": true },
                "references": { "dynamicRegistration": false },
                "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                "publishDiagnostics": { "versionSupport": true, "relatedInformation": false }
            },
            "workspace": { "symbol": { "dynamicRegistration": false } }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// 把一段字节喂给一个假的 stdout，验证帧解析。
    #[tokio::test]
    async fn reads_content_length_frame() {
        let (mut tx, rx) = tokio::io::duplex(4096);
        let body = br#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut frame = header.into_bytes();
        frame.extend_from_slice(body);
        frame.extend_from_slice(b"Content-Length: 2\r\n\r\n{}");
        tx.write_all(&frame).await.unwrap();
        drop(tx);

        let mut reader = BufReader::new(rx);
        let first = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(first["id"], 1);
        let second = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(second, json!({}));
    }

    #[tokio::test]
    async fn eof_is_not_an_error() {
        let (tx, rx) = tokio::io::duplex(64);
        drop(tx);
        let mut reader = BufReader::new(rx);
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }
}
