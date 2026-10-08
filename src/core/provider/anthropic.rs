//! Anthropic Messages 协议（claude 等，含 opencode 网关 /zen/v1/messages）。
//!
//! 关键差异点：
//! - 认证用 `x-api-key`（`sk-ant-oat` 前缀才转 Bearer）+ `anthropic-version` + beta 头
//! - thinking：adaptive 模型走 `{type:"adaptive"}` + effort；老模型走 budget（默认 1024）；
//!   thinking 块重放必须携带 signature，无 signature 降级为文本
//! - `signature_delta` 是追加不是覆盖；`input_json_delta` 累计解析
//! - 连续 toolResult 合并为一条 user 消息（z.ai 兼容端点要求）
//! - cache_control 挂在 system、最后一个 tool、最后一个 user 消息上

use super::{
    AgentMessage, Citation, ContentBlock, Cost, ModelConfig, PartialTranslator, RawStreamEvent,
    StreamResult, Usage,
    convert::{
        BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX, COMPACTION_SUMMARY_PREFIX,
        COMPACTION_SUMMARY_SUFFIX, sanitize_surrogates, truncate_for_error,
    },
    provider_session_headers,
    retry::{DEFAULT_MAX_RETRIES, send_with_retry},
    session_affinity_headers, track_stream_bytes,
    usage::{compute_cost, parse_streaming_json},
};
use crate::{
    core::{
        self, model_resolver,
        provider::{PayloadHook, apply_payload_hook},
    },
    error::{Error, Result},
    utils::{http, time::now_ms},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Anthropic API 版本头 `anthropic-version` 的值。
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// 启用交错思考（interleaved thinking）的 beta 头值。
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
/// 启用细粒度工具流式输出的 beta 头值。
const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
/// 服务端 fallback（目录 `compat.allowedFallbackModels` 非空时启用）
const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// 被服务端打码的 thinking 块的占位文本，重放时按此内容识别 redacted 块。
const REDACTED_THINKING_TEXT: &str = "[Reasoning redacted]";
/// 未显式指定时每级 thinking 的默认 token 预算。
const DEFAULT_THINKING_BUDGET: u32 = 1024;

/// OAuth 订阅登录时模拟的 Claude Code 版本
const CLAUDE_CODE_VERSION: &str = "2.1.280";

/// 连续的 toolResult 消息可能带 signature 的 thinking 要做 redacted 判定，重放时按文本内容识别
fn is_redacted_thinking(block: &ContentBlock) -> bool {
    matches!(block, ContentBlock::Thinking { thinking, thinking_signature: Some(_), .. }
        if thinking == REDACTED_THINKING_TEXT)
}

/// Messages API 端点：`{base_url}/v1/messages`（忽略 base_url 末尾的 '/'）。
fn endpoint(model: &ModelConfig) -> String {
    format!("{}/v1/messages", model.base_url.trim_end_matches('/'))
}

/// 非法字符替换 + 截断 64 字节
fn normalize_tool_call_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    sanitized.chars().take(64).collect()
}

/// 模型是否启用 adaptive thinking
// ModelConfig 目前不透传 compat；models-store 的 forceAdaptiveThinking 通过
// thinking_format 简化映射不可靠，这里直接读模型目录原始条目。
fn compat_force_adaptive_thinking(model: &ModelConfig) -> bool {
    model_resolver::provider_models(&model.provider)
        .iter()
        .find(|m| m.get("id").and_then(|v| v.as_str()) == Some(model.model_id.as_str()))
        .and_then(|m| m.get("compat").and_then(|c| c.get("forceAdaptiveThinking")))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 模型是否接受无 signature 的 thinking 块重放（Fireworks / Vercel 等兼容端点）。
/// 开启时保留 thinking 块并发送空 signature；关闭时降级为文本。
fn compat_allow_empty_signature(model: &ModelConfig) -> bool {
    model_resolver::provider_models(&model.provider)
        .iter()
        .find(|m| m.get("id").and_then(|v| v.as_str()) == Some(model.model_id.as_str()))
        .and_then(|m| m.get("compat").and_then(|c| c.get("allowEmptySignature")))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 模型目录 `compat.supportsTemperature`，缺失时默认允许（true）。
fn compat_supports_temperature(model: &ModelConfig) -> bool {
    model_resolver::provider_models(&model.provider)
        .iter()
        .find(|m| m.get("id").and_then(|v| v.as_str()) == Some(model.model_id.as_str()))
        .and_then(|m| m.get("compat").and_then(|c| c.get("supportsTemperature")))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// 模型目录 `compat.supportsEagerToolInputStreaming`，缺失时默认 true；
/// 为 false 时需改用 fine-grained tool streaming beta 头。
fn compat_supports_eager_input_streaming(model: &ModelConfig) -> bool {
    model_resolver::provider_models(&model.provider)
        .iter()
        .find(|m| m.get("id").and_then(|v| v.as_str()) == Some(model.model_id.as_str()))
        .and_then(|m| {
            m.get("compat")
                .and_then(|c| c.get("supportsEagerToolInputStreaming"))
        })
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// 切换 prompt cache control 时长；
/// 默认 ephemeral；long → 1h（cache_control hours 参数，Anthropic 长期缓存语义）
fn cache_control() -> Value {
    let long = std::env::var("PRUX_CACHE_RETENTION")
        .map(|v| v == "long")
        .unwrap_or(false);
    if long {
        json!({ "type": "ephemeral", "ttl": "1h" })
    } else {
        json!({ "type": "ephemeral" })
    }
}

/// thinkingLevelMap 命中直取，否则按级别归类
fn map_thinking_level_to_effort(model: &ModelConfig, level: &str) -> String {
    if let Some(mapped) = model
        .thinking_level_map
        .as_ref()
        .and_then(|m| m.get(level))
        .and_then(|v| v.as_deref())
    {
        return mapped.to_string();
    }
    match level {
        "minimal" | "low" => "low",
        "medium" => "medium",
        _ => "high",
    }
    .to_string()
}

// 消息转换
/// 转为 anthropic wire 格式的内容块。
/// 无图片时压成单个文本串（多个文本块以换行连接）；有图片时输出 text/image 块数组，
/// 若数组里没有文本块则在开头补一句占位说明（anthropic 不接受纯图片内容）。
fn convert_content_blocks(content: &[ContentBlock]) -> Value {
    let has_images = content
        .iter()
        .any(|b| matches!(b, ContentBlock::Image { .. }));
    if !has_images {
        let text: String = content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return json!(sanitize_surrogates(&text));
    }
    let mut blocks: Vec<Value> = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text { text, .. } => blocks.push(json!({
                "type": "text",
                "text": sanitize_surrogates(text)
            })),
            ContentBlock::Image { data, mime_type } => blocks.push(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": mime_type,
                    "data": data
                }
            })),
            _ => {}
        }
    }
    let has_text = blocks.iter().any(|b| b["type"] == "text");
    if !has_text {
        blocks.insert(0, json!({ "type": "text", "text": "(see attached image)" }));
    }
    Value::Array(blocks)
}

/// 生成 anthropic wire 消息数组（不含 system）。
/// 公共规则：synthetic tool results、跳过 error/aborted、tool call id 归一化配对。
fn insert_synthetic_tool_results(
    params: &mut Vec<Value>,
    pending: &mut Vec<(String, String)>,
    existing: &mut HashSet<String>,
) {
    if pending.is_empty() {
        return;
    }
    for (id, _name) in pending.drain(..) {
        if existing.contains(&id) {
            continue;
        }
        params.push(json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": id,
                "content": "No result provided",
                "is_error": true
            }]
        }));
    }
    existing.clear();
}

/// 把一条 user 消息转为 anthropic 块数组（不含 system）。
/// 模型 input 不含 image 时，图片被替换为占位文本（连续图片只保留一条）；
/// 空白文本被丢弃，最终无任何块时返回 `None`（该消息整体跳过）。
fn convert_anthropic_user_message(msg: &AgentMessage, model: &ModelConfig) -> Option<Value> {
    let supports_images = model.input.iter().any(|x| x == "image");
    let mut blocks: Vec<Value> = Vec::new();
    let mut previous_was_placeholder = false;

    for block in &msg.content {
        match block {
            ContentBlock::Text { text, .. } => {
                if text.trim().is_empty() {
                    continue;
                }
                blocks.push(json!({
                    "type": "text",
                    "text": sanitize_surrogates(text)
                }));
                previous_was_placeholder = text == "(image omitted: model does not support images)";
            }
            ContentBlock::Image { data, mime_type } => {
                if supports_images {
                    blocks.push(json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": mime_type,
                            "data": data
                        }
                    }));
                    previous_was_placeholder = false;
                } else if !previous_was_placeholder {
                    blocks.push(json!({
                        "type": "text",
                        "text": "(image omitted: model does not support images)"
                    }));
                    previous_was_placeholder = true;
                }
            }
            _ => {}
        }
    }
    if blocks.is_empty() {
        return None;
    }
    Some(json!({ "role": "user", "content": blocks }))
}

