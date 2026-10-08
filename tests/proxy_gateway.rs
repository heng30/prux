//! `proxy` 扩展的端到端回归：真实 HTTP 客户端 → 网关 → 假上游 SSE 服务器。
//!
//! 覆盖点（对应设计决策）：
//! - SSE 帧序列：首帧 role、正文增量、`finish_reason`、`include_usage`、`[DONE]`；
//! - 思考内容走 `reasoning_content`；
//! - 工具调用以「整块 delta」下发（prux 内部到 `ToolCallEnd` 才揭示 id/name）；
//! - 非流式聚合（`stream: false`）；
//! - `/v1/models`、`/v1/models/{id}`、`/health`；
//! - 鉴权 `401`、未知 model `404`、不支持参数 `400`、上游 429/500 透传与 `502`；
//! - 并发上限 `429`。

use futures_util::StreamExt;
use prux::core::{auth::list_configured_providers, settings_manager::agent_dir};
use prux::extensions::proxy::{MAX_CONCURRENT, Runtime, serve};
use prux::test_support::AgentDirGuard;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

/// 假上游脚本
#[derive(Clone)]
enum Script {
    /// 正常 SSE：逐条原样写出
    Sse(Vec<String>),
    /// 带间隔的 SSE：验证网关是真的逐帧外发（而不是攒完再回）
    SsePaced(Vec<String>, u64),
    /// 直接回一个错误状态码（验证上游 4xx/5xx 的映射）
    Status(u16),
}

/// 假上游：每个连接一个任务；支持「慢响应」以制造并发。
struct FakeUpstream {
    port: u16,
    requests: Arc<AtomicUsize>,
    /// 收到过的请求头（按到达顺序），供断言网关发出的上游请求用
    heads: Arc<Mutex<Vec<String>>>,
}

async fn fake_upstream(script: Script, hold_ms: u64) -> FakeUpstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    let heads = Arc::new(Mutex::new(Vec::new()));
    let record = heads.clone();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let script = script.clone();
            let counter = counter.clone();
            let record = record.clone();
            tokio::spawn(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                let Ok(raw) = read_http_request(&mut socket).await else {
                    return;
                };
                if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    record
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&raw[..pos]).to_string());
                }
                if hold_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(hold_ms)).await;
                }
                match script {
                    Script::Status(code) => {
                        let body = format!(
                            "{{\"error\":{{\"message\":\"upstream says {code}\",\"type\":\"x\"}}}}"
                        );
                        let head = format!(
                            "HTTP/1.1 {code} Upstream\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(head.as_bytes()).await;
                        let _ = socket.write_all(body.as_bytes()).await;
                    }
                    Script::Sse(frames) => {
                        let _ = socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                            )
                            .await;
                        for f in frames {
                            let _ = socket.write_all(f.as_bytes()).await;
                        }
                    }
                    Script::SsePaced(frames, gap) => {
                        let _ = socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                            )
                            .await;
                        for f in frames {
                            let _ = socket.write_all(f.as_bytes()).await;
                            if gap > 0 {
                                tokio::time::sleep(Duration::from_millis(gap)).await;
                            }
                        }
                    }
                }
                let _ = socket.shutdown().await;
            });
        }
    });

    FakeUpstream {
        port,
        requests,
        heads,
    }
}

impl FakeUpstream {
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// 收到过的请求头（按到达顺序）
    fn heads(&self) -> Vec<String> {
        self.heads.lock().unwrap().clone()
    }
}

/// 从原始请求头文本里取某个头（大小写不敏感）；缺失返回空串
fn header_value(head: &str, name: &str) -> String {
    head.lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default()
}

