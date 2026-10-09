//! 协议流式端到端测试：本地 TcpListener 假 SSE 服务器验证
//! StreamResult 组装、SSE 解析、usage 映射、stop reason（不联网）。

use super::*;
use serde_json::json;

/// 非 chat 类型（目录 `type: image`/`classifier`）必须在分发到协议实现前被拒，
/// 且拒绝不依赖网络（base_url 指向必然连不上的端口）。
#[test]
fn non_chat_model_is_rejected_before_dispatch() {
    run_net_test(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            for (kind, api, id) in [
                (ModelType::Image, "openrouter-images", "flux.2-flex"),
                (ModelType::Classifier, "typesafe-system-one", "jev-latest"),
            ] {
                let mut m = model_with_base("http://127.0.0.1:1", api);
                m.model_type = kind;
                m.model_id = id.into();
                let r = stream_chat(&m, &[], "", &[], None, None, None, None, None, None).await;
                let err = r.message.error_message.clone().unwrap_or_default();
                assert!(err.contains("is not a chat model"), "{err}");
                assert!(err.contains(kind.as_str()), "{err}");
                assert!(r.usage.is_none());
                assert_eq!(r.message.stop_reason.as_deref(), Some("error"));
            }
        });
    });
}
/// 解析不到凭据的 provider 必须在分发前被拒，文案对齐 pi `Provider is not configured: X`；
/// 非 chat 协议的 `api`（如路由哨兵 `prux-virtual`）仍报 `unsupported api type`，不被空 key 文案遮蔽。
#[test]
fn unconfigured_provider_is_rejected_before_dispatch() {
    run_net_test(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut m = model_with_base("http://127.0.0.1:1", "openai-completions");
            m.api_key = String::new();
            m.provider = "deepseek".into();
            let r = stream_chat(&m, &[], "", &[], None, None, None, None, None, None).await;
            let err = r.message.error_message.clone().unwrap_or_default();
            assert!(
                err.contains("Provider is not configured: deepseek"),
                "{err}"
            );
            assert_eq!(r.message.stop_reason.as_deref(), Some("error"));
            assert!(r.usage.is_none());

            let mut v = model_with_base("http://127.0.0.1:1", "prux-virtual");
            v.api_key = String::new();
            let r = stream_chat(&v, &[], "", &[], None, None, None, None, None, None).await;
            let err = r.message.error_message.clone().unwrap_or_default();
            assert!(err.contains("unsupported api type: prux-virtual"), "{err}");
        });
    });
}

/// 网络测试统一护栏：60s 未完成即 panic（挂起 → 可诊断失败）。
fn run_net_test(f: impl FnOnce() + Send + 'static) {
    crate::test_support::run_with_timeout(std::time::Duration::from_secs(90), f)
}

use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};

/// 启动假 SSE 服务器。`frames` 每项为一次 streamed JSON 或原始行序列。
/// 返回 base_url 与服务器 task。
async fn mock_server(
    frames: Vec<&'static str>,
    event_lines: bool,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        // 读完请求头 + body（reqwest 会发 Content-Length）
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = socket.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        // 写完请求头后发送 HTTP 响应（SSE）
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        for f in frames {
            let payload = if event_lines {
                format!("event: message_start\ndata: {}\n\n", f)
            } else {
                format!("data: {}\n\n", f)
            };
            socket.write_all(payload.as_bytes()).await.unwrap();
        }
        socket.shutdown().await.unwrap();
    });
    (format!("http://127.0.0.1:{}", addr.port()), handle)
}

