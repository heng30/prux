//! 内部流事件 → OpenAI SSE chunk 的翻译（以及非流式聚合）。
//!
//! 帧形状与官方一致：每帧 `data: {json}\n\n`，末帧 `data: [DONE]\n\n`。
//!
//! 两处有意为之的取舍：
//! - **思考内容**走 `delta.reasoning_content`（DeepSeek 生态事实标准），
//!   思考帧不带 `content` 键，避免客户端把空串插进正文流；
//! - **工具调用整块发出**：内部事件直到 `ToolCallEnd` 才揭示 `id`/`name`
//!   （`ToolCallStart`/`ToolCallDelta` 的 partial 里两者还是空串），而 OpenAI 的 SSE 契约
//!   要求 `id`/`type`/`function.name` 出现在该 `index` 的首个 delta 里，故参数增量先缓冲、
//!   在 `ToolCallEnd` 一次性发出（`arguments` 允许是完整 JSON 字符串，协议上完全合法）。

use super::stats;
use crate::{
    core::provider::{AgentMessage, ContentBlock, StreamEvent, StreamResult, Usage},
    extensions::util::make_id,
    utils::time::now_ms,
};
use serde_json::{Value, json};
use std::collections::HashMap;

/// SSE 结束帧
pub const DONE_FRAME: &str = "data: [DONE]\n\n";

/// 把 chunk 编成 SSE 帧
pub fn frame(chunk: &Value) -> String {
    format!("data: {chunk}\n\n")
}

/// 归一化 stop reason → OpenAI `finish_reason`
pub fn finish_reason(stop: Option<&str>) -> Option<&'static str> {
    match stop {
        Some("stop") => Some("stop"),
        Some("length") => Some("length"),
        Some("toolUse") | Some("deferred") => Some("tool_calls"),
        _ => None,
    }
}

/// usage → OpenAI `usage` 对象；`None`（上游未回传）按 0 计（客户端普遍假定该字段存在）
pub fn usage_json(usage: Option<&Usage>) -> Value {
    let (prompt, completion, total, cached, reasoning) = match usage {
        Some(u) => (
            u.input,
            u.output,
            u.total_tokens,
            u.cache_read,
            u.reasoning.unwrap_or(0),
        ),
        None => (0, 0, 0, 0, 0),
    };

    let total = if total > 0 {
        total
    } else {
        prompt + completion
    };

    let mut out = json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": total,
        "prompt_tokens_details": { "cached_tokens": cached },
    });

    if reasoning > 0 {
        out["completion_tokens_details"] = json!({ "reasoning_tokens": reasoning });
    }
    out
}

/// 流式 chunk 生成器（每请求一个实例）
pub struct ChunkWriter {
    /// completion id（`chatcmpl-<随机>`），同一请求的帧共享
    id: String,
    /// 响应里回填的 model id
    model: String,
    /// `created` 字段（unix 秒）
    created: u64,
    /// 内部 `content_index` → OpenAI `tool_calls[].index`（按出现顺序递增）
    tool_index: HashMap<usize, usize>,
    /// 下一个待分配的 tool_calls `index`
    next_tool: usize,
}

impl ChunkWriter {
    /// 新建一个请求的 chunk 生成器：随机 completion id + 当前时间戳，
    /// `model` 会原样回填到每个帧的 `model` 字段。
    pub fn new(model: &str) -> Self {
        Self {
            id: new_completion_id(),
            model: model.to_string(),
            created: created_now(),
            tool_index: HashMap::new(),
            next_tool: 0,
        }
    }

    /// 组装一个 `chat.completion.chunk` 帧（单一 choice，index 0）。
    /// `delta` 是该帧增量；`finish` 为 `None` 时 `finish_reason` 输出 null。
    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "logprobs": null,
                "finish_reason": finish,
            }],
        })
    }

    /// 首帧：`delta: {"role": "assistant", "content": ""}`（官方即如此开头）
    pub fn opening_chunk(&self) -> Value {
        self.chunk(json!({"role": "assistant", "content": ""}), None)
    }

    /// 增量事件 → chunk；返回 `None` 表示该事件不产生帧
    pub fn on_event(&mut self, event: &StreamEvent) -> Option<Value> {
        match event {
            StreamEvent::ThinkingDelta { delta, .. } if !delta.is_empty() => {
                Some(self.chunk(json!({"reasoning_content": delta}), None))
            }
            StreamEvent::TextDelta { delta, .. } if !delta.is_empty() => {
                Some(self.chunk(json!({"content": delta}), None))
            }
            // 参数增量先缓冲（见模块头注释），到 ToolCallEnd 一次性发出
            StreamEvent::ToolCallEnd {
                content_index,
                tool_call,
                ..
            } => {
                let ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
                } = tool_call
                else {
                    return None;
                };

                let index = *self.tool_index.entry(*content_index).or_insert_with(|| {
                    let i = self.next_tool;
                    self.next_tool += 1;
                    i
                });

                let arguments = serde_json::to_string(arguments).unwrap_or_else(|_| "{}".into());

                Some(self.chunk(
                    json!({"tool_calls": [{
                        "index": index,
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments},
                    }]}),
                    None,
                ))
            }
            _ => None,
        }
    }

    /// 收尾帧（带 `finish_reason`）
    pub fn finish_chunk(&self, reason: &str) -> Value {
        self.chunk(json!({}), Some(reason))
    }

    /// usage 帧（`stream_options.include_usage` 时作为倒数第二帧：`choices` 为空数组）
    pub fn usage_chunk(&self, usage: Option<&Usage>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [],
            "usage": usage_json(usage),
        })
    }

    /// 已发出 chunk 之后的上游失败：OpenAI 没有标准错误帧，用 `{"error": ...}` 帧收尾。
    pub fn error_chunk(&self, message: &str) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "error": { "message": message, "type": "api_error" },
        })
    }
}

