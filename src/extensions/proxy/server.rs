//! 入站 HTTP 服务（hyper 1.x，HTTP/1.1）与请求处理。
//!
//! 路由：
//! - `POST /v1/chat/completions`（流式 / 非流式）
//! - `GET /v1/models`、`GET /v1/models/{id}`（列出配置的 provider 可用模型）
//! - `GET /health`（免鉴权探活）
//! - 其余路径 `404`、其余方法 `405`，一律 OpenAI 错误体
//!
//! 关键运行时约定：
//! - 可选静态 Bearer token（未配置则免鉴权），比较走常量时间；
//! - 并发上限 [`MAX_CONCURRENT`]，超出立即 `429`（客户端本就实现了退避）；
//! - 上游失败且**首个 chunk 之前**发现 → 如实回 HTTP 状态码；
//!   已发 chunk 之后 → SSE `{"error"}` 帧 + `[DONE]` 收尾；
//! - 客户端断连（响应体被 drop）→ 立刻丢弃上游流（不浪费 token）。

use super::{
    error::GatewayError,
    openai::{ChatRequest, parse_chat_request},
    stats,
    stream::{self, ChunkWriter},
};
use crate::{
    core::{
        auth, model_resolver,
        provider::{self, ModelConfig, PayloadHook, StreamEvent, Usage},
    },
    utils::time::now_ms,
};
use bytes::Bytes;
use futures_util::{
    StreamExt,
    stream::{once as stream_once, unfold},
};
use http_body_util::{BodyExt, Full, Limited, StreamBody, combinators::BoxBody};
use hyper::{
    HeaderMap, Method, Request, Response, StatusCode,
    body::{Frame, Incoming},
    header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE},
    service::service_fn,
};
use hyper_util::rt::TokioIo;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    convert::Infallible,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::{
    net::TcpListener,
    sync::{
        mpsc::{self, UnboundedSender},
        watch::Receiver,
    },
};

/// 并发在途请求上限：超出立即 `429`（该扩展与 TUI 同进程，必须防住内存被吃干）
pub const MAX_CONCURRENT: usize = 30;

/// 请求体上限
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// 未写出帧的排队上限：客户端读得太慢时宁可中止，也不让同进程的 TUI 被内存拖垮
const MAX_QUEUED_BYTES: usize = 1024 * 1024;

/// 入站会话 id 长度上限（防超长值被原样转发给上游）
const MAX_SESSION_ID_LEN: usize = 256;

/// 响应体类型：统一为 boxed body，便于流式/整体响应共用同一返回类型
type ResBody = BoxBody<Bytes, std::io::Error>;

/// 加锁并在锁中毒（持锁线程 panic）时取回内部值，避免一次 panic 让整个网关永久失败。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 服务端运行时状态（与扩展生命周期解耦：改 provider / token 不需要重启监听）
pub struct Runtime {
    /// 目标 provider（每请求时读取，可用 `set_provider` 热切换）
    provider: Mutex<Option<String>>,
    /// 入站 Bearer token；`None` = 免鉴权
    token: Mutex<Option<String>>,
    /// 在途 `/v1/chat/completions` 请求数（并发上限用）
    in_flight: AtomicUsize,
}

impl Runtime {
    /// 创建运行时；`provider` 为 `None` 表示尚未配置目标 provider，`token` 为 `None` 表示不校验入站鉴权。
    pub fn new(provider: Option<String>, token: Option<String>) -> Self {
        Self {
            provider: Mutex::new(provider),
            token: Mutex::new(token),
            in_flight: AtomicUsize::new(0),
        }
    }

    /// 热切换目标 provider，立即对后续请求生效（不影响在途请求）。
    pub fn set_provider(&self, provider: Option<String>) {
        *lock(&self.provider) = provider;
    }

    /// 热切换入站 Bearer token；传 `None` 表示关闭鉴权。
    pub fn set_token(&self, token: Option<String>) {
        *lock(&self.token) = token;
    }

    /// 读取当前目标 provider；未配置时为 `None`。
    pub fn provider(&self) -> Option<String> {
        lock(&self.provider).clone()
    }

    /// 读取当前入站 token；`None` 表示不鉴权。
    pub fn token(&self) -> Option<String> {
        lock(&self.token).clone()
    }
}

/// 常量时间字节比较（避免逐字节提前返回泄漏 token 前缀）
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }

    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 接受连接直到 `shutdown` 置位；每个连接一个任务，连接本身也随关机信号丢弃。