/// 构造指向本地假服务器的测试模型配置：`base_url` 是 mock 地址，`api` 选协议（completions / responses / anthropic / google）。
fn model_with_base(base_url: &str, api: &str) -> ModelConfig {
    ModelConfig {
        model_type: Default::default(),
        image_resize: crate::utils::image::ImageResizeLimits::default(),
        allowed_fallback_models: Vec::new(),
        provider: "opencode".into(),
        model_id: "test-model".into(),
        base_url: base_url.into(),
        api_key: "sk-test".into(),
        api: api.into(),
        output: Vec::new(),
        input: vec!["text".into()],
        reasoning: false,
        max_tokens: Some(4096),
        temperature: None,
        context_window: 200_000,
        thinking_format: String::new(),
        supports_reasoning_effort: false,
        thinking_level_map: None,
        auth_header: false,
        supports_developer_role: false,
        requires_reasoning_content_on_assistant_messages: false,

        supports_usage_in_streaming: true,

        supports_store: true,

        supports_finish_reason: true,

        requires_assistant_after_tool_result: false,
        max_tokens_field: "max_tokens".into(),
        supports_strict_mode: false,
        send_session_affinity_headers: None,
        session_affinity_format: None,
        cost: None,
        session_id: None,
        max_retry_delay_ms: None,
        sampling_params: None,
        sampling_params_by_thinking_level: None,
        provider_routing: None,
        thinking_budgets: None,
    }
}

// =========================================================================
// completions
// =========================================================================

/// openai-completions 流式：验证文本增量拼接、tool_call 参数分片组装、usage 映射与 stop_reason=toolUse。
#[test]
fn completions_streams_text_and_tool_call() {
    run_net_test(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
        let events = vec![
            r#"{"id":"cmpl_1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"}}]}"#,
            r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{"content":"lo"}}]}"#,
            r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":"}}]}}]}"#,
            r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}"#,
            r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"id":"cmpl_1","usage":{"prompt_tokens":10,"completion_tokens":6}}"#,
            r#"[DONE]"#,
        ];
        let (base, handle) = mock_server(events, false).await;
        let m = model_with_base(&base, "openai-completions");
        let result = completions::stream(&m, &[], "", &[], None, None, None, None, None, None)
            .await
            .unwrap();
        handle.await.unwrap();

        assert_eq!(result.message.text(), "Hello");
        assert_eq!(result.message.stop_reason.as_deref(), Some("toolUse"));
        let calls = result.message.tool_calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(calls[0], ContentBlock::ToolCall { id, name, arguments, .. }
            if id == "call_1" && name == "read" && *arguments == serde_json::json!({"path":"a.rs"})));
        let usage = result.usage.unwrap();
        assert_eq!(usage.output, 6);
        assert_eq!(usage.total_tokens, 16);
    });
    });
}

// =========================================================================
// responses
// =========================================================================

/// openai-responses 流式：验证 reasoning 摘要、文本、function_call 参数增量，以及 input 扣除 cached / cache_write 的 usage 映射。
#[test]
fn responses_streams_thinking_text_and_tool_call() {
    run_net_test(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
        let events = vec![
            r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"think"}"#,
            r#"{"type":"response.reasoning_summary_part.done","output_index":0}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"thinking..."}],"encrypted_content":"abc=="}}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1"}}"#,
            r#"{"type":"response.output_text.delta","output_index":1,"delta":"Hello"}"#,
            r#"{"type":"response.output_text.delta","output_index":1,"delta":" world"}"#,
            r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"Hello world"}]}}"#,
            r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"path\":"}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":2,"delta":"\"a.rs\"}"}"#,
            r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{\"path\":\"a.rs\"}"}}"#,
            r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":30,"cache_write_tokens":10}}}}"#,
        ];
        let (base, handle) = mock_server(events, false).await;
        let m = model_with_base(&base, "openai-responses");
        let result = responses::stream(&m, &[], "", &[], None, None, None, None, None, None)
            .await
            .unwrap();
        handle.await.unwrap();

        assert_eq!(result.message.text(), "Hello world");
        assert_eq!(result.message.thinking(), "thinking...");
        assert_eq!(result.message.stop_reason.as_deref(), Some("toolUse"));
        let calls = result.message.tool_calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(calls[0], ContentBlock::ToolCall { id, name, arguments, .. }
            if id == "call_1|fc_1" && name == "read" && *arguments == serde_json::json!({"path":"a.rs"})));
        let usage = result.usage.unwrap();
        // input = 100 - cached 30 - cache_write 10
        assert_eq!(usage.input, 60);
        assert_eq!(usage.output, 20);
        assert_eq!(usage.cache_read, 30);
        assert_eq!(usage.cache_write, 10);
        assert_eq!(usage.total_tokens, 120);
    });
    });
}