/// 读完请求头 + Content-Length 指定的 body（body 不读完就 close 会触发 RST，让客户端读响应失败）
async fn read_http_request(socket: &mut tokio::net::TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let head_end = loop {
        let n = socket.read(&mut tmp).await?;
        if n == 0 {
            return Ok(buf);
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
    let content_length = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);

    while buf.len() < head_end + content_length {
        let n = socket.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    Ok(buf)
}

/// 起网关（真实 hyper 服务），返回端口
async fn start_gateway(provider: &str, token: Option<String>) -> u16 {
    let rt = Arc::new(Runtime::new(Some(provider.to_string()), token));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = watch::channel(false);
    tokio::spawn(serve(listener, rt, rx));
    // 发送端留在后台（测试结束随 runtime 一起丢）
    tokio::spawn(async move {
        let _keep = tx;
        std::future::pending::<()>().await;
    });
    port
}

/// 在临时 agent_dir 里声明一个指向假上游的 provider（`provider` 可传内置名，
/// 用于验证「内置 provider 的私有请求头」这类路径）
fn write_provider_config(provider: &str, base_url: &str, model_id: &str) {
    let cfg = json!({
        "providers": {
            provider: {
                "baseUrl": base_url,
                "apiKey": "sk-test",
                "models": [{
                    "id": model_id,
                    "name": "Test Model",
                    "api": "openai-completions",
                    "baseUrl": base_url,
                    "contextWindow": 128000,
                    "maxTokens": 4096,
                    "compat": {
                        "supportsUsageInStreaming": true,
                        "supportsFinishReason": true
                    }
                }]
            }
        }
    });
    std::fs::write(
        agent_dir().join("models.json"),
        serde_json::to_string_pretty(&cfg).unwrap(),
    )
    .unwrap();
}

/// 解析 SSE 响应体为 chunk 列表（剥掉 `[DONE]`）
fn parse_frames(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .map(|d| serde_json::from_str::<Value>(d).unwrap_or_else(|e| panic!("坏帧 {d}: {e}")))
        .collect()
}

fn sse(frames: &[&str]) -> Vec<String> {
    frames.iter().map(|f| format!("data: {f}\n\n")).collect()
}

/// 上游正常流：思考 + 正文 + usage
fn text_script() -> Script {
    Script::Sse(sse(&[
        r#"{"id":"c1","object":"chat.completion.chunk","model":"test-model","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"reasoning_content":"thinking hard"},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#,
        "data: [DONE]\n\n",
    ]))
}

async fn chat(port: u16, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

fn chat_body(model: &str) -> Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": "hi"}],
        "stream": true
    })
}