pub async fn serve(listener: TcpListener, rt: Arc<Runtime>, mut shutdown: Receiver<bool>) {
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let Ok((stream, _peer)) = accepted else { continue };
                stats::conn_opened();

                let rt = rt.clone();
                let mut conn_shutdown = shutdown.clone();

                tokio::spawn(async move {
                    let service = service_fn(move |req| handle(req, rt.clone()));
                    let conn = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service);

                    tokio::select! {
                        _ = conn => {}
                        _ = conn_shutdown.changed() => {}
                    }

                    stats::conn_closed();
                });
            }
        }
    }
}

/// 连接级入口：把 `route` 的结果包成 `Ok`，消化掉 `Infallible` 错误类型。
async fn handle(req: Request<Incoming>, rt: Arc<Runtime>) -> Result<Response<ResBody>, Infallible> {
    Ok(route(req, rt).await)
}

/// 按路径与方法分派：`/health` 免鉴权探活；其余先校验 token，再路由到模型列表
/// 或补全接口；未知路径回 404，已知路径的错误方法回 405。
async fn route(req: Request<Incoming>, rt: Arc<Runtime>) -> Response<ResBody> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    // 探活：免鉴权、不区分方法（便于 curl / 脚本）
    if path == "/health" {
        return text_response(StatusCode::OK, "ok\n");
    }

    if !authorized(&req, &rt) {
        return error_response(&GatewayError::unauthorized());
    }

    match path.as_str() {
        "/v1/models" if method == Method::GET => list_models(&rt, None),
        p if p.starts_with("/v1/models/") && method == Method::GET => {
            let id = p.trim_start_matches("/v1/models/").to_string();
            list_models(&rt, Some(id))
        }
        "/v1/chat/completions" if method == Method::POST => chat_completions(req, rt).await,
        "/v1/models" | "/v1/chat/completions" => {
            error_response(&GatewayError::method_not_allowed(method.as_str(), &path))
        }
        p if p.starts_with("/v1/models/") => {
            error_response(&GatewayError::method_not_allowed(method.as_str(), &path))
        }
        _ => error_response(&GatewayError::not_found(&path)),
    }
}

/// 校验入站 `Authorization: Bearer` 头；未配置 token 时恒为真，头缺失、非 `Bearer`
/// 或值不等（常量时间比较）时为假。
fn authorized<B>(req: &Request<B>, rt: &Runtime) -> bool {
    let Some(expected) = rt.token() else {
        return true;
    };
    let Some(raw) = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(provided) = raw.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(provided.trim().as_bytes(), expected.as_bytes())
}

/// `/v1/models`（`only` 给定时为 `/v1/models/{id}` 的校验）
fn list_models(rt: &Runtime, only: Option<String>) -> Response<ResBody> {
    let Some(provider) = rt.provider() else {
        return error_response(&GatewayError::invalid(
            "no provider configured: run `/proxy provider <provider>` in prux",
            None,
        ));
    };

    let created = stream::created_now();
    let models: Vec<Value> = model_resolver::list_models(&provider)
        .into_iter()
        .map(|(id, _name)| stream::model_object(&id, created, &provider))
        .collect();

    match only {
        Some(id) => match models.iter().find(|m| m["id"] == json!(id)) {
            Some(m) => json_response(StatusCode::OK, m),
            None => error_response(&GatewayError::model_not_found(&id, &provider, &[])),
        },
        None => json_response(StatusCode::OK, &json!({"object": "list", "data": models})),
    }
}

