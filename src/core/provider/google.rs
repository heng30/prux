//! Google Generative AI 协议（gemini-3 系，经 opencode 网关）。
//!
//! 关键差异点：
//! - 认证用 `x-goog-api-key`；端点 `{baseUrl}/models/{model}:streamGenerateContent`
//! - SSE 每 chunk 是 GenerateContentResponse JSON（candidates/usageMetadata）
//! - thinking 用 `thought: true` 标记 + `thoughtSignature`（base64）保留推理上下文
//! - gemini 3+ 要求显式 tool call id；functionCall 不带 id 时生成唯一 id
//! - thinking 配置：3-pro/3-flash 用 thinkingLevel（大写），老模型用 thinkingBudget

use super::{
    AgentMessage, ContentBlock, Cost, ModelConfig, PartialTranslator, RawStreamEvent, StreamResult,
    Usage,
    convert::{
        BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX, COMPACTION_SUMMARY_PREFIX,
        COMPACTION_SUMMARY_SUFFIX, sanitize_surrogates, truncate_for_error,
    },
    retry::{DEFAULT_MAX_RETRIES, send_with_retry},
    track_stream_bytes,
    usage::compute_cost,
};
use crate::{
    core::{self, provider::apply_payload_hook},
    error::{Error, Result},
    utils::{http, time::now_ms},
};
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use std::{
    collections::HashSet,
    sync::atomic::{AtomicU64, Ordering},
};

/// 自动生成 tool call id 时追加的自增序号，保证同一毫秒内也不重名。
static TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 拼接 Gemini 流式端点 `{baseUrl}/models/{model}:streamGenerateContent`（baseUrl 去掉尾部 `/`）。
fn endpoint(model: &ModelConfig) -> String {
    format!(
        "{}/models/{}:streamGenerateContent",
        model.base_url.trim_end_matches('/'),
        model.model_id
    )
}

/// gemini 3+ 模型要求显式 tool call id
fn requires_tool_call_id(model_id: &str) -> bool {
    if model_id.starts_with("claude-") || model_id.starts_with("gpt-oss-") {
        return true;
    }
    let lower = model_id.to_lowercase();
    if let Some(rest) = lower.strip_prefix("gemini") {
        let rest = rest.strip_prefix("-live").unwrap_or(rest);
        if let Some(digits) = rest.strip_prefix('-') {
            let major: u32 = digits
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            return major >= 3;
        }
    }
    false
}

/// 模型是否使用 Gemini 的离散 `thinkingLevel` 控制（而非 token 的 `thinkingBudget`）。
/// Gemini 3 Pro/Flash（可带次版本号）、`gemini-flash-latest`/`gemini-flash-lite-latest`、Gemma 4（两种命名）。
fn uses_google_thinking_level(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    is_gemini3_pro_or_flash(&id)
        || id == "gemini-flash-latest"
        || id == "gemini-flash-lite-latest"
        || is_gemma4(&id)
}

/// `gemini-3(\.[0-9]+)?-(pro|flash)`
fn is_gemini3_pro_or_flash(id: &str) -> bool {
    /// 匹配 `gemini-3[.x]-pro|flash` 的编译后正则，只初始化一次。
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"gemini-3(?:\.\d+)?-(?:pro|flash)").expect("valid"))
        .is_match(id)
}

/// `gemma-?4`（两种命名：`gemma-4-*` / `gemma4-*`）
fn is_gemma4(id: &str) -> bool {
    /// 匹配 `gemma-4` / `gemma4` 两种写法的编译后正则，只初始化一次。
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"gemma-?4").expect("valid"))
        .is_match(id)
}

/// 解析 thinking 级别到 Google 的 ThinkingLevel 枚举值（大写）。
/// 否则直接使用级别本身（不再按模型族改写）。
fn resolve_google_thinking_level(model: &ModelConfig, level: &str) -> String {
    let resolved = model
        .thinking_level_map
        .as_ref()
        .and_then(|m| m.get(level))
        .and_then(|v| v.as_deref())
        .map(|s| s.to_lowercase())
        .unwrap_or_else(|| level.to_string());
    match resolved.as_str() {
        "minimal" => "MINIMAL",
        "low" => "LOW",
        "medium" => "MEDIUM",
        "high" => "HIGH",
        _ => "HIGH",
    }
    .to_string()
}

/// 关闭 thinking 时的配置。非 thinkingLevel 模型 → `thinkingBudget: 0`；
/// thinkingLevel 模型用钳制后的最低受支持级别（如 Gemini 3 Pro 无法真正关闭）。
fn disabled_thinking_config(model: &ModelConfig) -> Value {
    if !uses_google_thinking_level(&model.model_id) {
        return json!({ "thinkingBudget": 0 });
    }
    let fallback = model.clamp_thinking_level("off");
    if fallback == "off" {
        return json!({ "thinkingBudget": 0 });
    }
    json!({ "thinkingLevel": resolve_google_thinking_level(model, &fallback) })
}

/// 把 tool call id 归一化为 Gemini 接受的字符集（字母数字 / `_` / `-`，其余替换为 `_`）并截断到 64 字符。
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

/// thoughtSignature 必须是合法 base64 才保留
fn valid_thought_signature(sig: &str) -> bool {
    if sig.is_empty() || !sig.len().is_multiple_of(4) {
        return false;
    }
    sig.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
}

