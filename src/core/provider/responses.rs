//! OpenAI Responses 协议（gpt-5.x 等）
//!
//! 消息转换要点：
//! - reasoning item（含 encrypted_content）以 `thinkingSignature` 整条 JSON 透传
//! - assistant text 生成 `message` item，id 用 fallback `msg_pi_{n}`
//! - tool call id 存复合格式 `call_id|item_id`，重放时拆分；外来 fc_ id 跨模型时置空
//! - tool result 输出 `function_call_output`
//!
//! 流式要点：response.output_item.added 建 slot，delta 事件累积，output_item.done 定稿，
//! response.completed/incomplete 结算 usage 与 stop reason。

use super::{
    AgentMessage, ContentBlock, Cost, ModelConfig, PartialTranslator, RawStreamEvent, StreamResult,
    Usage, azure,
    convert::{
        BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX, COMPACTION_SUMMARY_PREFIX,
        COMPACTION_SUMMARY_SUFFIX, sanitize_surrogates, truncate_for_error,
    },
    retry::{DEFAULT_MAX_RETRIES, send_with_retry},
    track_stream_bytes,
    usage::{apply_service_tier_cost, compute_cost, parse_streaming_json},
};
use crate::{
    core::{
        self,
        provider::{
            PayloadHook, apply_payload_hook, grammar_input_property_for_tool, grammar_tool,
        },
    },
    error::{Error, Result},
    utils::{http, time::now_ms},
};
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

/// OpenAI Responses 拒绝 max_output_tokens < 16
pub(crate) const MIN_OUTPUT_TOKENS: u32 = 16;

/// 走「callId|itemId 拆分」路径的 provider（opencode）
const ALLOWED_TOOL_CALL_PROVIDERS: &[&str] = &["opencode"];

/// Sign in with ChatGPT 的用量上限错误：不重试，并附 ChatGPT 用量页。
const CHATGPT_USAGE_URL: &str = "https://chatgpt.com/settings/usage";

/// 非 2xx 响应体 → 错误文案（ChatGPT 订阅用量上限附用量页链接）。
fn provider_error_body(text: &str) -> String {
    let body = truncate_for_error(text);
    if text.contains("subscription_sharing_usage_limit_exceeded") {
        format!("{}\nCheck your ChatGPT usage: {}", body, CHATGPT_USAGE_URL)
    } else {
        body
    }
}

/// OpenAI API key 以 `sk-` 开头；直连 api.openai.com 的其它凭据即 ChatGPT 订阅签发的 access token。
fn is_chatgpt_sign_in(model: &ModelConfig) -> bool {
    model.provider == "openai"
        && model.base_url.trim_end_matches('/') == "https://api.openai.com/v1"
        && !model.api_key.is_empty()
        && !model.api_key.starts_with("sk-")
}

/// Responses 接口地址：base_url 去掉尾部 '/' 后追加 `/responses`。
fn endpoint(model: &ModelConfig) -> String {
    azure::with_api_version(
        model,
        format!("{}/responses", model.base_url.trim_end_matches('/')),
    )
}

/// 把 id 片段规整为 Responses 接受的字符集：`[A-Za-z0-9_-]` 之外一律替换为 `_`，
/// 截断到 64 字符并去掉尾部下划线。
fn normalize_id_part(part: &str) -> String {
    let sanitized: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let truncated: String = sanitized.chars().take(64).collect();
    truncated.trim_end_matches('_').to_string()
}

/// 把工具结果的 content blocks 转成 `function_call_output` 的 output 字段。
///
/// 只有文本时输出字符串；含图片且模型支持视觉时输出 `input_text`/`input_image` 数组；
/// 模型不支持图片、或既无文本又无图片时退化为占位文案。
fn convert_tool_result_output(model: &ModelConfig, content: &[ContentBlock]) -> Value {
    let text_result: String = content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images: Vec<&ContentBlock> = content
        .iter()
        .filter(|b| matches!(b, ContentBlock::Image { .. }))
        .collect();
    let has_text = !text_result.is_empty();
    let supports_images = model.input.iter().any(|x| x == "image");
    if images.is_empty() || !supports_images {
        return json!(sanitize_surrogates(if has_text {
            text_result.as_str()
        } else if !images.is_empty() {
            "(see attached image)"
        } else {
            "(no tool output)"
        }));
    }
    let mut output: Vec<Value> = Vec::new();
    if has_text {
        output.push(json!({ "type": "input_text", "text": sanitize_surrogates(&text_result) }));
    }
    for b in &images {
        if let ContentBlock::Image { data, mime_type } = b {
            output.push(json!({
                "type": "input_image",
                "detail": "auto",
                "image_url": format!("data:{};base64,{}", mime_type, data),
            }));
        }
    }
    Value::Array(output)
}

/// 当前模型是否支持 grammar 约束工具，决定 tool call 回放成 `custom_tool_call` 还是 `function_call`。
///
/// 与下发侧（[`convert_responses_tools_with_capabilities`] 传的 `grammar`）保持同一判据：
/// 不支持该形状的上游收到 `custom_tool_call` 会 400，而且这条历史一旦落库，
/// 之后每次请求都会 400（会话被毒化）。
fn responses_grammar_supported(model: &ModelConfig) -> bool {
    core::model_resolver::supports_openai_grammar_tools(&model.provider, &model.model_id)
}

/// 为未配对的 tool call 补 synthetic tool result item（`output` 为占位文案）。
///
/// `grammar_supported` 为 true 时，grammar 工具（如 `codemode`）的补位 item 用
/// `custom_tool_call_output`，否则用 `function_call_output`（必须与回放的 call item 同类型）。
/// `pending` 清空，`existing` 也一并清空（已配对结果只在本批次内去重）。
fn insert_synthetic_tool_results(
    out: &mut Vec<Value>,
    pending: &mut Vec<(String, String)>,
    existing: &mut HashSet<String>,
    grammar_supported: bool,
) {
    if pending.is_empty() {
        return;
    }
    for (call_id, name) in pending.drain(..) {
        if existing.contains(&call_id) {
            continue;
        }

        // grammar 工具的调用项是 `custom_tool_call`，配对的结果必须用同一个类型
        let item_type = if grammar_supported && grammar_input_property_for_tool(&name).is_some() {
            "custom_tool_call_output"
        } else {
            "function_call_output"
        };

        out.push(json!({
            "type": item_type,
            "call_id": call_id,
            "output": "No result provided"
        }));
    }
    existing.clear();
}

/// 把 user 消息转成 Responses input item；没有任何可用内容块时返回 `None`。
///
/// `supports_images` 为 false 时图片降级为一行占位文本，连续多张图只提示一次。
fn convert_responses_user_message(msg: &AgentMessage, supports_images: bool) -> Option<Value> {
    let mut content_arr: Vec<Value> = Vec::new();
    let mut previous_was_placeholder = false;
    for block in &msg.content {
        match block {
            ContentBlock::Text { text, .. } => {
                content_arr.push(json!({
                    "type": "input_text",
                    "text": sanitize_surrogates(text)
                }));
                previous_was_placeholder = text == "(image omitted: model does not support images)";
            }
            ContentBlock::Image { data, mime_type } => {
                if supports_images {
                    content_arr.push(json!({
                        "type": "input_image",
                        "detail": "auto",
                        "image_url": format!("data:{};base64,{}", mime_type, data)
                    }));
                    previous_was_placeholder = false;
                } else if !previous_was_placeholder {
                    content_arr.push(json!({
                        "type": "input_text",
                        "text": "(image omitted: model does not support images)"
                    }));
                    previous_was_placeholder = true;
                }
            }
            _ => {}
        }
    }
    if content_arr.is_empty() {
        return None;
    }
    Some(json!({ "role": "user", "content": content_arr }))
}

/// 转换 assistant 消息为 responses output item 数组；tool call id 按
/// 白名单 provider 归一化并记入 pending,供 synthetic tool result 使用。
fn convert_responses_assistant_message(
    msg: &AgentMessage,
    model: &ModelConfig,
    msg_index: usize,
    pending_tool_calls: &mut Vec<(String, String)>,
) -> Vec<Value> {
    if msg.stop_reason.as_deref() == Some("error") || msg.stop_reason.as_deref() == Some("aborted")
    {
        return Vec::new();
    }

    let is_same_model = msg.model.as_deref() == Some(model.model_id.as_str())
        && msg.provider.as_deref() == Some(model.provider.as_str())
        && msg.api.as_deref() == Some(model.api.as_str());

    let mut output_items: Vec<Value> = Vec::new();
    let mut text_block_index = 0usize;

    for block in &msg.content {
        match block {
            ContentBlock::Thinking {
                thinking,
                thinking_signature,
                ..
            } => {
                if let Some(sig) = thinking_signature {
                    // thinkingSignature 是完整 reasoning item JSON，原样透传
                    if let Ok(item) = serde_json::from_str::<Value>(sig) {
                        output_items.push(item);
                    }
                } else if !is_same_model && !thinking.trim().is_empty() {
                    // 外来 thinking（跨模型）：降级为文本
                    let id = if text_block_index == 0 {
                        format!("msg_pi_{}", msg_index)
                    } else {
                        format!("msg_pi_{}_{}", msg_index, text_block_index)
                    };
                    text_block_index += 1;
                    output_items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": sanitize_surrogates(thinking),
                            "annotations": []
                        }],
                        "status": "completed",
                        "id": id
                    }));
                }
                // 同模型无签名 thinking：responses 无意义，丢弃
            }
            ContentBlock::Text { text, .. } => {
                let id = if text_block_index == 0 {
                    format!("msg_pi_{}", msg_index)
                } else {
                    format!("msg_pi_{}_{}", msg_index, text_block_index)
                };
                text_block_index += 1;
                output_items.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": sanitize_surrogates(text),
                        "annotations": []
                    }],
                    "status": "completed",
                    "id": id
                }));
            }
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                let allowed = ALLOWED_TOOL_CALL_PROVIDERS.contains(&model.provider.as_str());
                let (call_id, item_id_raw) = if allowed {
                    id.split_once('|')
                        .map(|(a, b)| (a.to_string(), Some(b.to_string())))
                        .unwrap_or((id.clone(), None))
                } else {
                    // 非白名单 provider（如 opencode-go）：整个 id 归一化为单段
                    (normalize_id_part(id), None)
                };
                let normalized_call_id = if allowed {
                    normalize_id_part(&call_id)
                } else {
                    call_id
                };

                // grammar 工具用 `custom_tool_call` 回放（裸源码 input），否则是 `function_call`
                let grammar_property = responses_grammar_supported(model)
                    .then(|| grammar_input_property_for_tool(name))
                    .flatten();

                // 仅同模型 + 对应前缀的 item id 保留（function 为 fc_、custom 为 ctc_），否则置空避免 pairing 校验
                let item_id = if is_same_model && allowed {
                    let prefix = if grammar_property.is_some() {
                        "ctc_"
                    } else {
                        "fc_"
                    };
                    item_id_raw.filter(|raw| raw.starts_with(prefix))
                } else {
                    None
                };

                let mut item = if let Some(property) = grammar_property {
                    json!({
                        "type": "custom_tool_call",
                        "call_id": normalized_call_id,
                        "name": name,
                        "input": arguments
                            .get(&property)
                            .and_then(|v| v.as_str())
                            .unwrap_or_default(),
                    })
                } else {
                    json!({
                        "type": "function_call",
                        "call_id": normalized_call_id,
                        "name": name,
                        "arguments": serde_json::to_string(arguments)
                            .unwrap_or_else(|_| "{}".into())
                    })
                };

                if let Some(iid) = item_id {
                    item["id"] = json!(iid);
                }
                output_items.push(item);
                pending_tool_calls.push((normalized_call_id, name.clone()));
            }
            _ => {}
        }
    }
    output_items
}