/// 处理 `/v1/chat/completions`：先限并发（超出直接回 429 + Retry-After）、读体解析，
/// 再按 `stream` 决定走 SSE 流式还是聚合后一次性返回。
async fn chat_completions(req: Request<Incoming>, rt: Arc<Runtime>) -> Response<ResBody> {
    // 入站头先于 body 抽取（read_body 会吃掉 req）
    let client_session = client_session_id(req.headers());
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(e) => return error_response(&e),
    };
    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(&GatewayError::invalid(
                format!("request body is not valid JSON: {e}"),
                None,
            ));
        }
    };

    // 记账用：日志里尽量给出 model（解析失败也能看见客户端想打谁）
    let log_model = value
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("(none)")
        .to_string();

    let previous = rt.in_flight.fetch_add(1, Ordering::Relaxed);
    if previous >= MAX_CONCURRENT {
        rt.in_flight.fetch_sub(1, Ordering::Relaxed);
        stats::request_started();
        stats::request_finished(false);
        stream::log_request(&log_model, now_ms(), 429, None);

        let mut resp = error_response(&GatewayError::too_many_requests(MAX_CONCURRENT));
        resp.headers_mut().insert(
            hyper::header::RETRY_AFTER,
            hyper::header::HeaderValue::from_static("1"),
        );
        return resp;
    }

    let mut request = match parse_chat_request(&value) {
        Ok(r) => r,
        Err(e) => {
            rt.in_flight.fetch_sub(1, Ordering::Relaxed);
            stats::request_started();
            stats::request_finished(false);
            stream::log_request(&log_model, now_ms(), e.status.as_u16(), None);
            return error_response(&e);
        }
    };
    request.client_session_id = client_session;

    stats::request_started();
    let guard = RequestGuard {
        rt: rt.clone(),
        model: request.model.clone(),
        started_ms: now_ms(),
        done: false,
    };

    if request.stream {
        stream_response(rt, request, guard).await
    } else {
        // 非流式：内部照常流式调用，聚合后一次性返回
        let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
        tokio::spawn(run_chat(rt, request, tx, guard, false));
        match rx.recv().await {
            Some(Out::Body(bytes)) => bytes_response(StatusCode::OK, bytes),
            Some(Out::Fail(e)) => error_response(&e),
            // 首帧之前通道关闭：只能是内部异常
            _ => error_response(&GatewayError::internal("gateway produced no response")),
        }
    }
}

/// 以 SSE 流式返回补全结果：先等首个产物，若「开流前就失败」则改回真实 HTTP 错误状态，
/// 否则先发首帧再续流后续数据帧。
async fn stream_response(
    rt: Arc<Runtime>,
    request: ChatRequest,
    guard: RequestGuard,
) -> Response<ResBody> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    tokio::spawn(run_chat(rt, request, tx, guard, true));

    // 先等第一个产物：可能是首帧，也可能是「开流前就失败」——后者仍能回真实 HTTP 状态
    let first = match rx.recv().await {
        Some(first) => first,
        None => return error_response(&GatewayError::internal("gateway produced no response")),
    };

    match first {
        Out::Fail(e) => error_response(&e),
        Out::Chunk(first_frame) => {
            let rest = unfold(rx, |mut rx| async move {
                match rx.recv().await {
                    Some(Out::Chunk(bytes)) => {
                        Some((Ok::<_, std::io::Error>(Frame::data(bytes)), rx))
                    }
                    _ => None,
                }
            });

            let body = stream_once(async move { Ok(Frame::data(first_frame)) }).chain(rest);

            Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, "text/event-stream")
                .header(CACHE_CONTROL, "no-cache")
                .header("x-accel-buffering", "no")
                .body(BodyExt::boxed(StreamBody::new(body)))
                .unwrap_or_else(|_| {
                    error_response(&GatewayError::internal("failed to build response"))
                })
        }
        Out::Body(bytes) => bytes_response(StatusCode::OK, bytes),
    }
}

/// 网关内部产物：首帧之外还承担「开流前失败」这一控制信号
enum Out {
    /// SSE 帧（已编码）
    Chunk(Bytes),
    /// 非流式响应体
    Body(Bytes),
    /// 开流前失败（可回真实 HTTP 状态）
    Fail(GatewayError),
}

/// 请求记账守卫：无论正常结束、失败还是客户端断连，都在 drop 时结账一次。
struct RequestGuard {
    /// 服务端运行时（用于归还在途计数）
    rt: Arc<Runtime>,
    /// 日志记录的 model（解析失败时为客户端原始请求值）
    model: String,
    /// 请求开始时刻（epoch ms）
    started_ms: u64,
    /// 是否已结账；保证 `finish` 与 `drop` 只生效一次
    done: bool,
}

impl RequestGuard {
    /// 结账一次：归还在途计数并按 `status` 写请求日志（`< 400` 记为成功）。
    /// 已结账时直接返回，保证 `finish` 与 `drop` 只生效一次。
    fn finish(&mut self, status: u16, usage: Option<&Usage>) {
        if self.done {
            return;
        }

        self.done = true;
        self.rt.in_flight.fetch_sub(1, Ordering::Relaxed);
        stats::request_finished(status < 400);
        stream::log_request(&self.model, self.started_ms, status, usage);
    }
}

impl Drop for RequestGuard {
    /// 未显式结账就被丢弃（多为客户端中途断连）时按 499 结账。
    fn drop(&mut self) {
        // 499：客户端在流式过程中断连（nginx 同款约定语义）
        self.finish(499, None);
    }
}