// 消息转换
/// 为尚未收到结果的 tool call 补发一条「无结果」functionResponse。
///
/// Gemini 要求每个 functionCall 都有对应响应，否则整条请求被拒；`existing` 是已有结果的 id 集合
/// （命中的跳过，结束时清空），`include_id` 决定占位响应是否带 id（gemini 3+ 必需），`name` 目前未使用。
fn insert_synthetic_tool_results(
    contents: &mut Vec<Value>,
    pending: &mut Vec<(String, String)>,
    existing: &mut HashSet<String>,
    include_id: bool,
    name: &str,
) {
    if pending.is_empty() {
        return;
    }
    for (id, tool_name) in pending.drain(..) {
        if existing.contains(&id) {
            continue;
        }
        let mut part = json!({
            "functionResponse": {
                "name": tool_name,
                "response": { "error": "No result provided" }
            }
        });
        if include_id {
            part["functionResponse"]["id"] = json!(id);
        }
        let _ = name;
        contents.push(json!({ "role": "user", "parts": [part] }));
    }
    existing.clear();
}

/// 把 user 消息转成 Gemini `contents` 条目；没有任何文本/图片块时返回 `None`。
///
/// 模型不支持图片时用占位文本代替 inlineData，并合并连续占位以免重复刷屏。
fn convert_google_user_message(msg: &AgentMessage, supports_images: bool) -> Option<Value> {
    let mut parts: Vec<Value> = Vec::new();
    let mut previous_was_placeholder = false;
    for block in &msg.content {
        match block {
            ContentBlock::Text { text, .. } => {
                parts.push(json!({ "text": sanitize_surrogates(text) }));
                previous_was_placeholder = text == "(image omitted: model does not support images)";
            }
            ContentBlock::Image { data, mime_type } => {
                if supports_images {
                    parts.push(json!({
                        "inlineData": {
                            "mimeType": mime_type,
                            "data": data
                        }
                    }));
                    previous_was_placeholder = false;
                } else if !previous_was_placeholder {
                    parts.push(json!({
                        "text": "(image omitted: model does not support images)"
                    }));
                    previous_was_placeholder = true;
                }
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(json!({ "role": "user", "parts": parts }))
}

/// 转换 assistant 消息；functionCall 块按 include_id 决定是否归一化 id 并
/// 记入 pending,供后续 synthetic functionResponse 使用。
fn convert_google_assistant_message(
    msg: &AgentMessage,
    model: &ModelConfig,
    include_id: bool,
    pending_tool_calls: &mut Vec<(String, String)>,
) -> Option<Value> {
    if msg.stop_reason.as_deref() == Some("error") || msg.stop_reason.as_deref() == Some("aborted")
    {
        return None;
    }

    let is_same_provider_and_model = msg.provider.as_deref() == Some(model.provider.as_str())
        && msg.model.as_deref() == Some(model.model_id.as_str());

    let mut parts: Vec<Value> = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text {
                text,
                text_signature,
                ..
            } => {
                // 空文本块只有「既为空又无签名」才丢弃；
                // Gemini 会把签名附着在可见文本为空的 part 上，丢弃会断裂推理链。
                let sig = text_signature
                    .as_deref()
                    .filter(|s| valid_thought_signature(s));
                let thought_sig = if is_same_provider_and_model {
                    sig
                } else {
                    None
                };
                if text.trim().is_empty() && thought_sig.is_none() {
                    continue;
                }
                let mut part = json!({ "text": sanitize_surrogates(text) });
                if let Some(s) = thought_sig {
                    part["thoughtSignature"] = json!(s);
                }
                parts.push(part);
            }
            ContentBlock::Thinking {
                thinking,
                thinking_signature,
                ..
            } => {
                let sig = thinking_signature
                    .as_deref()
                    .filter(|s| valid_thought_signature(s));
                if is_same_provider_and_model {
                    if thinking.trim().is_empty() && sig.is_none() {
                        continue;
                    }
                    let mut part = json!({
                        "thought": true,
                        "text": sanitize_surrogates(thinking)
                    });
                    if let Some(s) = sig {
                        part["thoughtSignature"] = json!(s);
                    }
                    parts.push(part);
                } else if !thinking.trim().is_empty() {
                    parts.push(json!({ "text": sanitize_surrogates(thinking) }));
                }
            }
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                let mut part = json!({
                    "functionCall": {
                        "name": name,
                        "args": arguments.clone()
                    }
                });
                if include_id {
                    let normalized = pending_tool_calls
                        .iter()
                        .find(|(i, _)| *i == *id)
                        .map(|(i, _)| i.clone())
                        .unwrap_or_else(|| normalize_tool_call_id(id));
                    part["functionCall"]["id"] = json!(normalized);
                    pending_tool_calls.push((normalized, name.clone()));
                } else {
                    pending_tool_calls.push((id.clone(), name.clone()));
                }
                parts.push(part);
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(json!({ "role": "model", "parts": parts }))
}

/// 把压缩/分支摘要消息包成带专用前缀后缀的 user 文本条目（Gemini 没有独立的摘要角色）。
fn convert_google_summary_message(msg: &AgentMessage) -> Value {
    let (prefix, suffix) = if msg.role == "compactionSummary" {
        (COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX)
    } else {
        (BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX)
    };
    let text = format!("{}{}{}", prefix, msg.text(), suffix);
    json!({
        "role": "user",
        "parts": [{ "text": sanitize_surrogates(&text) }]
    })
}

/// 推送 functionResponse 到 user 消息；相邻 functionResponse turn 合并。
fn push_google_tool_result(
    contents: &mut Vec<Value>,
    msg: &AgentMessage,
    include_id: bool,
    supports_images: bool,
    multimodal_function_response: bool,
    existing_tool_result_ids: &mut std::collections::HashSet<String>,
) {
    let text_result: Vec<String> = msg
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    let text_joined = text_result.join("\n");
    let image_parts: Vec<Value> = if supports_images {
        msg.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Image { data, mime_type } => Some(json!({
                    "inlineData": { "mimeType": mime_type, "data": data }
                })),
                _ => None,
            })
            .collect()
    } else {
        Vec::new()
    };
    let has_text = !text_joined.is_empty();
    let has_images = !image_parts.is_empty();
    let response_value = if has_text {
        sanitize_surrogates(&text_joined)
    } else if has_images {
        "(see attached image)".to_string()
    } else {
        String::new()
    };
    let tool_name = msg.tool_name.clone().unwrap_or_default();
    let tool_call_id = msg.tool_call_id.clone().unwrap_or_default();
    let mut part = json!({
        "functionResponse": {
            "name": tool_name,
            "response": if msg.is_error {
                json!({ "error": response_value })
            } else {
                json!({ "output": response_value })
            }
        }
    });

    if has_images && multimodal_function_response {
        part["functionResponse"]["parts"] = Value::Array(image_parts.clone());
    }

    if include_id {
        part["functionResponse"]["id"] = json!(tool_call_id);
        existing_tool_result_ids.insert(tool_call_id);
    } else {
        existing_tool_result_ids.insert(tool_call_id);
    }

    // 相邻 functionResponse user turn 合并
    if let Some(last) = contents.last_mut()
        && last.get("role") == Some(&json!("user"))
        && last["parts"]
            .as_array()
            .map(|a| a.iter().any(|p| p.get("functionResponse").is_some()))
            .unwrap_or(false)
        && let Some(arr) = last["parts"].as_array_mut()
    {
        arr.push(part);
        return;
    }

    contents.push(json!({ "role": "user", "parts": [part] }));
}

/// 把整段消息历史转成 Gemini `contents` 数组（user / assistant / toolResult / 摘要）。
///
/// 每次角色切换前都会补齐缺失的 tool 响应，保证 functionCall 与 functionResponse 成对出现。
pub(crate) fn convert_google_messages(
    messages: &[AgentMessage],
    _system_prompt: &str,
    model: &ModelConfig,
) -> Vec<Value> {
    let mut contents: Vec<Value> = Vec::new();
    let mut pending_tool_calls: Vec<(String, String)> = Vec::new();
    let mut existing_tool_result_ids: HashSet<String> = HashSet::new();

    let include_id = requires_tool_call_id(&model.model_id);
    let supports_images = model.input.iter().any(|x| x == "image");
    // gemini 3+ 支持 function response 内嵌图片
    let multimodal_function_response = supports_images;

    for msg in messages {
        match msg.role.as_str() {
            "user" => {
                insert_synthetic_tool_results(
                    &mut contents,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                    include_id,
                    "",
                );
                if let Some(user_msg) = convert_google_user_message(msg, supports_images) {
                    contents.push(user_msg);
                }
            }
            "assistant" => {
                insert_synthetic_tool_results(
                    &mut contents,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                    include_id,
                    "",
                );
                if let Some(assistant_msg) = convert_google_assistant_message(
                    msg,
                    model,
                    include_id,
                    &mut pending_tool_calls,
                ) {
                    contents.push(assistant_msg);
                }
            }
            "compactionSummary" | "branchSummary" => {
                insert_synthetic_tool_results(
                    &mut contents,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                    include_id,
                    "",
                );
                contents.push(convert_google_summary_message(msg));
            }
            "toolResult" => {
                push_google_tool_result(
                    &mut contents,
                    msg,
                    include_id,
                    supports_images,
                    multimodal_function_response,
                    &mut existing_tool_result_ids,
                );
            }
            _ => {}
        }
    }

    insert_synthetic_tool_results(
        &mut contents,
        &mut pending_tool_calls,
        &mut existing_tool_result_ids,
        include_id,
        "",
    );

    contents
}

/// 把工具定义列表转成 `{functionDeclarations: [...]}`，参数用 `parametersJsonSchema` 字段。
fn convert_google_tools(tool_defs: &[(String, String, Value)]) -> Value {
    let declarations: Vec<Value> = tool_defs
        .iter()
        .map(|(name, description, parameters)| {
            json!({
                "name": name,
                "description": description,
                "parametersJsonSchema": parameters
            })
        })
        .collect();
    json!({ "functionDeclarations": declarations })
}

/// 把 Gemini 的 finishReason 映射为内部 stop_reason；未知原因归为 `error` 并附带原文说明。
fn map_google_stop_reason(reason: &str) -> Result<(&'static str, Option<String>)> {
    match reason {
        "STOP" => Ok(("stop", None)),
        "MAX_TOKENS" => Ok(("length", None)),
        other => Ok(("error", Some(format!("Provider stopped with: {}", other)))),
    }
}

/// Gemini SSE 解析：data 行累积，空行 flush（复用 responses 的累积器模式）
struct GoogleSse {
    /// 尚未按换行切分的原始字节，跨 chunk 累积以免多字节 UTF-8 被截断
    buf: Vec<u8>,
    /// 当前事件已累积的 `data:` 行内容，遇空行时合并为一个事件并清空
    data_lines: Vec<String>,
}

impl GoogleSse {
    /// 建一个空的 SSE 累积器。
    fn new() -> Self {
        GoogleSse {
            buf: Vec::new(),
            data_lines: Vec::new(),
        }
    }

    /// 追加一段原始字节，按空行切分出完整的 SSE 事件（同一事件的 `data:` 行以 `\n` 连接）。
    ///
    /// 未消费的尾部字节留在 `buf` 里等后续 chunk，`[DONE]` 标记被丢弃；返回值可能是 0 到多个事件。
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut i = 0usize;
        let bytes = &self.buf;
        while i < bytes.len() {
            let mut line_end = i;
            while line_end < bytes.len() && bytes[line_end] != b'\n' {
                line_end += 1;
            }
            let at_eof = line_end >= bytes.len();
            let line = String::from_utf8_lossy(&bytes[i..line_end]);
            let l = line.strip_suffix('\r').unwrap_or(&line);
            if l.is_empty() {
                if !self.data_lines.is_empty() {
                    events.push(self.data_lines.join("\n"));
                    self.data_lines.clear();
                }
            } else if let Some(d) = l.strip_prefix("data:") {
                let d = d.strip_prefix(' ').unwrap_or(d);
                if d == "[DONE]" {
                    // 流结束标记
                } else {
                    self.data_lines.push(d.to_string());
                }
            }
            if at_eof {
                break;
            }
            i = line_end + 1;
        }
        let consumed = i;
        if consumed > 0 {
            self.buf.drain(..consumed);
        }
        events
    }
}