/// 非流式响应体（内部仍是流式调用，此处聚合）
pub fn completion_body(id: &str, model: &str, created: u64, result: &StreamResult) -> Value {
    let msg = &result.message;
    let text = join_blocks(msg, |b| match b {
        ContentBlock::Text { text, .. } => Some(text.as_str()),
        _ => None,
    });

    let reasoning = join_blocks(msg, |b| match b {
        ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
        _ => None,
    });

    let tool_calls: Vec<Value> = msg
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => Some(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(arguments).unwrap_or_else(|_| "{}".into()),
                }
            })),
            _ => None,
        })
        .collect();

    let mut message = json!({
        "role": "assistant",
        "content": if text.is_empty() { Value::Null } else { json!(text) },
        "refusal": null,
    });

    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }

    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }

    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "logprobs": null,
            "finish_reason": finish_reason(msg.stop_reason.as_deref()).unwrap_or("stop"),
        }],
        "usage": usage_json(result.usage.as_ref()),
    })
}

/// 按顺序拼接 `msg.content` 里被 `pick` 选中的文本片段；`pick` 返回 `None` 的块跳过。
fn join_blocks(msg: &AgentMessage, pick: impl Fn(&ContentBlock) -> Option<&str>) -> String {
    let mut out = String::new();
    for b in &msg.content {
        if let Some(s) = pick(b) {
            out.push_str(s);
        }
    }
    out
}

/// `/v1/models`（及 `/v1/models/{id}`）的条目
pub fn model_object(id: &str, created: u64, provider: &str) -> Value {
    json!({
        "id": id,
        "object": "model",
        "created": created,
        "owned_by": provider,
    })
}

/// 新的 completion id（`chatcmpl-<随机>`）
pub fn new_completion_id() -> String {
    format!("chatcmpl-{}", make_id())
}

/// `created` 字段：unix 秒
pub fn created_now() -> u64 {
    now_ms() / 1000
}