#[tokio::test]
async fn streams_openai_chunks_with_reasoning_usage_and_done() {
    let _ad = AgentDirGuard::temp();
    let up = fake_upstream(text_script(), 0).await;
    write_provider_config("proxy-test", &up.base_url(), "test-model");
    let gw = start_gateway("proxy-test", None).await;

    let mut body = chat_body("test-model");
    body["stream_options"] = json!({"include_usage": true});
    let resp = chat(gw, body).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["content-type"],
        "text/event-stream",
        "SSE 响应头"
    );
    assert_eq!(resp.headers()["cache-control"], "no-cache");

    let text = resp.text().await.unwrap();
    assert!(text.ends_with("data: [DONE]\n\n"), "以 [DONE] 收尾：{text}");
    let frames = parse_frames(&text);
    assert_eq!(frames.len(), 6, "role/思考/正文x2/结束/usage：{frames:#?}");

    assert_eq!(frames[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(
        frames[1]["choices"][0]["delta"]["reasoning_content"],
        "thinking hard"
    );
    assert!(
        frames[1]["choices"][0]["delta"].get("content").is_none(),
        "思考帧不带 content 键"
    );
    assert_eq!(frames[2]["choices"][0]["delta"]["content"], "Hello");
    assert_eq!(frames[3]["choices"][0]["delta"]["content"], " world");
    assert_eq!(frames[4]["choices"][0]["finish_reason"], "stop");
    assert_eq!(frames[5]["choices"], json!([]), "usage 帧 choices 为空");
    assert_eq!(frames[5]["usage"]["prompt_tokens"], 3);
    assert_eq!(frames[5]["usage"]["completion_tokens"], 2);
    assert_eq!(frames[5]["usage"]["total_tokens"], 5);
    assert_eq!(frames[0]["model"], "test-model");
    assert_eq!(up.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn chunks_are_flushed_as_they_arrive() {
    let _ad = AgentDirGuard::temp();
    // 上游三帧、每帧间隔 150ms：若网关攒完再回，首字节也要 ~450ms 才会出现
    let up = fake_upstream(
        Script::SsePaced(
            sse(&[
                r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"content":"first"},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            ]),
            150,
        ),
        0,
    )
    .await;
    write_provider_config("proxy-test", &up.base_url(), "test-model");
    let gw = start_gateway("proxy-test", None).await;

    let resp = chat(gw, chat_body("test-model")).await;
    let mut stream = resp.bytes_stream();
    let first = tokio::time::timeout(Duration::from_millis(100), stream.next())
        .await
        .expect("首帧应在 100ms 内到达（流式外发，不攒整包）")
        .unwrap()
        .unwrap();
    let first = String::from_utf8_lossy(&first).to_string();
    assert!(first.contains("chat.completion.chunk"), "{first}");

    // 剩余帧照常收完
    let mut rest = String::new();
    while let Some(chunk) = stream.next().await {
        rest.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
    }
    assert!(rest.contains("[DONE]"), "流应正常收尾：{rest}");
}

#[tokio::test]
async fn usage_chunk_is_omitted_without_stream_options() {
    let _ad = AgentDirGuard::temp();
    let up = fake_upstream(text_script(), 0).await;
    write_provider_config("proxy-test", &up.base_url(), "test-model");
    let gw = start_gateway("proxy-test", None).await;

    let text = chat(gw, chat_body("test-model"))
        .await
        .text()
        .await
        .unwrap();
    let frames = parse_frames(&text);
    assert_eq!(frames.len(), 5, "无 include_usage：{frames:#?}");
    assert!(frames.iter().all(|f| f.get("usage").is_none()));
}

#[tokio::test]
async fn tool_calls_are_delivered_as_one_complete_delta() {
    let _ad = AgentDirGuard::temp();
    let up = fake_upstream(
        Script::Sse(sse(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a\"}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":7,"completion_tokens":9,"total_tokens":16}}"#,
            "data: [DONE]\n\n",
        ])),
        0,
    )
    .await;
    write_provider_config("proxy-test", &up.base_url(), "test-model");
    let gw = start_gateway("proxy-test", None).await;

    let text = chat(gw, chat_body("test-model"))
        .await
        .text()
        .await
        .unwrap();
    let frames = parse_frames(&text);
    let tool_frame = frames
        .iter()
        .find(|f| f["choices"][0]["delta"].get("tool_calls").is_some())
        .expect("应有工具调用帧");
    let tc = &tool_frame["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(tc["index"], 0);
    assert_eq!(tc["id"], "call_1");
    assert_eq!(tc["type"], "function");
    assert_eq!(tc["function"]["name"], "read");
    assert_eq!(tc["function"]["arguments"], "{\"path\":\"a\"}");
    assert_eq!(
        frames.last().unwrap()["choices"][0]["finish_reason"],
        "tool_calls"
    );
}

#[tokio::test]
async fn non_streaming_request_aggregates_the_completion() {
    let _ad = AgentDirGuard::temp();
    let up = fake_upstream(text_script(), 0).await;
    write_provider_config("proxy-test", &up.base_url(), "test-model");
    let gw = start_gateway("proxy-test", None).await;

    let mut body = chat_body("test-model");
    body["stream"] = json!(false);
    let resp = chat(gw, body).await;
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["model"], "test-model");
    assert_eq!(v["choices"][0]["message"]["content"], "Hello world");
    assert_eq!(
        v["choices"][0]["message"]["reasoning_content"],
        "thinking hard"
    );
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(v["usage"]["total_tokens"], 5);
}