/// 转换 assistant 消息；tool_use 块做 id 归一化并记入 pending,
/// 供后续消息插入 synthetic tool results 使用
fn convert_anthropic_assistant_message(
    msg: &AgentMessage,
    model: &ModelConfig,
    tool_call_id_map: &mut HashMap<String, String>,
    pending_tool_calls: &mut Vec<(String, String)>,
) -> Option<Value> {
    if msg.stop_reason.as_deref() == Some("error") || msg.stop_reason.as_deref() == Some("aborted")
    {
        return None;
    }

    let is_same_model = msg.model.as_deref() == Some(model.model_id.as_str())
        && msg.provider.as_deref() == Some(model.provider.as_str())
        && msg.api.as_deref() == Some(model.api.as_str());

    let mut blocks: Vec<Value> = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text { text, .. } => {
                if text.trim().is_empty() {
                    continue;
                }
                blocks.push(json!({
                    "type": "text",
                    "text": sanitize_surrogates(text)
                }));
            }
            ContentBlock::Thinking {
                thinking,
                thinking_signature,
                ..
            } => {
                if is_redacted_thinking(block) {
                    // 同模型才保留 opaque 的 redacted payload
                    if is_same_model && let Some(sig) = thinking_signature {
                        blocks.push(json!({
                            "type": "redacted_thinking",
                            "data": sig
                        }));
                    }
                    continue;
                }
                let has_signature = thinking_signature
                    .as_deref()
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false);
                if thinking.trim().is_empty() && !has_signature {
                    continue;
                }
                if !has_signature {
                    if compat_allow_empty_signature(model) {
                        // 兼容端点（Fireworks/Vercel）发出并接受空 signature：保留 thinking 块
                        blocks.push(json!({
                            "type": "thinking",
                            "thinking": sanitize_surrogates(thinking),
                            "signature": ""
                        }));
                    } else {
                        // 无 signature（跨模型/中断流）：降级为普通文本
                        blocks.push(json!({
                            "type": "text",
                            "text": sanitize_surrogates(thinking)
                        }));
                    }
                } else {
                    blocks.push(json!({
                        "type": "thinking",
                        "thinking": sanitize_surrogates(thinking),
                        "signature": thinking_signature.as_deref().unwrap_or("")
                    }));
                }
            }
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                let normalized_id = tool_call_id_map
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| normalize_tool_call_id(id));
                tool_call_id_map.insert(id.clone(), normalized_id.clone());
                blocks.push(json!({
                    "type": "tool_use",
                    "id": normalized_id,
                    "name": name,
                    "input": arguments.clone()
                }));
                pending_tool_calls.push((normalized_id, name.clone()));
            }
            _ => {}
        }
    }
    if blocks.is_empty() {
        return None;
    }
    Some(json!({ "role": "assistant", "content": blocks }))
}

/// 把压缩/分支摘要消息包上对应的前后缀，转为一条 user 文本消息。
fn convert_anthropic_summary_message(msg: &AgentMessage) -> Value {
    let (prefix, suffix) = if msg.role == "compactionSummary" {
        (COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX)
    } else {
        (BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX)
    };
    let text = format!("{}{}{}", prefix, msg.text(), suffix);
    json!({
        "role": "user",
        "content": sanitize_surrogates(&text)
    })
}

/// 推送 tool_result 到 user 消息；连续 toolResult 与上一条单块 tool_result
/// 合并为同一条 user 消息（z.ai 兼容端点要求）。
fn push_anthropic_tool_result(
    params: &mut Vec<Value>,
    msg: &AgentMessage,
    tool_call_id_map: &mut HashMap<String, String>,
    existing_tool_result_ids: &mut HashSet<String>,
) {
    // 归一化 tool_call_id（同步 assistant 端的归一化）
    let tool_use_id = msg
        .tool_call_id
        .as_deref()
        .map(|id| {
            tool_call_id_map
                .get(id)
                .cloned()
                .unwrap_or_else(|| normalize_tool_call_id(id))
        })
        .unwrap_or_default();
    existing_tool_result_ids.insert(tool_use_id.clone());
    let converted = convert_content_blocks(&msg.content);
    let tool_result = json!({
        "type": "tool_result",
        "tool_use_id": tool_use_id,
        "content": converted,
        "is_error": msg.is_error
    });

    if let Some(last) = params.last_mut()
        && last.get("role") == Some(&json!("user"))
        && last["content"]
            .as_array()
            .map(|a| a.len() == 1)
            .unwrap_or(false)
        && last["content"][0].get("type") == Some(&json!("tool_result"))
        && let Some(arr) = last["content"].as_array_mut()
    {
        arr.push(tool_result);
        return;
    }
    params.push(json!({ "role": "user", "content": [tool_result] }));
}

/// cache_control 挂在最后一个 user 消息的最后块上。
fn apply_anthropic_cache_control(params: &mut [Value]) {
    if let Some(last) = params.last_mut()
        && last.get("role") == Some(&json!("user"))
        && let Some(content) = last.get_mut("content")
    {
        let attach = |block: &mut Value| {
            if block.get("type").is_some() {
                block["cache_control"] = cache_control();
            }
        };

        if content.is_array() {
            if let Some(arr) = content.as_array_mut()
                && let Some(last_block) = arr.last_mut()
            {
                attach(last_block);
            }
        } else {
            // 字符串内容 → 转单块数组
            if let Some(s) = content.as_str() {
                *content = json!([{
                    "type": "text",
                    "text": s,
                    "cache_control": cache_control()
                }]);
            }
        }
    }
}

/// 生成 anthropic wire 消息数组（不含 system）。
/// 公共规则：synthetic tool results、跳过 error/aborted、tool call id 归一化配对。
pub(crate) fn convert_anthropic_messages(
    messages: &[AgentMessage],
    _system_prompt: &str,
    model: &ModelConfig,
) -> Vec<Value> {
    // tool call id 归一化映射：原 id → 归一化 id
    let mut tool_call_id_map: HashMap<String, String> = HashMap::new();
    let mut pending_tool_calls: Vec<(String, String)> = Vec::new(); // (归一化 id, name)
    let mut existing_tool_result_ids: HashSet<String> = HashSet::new();
    let mut params: Vec<Value> = Vec::new();

    for msg in messages {
        match msg.role.as_str() {
            "user" => {
                insert_synthetic_tool_results(
                    &mut params,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                );
                if let Some(user_msg) = convert_anthropic_user_message(msg, model) {
                    params.push(user_msg);
                }
            }
            "assistant" => {
                insert_synthetic_tool_results(
                    &mut params,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                );
                if let Some(assistant_msg) = convert_anthropic_assistant_message(
                    msg,
                    model,
                    &mut tool_call_id_map,
                    &mut pending_tool_calls,
                ) {
                    params.push(assistant_msg);
                }
            }
            "compactionSummary" | "branchSummary" => {
                insert_synthetic_tool_results(
                    &mut params,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                );
                params.push(convert_anthropic_summary_message(msg));
            }
            "toolResult" => {
                push_anthropic_tool_result(
                    &mut params,
                    msg,
                    &mut tool_call_id_map,
                    &mut existing_tool_result_ids,
                );
            }
            _ => {}
        }
    }

    insert_synthetic_tool_results(
        &mut params,
        &mut pending_tool_calls,
        &mut existing_tool_result_ids,
    );

    apply_anthropic_cache_control(&mut params);

    params
}

/// 把 `(name, description, parameters)` 工具定义转为 anthropic tools 数组：
/// `input_schema` 只取原 schema 的 properties/required，`eager_input_streaming` 逐工具透传，
/// 并在最后一个工具上挂 `cache_control`（提示缓存断点）。
fn convert_anthropic_tools(
    tool_defs: &[(String, String, Value)],
    eager_input_streaming: bool,
) -> Vec<Value> {
    let mut tools: Vec<Value> = tool_defs
        .iter()
        .map(|(name, description, parameters)| {
            let properties = parameters.get("properties").cloned().unwrap_or(json!({}));
            let required = parameters.get("required").cloned().unwrap_or(json!([]));
            json!({
                "name": name,
                "description": description,
                "eager_input_streaming": eager_input_streaming,
                "input_schema": {
                    "type": "object",
                    "properties": properties,
                    "required": required
                }
            })
        })
        .collect();

    if let Some(last) = tools.last_mut() {
        last["cache_control"] = cache_control();
    }
    tools
}

/// Anthropic SSE 解析：按行累积，空行 flush 成 (event, data)
struct AnthropicSse {
    /// 跨 chunk 保留的未完整行字节，等下次 push 补齐换行后再切分。
    buf: Vec<u8>,
    /// 当前事件的 `event:` 名（如 content_block_delta），遇空行 flush 后清空。
    current_event: String,
    /// 当前事件已累积的多条 `data:` 行，空行时以换行拼接成一个事件负载。
    data_lines: Vec<String>,
}