/// 把压缩/分支摘要消息包上对应前后缀，转成一条 user 文本消息。
fn convert_responses_summary_message(msg: &AgentMessage) -> Value {
    let (prefix, suffix) = if msg.role == "compactionSummary" {
        (COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX)
    } else {
        (BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX)
    };
    let text = format!("{}{}{}", prefix, msg.text(), suffix);
    json!({
        "role": "user",
        "content": [{ "type": "input_text", "text": sanitize_surrogates(&text) }]
    })
}

/// 推送 function_call_output；call_id 按白名单 provider 归一化。
fn push_responses_tool_result(
    out: &mut Vec<Value>,
    msg: &AgentMessage,
    model: &ModelConfig,
    existing_tool_result_ids: &mut HashSet<String>,
) {
    let allowed = ALLOWED_TOOL_CALL_PROVIDERS.contains(&model.provider.as_str());
    let call_id = msg
        .tool_call_id
        .as_deref()
        .map(|id| {
            if allowed {
                normalize_id_part(id.split('|').next().unwrap_or(id))
            } else {
                normalize_id_part(id)
            }
        })
        .unwrap_or_default();
    existing_tool_result_ids.insert(call_id.clone());
    let output = convert_tool_result_output(model, &msg.content);

    // grammar 工具的调用项是 `custom_tool_call`，结果必须用同一个类型
    let item_type = msg
        .tool_name
        .as_deref()
        .filter(|name| {
            responses_grammar_supported(model) && grammar_input_property_for_tool(name).is_some()
        })
        .map(|_| "custom_tool_call_output")
        .unwrap_or("function_call_output");

    out.push(json!({
        "type": item_type,
        "call_id": call_id,
        "output": output
    }));
}

/// 把整段对话历史转成 Responses 的 `input` 数组。
///
/// 公共规则：图片降级（不支持视觉的模型）、tool call id 归一化 + toolResult 配对、
/// 孤儿 tool call 补 synthetic result、跳过 stopReason=error/aborted 的 assistant 消息。
/// system prompt 按模型能力选 developer/system 角色；随后按 role 分发到各转换函数，
/// 并在每条消息边界补齐缺失的 synthetic tool result。
pub(crate) fn convert_responses_messages(
    messages: &[AgentMessage],
    system_prompt: &str,
    model: &ModelConfig,
) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();

    if !system_prompt.is_empty() {
        let role = if model.reasoning && model.supports_developer_role {
            "developer"
        } else {
            "system"
        };
        out.push(json!({
            "role": role,
            "content": sanitize_surrogates(system_prompt)
        }));
    }

    // (归一化 call_id, name) → 供 synthetic tool result
    let mut pending_tool_calls: Vec<(String, String)> = Vec::new();
    let mut existing_tool_result_ids: HashSet<String> = HashSet::new();
    let supports_images = model.input.iter().any(|x| x == "image");
    let grammar_supported = responses_grammar_supported(model);

    for (msg_index, msg) in messages.iter().enumerate() {
        match msg.role.as_str() {
            "user" => {
                insert_synthetic_tool_results(
                    &mut out,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                    grammar_supported,
                );
                if let Some(user_msg) = convert_responses_user_message(msg, supports_images) {
                    out.push(user_msg);
                }
            }
            "assistant" => {
                insert_synthetic_tool_results(
                    &mut out,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                    grammar_supported,
                );
                out.extend(convert_responses_assistant_message(
                    msg,
                    model,
                    msg_index,
                    &mut pending_tool_calls,
                ));
            }
            "compactionSummary" | "branchSummary" => {
                insert_synthetic_tool_results(
                    &mut out,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                    grammar_supported,
                );
                out.push(convert_responses_summary_message(msg));
            }
            "toolResult" => {
                push_responses_tool_result(&mut out, msg, model, &mut existing_tool_result_ids);
            }
            _ => {}
        }
    }

    insert_synthetic_tool_results(
        &mut out,
        &mut pending_tool_calls,
        &mut existing_tool_result_ids,
        grammar_supported,
    );
    out
}

/// 把 `(名称, 描述, JSON schema)` 工具定义转成 Responses 的 function 工具数组。
/// 带能力的工具转换：`grammar` = 模型支持 grammar 约束工具（转为 `custom` 工具）。
pub(crate) fn convert_responses_tools_with_capabilities(
    tool_defs: &[(String, String, Value)],
    grammar: bool,
) -> Vec<Value> {
    tool_defs
        .iter()
        .map(|(name, description, parameters)| {
            if grammar && let Some(spec) = grammar_tool(parameters) {
                return json!({
                    "type": "custom",
                    "name": name,
                    "description": description,
                    "format": {
                        "type": "grammar",
                        "syntax": spec.syntax,
                        "definition": spec.definition,
                    }
                });
            }

            json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": parameters,
            })
        })
        .collect()
}

/// 进行中输出槽的类型：思考摘要、正文文本或工具调用。
#[derive(Debug, Clone, Copy, PartialEq)]
enum SlotKind {
    /// 模型的思考/推理摘要增量。
    Thinking,
    /// assistant 正文文本增量。
    Text,
    /// 工具调用的参数流式累积。
    ToolCall,
}

/// 按 output_index 归位的输出槽，累积增量文本与 tool call 参数直到定稿。
struct Slot {
    /// 该槽承载的输出类型，决定增量如何累积。
    kind: SlotKind,
    /// 在最终 content 列表中的保序下标，按 output item 出现顺序分配。
    content_index: usize,
    /// 正文增量缓冲（仅 Text 槽使用）。
    text: String,
    /// 思考摘要/推理增量缓冲（仅 Thinking 槽使用）。
    thinking: String,
    /// tool call 增量 JSON 缓冲
    partial_json: Option<String>,
    /// tool call 定稿 arguments
    arguments: Value,
    /// tool call：call_id 与 item_id
    call_id: Option<String>,
    /// 工具调用的响应侧 item id（`fc_` 前缀），跨模型重放时置空。
    item_id: Option<String>,
    /// 工具调用的函数名，来自 function_call item。
    name: Option<String>,
    /// OpenAI Responses 自定义工具命名空间
    namespace: Option<String>,
    /// grammar 约束工具调用的裸输入包装状态；普通 function call 为 None。
    grammar: Option<GrammarSlot>,
}

/// grammar 约束工具调用的累积状态：裸源码 → `{"<prop>":"…"}` JSON 包装。
struct GrammarSlot {
    /// 承载源码的属性名（工具 schema 里唯一必填的 string 属性）。
    property: String,
    /// 包装前缀是否已写入。
    started: bool,
    /// 是否已闭合（补过 `"}`）。
    closed: bool,
}

/// 已定稿的输出块（按 content_index 保序组装）
enum AssistBlock {
    /// 已定稿的思考摘要块，携带完整思考文本。
    Thinking(String),
    /// 已定稿的正文文本块。
    Text(String),
    /// 已定稿的工具调用块。
    ToolCall {
        /// 工具调用 id，复合格式 `call_id|item_id`。
        id: String,
        /// 被调用的函数名。
        name: String,
        /// 完整解析后的 JSON 参数对象。
        arguments: Value,
        /// 自定义工具的命名空间；普通工具为 None。
        namespace: Option<String>,
    },
}

/// OpenAI Responses SSE 按 \n\n flush 的 data 行累积器
struct SseAccumulator {
    /// 尚未遇到空行分隔符、等待后续 chunk 的 SSE 原始字节。
    buf: Vec<u8>,
}

impl SseAccumulator {
    /// 建一个尚无待处理字节的空累积器。
    fn new() -> Self {
        SseAccumulator { buf: Vec::new() }
    }