/// 跑一次上游调用并翻译：`streaming` 为真时逐帧推 SSE，否则聚合后推整体响应体。
async fn run_chat(
    rt: Arc<Runtime>,
    request: ChatRequest,
    tx: mpsc::UnboundedSender<Out>,
    mut guard: RequestGuard,
    streaming: bool,
) {
    let model = match resolve_model(&rt, &request).await {
        Ok(m) => m,
        Err(e) => {
            guard.finish(e.status.as_u16(), None);
            let _ = tx.send(Out::Fail(e));
            return;
        }
    };

    if !streaming {
        run_collect(&model, &request, tx, guard).await;
        return;
    }

    let mut writer = ChunkWriter::new(&model.model_id);
    let queued = Arc::new(AtomicUsize::new(0));
    let overflowed = Arc::new(AtomicBool::new(false));
    let tx_sink = tx.clone();

    // 开流前失败（决定能否改判为 HTTP 错误）；以及「首帧 role 是否已发」——
    // 首帧只能在确认上游没有立刻失败之后发，否则会在失败前就把响应钉成 200。
    let mut failed: Option<GatewayError> = None;
    let mut opening_sent = false;
    {
        let queued = queued.clone();
        let overflowed = overflowed.clone();
        let mut sink = |event: StreamEvent| {
            // 开流前失败：provider 层用「首个事件即失败消息」表达（见 provider::stream_chat）
            if failed.is_none()
                && let StreamEvent::Start { partial } = &event
                && partial.stop_reason.as_deref() == Some("error")
            {
                failed = Some(GatewayError::from_upstream(
                    partial
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "upstream failed".to_string()),
                ));
                return;
            }

            if failed.is_some() || overflowed.load(Ordering::Relaxed) {
                return;
            }

            if !opening_sent {
                opening_sent = true;
                send_frame(&tx_sink, writer.opening_chunk());
            }

            let Some(chunk) = writer.on_event(&event) else {
                return;
            };

            let bytes = Bytes::from(stream::frame(&chunk));
            let total = queued.fetch_add(bytes.len(), Ordering::Relaxed) + bytes.len();
            if total > MAX_QUEUED_BYTES {
                overflowed.store(true, Ordering::Relaxed);
                return;
            }

            stats::add_bytes_out(bytes.len() as u64);
            _ = tx_sink.send(Out::Chunk(bytes));
        };

        let payload_hook = top_p_hook(request.top_p);
        let result = tokio::select! {
            r = provider::stream_chat(
                &model,
                &request.messages,
                &request.system_prompt,
                &request.tools,
                request.reasoning_effort.as_deref(),
                request.temperature,
                request.max_tokens,
                Some(&mut sink),
                payload_hook,
                request.tool_choice.as_deref(),
            ) => Some(r),
            _ = tx.closed() => None, // 客户端断连：丢弃上游流（reqwest 流被 drop 即断开上游连接）
        };

        let Some(result) = result else {
            guard.finish(499, None);
            return;
        };

        if let Some(e) = failed.take() {
            guard.finish(e.status.as_u16(), None);
            _ = tx.send(Out::Fail(e));
            return;
        }

        if overflowed.load(Ordering::Relaxed) {
            if !opening_sent {
                send_frame(&tx, writer.opening_chunk());
            }

            _ = tx.send(Out::Chunk(Bytes::from(stream::frame(&writer.error_chunk(
                "gateway buffer overflow: client is reading too slowly",
            )))));

            _ = tx.send(Out::Chunk(Bytes::from(stream::DONE_FRAME)));

            guard.finish(200, result.usage.as_ref());
            return;
        }

        // 上游一个事件都没发（例如 200 + 空体）：仍按官方形状给出首帧
        if !opening_sent {
            send_frame(&tx, writer.opening_chunk());
        }

        // 中途失败：HTTP 状态已成 200，只能用错误帧收尾（并如实计入日志状态）
        if result.message.stop_reason.as_deref() == Some("error") {
            let e = GatewayError::from_upstream(
                result
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "upstream failed".to_string()),
            );
            send_frame(&tx, writer.error_chunk(&e.message));
            let _ = tx.send(Out::Chunk(Bytes::from(stream::DONE_FRAME)));
            guard.finish(e.status.as_u16(), result.usage.as_ref());
            return;
        }

        if let Some(reason) = stream::finish_reason(result.message.stop_reason.as_deref()) {
            send_frame(&tx, writer.finish_chunk(reason));
        }

        if request.include_usage {
            send_frame(&tx, writer.usage_chunk(result.usage.as_ref()));
        }

        _ = tx.send(Out::Chunk(Bytes::from(stream::DONE_FRAME)));
        guard.finish(200, result.usage.as_ref());
    }
}