impl AnthropicSse {
    /// 新建一个空的行累积器（无残留缓冲与未闭合事件）。
    fn new() -> Self {
        AnthropicSse {
            buf: Vec::new(),
            current_event: String::new(),
            data_lines: Vec::new(),
        }
    }

    /// 处理一行 SSE 文本
    fn handle_line(
        current_event: &mut String,
        data_lines: &mut Vec<String>,
        line: &str,
        events: &mut Vec<(String, String)>,
    ) {
        let l = line.strip_suffix('\r').unwrap_or(line);
        if l.is_empty() {
            if !data_lines.is_empty() {
                events.push((std::mem::take(current_event), data_lines.join("\n")));
                data_lines.clear();
            }
            current_event.clear();
        } else if let Some(ev) = l.strip_prefix("event:") {
            *current_event = ev.trim().to_string();
        } else if let Some(d) = l.strip_prefix("data:") {
            data_lines.push(d.strip_prefix(' ').unwrap_or(d).to_string());
        } else if l.trim_start().starts_with('{') {
            // 容错：无 data: 前缀的裸 JSON 行（部分兼容代理发 JSON Lines）
            data_lines.push(l.to_string());
        }
        // 注释行（: ...）忽略
    }

    /// 喂入一段字节流，按行切分并解析出完整事件。
    /// 末尾不完整的行留在内部缓冲等下次调用（避免拆包导致同一行被处理两次）；
    /// 返回本次 flush 出的 `(event 名, data 负载)` 列表（无完整事件时为空）。
    fn push(&mut self, chunk: &[u8]) -> Vec<(String, String)> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        let n = self.buf.len();
        let mut i = 0usize;
        while i < n {
            let mut line_end = i;
            while line_end < n && self.buf[line_end] != b'\n' {
                line_end += 1;
            }
            if line_end >= n {
                // 行不完整（无换行结尾）：保留给下一次 push。否则拆包时这一行会被处理两遍，
                // 拼接出的 data 中间带 \n，JSON 解析失败导致整个事件丢失（MiniMax 端点实测）。
                break;
            }
            // into_owned：line 不再借用 self.buf，随后可安全借用 self 的其他字段
            let line = String::from_utf8_lossy(&self.buf[i..line_end]).into_owned();
            AnthropicSse::handle_line(
                &mut self.current_event,
                &mut self.data_lines,
                &line,
                &mut events,
            );
            i = line_end + 1;
        }
        if i > 0 {
            self.buf.drain(..i);
        }
        events
    }

    /// 流结束时调用：剩余 buf 不再有新字节，把剩余行按完整行处理，并 flush 未闭合的事件。
    fn finish(&mut self) -> Vec<(String, String)> {
        let mut events = Vec::new();
        let remaining = std::mem::take(&mut self.buf);
        let mut i = 0usize;
        while i < remaining.len() {
            let mut line_end = i;
            while line_end < remaining.len() && remaining[line_end] != b'\n' {
                line_end += 1;
            }
            if line_end >= remaining.len() {
                line_end = remaining.len();
            }
            let line = String::from_utf8_lossy(&remaining[i..line_end]).into_owned();
            AnthropicSse::handle_line(
                &mut self.current_event,
                &mut self.data_lines,
                &line,
                &mut events,
            );
            i = line_end + 1;
        }
        if !self.data_lines.is_empty() {
            events.push((
                std::mem::take(&mut self.current_event),
                self.data_lines.join("\n"),
            ));
            self.data_lines.clear();
        }
        self.current_event.clear();
        events
    }
}

/// 把 anthropic 的 stop_reason 映射为统一语义 `stop`/`length`/`toolUse`/`error`。
/// refusal 取 `stop_details.explanation` 作为错误文案，sensitive 固定文案；
/// 未知原因返回 `Err`（不静默当作正常结束）。
fn map_anthropic_stop_reason(
    reason: &str,
    stop_details: Option<&Value>,
) -> Result<(&'static str, Option<String>)> {
    match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => Ok(("stop", None)),
        "max_tokens" => Ok(("length", None)),
        "tool_use" => Ok(("toolUse", None)),
        "refusal" => Ok((
            "error",
            Some(
                stop_details
                    .and_then(|d| d.get("explanation").and_then(|v| v.as_str()))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "The model refused to complete the request".to_string()),
            ),
        )),
        "sensitive" => Ok((
            "error",
            Some("Provider stopped with: sensitive".to_string()),
        )),
        other => Err(Error::msg(format!(
            "Anthropic unhandled stop reason: {}",
            other
        ))),
    }
}

/// 流式解析中按 content index 累积的单个内容块中间状态，收尾后转为 assist 块。
struct AnthropicSlot {
    /// 该槽位累积的内容块类型，决定 delta 事件如何归类。
    kind: SlotKind,
    /// 内容块在 assistant 消息中的顺序下标，收尾时据此保序组装。
    content_index: usize,
    /// 累积的正文文本增量（text_delta）。
    text: String,
    /// 累积的思考内容（thinking_delta）。
    thinking: String,
    /// 累积的思考签名（signature_delta），回放思考块时随块一起回传。
    thinking_signature: String,
    /// 工具参数的分片 JSON 原文，由 input_json_delta 逐段追加。
    partial_json: String,
    /// 由 partial_json 实时解析出的参数对象，供流式工具调用展示。
    arguments: Value,
    /// 工具调用 id（content_block_start 的 tool_use id）。
    id: String,
    /// 被调用工具名。
    name: String,
}

/// slot 所表示的内容块类型：思考、文本或工具调用。
#[derive(Debug, Clone, Copy, PartialEq)]
enum SlotKind {
    /// 思考块（thinking / redacted_thinking）。
    Thinking,
    /// 正文文本块。
    Text,
    /// 工具调用块（tool_use）。
    ToolCall,
}

/// 解析完成、待按顺序组装为 assistant 消息的内容块。
enum AnthropicAssistBlock {
    /// 已收尾的思考块，携带正文与签名（redacted 时正文为占位符）。
    Thinking {
        /// 完整思考文本。
        thinking: String,
        /// 服务端签名，回放时必须原样带回。
        signature: String,
    },
    /// 已收尾的正文文本块，空文本在组装时被丢弃。
    Text(String),
    /// 已收尾的工具调用块。
    ToolCall {
        /// 工具调用 id。
        id: String,
        /// 被调用工具名。
        name: String,
        /// 解析完成的参数对象。
        arguments: Value,
    },
}

/// 构造 anthropic 请求体所需的参数集合（系统提示、消息、工具、思考开关等）。
struct AnthropicBuildConfig<'a> {
    system_prompt: &'a str,
    messages_wire: Vec<Value>,
    tools: &'a [(String, String, Value)],
    reasoning_effort: Option<&'a str>,
    thinking_enabled: bool,
    adaptive: bool,
    temperature: Option<f64>,
    supports_temperature: bool,
    eager_input_streaming: bool,
    max_tokens: Option<u32>,
    tool_choice: Option<&'a str>,
}

/// 构建并序列化 anthropic 请求 body（system/messages/tools/thinking 等）。
fn build_anthropic_body(model: &ModelConfig, cfg: AnthropicBuildConfig<'_>) -> Result<String> {
    let mut body_map = serde_json::Map::new();
    body_map.insert("model".into(), json!(model.model_id));
    body_map.insert(
        "max_tokens".into(),
        json!(
            cfg.max_tokens
                .or(model.max_tokens)
                .unwrap_or(DEFAULT_THINKING_BUDGET * 4)
        ),
    );
    body_map.insert("stream".into(), json!(true));
    if !cfg.system_prompt.is_empty() {
        body_map.insert(
            "system".into(),
            json!([{
                "type": "text",
                "text": sanitize_surrogates(cfg.system_prompt),
                "cache_control": cache_control()
            }]),
        );
    }
    body_map.insert("messages".into(), Value::Array(cfg.messages_wire));
    if !cfg.tools.is_empty() {
        body_map.insert(
            "tools".into(),
            Value::Array(convert_anthropic_tools(
                cfg.tools,
                cfg.eager_input_streaming,
            )),
        );
    }

    if let Some(tc) = cfg.tool_choice {
        body_map.insert("tool_choice".into(), json!({ "type": tc })); // "auto" | "none"
    }

    // 服务端 fallback：主模型降级时由服务端自动切换
    if !model.allowed_fallback_models.is_empty() {
        body_map.insert(
            "fallbacks".into(),
            Value::Array(
                model
                    .allowed_fallback_models
                    .iter()
                    .map(|f| json!({ "model": f.model }))
                    .collect(),
            ),
        );
    }

    if model.reasoning {
        if cfg.thinking_enabled {
            if cfg.adaptive {
                let effort = cfg
                    .reasoning_effort
                    .map(|l| map_thinking_level_to_effort(model, l))
                    .unwrap_or_default();
                body_map.insert(
                    "thinking".into(),
                    json!({ "type": "adaptive", "display": "summarized" }),
                );
                body_map.insert("output_config".into(), json!({ "effort": effort }));
            } else {
                body_map.insert(
                    "thinking".into(),
                    json!({
                        "type": "enabled",
                        "budget_tokens": DEFAULT_THINKING_BUDGET,
                        "display": "summarized"
                    }),
                );
            }
        } else {
            // off 为 null 时保持 API 默认（不发 disabled）
            let should_disable = model
                .thinking_level_map
                .as_ref()
                .and_then(|m| m.get("off"))
                .is_none_or(|v| v.is_some());
            if should_disable {
                body_map.insert("thinking".into(), json!({ "type": "disabled" }));
            }
        }
    }

    if let Some(t) = cfg.temperature
        && !cfg.thinking_enabled
        && cfg.supports_temperature
    {
        body_map.insert("temperature".into(), json!(t));
    }

    serde_json::to_string(&body_map).map_err(|source| Error::Json {
        context: "failed to serialize anthropic request".to_string(),
        source,
    })
}