// =========================================================================
// anthropic
// =========================================================================

/// anthropic-messages 流式：验证 thinking 与 signature_delta 追加、text / tool_use 分块，以及 cache_read / cache_write(_1h) 的 usage 映射。
#[test]
fn anthropic_streams_thinking_text_and_tool_use() {
    run_net_test(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
        let events = vec![
            r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":100,"output_tokens":0,"cache_read_input_tokens":20,"cache_creation_input_tokens":5,"cache_creation":{"ephemeral_1h_input_tokens":5}}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reason"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_part1"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"_part2"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hel"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"lo"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"a.rs\"}"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":10}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let (base, handle) = mock_server(events, true).await;
        let m = model_with_base(&base, "anthropic-messages");
        let result = anthropic::stream(&m, &[], "", &[], None, None, None, None, None, None)
            .await
            .unwrap();
        handle.await.unwrap();

        assert_eq!(result.message.text(), "Hello");
        // thinking + signature_delta 是追加（sig_part1 + _part2）
        let thinking_blocks: Vec<_> = result
            .message
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Thinking { thinking, thinking_signature, .. } => {
                    Some((thinking.clone(), thinking_signature.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(thinking_blocks.len(), 1);
        assert_eq!(thinking_blocks[0].0, "reason");
        assert_eq!(thinking_blocks[0].1.as_deref(), Some("sig_part1_part2"));
        assert_eq!(result.message.stop_reason.as_deref(), Some("stop"));
        let calls = result.message.tool_calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(calls[0], ContentBlock::ToolCall { id, name, arguments, .. }
            if id == "toolu_1" && name == "read" && *arguments == serde_json::json!({"path":"a.rs"})));
        let usage = result.usage.unwrap();
        assert_eq!(usage.input, 100);
        // message_delta 覆盖 output
        assert_eq!(usage.output, 10);
        assert_eq!(usage.cache_read, 20);
        assert_eq!(usage.cache_write, 5);
        assert_eq!(usage.cache_write_1h, Some(5));
    });
    });
}

// =========================================================================
// google
// =========================================================================

/// google-generative-ai 流式：验证文本拼接、functionCall 解析，以及 output = candidates + thoughts、input 扣除 cached 的 usage 映射。
#[test]
fn google_streams_text_thinking_and_function_call() {
    run_net_test(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
        let events = vec![
            r#"{"responseId":"g1","candidates":[{"content":{"role":"model","parts":[{"text":"Hel"}]}}],"usageMetadata":{"promptTokenCount":100,"cachedContentTokenCount":40,"candidatesTokenCount":4,"thoughtsTokenCount":3,"totalTokenCount":110}}"#,
            r#"{"responseId":"g1","candidates":[{"content":{"role":"model","parts":[{"text":"lo"}]}}]}"#,
            r#"{"responseId":"g1","candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{"path":"a.rs"},"id":"fc_1"}}]},"finishReason":"STOP"}]}"#,
        ];
        let (base, handle) = mock_server(events, false).await;
        let mut m = model_with_base(&base, "google-generative-ai");
        m.model_id = "gemini-3-flash".into();
        m.reasoning = true;
        let result = google::stream(&m, &[], "", &[], None, None, None, None, None)
            .await
            .unwrap();
        handle.await.unwrap();

        assert_eq!(result.message.text(), "Hello");
        assert_eq!(result.message.stop_reason.as_deref(), Some("toolUse"));
        let calls = result.message.tool_calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(calls[0], ContentBlock::ToolCall { id, name, arguments, .. }
            if id == "fc_1" && name == "read" && *arguments == serde_json::json!({"path":"a.rs"})));
        let usage = result.usage.unwrap();
        // input = prompt - cached
        assert_eq!(usage.input, 60);
        assert_eq!(usage.cache_read, 40);
        // output = candidates + thoughts
        assert_eq!(usage.output, 7);
    });
    });
}

// =========================================================================
// SSE 解析：跨 chunk 拆行（MiniMax-M3 端点实测触发）
// =========================================================================