/// 非流式：无事件回调地跑完，聚合出响应体
async fn run_collect(
    model: &ModelConfig,
    request: &ChatRequest,
    tx: UnboundedSender<Out>,
    mut guard: RequestGuard,
) {
    let payload_hook = top_p_hook(request.top_p);
    let result = provider::stream_chat(
        model,
        &request.messages,
        &request.system_prompt,
        &request.tools,
        request.reasoning_effort.as_deref(),
        request.temperature,
        request.max_tokens,
        None,
        payload_hook,
        request.tool_choice.as_deref(),
    )
    .await;

    if result.message.stop_reason.as_deref() == Some("error") {
        let e = GatewayError::from_upstream(
            result
                .error_message
                .clone()
                .unwrap_or_else(|| "upstream failed".to_string()),
        );
        guard.finish(e.status.as_u16(), None);
        _ = tx.send(Out::Fail(e));
        return;
    }

    let body = stream::completion_body(
        &stream::new_completion_id(),
        &model.model_id,
        stream::created_now(),
        &result,
    );

    guard.finish(200, result.usage.as_ref());

    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    stats::add_bytes_out(bytes.len() as u64);
    _ = tx.send(Out::Body(Bytes::from(bytes)));
}

/// 目标协议能否接受顶层 `top_p` 字段（openai / anthropic / responses 系可以，google 系不行）
fn top_p_supported(api: &str) -> bool {
    !api.starts_with("google")
}

/// `top_p` 没有走 `stream_chat` 的显式参数，经 `on_payload` 注入上游请求体顶层
fn top_p_hook(top_p: Option<f64>) -> Option<PayloadHook> {
    let p = top_p?;
    Some(Arc::new(Mutex::new(Box::new(move |body: &mut Value| {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("top_p".to_string(), json!(p));
        }
    })
        as Box<dyn FnMut(&mut Value) + Send>)))
}

/// 客户端自带的会话标识（大小写不敏感）：`x-session-affinity` 是客户端
/// 给所有协议发的通用亲和头，`x-opencode-session` 是 opencode 系客户端的原生头 ——
/// 客户端已经知道自己的会话，网关就不用自己的派生值覆盖它。
///
/// 值不合法（空 / 非可见 ASCII / 过长）时视为没带：宁可回退到派生 id，也不要拿一个
/// 上游会拒的非法值把请求搞成 400。
fn client_session_id(headers: &HeaderMap) -> Option<String> {
    ["x-session-affinity", "x-opencode-session"]
        .iter()
        .find_map(|name| headers.get(*name))
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= MAX_SESSION_ID_LEN)
        .map(str::to_string)
}

/// 把 JSON 块编码为 SSE 帧推入通道；接收端已关闭时静默忽略。
fn send_frame(tx: &UnboundedSender<Out>, chunk: Value) {
    _ = tx.send(Out::Chunk(Bytes::from(stream::frame(&chunk))));
}