/// 流式块：text 与 thinking 交替出现，functionCall 打断当前块
enum ActiveBlock {
    /// 正在累积的正文文本块
    Text {
        /// 已拼接的可见文本增量
        text: String,
        /// 服务端回传的 thoughtSignature，用于续接推理链（base64，可为空）
        signature: String,
    },
    /// 正在累积的思考过程块
    Thinking {
        /// 已拼接的思考文本增量
        thinking: String,
        /// 服务端回传的 thoughtSignature，非空时随消息回传（base64）
        signature: String,
    },
}

/// 按模型能力钳制 thinking 级别；"off" 归一到 None。
fn normalize_google_reasoning_effort(
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

/// 构建并序列化 google generateContent 请求 body。
fn build_google_body(
    model: &ModelConfig,
    contents: Vec<Value>,
    system_prompt: &str,
    tools: &[(String, String, Value)],
    reasoning_effort: Option<&str>,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
) -> Result<String> {
    let mut body = Map::new();
    body.insert("contents".into(), Value::Array(contents));
    if !system_prompt.is_empty() {
        body.insert(
            "systemInstruction".into(),
            json!({ "parts": [{ "text": sanitize_surrogates(system_prompt) }] }),
        );
    }
    if !tools.is_empty() {
        body.insert("tools".into(), convert_google_tools(tools));
    }
    let mut generation_config = Map::new();
    if let Some(t) = temperature
        && reasoning_effort.is_none()
    {
        generation_config.insert("temperature".into(), json!(t));
    }
    if let Some(mt) = max_tokens.or(model.max_tokens) {
        generation_config.insert("maxOutputTokens".into(), json!(mt));
    }
    if !generation_config.is_empty() {
        body.insert("generationConfig".into(), Value::Object(generation_config));
    }
    if model.reasoning {
        if let Some(level) = reasoning_effort {
            let thinking_level = resolve_google_thinking_level(model, level);
            body.insert(
                "thinkingConfig".into(),
                json!({
                    "includeThoughts": true,
                    "thinkingLevel": thinking_level
                }),
            );
        } else {
            body.insert("thinkingConfig".into(), disabled_thinking_config(model));
        }
    }

    serde_json::to_string(&body).map_err(|source| Error::Json {
        context: "failed to serialize google request".to_string(),
        source,
    })
}

/// 收尾进行中的 text/thinking 块并发出对应 End 事件。
fn finish_google_current(
    current: &mut Option<(usize, ActiveBlock)>,
    content_blocks: &mut Vec<ContentBlock>,
    on_event: &mut Option<&mut PartialTranslator<'_>>,
) {
    if let Some((ci, block)) = current.take() {
        match block {
            ActiveBlock::Text { text, .. } => {
                if !text.is_empty() {
                    content_blocks.push(ContentBlock::Text {
                        text: text.clone(),
                        text_signature: None,
                    });
                    if let Some(f) = on_event.as_mut() {
                        f.push(RawStreamEvent::TextEnd {
                            content_index: ci,
                            content: text,
                        });
                    }
                }
            }
            ActiveBlock::Thinking {
                thinking,
                signature,
            } => {
                if !thinking.trim().is_empty() {
                    content_blocks.push(ContentBlock::Thinking {
                        thinking: thinking.clone(),
                        thinking_signature: (!signature.is_empty()).then_some(signature),
                        redacted: None,
                    });
                    if let Some(f) = on_event.as_mut() {
                        f.push(RawStreamEvent::ThinkingEnd {
                            content_index: ci,
                            content: thinking,
                        });
                    }
                }
            }
        }
    }
}

/// 处理 functionCall part：打断当前块后直接落为 ToolCall content block。
fn handle_google_function_call_part(
    part: &Value,
    content_blocks: &mut Vec<ContentBlock>,
    on_event: &mut Option<&mut PartialTranslator<'_>>,
) {
    let Some(function_call) = part.get("functionCall") else {
        return;
    };
    let name = function_call
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let args = function_call.get("args").cloned().unwrap_or(json!({}));
    let provided_id = function_call
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let needs_new_id = provided_id.is_none()
        || content_blocks.iter().any(|b| {
            matches!(b, ContentBlock::ToolCall { id, .. } if Some(id.as_str()) == provided_id.as_deref())
        });
    let tool_call_id = if needs_new_id {
        format!(
            "{}_{}_{}",
            name,
            now_ms(),
            TOOL_CALL_COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    } else {
        provided_id.clone().unwrap_or_default()
    };
    let ci = content_blocks.len();
    content_blocks.push(ContentBlock::ToolCall {
        id: tool_call_id.clone(),
        name: name.clone(),
        arguments: args.clone(),
        thought_signature: part
            .get("thoughtSignature")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        namespace: None,
    });
    if let Some(f) = on_event.as_mut() {
        f.push(RawStreamEvent::ToolCallStart { content_index: ci });
        f.push(RawStreamEvent::ToolCallDelta {
            content_index: ci,
            delta: serde_json::to_string(&args).unwrap_or_default(),
        });
        f.push(RawStreamEvent::ToolCallEnd {
            content_index: ci,
            id: tool_call_id,
            name,
            arguments: args,
        });
    }
}

/// 处理一个 candidate 的 parts 数组：text/thinking 增量与 functionCall。
fn handle_google_parts(
    parts: &[Value],
    current: &mut Option<(usize, ActiveBlock)>,
    content_blocks: &mut Vec<ContentBlock>,
    next_content_index: &mut usize,
    on_event: &mut Option<&mut PartialTranslator<'_>>,
) {
    for part in parts {
        if let Some(part_text) = part.get("text").and_then(|t| t.as_str()) {
            let is_thinking = part
                .get("thought")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let signature = part
                .get("thoughtSignature")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let type_mismatch = match current {
                Some((_, ActiveBlock::Text { .. })) => is_thinking,
                Some((_, ActiveBlock::Thinking { .. })) => !is_thinking,
                None => false,
            };
            if current.is_none() || type_mismatch {
                finish_google_current(current, content_blocks, on_event);
                let ci = *next_content_index;
                *next_content_index += 1;
                if is_thinking {
                    current.replace((
                        ci,
                        ActiveBlock::Thinking {
                            thinking: part_text.to_string(),
                            signature: signature.to_string(),
                        },
                    ));
                    if let Some(f) = on_event.as_mut() {
                        f.push(RawStreamEvent::ThinkingStart { content_index: ci });
                    }
                } else {
                    current.replace((
                        ci,
                        ActiveBlock::Text {
                            text: part_text.to_string(),
                            signature: signature.to_string(),
                        },
                    ));
                    if let Some(f) = on_event.as_mut() {
                        f.push(RawStreamEvent::TextStart { content_index: ci });
                    }
                }
            } else if let Some((_ci, block)) = current.as_mut() {
                // 追加到进行中块；retainThoughtSignature：incoming 非空才覆盖
                match block {
                    ActiveBlock::Text { text, signature } => {
                        text.push_str(part_text);
                        if !signature.is_empty() {
                            *signature = signature.to_string();
                        }
                    }
                    ActiveBlock::Thinking {
                        thinking,
                        signature,
                    } => {
                        thinking.push_str(part_text);
                        if !signature.is_empty() {
                            *signature = signature.to_string();
                        }
                    }
                }
            }
            // 每个 part 片段发一次 delta（首段也在启动后发）
            if let Some((ci, block)) = current.as_mut() {
                match block {
                    ActiveBlock::Text { .. } => {
                        if let Some(f) = on_event.as_mut() {
                            f.push(RawStreamEvent::TextDelta {
                                content_index: *ci,
                                delta: part_text.to_string(),
                            });
                        }
                    }
                    ActiveBlock::Thinking { .. } => {
                        if let Some(f) = on_event.as_mut() {
                            f.push(RawStreamEvent::ThinkingDelta {
                                content_index: *ci,
                                delta: part_text.to_string(),
                            });
                        }
                    }
                }
            }
        }
        if part.get("functionCall").is_some() {
            finish_google_current(current, content_blocks, on_event);
            handle_google_function_call_part(part, content_blocks, on_event);
        }
    }
}

/// 从 candidate 读 finishReason 写入 stop_reason/error_message；已有 ToolCall 时把 `stop` 改判为 `toolUse`。
fn handle_google_finish_reason(
    candidate: &Value,
    finish_reason: &mut Option<String>,
    stop_reason: &mut Option<String>,
    error_message: &mut Option<String>,
    content_blocks: &[ContentBlock],
) -> Result<()> {
    if let Some(fr) = candidate.get("finishReason").and_then(|v| v.as_str()) {
        *finish_reason = Some(fr.to_string());
        let (mapped, em) = map_google_stop_reason(fr)?;
        *stop_reason = Some(mapped.to_string());
        *error_message = em;
        if content_blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
            && stop_reason.as_deref() == Some("stop")
        {
            *stop_reason = Some("toolUse".to_string());
        }
    }
    Ok(())
}

/// 解析 usageMetadata 覆盖写入 usage（input 扣除缓存命中、output 计入思考 token）并算好成本；无该字段时不动。
fn handle_google_usage_metadata(ev: &Value, usage: &mut Option<Usage>, model: &ModelConfig) {
    let Some(um) = ev.get("usageMetadata") else {
        return;
    };
    let prompt_tokens = um
        .get("promptTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let cached = um
        .get("cachedContentTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let candidates_tokens = um
        .get("candidatesTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let thoughts_tokens = um
        .get("thoughtsTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let total = um
        .get("totalTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let mut parsed = Usage {
        input: prompt_tokens.saturating_sub(cached),
        output: candidates_tokens + thoughts_tokens,
        cache_read: cached,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: (thoughts_tokens > 0).then_some(thoughts_tokens),
        total_tokens: total,
        cost: Cost::default(),
    };
    compute_cost(&mut parsed, model.cost.as_ref());
    *usage = Some(parsed);
}

/// 组装流式结果：把累积的 content block 包成 assistant 消息，带上 stop_reason、错误与 usage。
fn build_google_result(
    content_blocks: Vec<ContentBlock>,
    stop_reason: Option<String>,
    error_message: Option<String>,
    usage: Option<Usage>,
    _response_id: Option<String>,
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
        api: Some("google-generative-ai".to_string()),
        usage: usage.clone(),
        response_model: None,
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

/// 发起 Gemini 流式请求并解析 SSE，返回归一化后的 [`StreamResult`]。
///
/// `on_event` 接收增量/结束事件（[`PartialTranslator`]），`on_payload` 在发送前改写请求体；
/// 网络失败、非 2xx 响应或流结束仍未给出 finishReason 时返回 `Err`。
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
    on_payload: Option<crate::core::provider::PayloadHook>,
) -> Result<StreamResult> {
    let reasoning_effort = normalize_google_reasoning_effort(model, reasoning_effort);

    let client = http::build_client()?;
    let contents = convert_google_messages(messages, system_prompt, model);

    let mut body_json = build_google_body(
        model,
        contents,
        system_prompt,
        tools,
        reasoning_effort.as_deref(),
        temperature,
        max_tokens,
    )?;

    apply_payload_hook(&mut body_json, on_payload.as_ref());

    let response = send_with_retry(
        || {
            client
                .post(endpoint(model))
                .header("Content-Type", "application/json")
                .header("x-goog-api-key", &model.api_key)
                .body(body_json.clone())
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
    let mut stop_reason: Option<String> = None;
    let mut error_message: Option<String> = None;
    let mut usage: Option<Usage> = None;
    let mut response_id: Option<String> = None;

    // 块状态：current 为进行中的 text/thinking 块
    let mut current: Option<(usize, ActiveBlock)> = None;
    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    let mut next_content_index = 0usize;

    let mut finish_reason: Option<String> = None;
    let mut sse = GoogleSse::new();
    let mut total_bytes: usize = 0;

    // 供应商原始流事件：无扩展订阅时零开销（一次流只查一次）
    let capture_stream_events = core::extensions::has_provider_stream_event_handlers();

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => return Err(Error::msg(format!("stream read failed: {}", e))),
        };
        track_stream_bytes(&mut total_bytes, chunk.len())?;
        for data in sse.push(&chunk) {
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
            if let Some(rid) = ev.get("responseId").and_then(|v| v.as_str())
                && response_id.is_none()
            {
                response_id = Some(rid.to_string());
            }
            let candidate = ev
                .get("candidates")
                .and_then(|c| c.as_array())
                .and_then(|a| a.first());
            let Some(candidate) = candidate else { continue };

            if let Some(parts) = candidate
                .pointer("/content/parts")
                .and_then(|p| p.as_array())
            {
                handle_google_parts(
                    parts,
                    &mut current,
                    &mut content_blocks,
                    &mut next_content_index,
                    &mut on_event,
                );
            }

            handle_google_finish_reason(
                candidate,
                &mut finish_reason,
                &mut stop_reason,
                &mut error_message,
                &content_blocks,
            )?;
            handle_google_usage_metadata(&ev, &mut usage, model);
        }
    }

    finish_google_current(&mut current, &mut content_blocks, &mut on_event);

    if stop_reason.is_none() {
        return Err(Error::msg("Google stream ended without a finish reason"));
    }

    build_google_result(
        content_blocks,
        stop_reason,
        error_message,
        usage,
        response_id,
        model,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::ModelConfig;

    fn model(id: &str) -> ModelConfig {
        ModelConfig {
            model_type: Default::default(),
            image_resize: crate::utils::image::ImageResizeLimits::default(),
            allowed_fallback_models: Vec::new(),
            provider: "opencode".into(),
            model_id: id.into(),
            base_url: "https://opencode.ai/zen/v1".into(),
            api_key: "sk-test".into(),
            api: "google-generative-ai".into(),
            output: Vec::new(),
            input: vec!["text".into(), "image".into()],
            reasoning: true,
            max_tokens: None,
            temperature: None,
            context_window: 1_048_576,
            thinking_format: String::new(),
            supports_reasoning_effort: false,
            thinking_level_map: Some(
                [
                    ("off", None),
                    ("minimal", Some("MINIMAL")),
                    ("low", Some("LOW")),
                    ("medium", Some("MEDIUM")),
                    ("high", Some("HIGH")),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.map(|s| s.to_string())))
                .collect(),
            ),
            auth_header: false,
            supports_developer_role: false,
            requires_reasoning_content_on_assistant_messages: false,
            supports_usage_in_streaming: true,
            supports_store: true,
            supports_finish_reason: true,
            requires_assistant_after_tool_result: false,
            max_tokens_field: "maxOutputTokens".into(),
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
    fn user_message_becomes_parts() {
        let m = model("gemini-3-flash");
        let out = convert_google_messages(&[AgentMessage::user_text("hi")], "", &m);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[0]["parts"][0]["text"], "hi");
    }

    #[test]
    fn assistant_blocks_map_to_model_role() {
        let m = model("gemini-3-flash");
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.content = vec![
            ContentBlock::Text {
                text: "answer".into(),
                text_signature: None,
            },
            ContentBlock::ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: json!({ "path": "a.rs" }),
                thought_signature: None,
                namespace: None,
            },
        ];
        let out = convert_google_messages(&[a], "", &m);
        assert_eq!(out.len(), 2); // model 块 + synthetic tool result
        assert_eq!(out[0]["role"], "model");
        assert_eq!(out[0]["parts"][0]["text"], "answer");
        assert_eq!(out[0]["parts"][1]["functionCall"]["name"], "read");
        // gemini 3+ 需要显式 tool call id
        assert_eq!(out[0]["parts"][1]["functionCall"]["id"], "call_1");
    }

    #[test]
    fn thinking_same_model_keeps_thought_flag() {
        let m = model("gemini-3.1-pro");
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.provider = Some("opencode".into());
        a.model = Some("gemini-3.1-pro".into());
        a.content = vec![ContentBlock::Thinking {
            thinking: "reasoning".into(),
            thinking_signature: Some("AAAABBBB".into()),
            redacted: None,
        }];
        let out = convert_google_messages(&[a], "", &m);
        assert_eq!(out[0]["parts"][0]["thought"], true);
        assert_eq!(out[0]["parts"][0]["thoughtSignature"], "AAAABBBB");
    }

    #[test]
    fn thinking_cross_model_becomes_plain_text() {
        let m = model("gemini-3.1-pro");
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.provider = Some("anthropic".into());
        a.content = vec![ContentBlock::Thinking {
            thinking: "old reasoning".into(),
            thinking_signature: Some("not-base64!!".into()),
            redacted: None,
        }];
        let out = convert_google_messages(&[a], "", &m);
        assert_eq!(out[0]["parts"][0]["text"], "old reasoning");
        assert!(out[0]["parts"][0].get("thought").is_none());
    }

    #[test]
    fn tool_result_becomes_function_response() {
        let m = model("gemini-3-flash");
        let mut tr = AgentMessage::user_text("");
        tr.role = "toolResult".into();
        tr.tool_call_id = Some("call_1".into());
        tr.tool_name = Some("read".into());
        tr.content = vec![ContentBlock::Text {
            text: "contents".into(),
            text_signature: None,
        }];
        let out = convert_google_messages(&[tr], "", &m);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[0]["parts"][0]["functionResponse"]["name"], "read");
        assert_eq!(
            out[0]["parts"][0]["functionResponse"]["response"]["output"],
            "contents"
        );
        // gemini 3+ 带 id
        assert_eq!(out[0]["parts"][0]["functionResponse"]["id"], "call_1");
    }

    #[test]
    fn consecutive_function_responses_merge() {
        let m = model("gemini-3-flash");
        let mk = |id: &str| {
            let mut tr = AgentMessage::user_text("");
            tr.role = "toolResult".into();
            tr.tool_call_id = Some(id.into());
            tr.tool_name = Some("read".into());
            tr.content = vec![ContentBlock::Text {
                text: "out".into(),
                text_signature: None,
            }];
            tr
        };
        let out = convert_google_messages(&[mk("c1"), mk("c2")], "", &m);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["parts"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn orphan_tool_call_gets_synthetic_response() {
        let m = model("gemini-3-flash");
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.content = vec![ContentBlock::ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            arguments: json!({}),
            thought_signature: None,
            namespace: None,
        }];
        let out = convert_google_messages(&[a], "", &m);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[1]["parts"][0]["functionResponse"]["response"]["error"],
            "No result provided"
        );
    }

    #[test]
    fn requires_id_only_for_gemini3_plus() {
        assert!(requires_tool_call_id("gemini-3-flash"));
        assert!(requires_tool_call_id("gemini-3.1-pro"));
        assert!(requires_tool_call_id("claude-sonnet-5"));
        assert!(!requires_tool_call_id("gemini-2.5-pro"));
    }

    #[test]
    fn resolve_google_thinking_level_uses_map_then_level() {
        // thinkingLevelMap 命中优先（小写归一后大写）
        let m = model("gemini-3.1-pro");
        assert_eq!(resolve_google_thinking_level(&m, "low"), "LOW");
        assert_eq!(resolve_google_thinking_level(&m, "high"), "HIGH");
        // 无映射时直接用级别（pi 不再按模型族改判 medium/high）
        let mut no_map = model("gemini-3.1-pro");
        no_map.thinking_level_map = None;
        assert_eq!(resolve_google_thinking_level(&no_map, "medium"), "MEDIUM");
        assert_eq!(resolve_google_thinking_level(&no_map, "high"), "HIGH");
    }

    #[test]
    fn uses_google_thinking_level_matches_pi_patterns() {
        assert!(uses_google_thinking_level("gemini-3-pro-preview"));
        assert!(uses_google_thinking_level("gemini-3.1-flash"));
        assert!(uses_google_thinking_level("gemini-3.8-flash"));
        assert!(uses_google_thinking_level("gemini-flash-latest"));
        assert!(uses_google_thinking_level("gemini-flash-lite-latest"));
        assert!(uses_google_thinking_level("gemma-4-31b-it"));
        assert!(uses_google_thinking_level("gemma4-9b"));
        assert!(!uses_google_thinking_level("gemini-2.5-pro"));
        assert!(!uses_google_thinking_level("gemini-3-ultra"));
    }

    #[test]
    fn disabled_thinking_config_uses_lowest_supported_level() {
        // 真实 3.1 Pro：minimal 不受支持 → 钳制到 low
        let mut pro = model("gemini-3.1-pro");
        pro.thinking_level_map = Some(
            [
                ("off", None),
                ("minimal", None),
                ("low", Some("low")),
                ("medium", Some("medium")),
                ("high", Some("high")),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.map(|s| s.to_string())))
            .collect(),
        );
        assert_eq!(disabled_thinking_config(&pro)["thinkingLevel"], "LOW");
        // 3-flash：minimal 可用
        let flash = model("gemini-3.6-flash");
        assert_eq!(disabled_thinking_config(&flash)["thinkingLevel"], "MINIMAL");
        // gemma-4：最低 level 为 minimal（旧实现错发 thinkingBudget:0）
        let gemma = model("gemma-4-31b-it");
        assert_eq!(disabled_thinking_config(&gemma)["thinkingLevel"], "MINIMAL");
        // 非 thinkingLevel 模型（2.5）：thinkingBudget 0
        let legacy = model("gemini-2.5-pro");
        assert_eq!(disabled_thinking_config(&legacy)["thinkingBudget"], 0);
    }

    #[test]
    fn thought_signature_validation() {
        assert!(valid_thought_signature("AAAA"));
        assert!(valid_thought_signature("AAAABBBB"));
        assert!(!valid_thought_signature("not base64!"));
        assert!(!valid_thought_signature("ABC")); // 长度非 4 倍数
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(map_google_stop_reason("STOP").unwrap().0, "stop");
        assert_eq!(map_google_stop_reason("MAX_TOKENS").unwrap().0, "length");
        assert_eq!(map_google_stop_reason("SAFETY").unwrap().0, "error");
    }

    #[test]
    fn text_signed_empty_block_kept_same_model() {
        // 对齐 pi 0.84.0：Gemini 会把签名附着在可见文本为空的 part 上，
        // 同 provider/model 时空 text + 合法签名必须保留并回传 thoughtSignature。
        let m = model("gemini-3.1-pro");
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.provider = Some("opencode".into());
        a.model = Some("gemini-3.1-pro".into());
        a.content = vec![ContentBlock::Text {
            text: String::new(),
            text_signature: Some("AAAABBBB".into()),
        }];
        let out = convert_google_messages(&[a], "", &m);
        assert_eq!(out[0]["parts"][0]["text"], "");
        assert_eq!(out[0]["parts"][0]["thoughtSignature"], "AAAABBBB");
    }

    #[test]
    fn text_signed_empty_block_dropped_cross_model_or_bad_sig() {
        let m = model("gemini-3.1-pro");
        // 跨 provider/model：签名不可用，空 text 块照旧丢弃
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.provider = Some("anthropic".into());
        a.content = vec![ContentBlock::Text {
            text: String::new(),
            text_signature: Some("AAAABBBB".into()),
        }];
        let out = convert_google_messages(&[a], "", &m);
        assert!(out.is_empty() || out[0]["parts"].as_array().unwrap().is_empty());

        // 签名非法 base64：等价于无签名，空 text 丢弃
        let mut b = AgentMessage::user_text("");
        b.role = "assistant".into();
        b.provider = Some("opencode".into());
        b.model = Some("gemini-3.1-pro".into());
        b.content = vec![ContentBlock::Text {
            text: String::new(),
            text_signature: Some("not-base64!!".into()),
        }];
        let out2 = convert_google_messages(&[b], "", &m);
        assert!(out2.is_empty() || out2[0]["parts"].as_array().unwrap().is_empty());
    }

    #[test]
    fn text_signed_nonempty_block_keeps_signature() {
        let m = model("gemini-3.1-pro");
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.provider = Some("opencode".into());
        a.model = Some("gemini-3.1-pro".into());
        a.content = vec![ContentBlock::Text {
            text: "answer".into(),
            text_signature: Some("AAAABBBB".into()),
        }];
        let out = convert_google_messages(&[a], "", &m);
        assert_eq!(out[0]["parts"][0]["text"], "answer");
        assert_eq!(out[0]["parts"][0]["thoughtSignature"], "AAAABBBB");
    }
}