/// 与 mock_server 相同，但逐帧发送精确原始字节，模拟任意 TCP chunk 边界。
async fn mock_server_raw(frames: Vec<Vec<u8>>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = socket.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        for f in frames {
            socket.write_all(&f).await.unwrap();
        }
        socket.shutdown().await.unwrap();
    });
    (format!("http://127.0.0.1:{}", addr.port()), handle)
}

/// 完整 MiniMax 式流：message_start + 一个 text 块 + message_delta + message_stop。
const MINIMAX_FULL_STREAM: &str = r#"event: message_start

data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"minimax-m3","content":[],"usage":{"input_tokens":10,"output_tokens":0}}}

event: content_block_start

data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}

event: content_block_stop

data: {"type":"content_block_stop","index":0}

event: message_delta

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}

event: message_stop

data: {"type":"message_stop"}

"#;

/// 按 17 字节切分 SSE 字节流，验证跨 chunk 拆行时解析器仍能拼出完整事件（MiniMax 端点实测场景）。
#[test]
fn anthropic_stream_survives_line_splits_across_chunks() {
    run_net_test(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // 按固定小粒度切分字节，确保多个 chunk 边界落在 SSE 行中间（模拟网络拆包）
            let bytes = MINIMAX_FULL_STREAM.as_bytes();
            let frames: Vec<Vec<u8>> = bytes.chunks(17).map(|c| c.to_vec()).collect();
            let (base, handle) = mock_server_raw(frames).await;
            let m = model_with_base(&base, "anthropic-messages");
            let result = anthropic::stream(&m, &[], "", &[], None, None, None, None, None, None)
                .await
                .unwrap();
            handle.await.unwrap();

            assert_eq!(result.message.text(), "Hi");
            assert_eq!(result.message.stop_reason.as_deref(), Some("stop"));
            let usage = result.usage.unwrap();
            assert_eq!(usage.input, 10);
            assert_eq!(usage.output, 5);
        });
    });
}

/// 复现 MiniMax-M3 现场：message_start 的 data 行被 TCP 拆包后事件不应丢失，错误信息里应含 `message_start=true`。
#[test]
fn anthropic_minimax_split_message_start_reports_true() {
    run_net_test(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
        // 复现用户现场：MiniMax-M3 只回 message_start + ping + message_stop，
        // 且 message_start 的 data 行恰好被 TCP 拆包，导致该事件在解析器里丢失。
        let body = r#"event: message_start

data: {"type":"message_start","message":{"id":"06e201b42548fb9ccde6a518c9abbed4","type":"message","role":"assistant","stop_reason":null,"stop_sequence":null,"model":"minimax-m3","content":[],"usage":{"input_tokens":0,"output_tokens":0}}}

event: ping

data: {"type":"ping"}

event: message_stop

data: {"type":"message_stop"}

"#;
        let bytes = body.as_bytes();
        // 第一个 chunk 恰好切断 message_start 的 data 行
        let split = body.find("06e201b4").unwrap();
        let frames = vec![bytes[..split].to_vec(), bytes[split..].to_vec()];
        let (base, handle) = mock_server_raw(frames).await;
        let m = model_with_base(&base, "anthropic-messages");
        let err = anthropic::stream(&m, &[], "", &[], None, None, None, None, None, None)
            .await
            .unwrap_err()
            .to_string();
        handle.await.unwrap();

        // message_start 事件必须被识别；缺少 stop reason 是端点行为，错误信息应如实反映
        assert!(
            err.contains("message_start=true"),
            "期望 message_start=true，实际错误：{}",
            err
        );
        assert!(err.contains("message_stop=true"), "实际错误：{}", err);
    });
    });
}

/// 流末尾不带换行 / 空行时，最后一个事件（message_stop）仍须被 flush，否则拿不到 stop_reason 与 usage。
#[test]
fn anthropic_stream_flushes_final_event_without_trailing_newline() {
    run_net_test(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // 流结束时不带尾随换行/空行：最后一个事件必须仍然被 flush
            let body = r#"event: message_start

data: {"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":7,"output_tokens":0}}}

event: message_delta

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}