/// 解析目标模型：provider 已配置 + model id 在该 provider 上**精确**存在（拒绝前缀回退）
async fn resolve_model(rt: &Runtime, request: &ChatRequest) -> Result<ModelConfig, GatewayError> {
    let Some(provider) = rt.provider() else {
        return Err(GatewayError::invalid(
            "no provider configured: run `/proxy provider <provider>` in prux",
            None,
        ));
    };

    let entry = model_resolver::find_model(&provider, &request.model).map_err(|_| {
        GatewayError::model_not_found(&request.model, &provider, &model_examples(&provider))
    })?;

    // find_model 的第三档是前缀回退（内部使用方便，对外网关会静默换模型）→ 这里拒绝
    if entry.id != request.model && entry.name != request.model {
        return Err(GatewayError::model_not_found(
            &request.model,
            &provider,
            &model_examples(&provider),
        ));
    }

    // 会话标识：客户端自己带（`x-session-affinity` / `x-opencode-session`）就用它的 ——
    // 网关不拿派生值去盖真实会话；没带才按请求的会话稳定前缀派生（见 [`gateway_session_id`]）。
    let session_id = request
        .client_session_id
        .clone()
        .unwrap_or_else(|| gateway_session_id(request));
    let mut model = model_resolver::model_config_from_entry(&entry, None, Some(session_id));

    // 客户端自带会话头 = 明确要求按会话路由：目录未表态（缺 sendSessionAffinityHeaders）时
    // 按亲和规则下发，否则客户端给的会话 id 到不了上游（目录显式关掉的仍然尊重）。
    if request.client_session_id.is_some() && model.send_session_affinity_headers.is_none() {
        model.send_session_affinity_headers = Some(true);
    }

    // top_p 只能经 on_payload 注入请求体顶层：google 系协议把它放在 generationConfig 里，
    // 顶层多一个字段会被上游 400 —— 与其硬塞，不如如实说「这里表达不了」
    if request.top_p.is_some() && !top_p_supported(&model.api) {
        return Err(GatewayError::unsupported(
            "top_p",
            "this provider's API cannot express top_p through this gateway",
        ));
    }

    if request.has_image && !model.input.iter().any(|i| i == "image") {
        return Err(GatewayError::invalid(
            format!("model `{}` does not support image input", entry.id),
            Some("messages"),
        ));
    }

    // 每请求前刷新 OAuth 凭据（与 agent_loop::prepare_model_context 同款）
    match auth::ensure_oauth_valid(&model.provider).await {
        Ok(Some(access)) if model.api_key != access => model.api_key = access,
        Ok(_) => {}
        Err(e) => return Err(GatewayError::from_upstream(format!("{e}"))),
    }
    Ok(model)
}

/// 网关派生的会话标识，仅在客户端**没有**自带会话头时使用；写入 `ModelConfig.session_id`，
/// 由 provider 层翻译成各家需要的路由 / 提示缓存亲和头
/// （opencode `x-opencode-session`、anthropic `x-session-affinity`）。
///
/// 网关没有真实会话：客户端（任何 OpenAI 兼容程序）也不该为了某个 provider 的私有头
/// 而改代码。这里用请求里的**会话稳定前缀**（系统提示 + 首条消息）做 SHA-256：
/// 同一会话逐轮追加消息时该前缀不变 → id 稳定（路由与提示缓存才有效）；
/// 换了会话（系统提示或开场白不同）→ id 不同。
fn gateway_session_id(request: &ChatRequest) -> String {
    let anchor = request
        .messages
        .iter()
        .map(|m| m.text())
        .find(|t| !t.is_empty())
        .unwrap_or_default();

    let mut hasher = Sha256::new();
    hasher.update(request.system_prompt.as_bytes());
    hasher.update([0u8]); // 分隔符：避免 ("ab", "c") 与 ("a", "bc") 撞同一个哈希
    hasher.update(anchor.as_bytes());
    let digest = hasher.finalize();

    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("prux-gw-{hex}")
}

/// 404 提示里的可用模型样例
fn model_examples(provider: &str) -> Vec<String> {
    model_resolver::list_models(provider)
        .into_iter()
        .take(5)
        .map(|(id, _)| id)
        .collect()
}

/// 读取请求体并计入入站字节统计；超过 `MAX_BODY_BYTES` 时返回 `too_large` 错误。
async fn read_body(req: Request<Incoming>) -> Result<Bytes, GatewayError> {
    match Limited::new(req.into_body(), MAX_BODY_BYTES)
        .collect()
        .await
    {
        Ok(collected) => {
            let bytes = collected.to_bytes();
            stats::add_bytes_in(bytes.len() as u64);
            Ok(bytes)
        }
        Err(_) => Err(GatewayError::too_large(MAX_BODY_BYTES)),
    }
}

/// 把 JSON 值序列化为响应体（序列化失败时退回 `{}`），统一标注 `application/json`。
fn json_response(status: StatusCode, value: &Value) -> Response<ResBody> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    bytes_response(status, Bytes::from(body))
}

/// 构造 `application/json` 的定长字节响应；响应头非法时退回空体响应。
fn bytes_response(status: StatusCode, bytes: Bytes) -> Response<ResBody> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(bytes).map_err(|never| match never {}).boxed())
        .unwrap_or_else(|_| Response::new(empty_body()))
}

/// 按 `GatewayError` 自带的状态码与 JSON 体构造错误响应。
fn error_response(e: &GatewayError) -> Response<ResBody> {
    json_response(e.status, &e.body())
}