/// 按模型能力钳制 thinking 级别；"off" 归一到 None。
fn normalize_reasoning_effort(
    model: &ModelConfig,
    reasoning_effort: Option<&str>,
) -> Option<String> {
    match reasoning_effort {
        Some(level) => {
            let clamped = model.clamp_thinking_level(level);
            if clamped == "off" {
                None
            } else {
                Some(clamped)
            }
        }
        None => None,
    }
}

/// 处理 `message_start`：记录响应 id 与服务端 fallback 后的实际模型，
/// 并按 usage（含 cache 读/写、1h 拆分与 thinking token）计算本轮费用写入 `usage`。
fn handle_anthropic_message_start(
    ev: &Value,
    saw_message_start: &mut bool,
    response_id: &mut Option<String>,
    response_model: &mut Option<String>,
    usage: &mut Option<Usage>,
    model: &ModelConfig,
) {
    *saw_message_start = true;
    if let Some(id) = ev.pointer("/message/id").and_then(|v| v.as_str())
        && response_id.is_none()
    {
        *response_id = Some(id.to_string());
    }

    // 服务端 fallback：返回的 model 与请求不一致时，按目录中对应 fallback 的单价计费
    *response_model = ev
        .pointer("/message/model")
        .and_then(|v| v.as_str())
        .filter(|m| *m != model.model_id)
        .map(|m| m.to_string());

    if let Some(u) = ev.pointer("/message/usage") {
        let input = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let output = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let cache_read = u
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let cache_write = u
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let cache_write_1h = u
            .pointer("/cache_creation/ephemeral_1h_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let reasoning = u
            .pointer("/output_tokens_details/thinking_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let mut parsed = Usage {
            input,
            output,
            cache_read,
            cache_write,
            cache_write_1h: (cache_write_1h > 0).then_some(cache_write_1h),
            reasoning: (reasoning > 0).then_some(reasoning),
            total_tokens: input + output + cache_read + cache_write,
            cost: Cost::default(),
        };
        let fallback_cost = billing_cost(model, response_model.as_deref());
        compute_cost(&mut parsed, fallback_cost);
        *usage = Some(parsed);
    }
}

/// 新建一个空的块累积槽位，记下块类型与在 assistant 消息中的顺序下标。
fn anthropic_slot_for(kind: SlotKind, content_index: usize) -> AnthropicSlot {
    AnthropicSlot {
        kind,
        content_index,
        text: String::new(),
        thinking: String::new(),
        thinking_signature: String::new(),
        partial_json: String::new(),
        arguments: json!({}),
        id: String::new(),
        name: String::new(),
    }
}

/// 处理 `content_block_start`：按块类型（text/thinking/redacted_thinking/tool_use）
/// 建立槽位，向前端推 Start 事件（块自带的初始文本再补一次 Delta）。
/// `fallback` 块在已有输出后出现时返回 `Err`（mid-output fallback 无法安全重放）。
fn handle_anthropic_content_block_start(
    ev: &Value,
    slots: &mut HashMap<usize, AnthropicSlot>,
    next_content_index: &mut usize,
    on_event: &mut Option<&mut PartialTranslator<'_>>,
) -> Result<()> {
    let index = ev.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let block_type = ev
        .pointer("/content_block/type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match block_type {
        // 服务端 fallback 打在流开头（还没输出任何块）时直接忽略，继续消费降级后的内容；
        // 已有输出后再 fallback 无法安全重放，本轮失败（pi #9294 的
        // "unsupported mid-output model fallback"）。
        "fallback" => {
            if *next_content_index > 0 {
                return Err(Error::ProviderMessage(
                    "Anthropic performed an unsupported mid-output model fallback".to_string(),
                ));
            }
            return Ok(());
        }
        "text" => {
            let ci = *next_content_index;
            *next_content_index += 1;
            // 保留 content_block_start 自带的初始文本
            // （Anthropic 某些流会在 start 事件里直接携带初始文本，而非只靠后续 delta）
            let initial = ev
                .pointer("/content_block/text")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut slot = anthropic_slot_for(SlotKind::Text, ci);
            slot.text = initial.to_string();
            slots.insert(index, slot);
            if let Some(f) = on_event.as_mut() {
                f.push(RawStreamEvent::TextStart { content_index: ci });
            }
            if !initial.is_empty()
                && let Some(f) = on_event.as_mut()
            {
                f.push(RawStreamEvent::TextDelta {
                    content_index: ci,
                    delta: initial.to_string(),
                });
            }
        }
        "thinking" => {
            let ci = *next_content_index;
            *next_content_index += 1;
            let initial = ev
                .pointer("/content_block/thinking")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let signature = ev
                .pointer("/content_block/signature")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut slot = anthropic_slot_for(SlotKind::Thinking, ci);
            slot.thinking = initial.to_string();
            slot.thinking_signature = signature.to_string();
            slots.insert(index, slot);
            if let Some(f) = on_event.as_mut() {
                f.push(RawStreamEvent::ThinkingStart { content_index: ci });
            }
            if !initial.is_empty()
                && let Some(f) = on_event.as_mut()
            {
                f.push(RawStreamEvent::ThinkingDelta {
                    content_index: ci,
                    delta: initial.to_string(),
                });
            }
        }
        "redacted_thinking" => {
            let ci = *next_content_index;
            *next_content_index += 1;
            let signature = ev
                .pointer("/content_block/data")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut slot = anthropic_slot_for(SlotKind::Thinking, ci);
            slot.thinking = REDACTED_THINKING_TEXT.to_string();
            slot.thinking_signature = signature.to_string();
            slots.insert(index, slot);
            if let Some(f) = on_event.as_mut() {
                f.push(RawStreamEvent::ThinkingStart { content_index: ci });
            }
        }
        "tool_use" => {
            let ci = *next_content_index;
            *next_content_index += 1;
            let id = ev
                .pointer("/content_block/id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = ev
                .pointer("/content_block/name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input = ev
                .pointer("/content_block/input")
                .cloned()
                .unwrap_or(json!({}));
            let mut slot = anthropic_slot_for(SlotKind::ToolCall, ci);
            slot.arguments = input;
            slot.id = id;
            slot.name = name;
            slots.insert(index, slot);
            if let Some(f) = on_event.as_mut() {
                f.push(RawStreamEvent::ToolCallStart { content_index: ci });
            }
        }
        _ => {}
    }
    Ok(())
}

/// 处理 `content_block_delta`：把 text/thinking/partial_json 增量追加到对应槽位，
/// signature 为追加语义，并把文本/思考/工具参数增量转发给前端。
/// 未知块下标或与槽位类型不匹配的 delta 被忽略。
fn handle_anthropic_content_block_delta(
    ev: &Value,
    slots: &mut HashMap<usize, AnthropicSlot>,
    on_event: &mut Option<&mut PartialTranslator<'_>>,
) {
    let index = ev.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let delta_type = ev
        .pointer("/delta/type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let Some(slot) = slots.get_mut(&index) else {
        return;
    };
    match delta_type {
        "text_delta" => {
            if slot.kind == SlotKind::Text {
                let d = ev
                    .pointer("/delta/text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !d.is_empty() {
                    slot.text.push_str(d);
                    if let Some(f) = on_event.as_mut() {
                        f.push(RawStreamEvent::TextDelta {
                            content_index: slot.content_index,
                            delta: d.to_string(),
                        });
                    }
                }
            }
        }
        "thinking_delta" => {
            if slot.kind == SlotKind::Thinking {
                let d = ev
                    .pointer("/delta/thinking")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !d.is_empty() {
                    slot.thinking.push_str(d);
                    if let Some(f) = on_event.as_mut() {
                        f.push(RawStreamEvent::ThinkingDelta {
                            content_index: slot.content_index,
                            delta: d.to_string(),
                        });
                    }
                }
            }
        }
        "signature_delta" => {
            if slot.kind == SlotKind::Thinking {
                let d = ev
                    .pointer("/delta/signature")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                slot.thinking_signature.push_str(d); // 追加而不是覆盖
            }
        }
        "input_json_delta" if slot.kind == SlotKind::ToolCall => {
            let d = ev
                .pointer("/delta/partial_json")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            slot.partial_json.push_str(d);
            slot.arguments = parse_streaming_json(&slot.partial_json);
            if let Some(f) = on_event.as_mut() {
                f.push(RawStreamEvent::ToolCallDelta {
                    content_index: slot.content_index,
                    delta: d.to_string(),
                });
            }
        }
        _ => {}
    }
}

/// 处理 `content_block_stop`：移除该下标槽位，按类型收尾为 [`AnthropicAssistBlock`]
/// 存入 `finalized`（以 content_index 为键，收尾时据此保序组装）。
fn handle_anthropic_content_block_stop(
    ev: &Value,
    slots: &mut HashMap<usize, AnthropicSlot>,
    finalized: &mut BTreeMap<usize, AnthropicAssistBlock>,
) {
    let index = ev.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    if let Some(slot) = slots.remove(&index) {
        match slot.kind {
            SlotKind::Text => {
                finalized.insert(
                    slot.content_index,
                    AnthropicAssistBlock::Text(slot.text.clone()),
                );
            }
            SlotKind::Thinking => {
                finalized.insert(
                    slot.content_index,
                    AnthropicAssistBlock::Thinking {
                        thinking: slot.thinking.clone(),
                        signature: slot.thinking_signature.clone(),
                    },
                );
            }
            SlotKind::ToolCall => {
                let id = slot.id.clone();
                let name = slot.name.clone();
                let arguments = slot.arguments.clone();
                finalized.insert(
                    slot.content_index,
                    AnthropicAssistBlock::ToolCall {
                        id,
                        name,
                        arguments,
                    },
                );
            }
        }
    }
}

/// 本轮计费单价：服务端 fallback 返回的是别的模型且目录中有对应条目时用其 `cost`，
/// 否则用请求模型自身的 `cost`（pi `usageModel` 语义，#9294）。
fn billing_cost<'a>(model: &'a ModelConfig, response_model: Option<&str>) -> Option<&'a Value> {
    response_model
        .filter(|m| *m != model.model_id)
        .and_then(|m| {
            model
                .allowed_fallback_models
                .iter()
                .find(|f| f.provider == model.provider && f.model == m)
        })
        .and_then(|f| f.cost.as_ref())
        .or(model.cost.as_ref())
}

/// 处理 `message_delta`：解析 stop_reason（映射失败即返回 `Err`）与错误文案，
/// 并用事件里的 usage 刷新 token 计数与费用（覆盖式，缺失字段保留旧值）。
fn handle_anthropic_message_delta(
    ev: &Value,
    stop_reason: &mut Option<String>,
    error_message: &mut Option<String>,
    usage: &mut Option<Usage>,
    response_model: Option<&str>,
    model: &ModelConfig,
) -> Result<()> {
    if let Some(reason) = ev.pointer("/delta/stop_reason").and_then(|v| v.as_str()) {
        let stop_details = ev.get("delta").and_then(|d| d.get("stop_details"));
        let (mapped, em) = map_anthropic_stop_reason(reason, stop_details)?;
        *stop_reason = Some(mapped.to_string());
        *error_message = em;
    }

    if let Some(u) = ev.get("usage") {
        let mut current = usage.take().unwrap_or_default();
        if let Some(v) = u.get("input_tokens").and_then(|v| v.as_u64()) {
            current.input = v as u32;
        }
        if let Some(v) = u.get("output_tokens").and_then(|v| v.as_u64()) {
            current.output = v as u32;
        }
        if let Some(v) = u.get("cache_read_input_tokens").and_then(|v| v.as_u64()) {
            current.cache_read = v as u32;
        }
        if let Some(v) = u
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
        {
            current.cache_write = v as u32;
        }
        // Vercel AI Gateway 只在 message_delta 里带 TTL 拆分，不读就会把 1h cache write 按 5min 价计。
        if let Some(v) = u
            .pointer("/cache_creation/ephemeral_1h_input_tokens")
            .and_then(|v| v.as_u64())
        {
            current.cache_write_1h = (v > 0).then_some(v as u32);
        }
        current.total_tokens =
            current.input + current.output + current.cache_read + current.cache_write;
        let cost = billing_cost(model, response_model);
        compute_cost(&mut current, cost);
        *usage = Some(current);
    }
    Ok(())
}

/// 组装 assistant 消息 content blocks（按 content_index 保序）。
fn assemble_anthropic_blocks(
    finalized: BTreeMap<usize, AnthropicAssistBlock>,
) -> Vec<ContentBlock> {
    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    for (_ci, block) in finalized {
        match block {
            AnthropicAssistBlock::Thinking {
                thinking,
                signature,
            } => {
                if thinking.trim().is_empty() {
                    continue;
                }
                content_blocks.push(ContentBlock::Thinking {
                    thinking: thinking.clone(),
                    thinking_signature: Some(signature),
                    redacted: if thinking == REDACTED_THINKING_TEXT {
                        Some(true)
                    } else {
                        None
                    },
                });
            }
            AnthropicAssistBlock::Text(content) => {
                if content.is_empty() {
                    continue;
                }
                content_blocks.push(ContentBlock::Text {
                    text: content,
                    text_signature: None,
                });
            }
            AnthropicAssistBlock::ToolCall {
                id,
                name,
                arguments,
            } => {
                content_blocks.push(ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                    thought_signature: None,
                    namespace: None,
                });
            }
        }
    }
    content_blocks
}