event: message_stop

data: {"type":"message_stop"}"#;
            let (base, handle) = mock_server_raw(vec![body.as_bytes().to_vec()]).await;
            let m = model_with_base(&base, "anthropic-messages");
            let result = anthropic::stream(&m, &[], "", &[], None, None, None, None, None, None)
                .await
                .unwrap();
            handle.await.unwrap();

            assert_eq!(result.message.stop_reason.as_deref(), Some("stop"));
            let usage = result.usage.unwrap();
            assert_eq!(usage.input, 7);
            assert_eq!(usage.output, 3);
        });
    });
}

// =========================================================================
// partial 快照（translator 层：应用 stream_chat，非协议级 stream）
// =========================================================================

/// 通过公开 stream_chat 收集带 partial 快照的 StreamEvent，验证
/// translator 按 pi 语义增量构建 assistant 消息快照。
#[test]
fn stream_chat_events_carry_partial_snapshots() {
    run_net_test(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
        let events = vec![
            r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{"role":"assistant","content":"He"}}]}"#,
            r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{"content":"llo"}}]}"#,
            r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":"}}]}}]}"#,
            r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}"#,
            r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"[DONE]"#,
        ];
        let (base, handle) = mock_server(events, false).await;
        let m = model_with_base(&base, "openai-completions");

        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<StreamEvent>();
        let mut on_event = move |ev: StreamEvent| {
            let _ = ev_tx.send(ev);
        };

        let result = stream_chat(&m, &[], "", &[], None, None, None, Some(&mut on_event), None, None)
            .await;
        handle.await.unwrap();

        let events: Vec<StreamEvent> = ev_rx.try_iter().collect();

        // 对齐 pi：第一个事件必须是 Start（初始 partial 快照）
        assert!(matches!(events.first(), Some(StreamEvent::Start { partial }) if partial.role == "assistant" && partial.content.is_empty()));
        // Start 只发一次
        assert_eq!(events.iter().filter(|e| matches!(e, StreamEvent::Start { .. })).count(), 1);

        // TextStart：partial 已含空 text block
        let text_start = events.iter().find(|e| matches!(e, StreamEvent::TextStart { .. })).unwrap();
        if let StreamEvent::TextStart { content_index, partial } = text_start {
            assert_eq!(*content_index, 0);
            assert_eq!(partial.content.len(), 1);
            assert!(matches!(&partial.content[0], ContentBlock::Text { text, .. } if text.is_empty()));
        } else { unreachable!() }

        // TestDelta（"He"）：partial 文本增量
        let delta_he = events.iter().find(|e| matches!(e, StreamEvent::TextDelta { delta, .. } if delta == "He")).unwrap();
        if let StreamEvent::TextDelta { partial, .. } = delta_he {
            assert!(matches!(&partial.content[0], ContentBlock::Text { text, .. } if text == "He"));
        } else { unreachable!() }

        // ToolCallStart：partial.content 已有第二个 block（toolCall）
        let toolcall_start = events.iter().find(|e| matches!(e, StreamEvent::ToolCallStart { .. })).unwrap();
        if let StreamEvent::ToolCallStart { content_index, partial } = toolcall_start {
            assert_eq!(*content_index, 1);
            assert_eq!(partial.content.len(), 2);
        } else { unreachable!() }

        // ToolCallEnd：partial 完成（arguments 已解析）
        let toolcall_end = events.iter().find(|e| matches!(e, StreamEvent::ToolCallEnd { .. })).unwrap();
        if let StreamEvent::ToolCallEnd { tool_call, partial, .. } = toolcall_end {
            assert!(matches!(tool_call, ContentBlock::ToolCall { id, name, .. } if id == "call_1" && name == "read"));
            let blocks = &partial.content;
            assert_eq!(blocks.len(), 2);
            assert!(matches!(&blocks[1], ContentBlock::ToolCall { id, name, arguments, .. }
                if id == "call_1" && name == "read" && *arguments == serde_json::json!({"path": "a.rs"})));
        } else { unreachable!() }

        // 最终消息（stream_chat 返回值）
        assert_eq!(result.message.text(), "Hello");
        assert_eq!(result.message.stop_reason.as_deref(), Some("toolUse"));
    });
    });
}