/// 构造 `text/plain; charset=utf-8` 的定长文本响应；响应头非法时退回空体响应。
fn text_response(status: StatusCode, text: &str) -> Response<ResBody> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(
            Full::new(Bytes::from(text.to_string()))
                .map_err(|never| match never {})
                .boxed(),
        )
        .unwrap_or_else(|_| Response::new(empty_body()))
}

/// 空的 boxed 响应体（响应构造失败时的兜底）。
fn empty_body() -> ResBody {
    Full::new(Bytes::new())
        .map_err(|never| match never {})
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::AgentMessage;

    #[test]
    fn constant_time_eq_matches_exact_bytes_only() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn runtime_provider_and_token_are_swappable() {
        let rt = Runtime::new(Some("deepseek".into()), None);
        assert_eq!(rt.provider().as_deref(), Some("deepseek"));
        assert_eq!(rt.token(), None);
        rt.set_provider(None);
        rt.set_token(Some("t".into()));
        assert_eq!(rt.provider(), None);
        assert_eq!(rt.token().as_deref(), Some("t"));
    }

    #[test]
    fn models_route_requires_configured_provider() {
        let rt = Runtime::new(None, None);
        let resp = list_models(&rt, None);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn top_p_is_gated_by_target_api() {
        assert!(top_p_supported("openai-completions"));
        assert!(top_p_supported("openai-responses"));
        assert!(top_p_supported("anthropic-messages"));
        assert!(
            !top_p_supported("google-generative-ai"),
            "google 把 top_p 放在 generationConfig 里"
        );
    }

    #[test]
    fn top_p_hook_injects_field_into_payload() {
        let hook = top_p_hook(Some(0.25)).expect("hook");
        let mut body = json!({"model": "m"});
        {
            let mut g = hook.lock().unwrap();
            let f: &mut (dyn FnMut(&mut Value) + Send) = g.as_mut();
            f(&mut body);
        }
        assert_eq!(body["top_p"], 0.25);
        assert!(top_p_hook(None).is_none());
    }

    #[test]
    fn unauthorized_without_matching_token() {
        let rt = Runtime::new(None, Some("secret".into()));
        assert!(!authorized(&req_with(None), &rt));
        assert!(
            !authorized(&req_with(Some("secret")), &rt),
            "缺少 Bearer 前缀"
        );
        assert!(authorized(&req_with(Some("Bearer secret")), &rt));
        assert!(!authorized(&req_with(Some("Bearer nope")), &rt));

        // 未配置 token：免鉴权
        let open = Runtime::new(None, None);
        assert!(authorized(&req_with(None), &open));
    }

    /// 测试用请求（不需要真实 body 类型，[`authorized`] 对 body 泛型）
    fn req_with(header: Option<&str>) -> Request<()> {
        let mut builder = Request::builder().uri("/v1/models");
        if let Some(auth) = header {
            builder = builder.header(AUTHORIZATION, auth);
        }
        builder.body(()).unwrap()
    }

    #[test]
    fn request_guard_finishes_once_even_on_drop() {
        let _l = stats::serial();
        stats::reset();
        let rt = Arc::new(Runtime::new(None, None));
        rt.in_flight.fetch_add(1, Ordering::Relaxed);
        stats::request_started();
        {
            let mut guard = RequestGuard {
                rt: rt.clone(),
                model: "m".into(),
                started_ms: crate::utils::time::now_ms(),
                done: false,
            };
            guard.finish(200, None);
            // 二次 finish 与 drop 都必须幂等
            guard.finish(500, None);
        }
        let s = stats::snapshot();
        assert_eq!(s.requests_ok, 1);
        assert_eq!(s.requests_failed, 0);
        assert_eq!(rt.in_flight.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn resolve_model_rejects_unknown_and_prefix_only_ids() {
        // 用真实目录（deepseek 基线内嵌）验证「精确 id 才放行」。
        // 注意：模型目录是编译期内嵌的（与 agent_dir 无关）。
        let rt = Runtime::new(Some("deepseek".into()), None);
        let err = resolve_model(&rt, &chat_req("no-such-model"))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);

        // 前缀（真实 id 的截断）必须被拒绝，而不是静默命中别的模型
        let (id, _) = model_resolver::list_models("deepseek")
            .into_iter()
            .next()
            .expect("deepseek 目录应非空");
        if id.len() > 3 {
            let prefix = &id[..id.len() - 1];
            let err = resolve_model(&rt, &chat_req(prefix)).await.unwrap_err();
            assert_eq!(
                err.status,
                StatusCode::NOT_FOUND,
                "前缀 `{prefix}` 不应命中 `{id}`"
            );
        }

        // 精确 id 通过
        assert!(
            resolve_model(&rt, &chat_req(&id)).await.is_ok(),
            "精确 id `{id}` 应通过"
        );
    }

    #[tokio::test]
    async fn resolve_model_prefers_the_client_session_and_falls_back_to_derived() {
        // 会话标识必须有人填：provider 层的 opencode/anthropic 亲和头全靠它
        let rt = Runtime::new(Some("deepseek".into()), None);
        let (id, _) = model_resolver::list_models("deepseek")
            .into_iter()
            .next()
            .expect("deepseek 目录应非空");

        // 客户端没带：网关按会话稳定前缀派生一个
        let model = resolve_model(&rt, &chat_req(&id)).await.unwrap();
        assert!(
            model
                .session_id
                .as_deref()
                .is_some_and(|s| s.starts_with("prux-gw-")),
            "网关应派生会话 id：{:?}",
            model.session_id
        );

        // 客户端自带了：不拿派生值盖真实会话
        let mut req = chat_req(&id);
        req.client_session_id = Some("client-conv-1".into());
        assert_eq!(
            resolve_model(&rt, &req)
                .await
                .unwrap()
                .session_id
                .as_deref(),
            Some("client-conv-1")
        );
    }

    #[test]
    fn client_session_id_reads_the_inbound_headers() {
        let with = |headers: &[(&str, &str)]| {
            let mut builder = Request::builder();
            for (k, v) in headers {
                builder = builder.header(*k, *v);
            }
            client_session_id(builder.body(()).unwrap().headers())
        };

        assert_eq!(client_session_id(&hyper::HeaderMap::new()), None);
        // 头名大小写不敏感，值去首尾空白
        assert_eq!(
            with(&[("X-Session-Affinity", " sess-1 ")]).as_deref(),
            Some("sess-1")
        );
        // opencode 原生头也认
        assert_eq!(
            with(&[("x-opencode-session", "oc-1")]).as_deref(),
            Some("oc-1")
        );
        // 两个都给：通用的 x-session-affinity 优先
        assert_eq!(
            with(&[
                ("x-opencode-session", "oc-1"),
                ("x-session-affinity", "aff-1")
            ])
            .as_deref(),
            Some("aff-1")
        );
        // 空 / 过长：视为没带，回退派生（不要拿非法值把上游搞成 400）
        assert_eq!(with(&[("x-session-affinity", "   ")]), None);
        let too_long = "x".repeat(MAX_SESSION_ID_LEN + 1);
        assert_eq!(with(&[("x-session-affinity", too_long.as_str())]), None);
    }

    #[test]
    fn gateway_session_id_is_stable_per_conversation_and_distinct_across_them() {
        let with_history = |msgs: Vec<AgentMessage>, system: &str| ChatRequest {
            messages: msgs,
            system_prompt: system.to_string(),
            ..chat_req("m")
        };

        let first = with_history(vec![AgentMessage::user_text("hi")], "you are a bot");
        // 同一会话逐轮追加历史：稳定前缀（系统提示 + 首条消息）不变 → id 稳定
        let second = with_history(
            vec![
                AgentMessage::user_text("hi"),
                AgentMessage::user_text("again"),
            ],
            "you are a bot",
        );
        assert_eq!(
            gateway_session_id(&first),
            gateway_session_id(&second),
            "同一会话追加历史后 id 必须稳定（否则路由/提示缓存失效）"
        );

        // 另一段会话：开场白或系统提示不同 → id 不同
        let other = with_history(vec![AgentMessage::user_text("unrelated")], "you are a bot");
        assert_ne!(gateway_session_id(&first), gateway_session_id(&other));
        let other_system = with_history(vec![AgentMessage::user_text("hi")], "you are a poet");
        assert_ne!(
            gateway_session_id(&first),
            gateway_session_id(&other_system)
        );

        // 空请求不 panic（退化成一个常量 id）
        let empty = with_history(Vec::new(), "");
        assert!(gateway_session_id(&empty).starts_with("prux-gw-"));
    }

    fn chat_req(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.to_string(),
            messages: vec![AgentMessage::user_text("hi")],
            system_prompt: String::new(),
            tools: Vec::new(),
            tool_choice: None,
            stream: false,
            include_usage: false,
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            top_p: None,
            has_image: false,
            client_session_id: None,
        }
    }
}