    /// 喂入字节，返回完整事件体（每个元素是一次 SSE 事件的 data 内容）
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        let mut data_lines: Vec<String> = Vec::new();
        let mut i = 0usize;
        let bytes = &self.buf;
        while i < bytes.len() {
            // 找行尾（\n）
            let mut line_end = i;
            while line_end < bytes.len() && bytes[line_end] != b'\n' {
                line_end += 1;
            }

            let at_eof = line_end >= bytes.len();
            let line = String::from_utf8_lossy(&bytes[i..line_end]);
            let l = line.strip_suffix('\r').unwrap_or(&line);

            if l.is_empty() {
                // 空行 = 事件分隔：flush 已累积的 data 行
                if !data_lines.is_empty() {
                    events.push(data_lines.join("\n"));
                    data_lines.clear();
                }
            } else if let Some(d) = l.strip_prefix("data:") {
                data_lines.push(d.strip_prefix(' ').unwrap_or(d).to_string());
            }

            // event:/注释行忽略（协议类型在 JSON 体内）
            if at_eof {
                break; // 最后一行未结束：整体保留等下一个 chunk
            }
            i = line_end + 1;
        }
        let consumed = i;
        if consumed > 0 {
            self.buf.drain(..consumed);
        }
        events
    }

    /// 流结束时调用：剩余 buf 不再有新字节，把未终止的残帧当作完整事件处理。
    /// Responses 的终端事件（如 response.completed）常不带尾随空行，
    /// EOF 时若残留 data 行必须 flush，否则事件永远丢失。
    fn finish(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        let remaining = std::mem::take(&mut self.buf);
        let mut data_lines: Vec<String> = Vec::new();
        let mut i = 0usize;

        while i < remaining.len() {
            let mut line_end = i;
            while line_end < remaining.len() && remaining[line_end] != b'\n' {
                line_end += 1;
            }

            if line_end >= remaining.len() {
                line_end = remaining.len();
            }

            let line = String::from_utf8_lossy(&remaining[i..line_end]);
            let l = line.strip_suffix('\r').unwrap_or(&line);
            if l.is_empty() {
                if !data_lines.is_empty() {
                    events.push(data_lines.join("\n"));
                    data_lines.clear();
                }
            } else if let Some(d) = l.strip_prefix("data:") {
                data_lines.push(d.strip_prefix(' ').unwrap_or(d).to_string());
            }
            i = line_end + 1;
        }

        if !data_lines.is_empty() {
            events.push(data_lines.join("\n"));
        }
        events
    }
}

/// 把响应 status（配合 incomplete 原因）映射为 prux 的 stop reason 与错误文案。
///
/// `max_output_tokens` 截断映射为 `length`，其它 incomplete 及 `failed`/`cancelled`
/// 映射为 `error`；未知 status 返回 `Err`。
fn map_responses_stop_reason(
    status: &str,
    incomplete_reason: Option<&str>,
) -> Result<(&'static str, Option<String>)> {
    match status {
        "completed" => Ok(("stop", None)),
        "incomplete" => {
            if incomplete_reason == Some("max_output_tokens") {
                Ok(("length", None))
            } else {
                Ok((
                    "error",
                    Some(match incomplete_reason {
                        Some(r) => format!("Response incomplete: {}", r),
                        None => "Response incomplete without a provider reason".to_string(),
                    }),
                ))
            }
        }
        "failed" | "cancelled" => Ok(("error", None)),
        "in_progress" | "queued" => Ok(("stop", None)),
        other => Err(Error::msg(format!(
            "OpenAI Responses unhandled status: {}",
            other
        ))),
    }
}

/// 记录首个 `response.created` 事件里的 response id（已有值时不覆盖）。
fn handle_response_created(ev: &Value, response_id: &mut Option<String>) {
    if let Some(id) = ev.pointer("/response/id").and_then(|v| v.as_str())
        && response_id.is_none()
    {
        *response_id = Some(id.to_string());
    }
}