/// 把组装好的内容块、stop 原因/错误文案与 usage 打包为统一的 [`StreamResult`]。
fn build_anthropic_result(
    content_blocks: Vec<ContentBlock>,
    stop_reason: Option<String>,
    error_message: Option<String>,
    usage: Option<Usage>,
    _response_id: Option<String>,
    response_model: Option<String>,
    model: &ModelConfig,
) -> Result<StreamResult> {
    let message = AgentMessage {
        role: "assistant".to_string(),
        thinking_level: None,
        content: content_blocks,
        tool_call_id: None,
        tool_name: None,
        is_error: false,
        stop_reason,
        error_message: error_message.clone(),
        model: Some(model.model_id.clone()),
        provider: Some(model.provider.clone()),
        api: Some(model.api.clone()),
        usage: usage.clone(),
        response_model,
        response_id: None,
        diagnostics: None,
        deferred: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: now_ms(),
        duration_ms: None,
        entry_id: None,
        details: None,
        citations: None,
    };

    Ok(StreamResult {
        message,
        usage,
        error_message,
    })
}

/// Anthropic Messages 协议的流式请求入口。
/// 组装请求 body 与 beta 头后带重试发送，逐块解析 SSE 并通过 `on_event` 实时转发增量；
/// 返回完整结果，网络/协议/HTTP 错误（含未知 stop reason）返回 `Err`。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream(
    model: &ModelConfig,
    messages: &[AgentMessage],
    system_prompt: &str,
    tools: &[(String, String, Value)],
    reasoning_effort: Option<&str>,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    mut on_event: Option<&mut PartialTranslator<'_>>,
    on_payload: Option<PayloadHook>,
    tool_choice: Option<&str>,
) -> Result<StreamResult> {
    let reasoning_effort = normalize_reasoning_effort(model, reasoning_effort);
    let thinking_enabled = reasoning_effort.is_some();
    let adaptive = compat_force_adaptive_thinking(model);
    let supports_temperature = compat_supports_temperature(model);

    let client = http::build_client()?;
    let messages_wire = convert_anthropic_messages(messages, system_prompt, model);

    // 仅当模型不支持 eager_input_streaming 时才发 fine-grained beta（opencode 网关照收）
    let eager_input_streaming = compat_supports_eager_input_streaming(model);
    let use_fine_grained_beta = !tools.is_empty() && !eager_input_streaming;

    let mut body_json = build_anthropic_body(
        model,
        AnthropicBuildConfig {
            system_prompt,
            messages_wire,
            tools,
            reasoning_effort: reasoning_effort.as_deref(),
            thinking_enabled,
            adaptive,
            temperature,
            supports_temperature,
            eager_input_streaming,
            max_tokens,
            tool_choice,
        },
    )?;

    // beta 头：interleaved thinking（adaptive 不加）+ fine-grained tool streaming；
    // OAuth 订阅登录还需 Claude Code 身份 beta
    let mut betas: Vec<&str> = Vec::new();
    if thinking_enabled && !adaptive {
        betas.push(INTERLEAVED_THINKING_BETA);
    }
    if use_fine_grained_beta {
        betas.push(FINE_GRAINED_TOOL_STREAMING_BETA);
    }
    // 服务端 fallback（目录 compat.allowedFallbackModels 非空）
    if !model.allowed_fallback_models.is_empty() {
        betas.push(SERVER_SIDE_FALLBACK_BETA);
    }

    let is_oauth = model.api_key.starts_with("sk-ant-oat");
    if is_oauth {
        betas.push("claude-code-20250219");
        betas.push("oauth-2025-04-20");
    }

    apply_payload_hook(&mut body_json, on_payload.as_ref());

    let response = send_with_retry(
        || {
            let mut rb = client
                .post(endpoint(model))
                .header("Content-Type", "application/json")
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("anthropic-dangerous-direct-browser-access", "true");
            if !betas.is_empty() {
                rb = rb.header("anthropic-beta", betas.join(","));
            }
            if is_oauth {
                rb = rb
                    .bearer_auth(&model.api_key)
                    .header("user-agent", format!("claude-cli/{CLAUDE_CODE_VERSION}"))
                    .header("x-app", "cli");
            } else {
                rb = rb.header("x-api-key", &model.api_key);
            }

            // 会话亲和 + opencode 会话标识（规则见 provider::session_affinity_headers）
            for (k, v) in provider_session_headers(model).into_iter().chain(
                session_affinity_headers(model)
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v)),
            ) {
                rb = rb.header(k, v);
            }
            rb.body(body_json.clone())
        },
        DEFAULT_MAX_RETRIES,
        model.max_retry_delay_ms,
    )
    .await?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(Error::ProviderStatus {
            status,
            body: truncate_for_error(&text),
        });
    }

    let mut stream = response.bytes_stream();

    // 诊断：保留最近收到的原始字节，出错时辅助定位
    let mut debug_capture: Vec<u8> = Vec::new();
    let mut stop_reason: Option<String> = None;
    let mut error_message: Option<String> = None;
    let mut usage: Option<Usage> = None;
    let mut response_id: Option<String> = None;
    let mut response_model: Option<String> = None;
    let mut saw_message_start = false;
    let mut saw_message_stop = false;

    // content blocks 按 index 顺序
    let mut slots: HashMap<usize, AnthropicSlot> = HashMap::new();
    let mut finalized: BTreeMap<usize, AnthropicAssistBlock> = BTreeMap::new();
    let mut next_content_index = 0usize;
    let mut sse = AnthropicSse::new();
    let mut citations: Vec<Citation> = Vec::new();
    let mut total_bytes: usize = 0;

    // 供应商原始流事件：无扩展订阅时零开销（一次流只查一次）
    let capture_stream_events = core::extensions::has_provider_stream_event_handlers();

    let mut handle_events = |evs: Vec<(String, String)>| -> std::result::Result<(), Error> {
        for (ev_type, data) in evs {
            if ev_type == "error" {
                return Err(Error::ProviderMessage(data));
            }
            let ev: Value = match serde_json::from_str(&data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if capture_stream_events {
                core::extensions::dispatch_provider_stream_event(
                    &model.provider,
                    &model.api,
                    &model.model_id,
                    &ev,
                );
            }
            let event_type = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match event_type {
                "message_start" => handle_anthropic_message_start(
                    &ev,
                    &mut saw_message_start,
                    &mut response_id,
                    &mut response_model,
                    &mut usage,
                    model,
                ),
                "content_block_start" => {
                    handle_anthropic_content_block_start(
                        &ev,
                        &mut slots,
                        &mut next_content_index,
                        &mut on_event,
                    )?;
                    if ev.pointer("/content_block/type").and_then(|v| v.as_str())
                        == Some("citation")
                    {
                        citations.push(Citation {
                            kind: "citation".to_string(),
                            start_index: ev
                                .pointer("/content_block/start_char_index")
                                .and_then(|v| v.as_u64())
                                .map(|v| v as usize),
                            end_index: ev
                                .pointer("/content_block/end_char_index")
                                .and_then(|v| v.as_u64())
                                .map(|v| v as usize),
                            url: ev
                                .pointer("/content_block/document_url")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                            title: ev
                                .pointer("/content_block/document_title")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                            text: ev
                                .pointer("/content_block/cited_text")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                        });
                    }
                }
                "content_block_delta" => {
                    handle_anthropic_content_block_delta(&ev, &mut slots, &mut on_event)
                }
                "content_block_stop" => {
                    handle_anthropic_content_block_stop(&ev, &mut slots, &mut finalized)
                }
                "message_delta" => handle_anthropic_message_delta(
                    &ev,
                    &mut stop_reason,
                    &mut error_message,
                    &mut usage,
                    response_model.as_deref(),
                    model,
                )?,
                "message_stop" => {
                    saw_message_stop = true;
                }
                _ => {}
            }
        }
        Ok(())
    };

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => return Err(Error::msg(format!("stream read failed: {}", e))),
        };
        // 诊断捕获最近 2KB
        debug_capture.extend_from_slice(&chunk);
        track_stream_bytes(&mut total_bytes, chunk.len())?;
        let keep = debug_capture.len().saturating_sub(2048);
        if keep > 0 {
            debug_capture.drain(..keep);
        }
        handle_events(sse.push(&chunk))?;
    }
    handle_events(sse.finish())?;

    if saw_message_start && !saw_message_stop {
        return Err(Error::msg(format!(
            "Anthropic stream ended before message_stop (response_id={:?}, raw_tail={:?})",
            response_id,
            String::from_utf8_lossy(&debug_capture)
                .chars()
                .take(300)
                .collect::<String>()
        )));
    }
    if stop_reason.is_none() {
        return Err(Error::msg(format!(
            "Anthropic stream ended without a stop reason (message_start={}, message_stop={}, response_id={:?}, usage={:?}, raw_tail={:?})",
            saw_message_start,
            saw_message_stop,
            response_id,
            usage,
            String::from_utf8_lossy(&debug_capture)
                .chars()
                .take(300)
                .collect::<String>()
        )));
    }

    let content_blocks = assemble_anthropic_blocks(finalized);
    let mut result = build_anthropic_result(
        content_blocks,
        stop_reason,
        error_message,
        usage,
        response_id,
        response_model,
        model,
    )?;

    if !citations.is_empty() {
        result.message.citations = Some(citations);
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::{AllowedFallbackModel, ModelConfig};

    fn model(id: &str) -> ModelConfig {
        ModelConfig {
            model_type: Default::default(),
            image_resize: crate::utils::image::ImageResizeLimits::default(),
            allowed_fallback_models: Vec::new(),
            provider: "opencode".into(),
            model_id: id.into(),
            base_url: "https://opencode.ai/zen".into(),
            api_key: "sk-test".into(),
            api: "anthropic-messages".into(),
            output: Vec::new(),
            input: vec!["text".into(), "image".into()],
            reasoning: true,
            max_tokens: Some(64000),
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

    #[test]
    fn user_message_becomes_content_array() {
        let m = model("claude-sonnet-4-5");
        let out = convert_anthropic_messages(&[AgentMessage::user_text("hi")], "", &m);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[0]["content"][0]["type"], "text");
        assert_eq!(out[0]["content"][0]["text"], "hi");
    }

    /// pi #9210：Vercel AI Gateway 只在 message_delta 里带 cache TTL 拆分，
    /// 漏读就会把 1h cache write 按 5min 价计。
    #[test]
    fn message_delta_reports_1h_cache_write() {
        let m = model("anthropic/claude-haiku-4.5");
        let ev = json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn" },
            "usage": {
                "input_tokens": 3,
                "output_tokens": 4,
                "cache_creation_input_tokens": 6535,
                "cache_creation": { "ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 6535 }
            }
        });
        let (mut stop, mut err, mut usage) = (None, None, None);
        handle_anthropic_message_delta(&ev, &mut stop, &mut err, &mut usage, None, &m).unwrap();
        assert_eq!(stop.as_deref(), Some("stop"));
        let u = usage.unwrap();
        assert_eq!(u.cache_write, 6535);
        assert_eq!(u.cache_write_1h, Some(6535));
    }

    #[test]
    fn empty_thinking_signature_replayed_for_compat_models() {
        // Fireworks（compat.allowEmptySignature=true）：保留 thinking 块 + 空 signature（pi #9323/#9676）
        let mut m = model("accounts/fireworks/models/deepseek-v4p1-flash");
        m.provider = "fireworks".into();
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.content = vec![ContentBlock::Thinking {
            thinking: "reasoning".into(),
            thinking_signature: Some(String::new()),
            redacted: None,
        }];
        let out = convert_anthropic_messages(&[a.clone()], "", &m);
        let asst = out.iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(asst["content"][0]["type"], "thinking");
        assert_eq!(asst["content"][0]["signature"], "");

        // 普通 Anthropic（无该 compat）：仍降级为 text
        let out2 = convert_anthropic_messages(&[a], "", &model("claude-sonnet-4-5"));
        let asst2 = out2.iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(asst2["content"][0]["type"], "text");
    }

    /// pi #10047：OpenCode Zen / Go 的 qwen3.8-flash 端点返回空 thinking signature，
    /// 若不保留则后续轮次被重放为纯文本。目录已带 compat.allowEmptySignature。
    #[test]
    fn qwen38_flash_empty_signature_replayed() {
        for provider in ["opencode", "opencode-go"] {
            let mut m = model("qwen3.8-flash");
            m.provider = provider.into();
            assert!(
                compat_allow_empty_signature(&m),
                "{provider} 的 qwen3.8-flash 应带 allowEmptySignature"
            );
            let mut a = AgentMessage::user_text("");
            a.role = "assistant".into();
            a.content = vec![ContentBlock::Thinking {
                thinking: "reasoning".into(),
                thinking_signature: Some(String::new()),
                redacted: None,
            }];
            let out = convert_anthropic_messages(&[a], "", &m);
            let asst = out.iter().find(|m| m["role"] == "assistant").unwrap();
            assert_eq!(asst["content"][0]["type"], "thinking", "{provider}");
            assert_eq!(asst["content"][0]["signature"], "", "{provider}");
            assert_eq!(asst["content"][0]["thinking"], "reasoning", "{provider}");
        }
    }

    #[test]
    fn assistant_text_and_tool_use_blocks() {
        let m = model("claude-sonnet-4-5");
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.content = vec![
            ContentBlock::Text {
                text: "I'll check".into(),
                text_signature: None,
            },
            ContentBlock::ToolCall {
                id: "toolu_1".into(),
                name: "read".into(),
                arguments: json!({ "path": "a.rs" }),
                thought_signature: None,
                namespace: None,
            },
        ];
        let out = convert_anthropic_messages(&[a], "", &m);
        // assistant 块 + 孤儿 tool call 触发的 synthetic result
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["content"][0]["type"], "text");
        assert_eq!(out[0]["content"][1]["type"], "tool_use");
        assert_eq!(out[0]["content"][1]["id"], "toolu_1");
        assert_eq!(out[0]["content"][1]["input"]["path"], "a.rs");
        assert_eq!(out[1]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn tool_result_becomes_tool_result_block() {
        let m = model("claude-sonnet-4-5");
        let mut tr = AgentMessage::user_text("");
        tr.role = "toolResult".into();
        tr.tool_call_id = Some("toolu_1".into());
        tr.is_error = false;
        tr.content = vec![ContentBlock::Text {
            text: "contents".into(),
            text_signature: None,
        }];
        let out = convert_anthropic_messages(&[tr], "", &m);
        let last = out.last().unwrap();
        assert_eq!(last["role"], "user");
        assert_eq!(last["content"][0]["type"], "tool_result");
        assert_eq!(last["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(last["content"][0]["content"], "contents");
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_message() {
        let m = model("claude-sonnet-4-5");
        let mk = |id: &str| {
            let mut tr = AgentMessage::user_text("");
            tr.role = "toolResult".into();
            tr.tool_call_id = Some(id.into());
            tr.content = vec![ContentBlock::Text {
                text: "out".into(),
                text_signature: None,
            }];
            tr
        };
        let out = convert_anthropic_messages(&[mk("toolu_1"), mk("toolu_2")], "", &m);
        assert_eq!(out.len(), 1);
        let content = out[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["tool_use_id"], "toolu_1");
        assert_eq!(content[1]["tool_use_id"], "toolu_2");
    }

    #[test]
    fn thinking_block_keeps_signature() {
        let m = model("claude-sonnet-4-5");
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.model = Some("claude-sonnet-4-5".into());
        a.provider = Some("opencode".into());
        a.api = Some("anthropic-messages".into());
        a.content = vec![
            ContentBlock::Thinking {
                thinking: "hmm".into(),
                thinking_signature: Some("sig_abc".into()),
                redacted: None,
            },
            ContentBlock::Text {
                text: "answer".into(),
                text_signature: None,
            },
        ];
        let out = convert_anthropic_messages(&[a], "", &m);
        assert_eq!(out[0]["content"][0]["type"], "thinking");
        assert_eq!(out[0]["content"][0]["signature"], "sig_abc");
        assert_eq!(out[0]["content"][1]["type"], "text");
    }

    #[test]
    fn thinking_without_signature_downgrades_to_text() {
        let m = model("claude-sonnet-4-5");
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.content = vec![ContentBlock::Thinking {
            thinking: "old thinking".into(),
            thinking_signature: None,
            redacted: None,
        }];
        let out = convert_anthropic_messages(&[a], "", &m);
        assert_eq!(out[0]["content"][0]["type"], "text");
        assert_eq!(out[0]["content"][0]["text"], "old thinking");
    }

    #[test]
    fn cross_model_tool_id_is_normalized_and_paired() {
        let m = model("claude-sonnet-4-5");
        // 来自 responses 的复合 id：含 "|"
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.content = vec![ContentBlock::ToolCall {
            id: "call_1|fc_2".into(),
            name: "read".into(),
            arguments: json!({}),
            thought_signature: None,
            namespace: None,
        }];
        let mut tr = AgentMessage::user_text("");
        tr.role = "toolResult".into();
        tr.tool_call_id = Some("call_1|fc_2".into());
        tr.content = vec![ContentBlock::Text {
            text: "out".into(),
            text_signature: None,
        }];
        let out = convert_anthropic_messages(&[a, tr], "", &m);
        // tool_use id 归一化 + tool_result 同步配对
        let tool_use_id = out[0]["content"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(tool_use_id, "call_1_fc_2");
        assert_eq!(out[1]["role"], "user");
        assert_eq!(out[1]["content"][0]["tool_use_id"], tool_use_id);
    }

    #[test]
    fn redacted_thinking_keeps_opaque_data() {
        let m = model("claude-sonnet-4-5");
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.model = Some("claude-sonnet-4-5".into());
        a.provider = Some("opencode".into());
        a.api = Some("anthropic-messages".into());
        a.content = vec![ContentBlock::Thinking {
            thinking: "[Reasoning redacted]".into(),
            thinking_signature: Some("opaque_data".into()),
            redacted: None,
        }];
        let out = convert_anthropic_messages(&[a], "", &m);
        assert_eq!(out[0]["content"][0]["type"], "redacted_thinking");
        assert_eq!(out[0]["content"][0]["data"], "opaque_data");
    }

    #[test]
    fn last_user_message_gets_cache_control() {
        let m = model("claude-sonnet-4-5");
        let out = convert_anthropic_messages(
            &[
                AgentMessage::user_text("hi"),
                AgentMessage::user_text("again"),
            ],
            "",
            &m,
        );
        let last = out.last().unwrap();
        let content = last["content"].as_array().unwrap();
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn orphan_tool_call_gets_synthetic_result() {
        let m = model("claude-sonnet-4-5");
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.content = vec![ContentBlock::ToolCall {
            id: "toolu_1".into(),
            name: "read".into(),
            arguments: json!({}),
            thought_signature: None,
            namespace: None,
        }];
        let out = convert_anthropic_messages(&[a], "", &m);
        // assistant tool_use 后补 synthetic tool_result
        assert_eq!(out[0]["content"][0]["type"], "tool_use");
        assert_eq!(out[1]["content"][0]["type"], "tool_result");
        assert_eq!(out[1]["content"][0]["content"], "No result provided");
    }

    #[test]
    fn skips_error_assistant_messages() {
        let m = model("claude-sonnet-4-5");
        let mut failed = AgentMessage::user_text("");
        failed.role = "assistant".into();
        failed.stop_reason = Some("error".into());
        let out = convert_anthropic_messages(&[failed], "", &m);
        assert!(out.is_empty());
    }

    #[test]
    fn normalize_tool_call_id_replaces_illegal_chars() {
        assert_eq!(normalize_tool_call_id("a|b c"), "a_b_c");
        assert_eq!(normalize_tool_call_id(&"x".repeat(100)), "x".repeat(64));
    }

    // ============ AnthropicSse：跨 chunk 拆行保护 ============

    #[test]
    fn sse_parser_rejoins_line_split_across_chunks() {
        let mut sse = AnthropicSse::new();
        let full = b"event: x\ndata: {\"a\":1}\n\n";
        // 拆成三个 chunk，两处边界都切在行中间
        let mut out = Vec::new();
        out.extend(sse.push(&full[..7]));
        out.extend(sse.push(&full[7..15]));
        out.extend(sse.push(&full[15..]));
        assert_eq!(out.len(), 1);
        let (ev, data) = &out[0];
        assert_eq!(ev, "x");
        assert_eq!(data, r#"{"a":1}"#);
    }

    #[test]
    fn sse_parser_flushes_final_event_on_finish() {
        let mut sse = AnthropicSse::new();
        let mut out = Vec::new();
        out.extend(sse.push(b"event: x\ndata: {\"b\":2}\n\n"));
        out.extend(sse.push(b"event: y\ndata: {\"c\":3}"));
        // 最后一行无换行结尾：finish 必须把它 flush 出来
        out.extend(sse.finish());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "x");
        let (ev, data) = &out[1];
        assert_eq!(ev, "y");
        assert_eq!(data, r#"{"c":3}"#);
    }

    #[test]
    fn sse_parser_ignores_garbage_tail_on_finish() {
        let mut sse = AnthropicSse::new();
        let mut out = Vec::new();
        out.extend(sse.push(b"event: x\ndata: {\"b\":2}\n\n"));
        // 残片：事件中断在半路（如 MiniMax 断开时停在 "event" 前缀），finish 不应产生脏事件
        out.extend(sse.push(b"event"));
        out.extend(sse.finish());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "x");
    }

    #[test]
    fn content_block_start_keeps_initial_text() {
        // 对齐 pi 0.84.1：content_block_start 自带的初始文本必须保留（而非空初始化）
        let mut slots = HashMap::new();
        let mut next_content_index = 0usize;
        let ev = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "initial text" }
        });
        handle_anthropic_content_block_start(&ev, &mut slots, &mut next_content_index, &mut None)
            .unwrap();
        let slot = slots.get(&0).unwrap();
        assert_eq!(slot.kind, SlotKind::Text);
        assert_eq!(slot.text, "initial text");
        assert_eq!(next_content_index, 1);
    }

    #[test]
    fn content_block_start_initial_text_emits_delta() {
        // 初始文本必须以 TextDelta 事件发出（下游 UI 只消费 delta 渲染）
        let m = model("claude-sonnet-4-5");
        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<crate::core::provider::StreamEvent>();
        let mut sink_fn = move |ev: crate::core::provider::StreamEvent| {
            let _ = ev_tx.send(ev);
        };
        let mut translator = PartialTranslator::new(&mut sink_fn, &m);
        let mut on_event = Some(&mut translator);
        let mut slots = HashMap::new();
        let mut next_content_index = 0usize;
        let ev = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "initial text" }
        });
        handle_anthropic_content_block_start(
            &ev,
            &mut slots,
            &mut next_content_index,
            &mut on_event,
        )
        .unwrap();

        let events: Vec<crate::core::provider::StreamEvent> = ev_rx.try_iter().collect();
        assert!(matches!(
            events.first(),
            Some(crate::core::provider::StreamEvent::Start { .. })
        ));
        assert!(events.iter().any(|e| matches!(e, crate::core::provider::StreamEvent::TextDelta { delta, .. } if delta == "initial text")));
        // partial 快照应已包含初始文本
        let last = events.last().unwrap();
        let partial: &AgentMessage = match last {
            crate::core::provider::StreamEvent::TextDelta { partial, .. } => partial,
            _ => panic!("expected TextDelta tail"),
        };
        assert_eq!(partial.text(), "initial text");
    }

    #[test]
    fn content_block_start_initial_thinking_kept() {
        // thinking 分支同样保留初始 thinking + signature（对齐 pi 0.84.1）
        let mut slots = HashMap::new();
        let mut next_content_index = 0usize;
        let ev = json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": { "type": "thinking", "thinking": "think x", "signature": "sig_1" }
        });
        handle_anthropic_content_block_start(&ev, &mut slots, &mut next_content_index, &mut None)
            .unwrap();
        let slot = slots.get(&1).unwrap();
        assert_eq!(slot.kind, SlotKind::Thinking);
        assert_eq!(slot.thinking, "think x");
        assert_eq!(slot.thinking_signature, "sig_1");
    }

    /// 带服务端 fallback 的模型（pi #9294 测试同款：fallback 单价 input 3 / output 5）
    fn model_with_fallbacks() -> ModelConfig {
        let mut m = model("claude-fable-5");
        m.provider = "anthropic".into();
        m.cost =
            Some(json!({ "input": 10.0, "output": 50.0, "cacheRead": 1.0, "cacheWrite": 12.5 }));
        m.allowed_fallback_models = vec![AllowedFallbackModel {
            provider: "anthropic".into(),
            model: "fallback-model".into(),
            cost: Some(json!({ "input": 3.0, "output": 5.0, "cacheRead": 0.0, "cacheWrite": 0.0 })),
        }];
        m
    }

    fn body_config<'a>(messages: &'a [Value]) -> AnthropicBuildConfig<'a> {
        AnthropicBuildConfig {
            system_prompt: "sys",
            messages_wire: messages.to_vec(),
            tools: &[],
            reasoning_effort: None,
            thinking_enabled: false,
            adaptive: false,
            temperature: None,
            supports_temperature: true,
            eager_input_streaming: true,
            max_tokens: Some(100),
            tool_choice: None,
        }
    }

    #[test]
    fn body_carries_server_side_fallbacks() {
        let msgs = vec![json!({ "role": "user", "content": [{"type":"text","text":"hi"}] })];

        let body = build_anthropic_body(&model_with_fallbacks(), body_config(&msgs)).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["fallbacks"], json!([{ "model": "fallback-model" }]));

        // 无 compat.allowedFallbackModels：不带该字段
        let body = build_anthropic_body(&model("claude-sonnet-4-5"), body_config(&msgs)).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert!(v.get("fallbacks").is_none());
    }

    #[test]
    fn message_start_bills_returned_fallback_model() {
        let m = model_with_fallbacks();
        let mut saw = false;
        let mut id = None;
        let mut response_model = None;
        let mut usage = None;
        let ev = json!({
            "type": "message_start",
            "message": {
                "id": "msg_1",
                "model": "fallback-model",
                "usage": { "input_tokens": 100, "output_tokens": 20 }
            }
        });
        handle_anthropic_message_start(&ev, &mut saw, &mut id, &mut response_model, &mut usage, &m);
        assert!(saw);
        assert_eq!(id.as_deref(), Some("msg_1"));
        assert_eq!(response_model.as_deref(), Some("fallback-model"));
        // 按 fallback 单价（3 / 5）而非请求模型单价（10 / 50）计费
        let u = usage.unwrap();
        assert!((u.cost.input - 0.000_3).abs() < 1e-12, "{:?}", u.cost);
        assert!((u.cost.output - 0.000_1).abs() < 1e-12, "{:?}", u.cost);
    }

    #[test]
    fn message_start_keeps_request_model_cost_and_omits_response_model() {
        let m = model_with_fallbacks();
        let mut saw = false;
        let mut id = None;
        let mut response_model = None;
        let mut usage = None;
        let ev = json!({
            "type": "message_start",
            "message": {
                "id": "msg_2",
                "model": "claude-fable-5",
                "usage": { "input_tokens": 100, "output_tokens": 20 }
            }
        });
        handle_anthropic_message_start(&ev, &mut saw, &mut id, &mut response_model, &mut usage, &m);
        // 返回模型与请求一致：不设 responseModel，按请求模型单价计费
        assert!(response_model.is_none());
        let u = usage.unwrap();
        assert!((u.cost.input - 0.001).abs() < 1e-12, "{:?}", u.cost);
        assert!((u.cost.output - 0.001).abs() < 1e-12, "{:?}", u.cost);
    }

    #[test]
    fn leading_fallback_block_is_skipped_but_mid_output_fails() {
        // 流开头（尚无任何块）的 fallback 块：跳过，不占 content index
        let mut slots = HashMap::new();
        let mut next_content_index = 0usize;
        let leading = json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "fallback" } });
        handle_anthropic_content_block_start(
            &leading,
            &mut slots,
            &mut next_content_index,
            &mut None,
        )
        .unwrap();
        assert!(slots.is_empty());
        assert_eq!(next_content_index, 0);

        // 已有输出后再 fallback：无法安全重放 → 本轮失败
        let text = json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "partial" } });
        handle_anthropic_content_block_start(&text, &mut slots, &mut next_content_index, &mut None)
            .unwrap();
        let late = json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "fallback" } });
        let err = handle_anthropic_content_block_start(
            &late,
            &mut slots,
            &mut next_content_index,
            &mut None,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("unsupported mid-output model fallback"),
            "{err}"
        );
    }

    #[test]
    fn billing_cost_prefers_matching_fallback_entry() {
        let m = model_with_fallbacks();
        // 命中 fallback 条目 → 用其 cost
        assert_eq!(
            billing_cost(&m, Some("fallback-model"))
                .and_then(|c| c.get("input"))
                .and_then(|v| v.as_f64()),
            Some(3.0)
        );
        // 未命中的其它模型 → 回退到请求模型 cost
        assert_eq!(
            billing_cost(&m, Some("some-other-model"))
                .and_then(|c| c.get("input"))
                .and_then(|v| v.as_f64()),
            Some(10.0)
        );
        // 与请求模型同名 → 请求模型 cost
        assert_eq!(
            billing_cost(&m, Some("claude-fable-5"))
                .and_then(|c| c.get("input"))
                .and_then(|v| v.as_f64()),
            Some(10.0)
        );
    }
}