/// 验证 thinking 与 text 交错时 partial 快照同时跟踪两者。
#[test]
fn stream_chat_partial_tracks_thinking_and_text() {
    run_net_test(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
        // deepseek 风格：thinking 少于 content？此处用 reasoning_content 流
        let events = vec![
            r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":"think"}}]}"#,
            r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{"reasoning_content":"ing..."}}]}"#,
            r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{"content":"Ans"}}]}"#,
            r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{"content":"wer"}}]}"#,
            r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            r#"[DONE]"#,
        ];
        let (base, handle) = mock_server(events, false).await;
        let m = model_with_base(&base, "openai-completions");

        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<StreamEvent>();
        let mut on_event = move |ev: StreamEvent| {
            let _ = ev_tx.send(ev);
        };

        let result = stream_chat(&m, &[], "", &[], None, None, None, Some(&mut on_event), None, None)
            .await;
        handle.await.unwrap();

        let events: Vec<StreamEvent> = ev_rx.try_iter().collect();

        // Thinking 与 Text 分开 content_index；partial.content 两个 block
        let last = events.last().unwrap();
        if let StreamEvent::TextEnd { partial, .. } = last {
            assert_eq!(partial.content.len(), 2);
            assert!(matches!(&partial.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "thinking..."));
            assert!(matches!(&partial.content[1], ContentBlock::Text { text, .. } if text == "Answer"));
        } else { unreachable!() }

        assert_eq!(result.message.thinking(), "thinking...");
        assert_eq!(result.message.text(), "Answer");
    });
    });
}

/// onPayload（对齐 pi StreamOptions.onPayload）：provider 发送前调用钩子，
/// 可检查/替换请求体；mock 侧断言收到改写后的 body。
#[test]
fn on_payload_hook_receives_and_rewrites_request() {
    run_net_test(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (body_tx, body_rx) = std::sync::mpsc::channel::<String>();
        let captured_ser = body_tx.clone();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            // 读到头部结束（header+body 可能同包到达；body 保留在 buf 里，不再读 socket）
            loop {
                let n = socket.read(&mut tmp).await.unwrap();
                if n == 0 { break; }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") { break; }
            }
            let text = String::from_utf8_lossy(&buf);
            let cl = text
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            // body 从已读 buf（头部之后）截取；不足再读 socket
            let head_end = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
            let mut body: Vec<u8> = buf[head_end..].to_vec();
            while body.len() < cl {
                let n = socket.read(&mut tmp).await.unwrap_or(0);
                if n == 0 { break; }
                body.extend_from_slice(&tmp[..n]);
            }
            let _ = captured_ser.send(String::from_utf8_lossy(&body[..cl.min(body.len())]).into_owned());
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                .await
                .ok();
            socket
                .write_all(br#"data: {"id":"c","choices":[{"index":0,"delta":{"role":"assistant","content":"ok"}}]}

"#)
                .await
                .ok();
            socket
                .write_all(br#"data: {"id":"c","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

"#)
                .await
                .ok();
            socket.write_all(b"data: [DONE]\n\n").await.ok();
            socket.shutdown().await.ok();
        });

        let m = model_with_base(&format!("http://127.0.0.1:{}", addr.port()), "openai-completions");
        let seen_raw: std::sync::Arc<std::sync::atomic::AtomicBool> =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = seen_raw.clone();
        let hook: PayloadHook = Arc::new(Mutex::new(Box::new(move |body: &mut Value| {
            seen.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(model) = body.get_mut("model") {
                *model = json!("rewritten-by-hook");
            }
        })));
        let result = stream_chat(&m, &[], "", &[], None, None, None, None, Some(hook), None).await;
        handle.await.unwrap();
        assert!(seen_raw.load(std::sync::atomic::Ordering::Relaxed), "onPayload 钩子应被调用");
        assert_eq!(result.message.stop_reason.as_deref(), Some("stop"));
        let body = body_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap_or_default();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["model"], "rewritten-by-hook", "provider 应发送改写后的请求体");
    });
    });
}