/// 处理 `response.output_item.added`：按 item 类型建进行中输出槽并推进 content_index。
///
/// tool call 槽预填 call_id / item id / 函数名与初始 arguments。同一 output_index 在
/// 前一个 item 定稿前被复用（不合规服务器省略 output_index）时报错而非继续混跑。
fn handle_output_item_added(
    ev: &Value,
    slots: &mut HashMap<usize, Slot>,
    next_content_index: &mut usize,
    on_event: &mut Option<&mut PartialTranslator<'_>>,
) -> Result<()> {
    let output_index = ev.get("output_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

    // 不合规服务器（如省略 output_index 的 llama.cpp）会让多个 output item 落到同一索引，
    // 后到者覆盖前者 → 工具调用参数必然错位，直接报错而不是继续混跑。
    if slots.contains_key(&output_index) {
        return Err(Error::msg(format!(
            "OpenAI Responses stream reused output_index {} before the previous output item finished",
            output_index
        )));
    }

    let item = ev.get("item").cloned().unwrap_or(Value::Null);
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match item_type {
        "reasoning" => {
            let ci = *next_content_index;
            *next_content_index += 1;
            if let Some(f) = on_event.as_mut() {
                f.push(RawStreamEvent::ThinkingStart { content_index: ci });
            }
            slots.insert(
                output_index,
                Slot {
                    kind: SlotKind::Thinking,
                    content_index: ci,
                    text: String::new(),
                    thinking: String::new(),
                    partial_json: None,
                    arguments: json!({}),
                    call_id: None,
                    item_id: None,
                    name: None,
                    namespace: None,
                    grammar: None,
                },
            );
        }
        "message" => {
            let ci = *next_content_index;
            *next_content_index += 1;
            if let Some(f) = on_event.as_mut() {
                f.push(RawStreamEvent::TextStart { content_index: ci });
            }
            slots.insert(
                output_index,
                Slot {
                    kind: SlotKind::Text,
                    content_index: ci,
                    text: String::new(),
                    thinking: String::new(),
                    partial_json: None,
                    arguments: json!({}),
                    call_id: None,
                    item_id: None,
                    name: None,
                    namespace: None,
                    grammar: None,
                },
            );
        }
        "function_call" => {
            let ci = *next_content_index;
            *next_content_index += 1;
            if let Some(f) = on_event.as_mut() {
                f.push(RawStreamEvent::ToolCallStart { content_index: ci });
            }
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let item_id = item
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let namespace = item
                .get("namespace")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let init_args: String = item
                .get("arguments")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            if let Some(f) = on_event.as_mut() {
                f.push(RawStreamEvent::ToolCallDelta {
                    content_index: ci,
                    delta: String::new(),
                });
            }
            slots.insert(
                output_index,
                Slot {
                    kind: SlotKind::ToolCall,
                    content_index: ci,
                    text: String::new(),
                    thinking: String::new(),
                    partial_json: Some(String::new()),
                    arguments: parse_streaming_json(&init_args),
                    call_id,
                    item_id,
                    name,
                    namespace,
                    grammar: None,
                },
            );
        }
        "custom_tool_call" => {
            let ci = *next_content_index;
            *next_content_index += 1;
            if let Some(f) = on_event.as_mut() {
                f.push(RawStreamEvent::ToolCallStart { content_index: ci });
            }
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            // 属性名先从工具 schema 推，推不出时回退 `input`
            let property = name
                .as_deref()
                .and_then(grammar_input_property_for_tool)
                .unwrap_or_else(|| "input".to_string());
            let initial = item
                .get("input")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mut slot = Slot {
                kind: SlotKind::ToolCall,
                content_index: ci,
                text: String::new(),
                thinking: String::new(),
                partial_json: Some(String::new()),
                arguments: json!({}),
                call_id: item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                item_id: item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                name,
                namespace: item
                    .get("namespace")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                grammar: Some(GrammarSlot {
                    property,
                    started: false,
                    closed: false,
                }),
            };

            let delta = append_grammar_input(&mut slot, &initial);
            if let Some(f) = on_event.as_mut() {
                f.push(RawStreamEvent::ToolCallDelta {
                    content_index: ci,
                    delta,
                });
            }
            slots.insert(output_index, slot);
        }
        _ => {}
    }
    Ok(())
}

/// 追加一段 grammar 裸源码（增量），返回转发给订阅者的已包装 JSON 片段。
///
/// 首次写入时补 `{"<prop>":"` 前缀；槽不是 grammar 调用或已闭合时不做任何事。
fn append_grammar_input(slot: &mut Slot, delta_text: &str) -> String {
    let Some(grammar) = slot.grammar.as_ref() else {
        return String::new();
    };

    if grammar.closed {
        return String::new();
    }

    let prefix = if grammar.started {
        String::new()
    } else {
        format!(
            "{{\"{}\":\"",
            core::provider::escape_json_text(&grammar.property)
        )
    };

    let escaped = core::provider::escape_json_text(delta_text);
    if let Some(partial) = slot.partial_json.as_mut() {
        partial.push_str(&prefix);
        partial.push_str(&escaped);
    }

    if let Some(grammar) = slot.grammar.as_mut() {
        grammar.started = true;
    }

    refresh_grammar_arguments(slot);
    format!("{prefix}{escaped}")
}

/// 用完整裸源码重算包装（`done` 事件与 `output_item.done`），返回新增的 JSON 片段。
fn set_grammar_input(slot: &mut Slot, full_input: &str) -> String {
    let Some(grammar) = slot.grammar.as_ref() else {
        return String::new();
    };

    if grammar.closed {
        return String::new();
    }

    let rebuilt = format!(
        "{{\"{}\":\"{}",
        core::provider::escape_json_text(&grammar.property),
        core::provider::escape_json_text(full_input)
    );
    let previous = slot.partial_json.clone().unwrap_or_default();
    let delta = if rebuilt.starts_with(&previous) {
        rebuilt[previous.len()..].to_string()
    } else {
        String::new()
    };

    slot.partial_json = Some(rebuilt);
    if let Some(grammar) = slot.grammar.as_mut() {
        grammar.started = true;
    }

    refresh_grammar_arguments(slot);
    delta
}

/// 定稿 grammar 调用：补上 `"}` 并解析出最终参数对象。
fn close_grammar_input(slot: &mut Slot) {
    let Some(grammar) = slot.grammar.as_ref() else {
        return;
    };

    if grammar.closed {
        return;
    }

    let property = grammar.property.clone();
    let started = grammar.started;
    let text = if started {
        format!("{}\"}}", slot.partial_json.clone().unwrap_or_default())
    } else {
        format!(
            "{{\"{}\":\"\"}}",
            core::provider::escape_json_text(&property)
        )
    };

    slot.partial_json = Some(text.clone());
    slot.arguments = parse_streaming_json(&text);

    if let Some(grammar) = slot.grammar.as_mut() {
        grammar.closed = true;
    }
}

/// 按当前 JSON 缓冲刷新槽内已解析的 arguments。
fn refresh_grammar_arguments(slot: &mut Slot) {
    slot.arguments = parse_streaming_json(slot.partial_json.as_deref().unwrap_or("{}"));
}

/// `response.custom_tool_call_input.delta/done`：把裸源码包装成 JSON 参数并转发增量。
///
/// `full = true`（done 事件）时事件里的 `input` 是完整值，按完整值重算包装。
fn handle_grammar_input(
    ev: &Value,
    slots: &mut HashMap<usize, Slot>,
    on_event: &mut Option<&mut PartialTranslator<'_>>,
    full: bool,
) {
    let output_index = slot_index_from(ev);
    let Some(slot) = slots.get_mut(&output_index) else {
        return;
    };

    if slot.kind != SlotKind::ToolCall || slot.grammar.is_none() {
        return;
    }

    let delta = if full {
        let input = ev.get("input").and_then(|v| v.as_str()).unwrap_or("");
        set_grammar_input(slot, input)
    } else {
        let delta_text = ev.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        append_grammar_input(slot, delta_text)
    };

    if !delta.is_empty()
        && let Some(f) = on_event.as_mut()
    {
        f.push(RawStreamEvent::ToolCallDelta {
            content_index: slot.content_index,
            delta,
        });
    }
}

/// 取事件的 `output_index`；字段缺失或非数字时按 0 处理。
fn slot_index_from(ev: &Value) -> usize {
    ev.get("output_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize
}

/// reasoning_summary_text.delta / reasoning_text.delta：追加 thinking 文本。
fn handle_thinking_delta(
    ev: &Value,
    slots: &mut HashMap<usize, Slot>,
    on_event: &mut Option<&mut PartialTranslator<'_>>,
) {
    let output_index = slot_index_from(ev);
    if let Some(slot) = slots.get_mut(&output_index)
        && slot.kind == SlotKind::Thinking
    {
        let d = ev.get("delta").and_then(|v| v.as_str()).unwrap_or("");
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

/// reasoning summary 分段结束时向该思考槽追加空行，保留多段摘要的段落分隔。
fn handle_thinking_summary_part_done(
    ev: &Value,
    slots: &mut HashMap<usize, Slot>,
    on_event: &mut Option<&mut PartialTranslator<'_>>,
) {
    let output_index = slot_index_from(ev);
    if let Some(slot) = slots.get_mut(&output_index)
        && slot.kind == SlotKind::Thinking
    {
        slot.thinking.push_str("\n\n");
        if let Some(f) = on_event.as_mut() {
            f.push(RawStreamEvent::ThinkingDelta {
                content_index: slot.content_index,
                delta: "\n\n".to_string(),
            });
        }
    }
}

/// output_text.delta / refusal.delta：追加文本。
fn handle_text_delta(
    ev: &Value,
    slots: &mut HashMap<usize, Slot>,
    on_event: &mut Option<&mut PartialTranslator<'_>>,
) {
    let output_index = slot_index_from(ev);
    if let Some(slot) = slots.get_mut(&output_index)
        && slot.kind == SlotKind::Text
    {
        let d = ev.get("delta").and_then(|v| v.as_str()).unwrap_or("");
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

/// 把 `function_call_arguments.delta` 追加进工具调用槽的部分 JSON 缓冲，
/// 并立即重新解析出当前可见的 arguments。
fn handle_tool_call_arguments_delta(
    ev: &Value,
    slots: &mut HashMap<usize, Slot>,
    on_event: &mut Option<&mut PartialTranslator<'_>>,
) {
    let output_index = slot_index_from(ev);
    if let Some(slot) = slots.get_mut(&output_index)
        && slot.kind == SlotKind::ToolCall
        && let Some(partial) = slot.partial_json.as_mut()
    {
        let d = ev.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        partial.push_str(d);
        slot.arguments = parse_streaming_json(partial);
        if let Some(f) = on_event.as_mut() {
            f.push(RawStreamEvent::ToolCallDelta {
                content_index: slot.content_index,
                delta: d.to_string(),
            });
        }
    }
}

/// 用 `function_call_arguments.done` 的完整参数覆盖槽内缓冲并重新解析；
/// 若新参数是旧缓冲的延续，只把新增部分作为 delta 转发给 `on_event`。
fn handle_tool_call_arguments_done(
    ev: &Value,
    slots: &mut HashMap<usize, Slot>,
    on_event: &mut Option<&mut PartialTranslator<'_>>,
) {
    let output_index = slot_index_from(ev);
    if let Some(slot) = slots.get_mut(&output_index)
        && slot.kind == SlotKind::ToolCall
        && let Some(partial) = slot.partial_json.as_mut()
    {
        let prev = partial.clone();
        let done_args = ev.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
        *partial = done_args.to_string();
        slot.arguments = parse_streaming_json(partial);

        if let Some(f) = on_event.as_mut()
            && done_args.starts_with(&prev)
        {
            let delta = &done_args[prev.len()..];
            if !delta.is_empty() {
                f.push(RawStreamEvent::ToolCallDelta {
                    content_index: slot.content_index,
                    delta: delta.to_string(),
                });
            }
        }
    }
}

/// 处理 `response.output_item.done`：把对应槽定稿写入 `finalized`（按 content_index 保序）。
///
/// reasoning 取 summary/content 文本，message 拼接 output_text 与 refusal，
/// function_call 落定参数并组装 `call_id|item_id`。
fn handle_output_item_done(
    ev: &Value,
    slots: &mut HashMap<usize, Slot>,
    finalized: &mut BTreeMap<usize, AssistBlock>,
    _on_event: &mut Option<&mut PartialTranslator<'_>>,
) {
    let output_index = slot_index_from(ev);
    let item = ev.get("item").cloned().unwrap_or(Value::Null);
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let mut slot = slots.remove(&output_index);

    match item_type {
        "reasoning" => {
            if let Some(s) = slot.as_mut()
                && s.kind == SlotKind::Thinking
            {
                let summary_text = item
                    .get("summary")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .unwrap_or_default();
                let content_text = item
                    .get("content")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .unwrap_or_default();

                if !summary_text.is_empty() {
                    s.thinking = summary_text;
                } else if !content_text.is_empty() {
                    s.thinking = content_text;
                }
                finalized.insert(s.content_index, AssistBlock::Thinking(s.thinking.clone()));
            }
        }
        "message" => {
            if let Some(s) = slot.as_mut()
                && s.kind == SlotKind::Text
            {
                let joined = item
                    .get("content")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|x| {
                                let c = x.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                match c {
                                    "output_text" => {
                                        x.get("text").and_then(|t| t.as_str()).unwrap_or("")
                                    }
                                    "refusal" => {
                                        x.get("refusal").and_then(|t| t.as_str()).unwrap_or("")
                                    }
                                    _ => "",
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default();

                s.text = joined;
                finalized.insert(s.content_index, AssistBlock::Text(s.text.clone()));
            }
        }
        "function_call" | "custom_tool_call" => {
            if let Some(s) = slot.as_mut()
                && s.kind == SlotKind::ToolCall
            {
                if s.grammar.is_some() {
                    // grammar 调用：定稿的 `input` 是完整裸源码
                    let input = item.get("input").and_then(|v| v.as_str()).unwrap_or("");
                    set_grammar_input(s, input);
                    close_grammar_input(s);
                } else {
                    let final_args = item
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or("{}");
                    s.arguments = parse_streaming_json(final_args);
                }
                s.partial_json = None;
            }

            if let Some(s) = slot
                && s.kind == SlotKind::ToolCall
            {
                let call_id = s
                    .call_id
                    .clone()
                    .unwrap_or_else(|| format!("call_{}", s.content_index));
                let item_id = s.item_id.clone().unwrap_or_default();
                let id = format!("{}|{}", call_id, item_id);
                let name = s.name.clone().unwrap_or_default();
                let arguments = s.arguments.clone();
                finalized.insert(
                    s.content_index,
                    AssistBlock::ToolCall {
                        id,
                        name,
                        arguments,
                        namespace: s.namespace.clone(),
                    },
                );
            }
        }
        _ => {}
    }
}

/// 处理 `response.completed` / `response.incomplete` 终端事件。
///
/// 置 `saw_terminal`，记录 response id、service_tier 与 usage（input 扣掉缓存读写），
/// 映射 stop reason 与错误文案，并在已定稿内容含工具调用时把 stop 升级为 toolUse。
#[allow(clippy::too_many_arguments)]
fn handle_terminal_response(
    ev: &Value,
    saw_terminal: &mut bool,
    response_id: &mut Option<String>,
    usage: &mut Option<Usage>,
    stop_reason: &mut Option<String>,
    error_message: &mut Option<String>,
    service_tier: &mut Option<String>,
    finalized: &BTreeMap<usize, AssistBlock>,
) -> Result<()> {
    *saw_terminal = true;
    let resp = ev.get("response").cloned().unwrap_or(Value::Null);
    if let Some(rid) = resp.get("id").and_then(|v| v.as_str())
        && response_id.is_none()
    {
        *response_id = Some(rid.to_string());
    }

    // GPT-6 系列在 Fast 模式下回报 service_tier: "fast"，需按 fast 价计
    if let Some(tier) = resp.get("service_tier").and_then(|v| v.as_str()) {
        *service_tier = Some(tier.to_string());
    }

    if let Some(u) = resp.get("usage") {
        let input_tokens = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let cached = u
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let cache_write = u
            .pointer("/input_tokens_details/cache_write_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let output = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let reasoning = u
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let total = u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        *usage = Some(Usage {
            input: input_tokens
                .saturating_sub(cached)
                .saturating_sub(cache_write),
            output,
            cache_read: cached,
            cache_write,
            cache_write_1h: None,
            reasoning: (reasoning > 0).then_some(reasoning),
            total_tokens: total,
            cost: Cost::default(),
        });
    }

    let status = resp
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("completed")
        .to_string();
    let incomplete_reason = resp
        .pointer("/incomplete_details/reason")
        .and_then(|v| v.as_str());
    let (mapped, em) = map_responses_stop_reason(&status, incomplete_reason)?;
    *stop_reason = Some(mapped.to_string());
    *error_message = em;

    // 内容含 tool call 时 stop 升级为 toolUse
    if stop_reason.as_deref() == Some("stop")
        && finalized
            .values()
            .any(|b| matches!(b, AssistBlock::ToolCall { .. }))
    {
        *stop_reason = Some("toolUse".to_string());
    }
    Ok(())
}

/// 把流内 `error` 事件转成 ProviderMessage 错误（code/message 缺失时用 unknown）。
fn responses_error_from_error_event(ev: &Value) -> Error {
    let code = ev.get("code").and_then(|v| v.as_str()).unwrap_or("unknown");
    let message = ev
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown error");
    Error::ProviderMessage(format!("Error Code {}: {}", code, message))
}

/// 把 `response.failed` 事件转成 ProviderMessage 错误，
/// 依次从 response.error 与 incomplete_details 里提取原因，都没有则给出兜底文案。
fn responses_error_from_failed_event(ev: &Value) -> Error {
    let resp = ev.get("response").cloned().unwrap_or(Value::Null);
    let error = resp.get("error");
    let details = resp.get("incomplete_details");
    let msg = if let Some(e) = error {
        format!(
            "{}: {}",
            e.get("code").and_then(|v| v.as_str()).unwrap_or("unknown"),
            e.get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("no message")
        )
    } else if let Some(d) = details {
        format!(
            "incomplete: {}",
            d.get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
        )
    } else {
        "Unknown error (no error details in response)".to_string()
    };
    Error::ProviderMessage(msg)
}

/// 组装 assistant 消息 content blocks：先按 content_index 保序输出已定稿
/// 块，再防御性收尾流提前中断时残留的进行中 slot（正常路径不会走到）。
fn assemble_responses_blocks(
    finalized: BTreeMap<usize, AssistBlock>,
    slots: HashMap<usize, Slot>,
    on_event: &mut Option<&mut PartialTranslator<'_>>,
) -> Vec<ContentBlock> {
    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    for (ci, block) in finalized {
        match block {
            AssistBlock::Thinking(content) => {
                if content.trim().is_empty() {
                    continue;
                }
                content_blocks.push(ContentBlock::Thinking {
                    thinking: content.clone(),
                    thinking_signature: None,
                    redacted: None,
                });
                if let Some(f) = on_event.as_mut() {
                    f.push(RawStreamEvent::ThinkingEnd {
                        content_index: ci,
                        content,
                    });
                }
            }
            AssistBlock::Text(content) => {
                if content.is_empty() {
                    continue;
                }
                content_blocks.push(ContentBlock::Text {
                    text: content.clone(),
                    text_signature: None,
                });
                if let Some(f) = on_event.as_mut() {
                    f.push(RawStreamEvent::TextEnd {
                        content_index: ci,
                        content,
                    });
                }
            }
            AssistBlock::ToolCall {
                id,
                name,
                arguments,
                namespace,
            } => {
                content_blocks.push(ContentBlock::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                    thought_signature: None,
                    namespace,
                });
                if let Some(f) = on_event.as_mut() {
                    f.push(RawStreamEvent::ToolCallEnd {
                        content_index: ci,
                        id,
                        name,
                        arguments,
                    });
                }
            }
        }
    }
    let mut pending_slots: Vec<(usize, Slot)> = slots.into_iter().collect();
    pending_slots.sort_by_key(|(idx, _)| *idx);
    for (_idx, s) in pending_slots {
        match s.kind {
            SlotKind::Thinking => {
                if !s.thinking.trim().is_empty() {
                    content_blocks.push(ContentBlock::Thinking {
                        thinking: s.thinking.clone(),
                        thinking_signature: None,
                        redacted: None,
                    });
                    if let Some(f) = on_event.as_mut() {
                        f.push(RawStreamEvent::ThinkingEnd {
                            content_index: s.content_index,
                            content: s.thinking,
                        });
                    }
                }
            }
            SlotKind::Text => {
                if !s.text.is_empty() {
                    content_blocks.push(ContentBlock::Text {
                        text: s.text.clone(),
                        text_signature: None,
                    });
                    if let Some(f) = on_event.as_mut() {
                        f.push(RawStreamEvent::TextEnd {
                            content_index: s.content_index,
                            content: s.text,
                        });
                    }
                }
            }
            SlotKind::ToolCall => {
                let call_id = s
                    .call_id
                    .clone()
                    .unwrap_or_else(|| format!("call_{}", s.content_index));
                let item_id = s.item_id.clone().unwrap_or_default();
                let id = format!("{}|{}", call_id, item_id);
                let name = s.name.clone().unwrap_or_default();
                let arguments = s.arguments.clone();
                content_blocks.push(ContentBlock::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                    thought_signature: None,
                    namespace: None,
                });
                if let Some(f) = on_event.as_mut() {
                    f.push(RawStreamEvent::ToolCallEnd {
                        content_index: s.content_index,
                        id,
                        name,
                        arguments,
                    });
                }
            }
        }
    }
    content_blocks
}

/// 把流式收集到的内容块与元信息组装成 StreamResult，
/// 并按模型定价与 service tier 计算 usage 成本。
fn build_responses_result(
    content_blocks: Vec<ContentBlock>,
    stop_reason: Option<String>,
    error_message: Option<String>,
    usage: Option<Usage>,
    _response_id: Option<String>,
    service_tier: Option<&str>,
    model: &ModelConfig,
) -> Result<StreamResult> {
    let mut usage = usage;
    if let Some(u) = usage.as_mut() {
        compute_cost(u, model.cost.as_ref());
        apply_service_tier_cost(u, &model.model_id, service_tier);
    }

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

/// 消费 Responses 的 SSE 字节流：边解析边把增量事件转发给 `on_event`，
/// 结束时返回组装好的 StreamResult。
///
/// 流结束前未收到终端事件、或仍有未定稿的工具调用时报错
/// （此时参数可能已被截断或错位，不能交给 agent 执行）。
pub(crate) async fn process_responses_feed<E>(
    model: &ModelConfig,
    on_event: &mut Option<&mut PartialTranslator<'_>>,
    mut feed: E,
) -> Result<StreamResult>
where
    E: futures_util::Stream<Item = Result<bytes::Bytes>> + Unpin,
{
    // 流式状态
    let mut stop_reason: Option<String> = None;
    let mut error_message: Option<String> = None;
    let mut usage: Option<Usage> = None;
    let mut response_id: Option<String> = None;
    let mut saw_terminal = false;
    let mut service_tier: Option<String> = None;

    // slots 按 output_index 索引（进行中），finalized 按 content_index 保序（已定稿）
    let mut slots: HashMap<usize, Slot> = HashMap::new();
    let mut finalized: BTreeMap<usize, AssistBlock> = BTreeMap::new();
    let mut next_content_index = 0usize;
    let mut sse = SseAccumulator::new();
    let mut total_bytes: usize = 0;

    // 供应商原始流事件：无扩展订阅时零开销（一次流只查一次）
    let capture_stream_events = core::extensions::has_provider_stream_event_handlers();

    let mut handle_data = |data: &str| -> Result<()> {
        let ev: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        if capture_stream_events {
            core::extensions::dispatch_provider_stream_event(
                &model.provider,
                &model.api,
                &model.model_id,
                &ev,
            );
        }
        let ev_type = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match ev_type {
            "response.created" => handle_response_created(&ev, &mut response_id),
            "response.output_item.added" => {
                handle_output_item_added(&ev, &mut slots, &mut next_content_index, on_event)?
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                handle_thinking_delta(&ev, &mut slots, on_event)
            }
            "response.reasoning_summary_part.done" => {
                handle_thinking_summary_part_done(&ev, &mut slots, on_event)
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                handle_text_delta(&ev, &mut slots, on_event)
            }
            "response.function_call_arguments.delta" => {
                handle_tool_call_arguments_delta(&ev, &mut slots, on_event)
            }
            "response.function_call_arguments.done" => {
                handle_tool_call_arguments_done(&ev, &mut slots, on_event)
            }
            "response.custom_tool_call_input.delta" => {
                handle_grammar_input(&ev, &mut slots, on_event, false)
            }
            "response.custom_tool_call_input.done" => {
                handle_grammar_input(&ev, &mut slots, on_event, true)
            }
            "response.output_item.done" => {
                handle_output_item_done(&ev, &mut slots, &mut finalized, on_event)
            }
            "response.completed" | "response.incomplete" => handle_terminal_response(
                &ev,
                &mut saw_terminal,
                &mut response_id,
                &mut usage,
                &mut stop_reason,
                &mut error_message,
                &mut service_tier,
                &finalized,
            )?,
            "error" => return Err(responses_error_from_error_event(&ev)),
            "response.failed" => return Err(responses_error_from_failed_event(&ev)),
            _ => {}
        }
        Ok(())
    };

    while let Some(chunk) = feed.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => return Err(e),
        };
        track_stream_bytes(&mut total_bytes, chunk.len())?;
        let events = sse.push(&chunk);
        for data in events {
            handle_data(&data)?;
        }
    }

    // 流结束时 flush 未终止的残帧（终端事件常不带尾随空行）
    for data in sse.finish() {
        handle_data(&data)?;
    }

    if !saw_terminal {
        return Err(Error::msg(
            "OpenAI Responses stream ended before a terminal response event",
        ));
    }

    // agent 会执行末条消息里的每个工具调用，因此拒绝交出未定稿的调用——
    // 其参数可能被截断或错位（不合规服务器省略 output_index 时即如此）。
    if let Some(s) = slots
        .values()
        .find(|s| s.kind == SlotKind::ToolCall && s.partial_json.is_some())
    {
        let name = s.name.clone().unwrap_or_default();
        let id = s
            .call_id
            .clone()
            .unwrap_or_else(|| format!("call_{}", s.content_index));

        return Err(Error::msg(format!(
            "OpenAI Responses stream completed with an unfinished tool call: {} ({})",
            name, id
        )));
    }

    let content_blocks = assemble_responses_blocks(finalized, slots, on_event);
    build_responses_result(
        content_blocks,
        stop_reason,
        error_message,
        usage,
        response_id,
        service_tier.as_deref(),
        model,
    )
}

/// 构造 responses 请求体（Sign in with ChatGPT 时略过 `max_output_tokens` / `temperature`）。
///
/// `level_sampling_params` 是当前 thinking level 的 `samplingParamsByThinkingLevel`
/// 覆盖项：在自设字段之后写入，同名键覆盖；目录 `samplingParams` 只补未设的键。
#[allow(clippy::too_many_arguments)]
fn build_request_body(
    model: &ModelConfig,
    messages: &[AgentMessage],
    system_prompt: &str,
    tools: &[(String, String, Value)],
    reasoning_effort: Option<&str>,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    tool_choice: Option<&str>,
    level_sampling_params: Option<&Value>,
) -> Map<String, Value> {
    let input = convert_responses_messages(messages, system_prompt, model);

    let mut body = Map::new();
    body.insert("model".into(), json!(azure::request_model_name(model)));
    body.insert("input".into(), Value::Array(input));
    body.insert("stream".into(), json!(true));
    body.insert("store".into(), json!(false));

    // Sign in with ChatGPT 拒绝这两个请求字段
    if !is_chatgpt_sign_in(model) {
        if let Some(mt) = max_tokens.or(model.max_tokens) {
            body.insert("max_output_tokens".into(), json!(mt.max(MIN_OUTPUT_TOKENS)));
        }
        if let Some(t) = temperature {
            body.insert("temperature".into(), json!(t));
        }
    }

    if model.reasoning {
        match reasoning_effort {
            Some(level) => {
                let effort = model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|m| m.get(level))
                    .and_then(|v| v.as_deref())
                    .unwrap_or(level)
                    .to_string();
                body.insert(
                    "reasoning".into(),
                    json!({ "effort": effort, "summary": "auto" }),
                );
                body.insert("include".into(), json!(["reasoning.encrypted_content"]));
            }
            None => {
                // off 为 null 时保持 API 默认（不发禁用）
                let should_disable = model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|m| m.get("off"))
                    .is_none_or(|v| v.is_some());
                if should_disable {
                    let effort = model
                        .thinking_level_map
                        .as_ref()
                        .and_then(|m| m.get("off"))
                        .and_then(|v| v.as_deref())
                        .unwrap_or("none")
                        .to_string();
                    body.insert("reasoning".into(), json!({ "effort": effort }));
                }
            }
        }
    }

    if !tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(convert_responses_tools_with_capabilities(
                tools,
                core::model_resolver::supports_openai_grammar_tools(
                    &model.provider,
                    &model.model_id,
                ),
            )),
        );
    }

    if let Some(tc) = tool_choice {
        body.insert("tool_choice".into(), json!(tc)); // "auto" | "none"
    }

    // samplingParams：目录值只补未设的键，级别覆盖项后写覆盖
    if let Some(sp) = &model.sampling_params
        && let Some(obj) = sp.as_object()
    {
        for (k, v) in obj {
            if !body.contains_key(k) {
                body.insert(k.clone(), v.clone());
            }
        }
    }
    core::provider::apply_level_sampling_params(&mut body, level_sampling_params);

    body
}

/// 发起一次 Responses 请求（带重试）并以流式方式解析结果。
///
/// reasoning effort 先按模型能力钳制并把 `off` 归一到「不发送」；请求体经 payload hook
/// 改写后发出，非 2xx 状态码转成 `ProviderStatus` 错误。
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
    // 按模型能力钳制级别；"off" 归一到 None。级别覆盖项要在归一之前按级别名查出（归一后 off 已丢失）
    let level_sampling_params = model.sampling_params_for_level(reasoning_effort).cloned();
    let reasoning_effort = match reasoning_effort {
        Some(level) => {
            let clamped = model.clamp_thinking_level(level);
            if clamped == "off" {
                None
            } else {
                Some(clamped)
            }
        }
        None => None,
    };

    let client = http::build_client()?;
    let body = build_request_body(
        model,
        messages,
        system_prompt,
        tools,
        reasoning_effort.as_deref(),
        temperature,
        max_tokens,
        tool_choice,
        level_sampling_params.as_ref(),
    );

    let mut body_json = serde_json::to_string(&body).map_err(|source| Error::Json {
        context: "failed to serialize responses request".to_string(),
        source,
    })?;

    apply_payload_hook(&mut body_json, on_payload.as_ref());

    let response = send_with_retry(
        || {
            client
                .post(endpoint(model))
                .header("Content-Type", "application/json")
                .headers(super::provider_extra_headers(model))
                .bearer_auth(&model.api_key)
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
            body: provider_error_body(&text),
        });
    }

    let stream = response.bytes_stream();
    // 传输层字节流 → 事件处理
    let feed =
        stream.map(|chunk| chunk.map_err(|e| Error::msg(format!("stream read failed: {}", e))));
    process_responses_feed(model, &mut on_event, feed).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::ModelConfig;

    fn model(reasoning: bool) -> ModelConfig {
        ModelConfig {
            model_type: Default::default(),
            image_resize: crate::utils::image::ImageResizeLimits::default(),
            allowed_fallback_models: Vec::new(),
            provider: "opencode".into(),
            model_id: "gpt-5.2".into(),
            base_url: "https://opencode.ai/zen/v1".into(),
            api_key: String::new(),
            api: "openai-responses".into(),
            output: Vec::new(),
            input: vec!["text".into(), "image".into()],
            reasoning,
            max_tokens: None,
            temperature: None,
            context_window: 200_000,
            thinking_format: String::new(),
            supports_reasoning_effort: false,
            thinking_level_map: None,
            auth_header: false,
            supports_developer_role: true,
            requires_reasoning_content_on_assistant_messages: false,

            supports_usage_in_streaming: true,

            supports_store: true,

            supports_finish_reason: true,

            requires_assistant_after_tool_result: false,
            max_tokens_field: "max_completion_tokens".into(),
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
    fn system_prompt_becomes_developer_for_reasoning_model() {
        let m = model(true);
        let out = convert_responses_messages(&[], "sys", &m);
        assert_eq!(out[0]["role"], "developer");
        assert_eq!(out[0]["content"], "sys");
        let plain = model(false);
        let out = convert_responses_messages(&[], "sys", &plain);
        assert_eq!(out[0]["role"], "system");
    }

    #[test]
    fn user_message_becomes_input_text() {
        let m = model(false);
        let out = convert_responses_messages(&[AgentMessage::user_text("hi")], "", &m);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[0]["content"][0]["type"], "input_text");
        assert_eq!(out[0]["content"][0]["text"], "hi");
    }

    #[test]
    fn assistant_text_becomes_message_item() {
        let m = model(false);
        let mut a = AgentMessage::user_text("hello");
        a.role = "assistant".into();
        let out = convert_responses_messages(&[a], "", &m);
        let item = &out[0];
        assert_eq!(item["type"], "message");
        assert_eq!(item["role"], "assistant");
        assert_eq!(item["id"], "msg_pi_0");
        assert_eq!(item["content"][0]["type"], "output_text");
        assert_eq!(item["content"][0]["text"], "hello");
        assert_eq!(item["status"], "completed");
    }

    #[test]
    fn thinking_signature_is_passed_through() {
        let m = model(true);
        let sig = json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{ "type": "summary_text", "text": "thinking..." }],
            "encrypted_content": "abc=="
        })
        .to_string();
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.content = vec![ContentBlock::Thinking {
            thinking: "thinking...".into(),
            thinking_signature: Some(sig.clone()),
            redacted: None,
        }];
        let out = convert_responses_messages(&[a], "", &m);
        // 原样透传 reasoning item（含加密内容）
        assert_eq!(out[0]["type"], "reasoning");
        assert_eq!(out[0]["id"], "rs_1");
        assert_eq!(out[0]["encrypted_content"], "abc==");
    }

    #[test]
    fn tool_call_splits_composite_id() {
        let m = model(true);
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.model = Some("gpt-5.2".into());
        a.provider = Some("opencode".into());
        a.api = Some("openai-responses".into());
        a.content = vec![ContentBlock::ToolCall {
            id: "call_1|fc_2".into(),
            name: "read".into(),
            arguments: json!({ "path": "a.rs" }),
            thought_signature: None,
            namespace: None,
        }];
        let out = convert_responses_messages(&[a], "", &m);
        assert_eq!(out[0]["type"], "function_call");
        assert_eq!(out[0]["call_id"], "call_1");
        assert_eq!(out[0]["id"], "fc_2");
        assert_eq!(out[0]["arguments"], "{\"path\":\"a.rs\"}");
    }

    #[test]
    fn cross_model_tool_call_drops_item_id() {
        let m = model(true);
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        // 跨模型：model 不同
        a.model = Some("gpt-5.1".into());
        a.provider = Some("opencode".into());
        a.api = Some("openai-responses".into());
        a.content = vec![ContentBlock::ToolCall {
            id: "call_1|fc_2".into(),
            name: "read".into(),
            arguments: json!({}),
            thought_signature: None,
            namespace: None,
        }];
        let out = convert_responses_messages(&[a], "", &m);
        // 跨模型 + fc_ 前缀 → 置空 id 避免 pairing 校验
        assert!(out[0].get("id").is_none());
        assert_eq!(out[0]["call_id"], "call_1");
    }

    #[test]
    fn non_whitelist_provider_normalizes_whole_id() {
        let mut m = model(true);
        m.provider = "opencode-go".into();
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.model = Some("gpt-5.6-luna".into());
        a.provider = Some("opencode-go".into());
        a.api = Some("openai-responses".into());
        a.content = vec![ContentBlock::ToolCall {
            id: "call_1|fc_2".into(),
            name: "read".into(),
            arguments: json!({}),
            thought_signature: None,
            namespace: None,
        }];
        let out = convert_responses_messages(&[a], "", &m);
        // 非白名单：id 归一化为单段（| → _），作为 call_id 使用
        assert_eq!(out[0]["call_id"], "call_1_fc_2");
        assert!(out[0].get("id").is_none());
    }

    #[test]
    fn tool_result_becomes_function_call_output() {
        let m = model(true);
        let mut tr = AgentMessage::user_text("");
        tr.role = "toolResult".into();
        tr.tool_call_id = Some("call_1|fc_2".into());
        tr.content = vec![ContentBlock::Text {
            text: "file contents".into(),
            text_signature: None,
        }];
        let out = convert_responses_messages(&[tr], "", &m);
        assert_eq!(out[0]["type"], "function_call_output");
        assert_eq!(out[0]["call_id"], "call_1");
        assert_eq!(out[0]["output"], "file contents");
    }

    /// grammar 能力开启时工具下发为 `custom`（responses 形状）；关闭时仍是 function。
    #[test]
    fn grammar_tools_convert_to_custom_items() {
        let tools = vec![(
            "codemode".to_string(),
            "run js".to_string(),
            json!({
                "type": "object",
                "properties": { "code": { "type": "string" } },
                "required": ["code"],
                "x-prux-grammar": { "openai_lark": "start: SOURCE", "input": "code" }
            }),
        )];

        let with_grammar = convert_responses_tools_with_capabilities(&tools, true);
        assert_eq!(with_grammar[0]["type"], "custom");
        assert_eq!(with_grammar[0]["name"], "codemode");
        assert_eq!(with_grammar[0]["format"]["type"], "grammar");
        assert_eq!(with_grammar[0]["format"]["syntax"], "lark");
        assert_eq!(with_grammar[0]["format"]["definition"], "start: SOURCE");

        let without = convert_responses_tools_with_capabilities(&tools, false);
        assert_eq!(without[0]["type"], "function");
    }

    /// custom_tool_call 的裸 `input` 增量被包成 `{"<prop>":"…"}` 的 JSON 参数。
    ///
    /// 同步测试 + 手动 runtime：属性名回退依赖「扩展注册表里没有 codemode」，
    /// 需要与注册 codemode 的回放测试互斥（锁不能跨 await 持有）。
    #[test]
    fn custom_tool_call_input_is_wrapped_into_arguments() {
        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
        let m = model(true);
        let feed = feed_of(&[
            r#"{"type":"response.created","response":{"id":"r"}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"custom_tool_call","call_id":"c1","id":"ctc_1","name":"codemode","input":""}}"#,
            r#"{"type":"response.custom_tool_call_input.delta","output_index":0,"delta":"return 1;"}"#,
            r#"{"type":"response.custom_tool_call_input.done","output_index":0,"input":"return 1;\n// done"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"custom_tool_call","call_id":"c1","id":"ctc_1","name":"codemode","input":"return 1;\n// done"}}"#,
            r#"{"type":"response.completed","response":{"id":"r","status":"completed"}}"#,
        ]);
        let mut on_event = None;
        let res = process_responses_feed(&m, &mut on_event, feed)
            .await
            .expect("正常流不应报错");
        let arguments = res
            .message
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::ToolCall { arguments, .. } => Some(arguments.clone()),
                _ => None,
            })
            .expect("tool call block");
        // 未注册 `codemode` 扩展时属性名回退 `input`（与 pi 同）
        assert_eq!(arguments, json!({ "input": "return 1;\n// done" }));
        });
    }

    /// grammar 工具的历史调用回放为 `custom_tool_call`，其结果回放为 `custom_tool_call_output`。
    #[test]
    fn grammar_tool_call_replays_as_custom_items() {
        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::core::extensions::register_extension(crate::extensions::codemode::Codemode);
        crate::core::extensions::set_extension_enabled("codemode", true);

        let m = model(true);
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.model = Some("gpt-5.2".into());
        a.provider = Some("opencode".into());
        a.api = Some("openai-responses".into());
        a.content = vec![ContentBlock::ToolCall {
            id: "call_1|ctc_2".into(),
            name: "codemode".into(),
            arguments: json!({ "code": "return 1;" }),
            thought_signature: None,
            namespace: None,
        }];
        let mut tr = AgentMessage::user_text("");
        tr.role = "toolResult".into();
        tr.tool_call_id = Some("call_1|ctc_2".into());
        tr.tool_name = Some("codemode".into());
        tr.content = vec![ContentBlock::Text {
            text: "Script completed".into(),
            text_signature: None,
        }];

        let out = convert_responses_messages(&[a, tr], "", &m);
        crate::core::extensions::set_extension_enabled("codemode", false);
        _ = crate::core::extensions::unregister_extension("codemode");

        assert_eq!(out[0]["type"], "custom_tool_call");
        assert_eq!(out[0]["name"], "codemode");
        assert_eq!(out[0]["input"], "return 1;");
        assert_eq!(out[0]["id"], "ctc_2", "custom item id 保留 ctc_ 前缀");
        assert_eq!(out[1]["type"], "custom_tool_call_output");
        assert_eq!(out[1]["output"], "Script completed");
    }

    /// 回归：模型不支持 grammar 工具时（如 opencode 的 `grok-4.6`），历史里的 grammar 工具调用
    /// 必须回放成 `function_call` / `function_call_output`。
    ///
    /// 曾经只按工具名判断，于是这类模型收到 `custom_tool_call`，上游报 400，
    /// 且这条历史一旦落库，之后每次请求都 400（整个会话作废）。
    #[test]
    fn grammar_tool_replay_follows_model_capability() {
        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::extensions::register_extension(crate::extensions::codemode::Codemode);
        crate::core::extensions::set_extension_enabled("codemode", true);

        let mut m = model(true);
        m.model_id = "grok-4.6".into();
        assert!(
            !responses_grammar_supported(&m),
            "前置条件：grok-4.6 不应声明 supportsOpenAIGrammarTools"
        );

        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.model = Some("grok-4.6".into());
        a.provider = Some("opencode".into());
        a.api = Some("openai-responses".into());
        a.content = vec![ContentBlock::ToolCall {
            id: "call_1|ctc_2".into(),
            name: "codemode".into(),
            arguments: json!({ "code": "return 1;" }),
            thought_signature: None,
            namespace: None,
        }];
        let mut tr = AgentMessage::user_text("");
        tr.role = "toolResult".into();
        tr.tool_call_id = Some("call_1|ctc_2".into());
        tr.tool_name = Some("codemode".into());
        tr.content = vec![ContentBlock::Text {
            text: "Script completed".into(),
            text_signature: None,
        }];

        let out = convert_responses_messages(&[a.clone(), tr], "", &m);
        assert_eq!(out[0]["type"], "function_call");
        assert_eq!(out[0]["name"], "codemode");
        assert_eq!(out[0]["arguments"], "{\"code\":\"return 1;\"}");
        // item id 前缀跟着形状走：function 形状只保留 fc_ 前缀的 item id
        assert!(out[0].get("id").is_none(), "ctc_2 不应配给 function_call");
        assert_eq!(out[1]["type"], "function_call_output");
        assert_eq!(out[1]["output"], "Script completed");

        // 孤儿调用补的 synthetic result 同样必须是 function 形状
        let out = convert_responses_messages(&[a, AgentMessage::user_text("next")], "", &m);
        assert_eq!(out[1]["type"], "function_call_output");
        assert_eq!(out[1]["output"], "No result provided");

        crate::core::extensions::set_extension_enabled("codemode", false);
        _ = crate::core::extensions::unregister_extension("codemode");
    }

    #[test]
    fn orphan_tool_call_gets_synthetic_result() {
        let m = model(true);
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.model = Some("gpt-5.2".into());
        a.provider = Some("opencode".into());
        a.api = Some("openai-responses".into());
        a.content = vec![ContentBlock::ToolCall {
            id: "call_1|fc_2".into(),
            name: "read".into(),
            arguments: json!({}),
            thought_signature: None,
            namespace: None,
        }];
        let user = AgentMessage::user_text("next");
        let out = convert_responses_messages(&[a, user], "", &m);
        // function_call 后补 synthetic output，再接 user 消息
        assert_eq!(out[0]["type"], "function_call");
        assert_eq!(out[1]["type"], "function_call_output");
        assert_eq!(out[1]["output"], "No result provided");
        assert_eq!(out[2]["role"], "user");
    }

    #[test]
    fn tool_result_with_images_mixes_input_image() {
        let m = model(true);
        let mut tr = AgentMessage::user_text("");
        tr.role = "toolResult".into();
        tr.tool_call_id = Some("call_1".into());
        tr.content = vec![
            ContentBlock::Text {
                text: "see".into(),
                text_signature: None,
            },
            ContentBlock::Image {
                data: "AA==".into(),
                mime_type: "image/png".into(),
            },
        ];
        let out = convert_responses_messages(&[tr], "", &m);
        let output = out[0]["output"].as_array().unwrap();
        assert_eq!(output[0]["type"], "input_text");
        assert_eq!(output[1]["type"], "input_image");
        assert_eq!(output[1]["image_url"], "data:image/png;base64,AA==");
    }

    #[test]
    fn non_vision_model_downgrades_tool_images() {
        let mut m = model(true);
        m.input = vec!["text".into()];
        let mut tr = AgentMessage::user_text("");
        tr.role = "toolResult".into();
        tr.tool_call_id = Some("call_1".into());
        tr.content = vec![ContentBlock::Image {
            data: "AA==".into(),
            mime_type: "image/png".into(),
        }];
        let out = convert_responses_messages(&[tr], "", &m);
        assert_eq!(out[0]["output"], "(see attached image)");
    }

    #[test]
    fn message_usage_cost_set_from_model_pricing_before_clone() {
        // 回归：cost 必须先结算再克隆进 message——否则底栏/session 恒为 $0
        let mut m = model(false);
        m.cost = Some(json!({
            "input": 1.0,
            "output": 2.0,
            "cacheRead": 0.5,
            "cacheWrite": 0.25,
        }));
        let usage = Usage {
            input: 1000,
            output: 2000,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: 3000,
            cost: Cost::default(),
        };
        let res = build_responses_result(
            Vec::new(),
            Some("stop".into()),
            None,
            Some(usage),
            None,
            None,
            &m,
        )
        .unwrap();
        // input 1000*1 + output 2000*2 = 0.005（$/1M tokens）
        let u = res.message.usage.expect("usage attached to message");
        assert!(
            (u.cost.total - 0.005).abs() < 1e-12,
            "message cost.total={} expected 0.005",
            u.cost.total
        );
        assert!((res.usage.unwrap().cost.total - 0.005).abs() < 1e-12);
    }

    /// pi #9974：服务器省略 output_index（如 llama.cpp）会让多个 output item 落到同一索引，
    /// 后到者覆盖前者，参数必然错位 → 流以错误终止而不是把混排的工具调用交出去。
    #[tokio::test]
    async fn reused_output_index_ends_with_error() {
        let feed = feed_of(&[
            r#"{"type":"response.created","response":{"id":"r"}}"#,
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"c1","id":"i1","name":"bash","arguments":""}}"#,
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"c2","id":"i2","name":"bash","arguments":""}}"#,
        ]);
        let mut on_event = None;
        let err = process_responses_feed(&model(false), &mut on_event, feed)
            .await
            .expect_err("混合 output_index 的流必须报错");
        assert!(
            err.to_string().contains("reused output_index 0"),
            "unexpected error: {err}"
        );
    }

    /// pi #9974：工具调用未收到 output_item.done 时参数可能被截断，拒绝交出。
    #[tokio::test]
    async fn unfinished_tool_call_ends_with_error() {
        let feed = feed_of(&[
            r#"{"type":"response.created","response":{"id":"r"}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"c1","id":"i1","name":"bash","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"cmd\":\"ls"}"#,
            r#"{"type":"response.completed","response":{"id":"r","status":"completed"}}"#,
        ]);
        let mut on_event = None;
        let err = process_responses_feed(&model(false), &mut on_event, feed)
            .await
            .expect_err("未定稿的工具调用必须报错");
        assert!(
            err.to_string().contains("unfinished tool call: bash (c1)"),
            "unexpected error: {err}"
        );
    }

    /// 正常流（added → done → completed）不受上述检查影响。
    #[tokio::test]
    async fn finished_tool_call_passes() {
        let feed = feed_of(&[
            r#"{"type":"response.created","response":{"id":"r"}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"c1","id":"i1","name":"bash","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"cmd\":\"ls\"}"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"c1","id":"i1","name":"bash","arguments":"{\"cmd\":\"ls\"}"}}"#,
            r#"{"type":"response.completed","response":{"id":"r","status":"completed"}}"#,
        ]);
        let mut on_event = None;
        let res = process_responses_feed(&model(false), &mut on_event, feed)
            .await
            .expect("正常流不应报错");
        assert_eq!(res.message.stop_reason.as_deref(), Some("toolUse"));
        assert_eq!(res.message.content.len(), 1);
    }

    /// pi #10034：GPT-6 系列在 Fast 模式下回报 service_tier: "fast"，需按 fast 价（2x）计。
    #[tokio::test]
    async fn fast_service_tier_applies_multiplier() {
        let mut m = model(false);
        m.model_id = "gpt-6-luna".into();
        m.cost = Some(json!({
            "input": 2.0,
            "output": 10.0,
            "cacheRead": 0.1,
            "cacheWrite": 2.5
        }));
        let feed = feed_of(&[
            r#"{"type":"response.created","response":{"id":"r"}}"#,
            r#"{"type":"response.completed","response":{"id":"r","status":"completed","service_tier":"fast","usage":{"input_tokens":100000,"output_tokens":100000,"total_tokens":200000,"input_tokens_details":{"cached_tokens":0}}}}"#,
        ]);
        let mut on_event = None;
        let res = process_responses_feed(&m, &mut on_event, feed)
            .await
            .unwrap();
        let u = res.usage.unwrap();
        // input 100k*2.0/1e6*2 = 0.4；output 100k*10/1e6*2 = 2.0
        assert!((u.cost.input - 0.4).abs() < 1e-12, "input={}", u.cost.input);
        assert!(
            (u.cost.output - 2.0).abs() < 1e-12,
            "output={}",
            u.cost.output
        );
        assert!((u.cost.total - 2.4).abs() < 1e-12, "total={}", u.cost.total);
        // message 里的 usage 与返回值一致（底栏/session 计入同一数）
        let mu = res.message.usage.unwrap();
        assert!((mu.cost.total - 2.4).abs() < 1e-12);
    }

    /// Sign in with ChatGPT（直连 api.openai.com 的非 sk- 凭据）不发 max_output_tokens / temperature；
    /// API key 与其它 OpenAI 兼容端点不受影响。
    #[test]
    fn chatgpt_sign_in_omits_rejected_request_fields() {
        let m = model(false);
        let sign_in = ModelConfig {
            model_id: "gpt-5.5".into(),
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: "chatgpt-access-token".into(),
            ..m.clone()
        };
        let body = build_request_body(
            &sign_in,
            &[],
            "",
            &[],
            None,
            Some(0.5),
            Some(4096),
            None,
            None,
        );
        assert!(!body.contains_key("max_output_tokens"));
        assert!(!body.contains_key("temperature"));
        assert_eq!(body["store"], json!(false));

        // API key 直连：保留
        let api_key = ModelConfig {
            api_key: "sk-proj-test".into(),
            ..sign_in.clone()
        };
        let body = build_request_body(
            &api_key,
            &[],
            "",
            &[],
            None,
            Some(0.5),
            Some(4096),
            None,
            None,
        );
        assert_eq!(body["max_output_tokens"], json!(4096));
        assert_eq!(body["temperature"], json!(0.5));

        // 其它端点（即使凭据非 sk-）：保留
        let gateway = ModelConfig {
            base_url: "https://gateway.example.com/v1".into(),
            api_key: "gateway-key".into(),
            ..sign_in.clone()
        };
        let body = build_request_body(
            &gateway,
            &[],
            "",
            &[],
            None,
            Some(0.5),
            Some(4096),
            None,
            None,
        );
        assert_eq!(body["max_output_tokens"], json!(4096));
        assert_eq!(body["temperature"], json!(0.5));
    }

    /// 采样参数：目录 `samplingParams` 只补空键，按 thinking level 的覆盖项后写覆盖。
    #[test]
    fn sampling_params_follow_thinking_level_overrides() {
        let mut m = model(false);
        m.sampling_params = Some(json!({ "temperature": 1.0, "top_p": 0.95 }));

        // 无级别覆盖：目录值补上未被显式设置的键，不压过显式 temperature
        let body = build_request_body(&m, &[], "", &[], None, Some(0.5), None, None, None);
        assert_eq!(body["temperature"], json!(0.5));
        assert_eq!(body["top_p"], json!(0.95));

        // 级别覆盖：后写覆盖显式 temperature，并带上级别专属键
        let level = json!({ "temperature": 0.6, "top_k": 64 });
        let body = build_request_body(&m, &[], "", &[], None, Some(0.5), None, None, Some(&level));
        assert_eq!(body["temperature"], json!(0.6));
        assert_eq!(body["top_k"], json!(64));
        assert_eq!(body["top_p"], json!(0.95));
    }

    /// ChatGPT 订阅用量上限错误：附加用量页链接；其它错误体原样。
    #[test]
    fn chatgpt_usage_limit_error_links_usage_page() {
        let limited = provider_error_body(
            r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}"#,
        );
        assert!(limited.contains("https://chatgpt.com/settings/usage"));
        let other = provider_error_body(r#"{"error":{"code":"invalid_request"}}"#);
        assert!(!other.contains("chatgpt.com"));
    }

    fn feed_of(events: &[&str]) -> impl futures_util::Stream<Item = Result<bytes::Bytes>> + Unpin {
        let body: String = events.iter().map(|e| format!("data: {}\n\n", e)).collect();
        futures_util::stream::iter(vec![Ok(bytes::Bytes::from(body))])
    }
}