/// 记录一条请求日志并累加 token（流式与非流式共用）
pub fn log_request(model: &str, started_ms: u64, status: u16, usage: Option<&Usage>) {
    if let Some(u) = usage {
        stats::add_tokens(u.input, u.output);
    }

    let now = now_ms();
    stats::push_log(stats::LogEntry {
        at_ms: now,
        model: model.to_string(),
        ms: now.saturating_sub(started_ms),
        status,
        tokens_out: usage.map(|u| u.output).unwrap_or(0),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::AgentMessage;

    fn text_event(delta: &str) -> StreamEvent {
        StreamEvent::TextDelta {
            content_index: 0,
            delta: delta.to_string(),
            partial: AgentMessage::user_text(""),
        }
    }

    fn thinking_event(delta: &str) -> StreamEvent {
        StreamEvent::ThinkingDelta {
            content_index: 0,
            delta: delta.to_string(),
            partial: AgentMessage::user_text(""),
        }
    }

    fn tool_end(content_index: usize, id: &str, name: &str, args: Value) -> StreamEvent {
        StreamEvent::ToolCallEnd {
            content_index,
            tool_call: ContentBlock::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: args,
                thought_signature: None,
                namespace: None,
            },
            partial: AgentMessage::user_text(""),
        }
    }

    #[test]
    fn opening_chunk_carries_role_and_empty_content() {
        let w = ChunkWriter::new("gpt-5");
        let c = w.opening_chunk();
        assert_eq!(c["object"], "chat.completion.chunk");
        assert_eq!(c["model"], "gpt-5");
        assert_eq!(c["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(c["choices"][0]["delta"]["content"], "");
        assert!(c["choices"][0]["finish_reason"].is_null());
        assert!(c["id"].as_str().unwrap().starts_with("chatcmpl-"));
    }

    #[test]
    fn text_and_thinking_deltas_map_to_expected_fields() {
        let mut w = ChunkWriter::new("m");
        let t = w.on_event(&thinking_event("hmm")).unwrap();
        assert_eq!(t["choices"][0]["delta"]["reasoning_content"], "hmm");
        assert!(
            t["choices"][0]["delta"].get("content").is_none(),
            "思考帧不带 content 键"
        );

        let c = w.on_event(&text_event("hello")).unwrap();
        assert_eq!(c["choices"][0]["delta"]["content"], "hello");
        assert!(c["choices"][0]["delta"].get("reasoning_content").is_none());
    }

    #[test]
    fn empty_deltas_produce_no_frame() {
        let mut w = ChunkWriter::new("m");
        assert!(w.on_event(&text_event("")).is_none());
        assert!(
            w.on_event(&StreamEvent::Start {
                partial: AgentMessage::user_text("")
            })
            .is_none()
        );
    }

    #[test]
    fn tool_calls_are_emitted_as_one_complete_delta_with_increasing_index() {
        let mut w = ChunkWriter::new("m");
        // 参数增量不单独发帧
        assert!(
            w.on_event(&StreamEvent::ToolCallDelta {
                content_index: 0,
                delta: "{\"a\"".into(),
                partial: AgentMessage::user_text(""),
            })
            .is_none()
        );

        let first = w
            .on_event(&tool_end(0, "call_1", "read", json!({"path": "a"})))
            .unwrap();
        let tc = &first["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["index"], 0);
        assert_eq!(tc["id"], "call_1");
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "read");
        assert_eq!(tc["function"]["arguments"], "{\"path\":\"a\"}");

        let second = w
            .on_event(&tool_end(1, "call_2", "write", json!({"path": "b"})))
            .unwrap();
        assert_eq!(second["choices"][0]["delta"]["tool_calls"][0]["index"], 1);
    }

    #[test]
    fn finish_and_usage_chunks_match_openai_shape() {
        let w = ChunkWriter::new("m");
        let f = w.finish_chunk("tool_calls");
        assert_eq!(f["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(f["choices"][0]["delta"], json!({}));

        let u = w.usage_chunk(Some(&Usage {
            input: 10,
            output: 20,
            cache_read: 3,
            total_tokens: 30,
            reasoning: Some(5),
            ..Default::default()
        }));
        assert_eq!(u["choices"], json!([]));
        assert_eq!(u["usage"]["prompt_tokens"], 10);
        assert_eq!(u["usage"]["completion_tokens"], 20);
        assert_eq!(u["usage"]["total_tokens"], 30);
        assert_eq!(u["usage"]["prompt_tokens_details"]["cached_tokens"], 3);
        assert_eq!(
            u["usage"]["completion_tokens_details"]["reasoning_tokens"],
            5
        );
    }

    #[test]
    fn usage_json_falls_back_to_zero_and_sums_total() {
        assert_eq!(usage_json(None)["total_tokens"], 0);
        let u = Usage {
            input: 4,
            output: 6,
            total_tokens: 0,
            ..Default::default()
        };
        assert_eq!(usage_json(Some(&u))["total_tokens"], 10);
    }

    #[test]
    fn finish_reason_maps_internal_stop_reasons() {
        assert_eq!(finish_reason(Some("stop")), Some("stop"));
        assert_eq!(finish_reason(Some("length")), Some("length"));
        assert_eq!(finish_reason(Some("toolUse")), Some("tool_calls"));
        assert_eq!(finish_reason(Some("deferred")), Some("tool_calls"));
        assert_eq!(finish_reason(Some("error")), None);
        assert_eq!(finish_reason(Some("aborted")), None);
    }

    #[test]
    fn completion_body_aggregates_text_reasoning_and_tools() {
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".into();
        msg.content = vec![
            ContentBlock::Thinking {
                thinking: "why".into(),
                thinking_signature: None,
                redacted: None,
            },
            ContentBlock::Text {
                text: "answer".into(),
                text_signature: None,
            },
            ContentBlock::ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: json!({"path": "a"}),
                thought_signature: None,
                namespace: None,
            },
        ];
        msg.stop_reason = Some("toolUse".into());

        let result = StreamResult {
            message: msg,
            usage: Some(Usage {
                input: 1,
                output: 2,
                total_tokens: 3,
                ..Default::default()
            }),
            error_message: None,
        };
        let b = completion_body("chatcmpl-x", "m", 7, &result);
        assert_eq!(b["object"], "chat.completion");
        assert_eq!(b["choices"][0]["message"]["content"], "answer");
        assert_eq!(b["choices"][0]["message"]["reasoning_content"], "why");
        assert_eq!(
            b["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a\"}"
        );
        assert_eq!(b["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(b["usage"]["total_tokens"], 3);
    }

    #[test]
    fn completion_body_uses_null_content_when_text_is_empty() {
        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".into();
        msg.content = vec![];
        msg.stop_reason = Some("stop".into());
        let result = StreamResult {
            message: msg,
            usage: None,
            error_message: None,
        };
        let b = completion_body("id", "m", 0, &result);
        assert!(b["choices"][0]["message"]["content"].is_null());
        assert_eq!(b["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn frames_are_sse_data_lines() {
        let f = frame(&json!({"a": 1}));
        assert_eq!(f, "data: {\"a\":1}\n\n");
        assert_eq!(DONE_FRAME, "data: [DONE]\n\n");
    }

    #[test]
    fn model_object_reports_provider_owner() {
        let m = model_object("gpt-5", 1, "openai");
        assert_eq!(m["object"], "model");
        assert_eq!(m["owned_by"], "openai");
    }
}