// =========================================================================
// EOF 残帧 / 截断摘要（对齐 pi 0.85.0 Codex SSE、0.84.3 截断摘要）
// =========================================================================

/// 流结束时不带尾随换行/空行：最后一个终端事件必须仍然被 flush。
/// 对齐 pi 0.85.0 `64eeb82a4`（Codex SSE 终端事件残帧）。
#[test]
fn responses_stream_flushes_final_event_without_trailing_newline() {
    run_net_test(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
        let body = r#"data: {"type":"response.created","response":{"id":"resp_1"}}

data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[],"status":"in_progress"}}

data: {"type":"response.output_text.delta","output_index":0,"delta":"ok"}

data: {"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"ok"}]}}

data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;
        let (base, handle) = mock_server_raw(vec![body.as_bytes().to_vec()]).await;
        let m = model_with_base(&base, "openai-responses");
        let result = responses::stream(&m, &[], "", &[], None, None, None, None, None, None)
            .await
            .unwrap();
        handle.await.unwrap();

        assert_eq!(result.message.text(), "ok");
        assert!(result.message.stop_reason.is_some());
        let usage = result.usage.unwrap();
        assert_eq!(usage.output, 1);
    });
    });
}

/// 摘要生成达到输出上限（finish_reason=length）时 simple_completion 必须拒绝，
/// 避免截断摘要被持久化（对齐 pi 0.84.3 `97fa14e39`）。
#[test]
fn simple_completion_rejects_truncated_summary() {
    run_net_test(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
        let events = vec![
            r#"{"id":"cmpl_1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"partial summary"}}]}"#,
            r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#,
            r#"[DONE]"#,
        ];
        let (base, handle) = mock_server(events, false).await;
        let m = model_with_base(&base, "openai-completions");
        let err = simple_completion(&m, "sys", "user prompt", Some(2048))
            .await
            .unwrap_err();
        handle.await.unwrap();

        assert!(
            err.to_string().contains("token cap"),
            "截断错误应说明 token 上限: {err}"
        );
    });
    });
}

/// 未截断的摘要正常返回（对照：finish_reason=stop 不触发拒绝）。
#[test]
fn simple_completion_accepts_complete_summary() {
    run_net_test(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
        let events = vec![
            r#"{"id":"cmpl_1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"full summary"}}]}"#,
            r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            r#"[DONE]"#,
        ];
        let (base, handle) = mock_server(events, false).await;
        let m = model_with_base(&base, "openai-completions");
        let (text, _usage) = simple_completion(&m, "sys", "user prompt", Some(2048))
            .await
            .unwrap();
        handle.await.unwrap();

        assert_eq!(text, "full summary");
    });
    });
}

// =========================================================================
// azure
// =========================================================================

/// Azure 相关的四个环境变量名。
const AZURE_ENV_NAMES: [&str; 4] = [
    "AZURE_OPENAI_BASE_URL",
    "AZURE_OPENAI_RESOURCE_NAME",
    "AZURE_OPENAI_API_VERSION",
    "AZURE_OPENAI_DEPLOYMENT_NAME_MAP",
];