#[tokio::test]
async fn models_endpoints_list_the_configured_provider() {
    let _ad = AgentDirGuard::temp();
    let up = fake_upstream(text_script(), 0).await;
    write_provider_config("proxy-test", &up.base_url(), "test-model");
    let gw = start_gateway("proxy-test", None).await;
    let client = reqwest::Client::new();

    let list: Value = client
        .get(format!("http://127.0.0.1:{gw}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["object"], "list");
    assert_eq!(list["data"][0]["id"], "test-model");
    assert_eq!(list["data"][0]["object"], "model");
    assert_eq!(list["data"][0]["owned_by"], "proxy-test");

    let one: Value = client
        .get(format!("http://127.0.0.1:{gw}/v1/models/test-model"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one["id"], "test-model");

    let missing = client
        .get(format!("http://127.0.0.1:{gw}/v1/models/nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn health_is_open_and_unknown_paths_are_404() {
    let _ad = AgentDirGuard::temp();
    let gw = start_gateway("proxy-test", Some("s3cret".into())).await;
    let client = reqwest::Client::new();

    // /health 免鉴权
    let resp = client
        .get(format!("http://127.0.0.1:{gw}/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok\n");

    // 未带 token 打 /v1/*：先鉴权（401），不泄漏路径存在性
    let resp = client
        .get(format!("http://127.0.0.1:{gw}/v1/embeddings"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "未鉴权时不区分路径");

    // 带 token 的未知路径：OpenAI 错误体
    let resp = client
        .get(format!("http://127.0.0.1:{gw}/v1/embeddings"))
        .bearer_auth("s3cret")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "not_found");

    // 已知路径方法不对 → 405
    let resp = client
        .get(format!("http://127.0.0.1:{gw}/v1/chat/completions"))
        .bearer_auth("s3cret")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 405);
}

#[tokio::test]
async fn bearer_token_is_enforced_when_configured() {
    let _ad = AgentDirGuard::temp();
    let up = fake_upstream(text_script(), 0).await;
    write_provider_config("proxy-test", &up.base_url(), "test-model");
    let gw = start_gateway("proxy-test", Some("s3cret".into())).await;
    let url = format!("http://127.0.0.1:{gw}/v1/chat/completions");
    let client = reqwest::Client::new();

    let unauthorized = client
        .post(&url)
        .json(&chat_body("test-model"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);
    let v: Value = unauthorized.json().await.unwrap();
    assert_eq!(v["error"]["code"], "invalid_api_key");

    let wrong = client
        .post(&url)
        .bearer_auth("nope")
        .json(&chat_body("test-model"))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    let ok = client
        .post(&url)
        .bearer_auth("s3cret")
        .json(&chat_body("test-model"))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    assert!(ok.text().await.unwrap().contains("[DONE]"));
}

#[tokio::test]
async fn unknown_model_is_404_and_missing_model_is_400() {
    let _ad = AgentDirGuard::temp();
    let up = fake_upstream(text_script(), 0).await;
    write_provider_config("proxy-test", &up.base_url(), "test-model");
    let gw = start_gateway("proxy-test", None).await;

    let resp = chat(gw, chat_body("no-such-model")).await;
    assert_eq!(resp.status(), 404, "未知 model 应 404");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "model_not_found");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("test-model"),
        "错误里应给出可用 id 提示：{v}"
    );

    let mut body = chat_body("test-model");
    body.as_object_mut().unwrap().remove("model");
    let resp = chat(gw, body).await;
    assert_eq!(resp.status(), 400);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["error"]["param"],
        "model"
    );

    // 前缀回退必须被拒绝（否则会静默换模型）
    let resp = chat(gw, chat_body("test-mod")).await;
    assert_eq!(resp.status(), 404, "前缀不应命中");

    assert_eq!(
        up.requests.load(Ordering::SeqCst),
        0,
        "这些请求不该打到上游"
    );
}

#[tokio::test]
async fn unsupported_semantic_parameters_are_rejected() {
    let _ad = AgentDirGuard::temp();
    let up = fake_upstream(text_script(), 0).await;
    write_provider_config("proxy-test", &up.base_url(), "test-model");
    let gw = start_gateway("proxy-test", None).await;

    for (key, value) in [
        ("n", json!(3)),
        ("stop", json!(["\n"])),
        ("logprobs", json!(true)),
        ("seed", json!(1)),
        ("response_format", json!({"type": "json_object"})),
    ] {
        let mut body = chat_body("test-model");
        body[key] = value.clone();
        let resp = chat(gw, body).await;
        assert_eq!(resp.status(), 400, "{key} 应 400");
        let v: Value = resp.json().await.unwrap();
        assert_eq!(v["error"]["code"], "unsupported_parameter", "{key}: {v}");
    }

    // 纯元数据忽略
    let mut body = chat_body("test-model");
    body["user"] = json!("u1");
    body["metadata"] = json!({"a": 1});
    body["store"] = json!(false);
    assert_eq!(chat(gw, body).await.status(), 200);
}

#[tokio::test]
async fn upstream_429_is_passed_through_and_5xx_becomes_502() {
    let _ad = AgentDirGuard::temp();
    let up = fake_upstream(Script::Status(429), 0).await;
    write_provider_config("proxy-test", &up.base_url(), "test-model");
    let gw = start_gateway("proxy-test", None).await;

    let resp = chat(gw, chat_body("test-model")).await;
    assert_eq!(resp.status(), 429, "429 必须透传（客户端靠它退避）");
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "rate_limit_exceeded");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("upstream says 429"),
        "保留上游原文：{v}"
    );

    let up5 = fake_upstream(Script::Status(500), 0).await;
    write_provider_config("proxy-test", &up5.base_url(), "test-model");
    let gw5 = start_gateway("proxy-test", None).await;
    let resp = chat(gw5, chat_body("test-model")).await;
    assert_eq!(resp.status(), 502, "上游 5xx 归为网关错误");
    assert_eq!(
        resp.json::<Value>().await.unwrap()["error"]["code"],
        "upstream_error"
    );
}

#[tokio::test]
async fn concurrency_overflow_returns_429_with_retry_after() {
    let _ad = AgentDirGuard::temp();
    // 上游故意慢：让 8 个请求同时挂在途
    let up = fake_upstream(text_script(), 500).await;
    write_provider_config("proxy-test", &up.base_url(), "test-model");
    let gw = start_gateway("proxy-test", None).await;

    let futures: Vec<_> = (0..(MAX_CONCURRENT + 1))
        .map(|_| chat(gw, chat_body("test-model")))
        .collect();
    let responses = futures_util::future::join_all(futures).await;

    let statuses: Vec<u16> = responses.iter().map(|r| r.status().as_u16()).collect();
    let rejected = statuses.iter().filter(|s| **s == 429).count();
    assert_eq!(rejected, 1, "并发上限外应恰好拒一个：{statuses:?}");

    let limited = responses
        .iter()
        .find(|r| r.status() == 429)
        .expect("应有 429");
    assert!(
        limited.headers().contains_key("retry-after"),
        "429 应带 Retry-After"
    );
}

#[tokio::test]
async fn provider_is_configurable_and_parsing_errors_are_reported() {
    let _ad = AgentDirGuard::temp();
    let up = fake_upstream(text_script(), 0).await;
    write_provider_config("proxy-test", &up.base_url(), "test-model");

    // 未配置 provider 的运行时：明确要求先配置，而不是静默失败
    let rt = Arc::new(Runtime::new(None, None));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = watch::channel(false);
    tokio::spawn(serve(listener, rt.clone(), rx));
    tokio::spawn(async move {
        let _keep = tx;
        std::future::pending::<()>().await;
    });

    let resp = chat(port, chat_body("test-model")).await;
    assert_eq!(resp.status(), 400);
    let v: Value = resp.json().await.unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("/proxy provider"),
        "{v}"
    );

    // 运行时可改 provider（无需重启监听）
    rt.set_provider(Some("proxy-test".into()));
    assert_eq!(chat(port, chat_body("test-model")).await.status(), 200);

    // 坏 JSON
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body("{ not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // 自定义 provider 已登记为「已配置凭据」（/proxy provider 的校验依据）
    assert!(
        list_configured_providers()
            .iter()
            .any(|p| p == "proxy-test"),
        "models.json 里的 apiKey 应算已配置"
    );
}

/// opencode / opencode-go（Console Go）要求带 `x-opencode-session` 做路由与提示缓存，
/// 否则上游直接 400 `MissingSessionID`。这个头是 **provider 的私有要求**，OpenAI 兼容
/// 客户端不知道也不该知道 —— 网关要代客户端表达：按「会话稳定前缀」派生一个稳定 id。
///
/// 回归：修复前网关传 `session_id = None`，`provider_session_headers` 直接返回空，
/// 上游拿到的是缺头的请求。
#[tokio::test]
async fn gateway_injects_opencode_session_header_and_keeps_it_stable() {
    let _ad = AgentDirGuard::temp();
    let up = fake_upstream(text_script(), 0).await;
    // 内置 provider 名（provider_session_headers 按 provider / baseUrl 识别 opencode），
    // 但端点指向假上游，避免测试打真实网络
    write_provider_config("opencode", &up.base_url(), "test-model");
    let gw = start_gateway("opencode", None).await;

    // 第一轮
    let turn1 = chat_body("test-model");
    assert_eq!(chat(gw, turn1.clone()).await.status(), 200);

    // 第二轮：同一会话，客户端照例把历史整段回传（首条消息不变）
    let mut turn2 = turn1.clone();
    turn2["messages"] = json!([
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": "Hello"},
        {"role": "user", "content": "再来一个"}
    ]);
    assert_eq!(chat(gw, turn2).await.status(), 200);

    // 另一段会话：开场白不同 → 另一个 id（路由/缓存不应互相干扰）
    let mut other = turn1;
    other["messages"] = json!([{"role": "user", "content": "unrelated opener"}]);
    assert_eq!(chat(gw, other).await.status(), 200);

    let heads = up.heads();
    assert_eq!(heads.len(), 3, "三次请求都应到达上游");
    let session = |i: usize| header_value(&heads[i], "x-opencode-session");

    assert!(
        session(0).starts_with("prux-gw-"),
        "网关应补上会话头：{:?}",
        heads[0]
    );
    assert_eq!(session(0), session(1), "同一会话逐轮追加历史 → id 必须稳定");
    assert_ne!(session(0), session(2), "不同会话 → 不同 id");
    assert_eq!(
        header_value(&heads[0], "x-opencode-client"),
        "prux",
        "opencode 还要求客户端自报身份"
    );
}

/// 非 opencode provider 不得被塞上 opencode 的私有头（网关只负责把「会话标识」
/// 交给 provider 层翻译，后者才决定这个 id 对当前 provider 意味着什么头）
#[tokio::test]
async fn gateway_does_not_send_opencode_headers_to_other_providers() {
    let _ad = AgentDirGuard::temp();
    let up = fake_upstream(text_script(), 0).await;
    write_provider_config("proxy-test", &up.base_url(), "test-model");
    let gw = start_gateway("proxy-test", None).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{gw}/v1/chat/completions"))
        .header("x-session-affinity", "client-conv")
        .json(&chat_body("test-model"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let heads = up.heads();
    assert_eq!(heads.len(), 1);
    assert_eq!(header_value(&heads[0], "x-opencode-session"), "");
    assert_eq!(header_value(&heads[0], "x-opencode-client"), "");
    assert_eq!(
        header_value(&heads[0], "x-session-affinity"),
        "client-conv",
        "其他 provider 的通用亲和头也应接受客户端自带的会话 id"
    );
}

/// 客户端自己就是会话感知的时候（prux / pi 系发 `x-session-affinity`，opencode 系发
/// `x-opencode-session`），网关必须用客户端的值，而不是拿自己的派生值盖掉真实会话
#[tokio::test]
async fn client_supplied_session_header_is_used_verbatim() {
    let _ad = AgentDirGuard::temp();
    let up = fake_upstream(text_script(), 0).await;
    write_provider_config("opencode", &up.base_url(), "test-model");
    let gw = start_gateway("opencode", None).await;

    for (name, value) in [
        ("x-session-affinity", "client-conv-1"),
        ("x-opencode-session", "client-conv-2"),
    ] {
        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{gw}/v1/chat/completions"))
            .header(name, value)
            .json(&chat_body("test-model"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    let heads = up.heads();
    assert_eq!(heads.len(), 2);
    assert_eq!(
        header_value(&heads[0], "x-opencode-session"),
        "client-conv-1",
        "客户端给的会话 id 必须原样用，不能被派生值覆盖"
    );
    assert_eq!(
        header_value(&heads[1], "x-opencode-session"),
        "client-conv-2"
    );
    assert_eq!(
        header_value(&heads[1], "x-session-affinity"),
        "client-conv-2",
        "同一个会话 id 跨协议一致"
    );
}