/// 测试内的 Azure 环境变量暂存：清空后按需设置，drop 时还原原值并释放 env 锁。
#[must_use = "guard 必须存活到断言结束"]
struct AzureEnv {
    /// 持有的叶级 env 互斥锁，串行化同进程 env 读写直到 drop。
    _lock: std::sync::MutexGuard<'static, ()>,
    /// 清空前的环境变量原值，drop 时逐个还原；None 表示原本未设置。
    saved: Vec<(&'static str, Option<String>)>,
}

impl AzureEnv {
    /// 清空全部 Azure 环境变量并按 `pairs` 设置初始值。
    fn new(pairs: &[(&'static str, &str)]) -> Self {
        let lock = crate::test_support::env_key_lock();
        let saved: Vec<(&'static str, Option<String>)> = AZURE_ENV_NAMES
            .iter()
            .map(|n| (*n, std::env::var(n).ok()))
            .collect();
        let this = Self { _lock: lock, saved };
        this.set(pairs);
        this
    }

    /// 在已持有锁的前提下重新设置这组变量（每个用例的假服务器端口不同）。
    fn set(&self, pairs: &[(&'static str, &str)]) {
        for name in AZURE_ENV_NAMES {
            unsafe { std::env::remove_var(name) };
        }
        for (key, value) in pairs {
            unsafe { std::env::set_var(key, value) };
        }
    }
}

impl Drop for AzureEnv {
    /// 逐个还原保存的环境变量（原本未设置的重新移除），并释放持有的 env 锁。
    fn drop(&mut self) {
        for (name, value) in self.saved.drain(..) {
            match value {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }
}

/// 启动假 SSE 服务器并保留**完整请求文本**（含 body），供 Azure 用例断言 URL 与请求体。
async fn mock_server_capturing(
    frames: Vec<&'static str>,
) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let mut header_end = None;
        loop {
            let n = socket.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if header_end.is_none() {
                header_end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
            }
            if let Some(he) = header_end {
                let head = String::from_utf8_lossy(&buf[..he]).to_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if buf.len() >= he + len {
                    break;
                }
            }
        }
        let request = String::from_utf8_lossy(&buf).to_string();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        for f in frames {
            socket
                .write_all(format!("data: {f}\n\n").as_bytes())
                .await
                .unwrap();
        }
        socket.shutdown().await.unwrap();
        request
    });
    (format!("http://127.0.0.1:{}", addr.port()), handle)
}

/// Azure provider：进入协议分发前按环境变量解析 `baseUrl`（目录里为空），
/// 请求体的 `model` 用部署名映射；responses 与 completions 两个协议共用同一套解析。
#[test]
fn azure_resolves_endpoint_and_deployment_for_both_apis() {
    run_net_test(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let _ad = crate::test_support::AgentDirGuard::temp();
            let env = AzureEnv::new(&[]);

            // azure-openai-responses：URL 来自 AZURE_OPENAI_BASE_URL，body.model 用部署名
            let responses_events = vec![
                r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1"}}"#,
                r#"{"type":"response.output_text.delta","output_index":0,"delta":"ok"}"#,
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"ok"}]}}"#,
                r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":5,"output_tokens":1,"total_tokens":6}}}"#,
            ];
            let (base, server) = mock_server_capturing(responses_events).await;
            env.set(&[
                ("AZURE_OPENAI_BASE_URL", base.as_str()),
                ("AZURE_OPENAI_DEPLOYMENT_NAME_MAP", "gpt-5.4=dep-1"),
            ]);

            let mut m = model_with_base("", "azure-openai-responses");
            m.provider = "azure".into();
            m.model_id = "gpt-5.4".into();
            let result = stream_chat(&m, &[], "", &[], None, None, None, None, None, None).await;
            assert_eq!(result.message.stop_reason.as_deref(), Some("stop"));
            let request = server.await.unwrap();
            assert!(request.starts_with("POST /responses "), "{request}");
            let body: serde_json::Value =
                serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            assert_eq!(body["model"], "dep-1");

            // openai-completions：同一套 baseUrl 解析；未命中映射的目录 id 原样作为 model
            let completions_events = vec![
                r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{"content":"ok"}}]}"#,
                r#"{"id":"cmpl_1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                r#"[DONE]"#,
            ];
            let (base2, server2) = mock_server_capturing(completions_events).await;
            env.set(&[
                ("AZURE_OPENAI_BASE_URL", base2.as_str()),
                ("AZURE_OPENAI_DEPLOYMENT_NAME_MAP", "gpt-5.4=dep-1"),
            ]);

            let mut m2 = model_with_base("", "openai-completions");
            m2.provider = "azure".into();
            m2.model_id = "deepseek-v4-pro".into();
            let result = stream_chat(&m2, &[], "", &[], None, None, None, None, None, None).await;
            assert_eq!(result.message.stop_reason.as_deref(), Some("stop"));
            let request = server2.await.unwrap();
            assert!(request.starts_with("POST /chat/completions "), "{request}");
            let body: serde_json::Value =
                serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            assert_eq!(body["model"], "deepseek-v4-pro");
        });
    });
}
