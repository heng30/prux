//! OpenAI chat completions 协议（deepseek / 多数 openai 兼容端点）。

use super::{
    AgentMessage, ContentBlock, Cost, ModelConfig, PartialTranslator, RawStreamEvent, StreamResult,
    Usage, azure,
    convert::{
        BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX, COMPACTION_SUMMARY_PREFIX,
        COMPACTION_SUMMARY_SUFFIX, sanitize_surrogates, truncate_for_error,
    },
    provider_extra_headers, track_stream_bytes,
    usage::{compute_cost, parse_streaming_json},
};
use crate::{
    core::{
        self, model_resolver,
        provider::{apply_payload_hook, grammar_input_property_for_tool},
        tools::tool_defs,
    },
    error::{Error, Result},
    utils::{http, time::now_ms},
};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;

/// 工具结果转换结果。
struct ConvertedToolResult {
    /// `role: "tool"` 消息
    tool: Value,
    /// 工具结果里的图片（data URL），由调用方延后到本批次末尾补发
    image_urls: Vec<String>,
}

// 请求/响应结构（OpenAI chat completions）
/// 按模型能力（工具、thinking、图片）组装的请求体，序列化后发往兼容端点。
#[derive(Serialize)]
struct ChatRequest {
    /// 请求使用的模型 id（取自 `ModelConfig.model_id`）。
    model: String,
    /// 已转成 OpenAI 格式的对话消息数组。
    messages: Vec<Value>,
    /// 固定为 true，向端点请求 SSE 流式响应。
    stream: bool,
    /// 输出 token 上限；None 表示不限制，序列化时按模型字段名注入。
    // max_tokens 不直接序列化：由 stream() 按 model.max_tokens_field 注入
    // （避免与 max_completion_tokens 同体出现导致 400）
    #[serde(skip)]
    max_tokens: Option<u32>,
    /// 请求流式用量统计（`include_usage`）；模型不支持流式用量时为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<Value>,
    /// 已转换的工具定义；无工具时为 None（避免发空数组）。
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
    /// zai/GLM 等 thinkingFormat 的 thinking 对象参数，按思考档位构造。
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
    /// 思考强度档位（如 low/high/none），模型不支持或关闭时不发。
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    /// qwen thinkingFormat：enable_thinking 布尔
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_thinking: Option<bool>,
    /// openrouter/together/baseten thinkingFormat：reasoning 对象
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Value>,
    /// qwen-chat-template thinkingFormat：chat_template_kwargs 对象
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_template_kwargs: Option<Value>,
    /// 采样温度；None 时由 samplingParams 的默认值兜底。
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    /// "auto" | "none"（provider-neutral）
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
}

/// 跨协议 tool call id 归一化：responses 的 `call|item` 复合 id 重放到
/// completions 时替换非法字符并截断（对齐 pi transformMessages）
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

/// 为仍未配对的 tool_calls 补 synthetic tool result。
///
/// `pending` 在收到对应 toolResult 时立即移除，因此这里剩下的就是真正没有结果的调用，
/// 无需再维护 `existing` 集合去重。
fn insert_synthetic_tool_results(params: &mut Vec<Value>, pending: &mut Vec<(String, String)>) {
    for (id, _name) in pending.drain(..) {
        params.push(json!({
            "role": "tool",
            "content": "No result provided",
            "tool_call_id": id
        }));
    }
}

/// 把 user 消息转成 openai 的 `user` 消息（content 为 parts 数组）。
/// 模型不支持图片时把图片块替换成占位文本；无任何可发内容时返回 None。
fn convert_user_message(msg: &AgentMessage, model: &ModelConfig) -> Option<Value> {
    let supports_images = model.input.iter().any(|x| x == "image");
    let mut content_arr: Vec<Value> = Vec::new();
    let mut previous_was_placeholder = false;
    for block in &msg.content {
        match block {
            ContentBlock::Text { text, .. } => {
                // 纯图片 user 消息不能带空 text part，否则部分 OpenAI-compatible provider 直接拒绝。
                if !text.is_empty() {
                    content_arr.push(json!({ "type": "text", "text": sanitize_surrogates(text) }));
                }
                previous_was_placeholder = text == "(image omitted: model does not support images)";
            }
            ContentBlock::Image { data, mime_type } => {
                if supports_images {
                    content_arr.push(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{}", mime_type, data)
                        }
                    }));
                    previous_was_placeholder = false;
                } else if !previous_was_placeholder {
                    content_arr.push(json!({
                        "type": "text",
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

/// 转换 assistant 消息；同时把本消息中未配对的 tool_calls 记入 pending,供
/// 后续消息插入 synthetic tool results 使用
fn convert_assistant_message(
    msg: &AgentMessage,
    model: &ModelConfig,
    pending_tool_calls: &mut Vec<(String, String)>,
) -> Option<Value> {
    // 跳过 error/aborted 的 assistant 消息
    if msg.stop_reason.as_deref() == Some("error") || msg.stop_reason.as_deref() == Some("aborted")
    {
        return None;
    }
    let assistant_text: String = msg
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");

    let mut assistant_msg = serde_json::Map::new();
    assistant_msg.insert("role".into(), json!("assistant"));
    // openai SDK 默认 content 为 null；deepseek 需要 content
    assistant_msg.insert("content".into(), Value::Null);

    let non_empty_thinking: Vec<String> = msg
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                Some(thinking.clone())
            }
            _ => None,
        })
        .collect();

    if !non_empty_thinking.is_empty() {
        if !assistant_text.is_empty() {
            assistant_msg.insert("content".into(), json!(assistant_text));
        }
        assistant_msg.insert(
            "reasoning_content".into(),
            json!(non_empty_thinking.join("\n")),
        );
    } else if !assistant_text.is_empty() {
        assistant_msg.insert("content".into(), json!(assistant_text));
    }

    let tool_calls: Vec<&ContentBlock> = msg
        .content
        .iter()
        .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
        .collect();

    if !tool_calls.is_empty() {
        let grammar_supported =
            model_resolver::supports_openai_grammar_tools(&model.provider, &model.model_id);

        let calls: Vec<Value> = tool_calls
            .iter()
            .map(|b| match b {
                ContentBlock::ToolCall { id, name, arguments, .. } => {
                    // grammar 工具的历史调用要回放成 `custom` 形状（裸源码），否则模型看到的是一段被 JSON 转义过的代码
                    if grammar_supported
                        && let Some(property) = grammar_input_property_for_tool(name)
                    {
                        return json!({
                            "id": normalize_tool_call_id(id),
                            "type": "custom",
                            "custom": {
                                "name": name,
                                "input": arguments
                                    .get(&property)
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default(),
                            }
                        });
                    }
                    json!({
                        "id": normalize_tool_call_id(id),
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(arguments).unwrap_or_else(|_| "{}".into())
                        }
                    })
                }
                _ => Value::Null,
            })
            .collect();
        assistant_msg.insert("tool_calls".into(), Value::Array(calls));
        for tc in &tool_calls {
            if let ContentBlock::ToolCall { id, name, .. } = tc {
                pending_tool_calls.push((normalize_tool_call_id(id), name.clone()));
            }
        }
    }

    if model.requires_reasoning_content_on_assistant_messages
        && model.reasoning
        && assistant_msg.get("reasoning_content").is_none()
    {
        assistant_msg.insert("reasoning_content".into(), json!(""));
    }

    let has_content = match assistant_msg.get("content") {
        Some(Value::Null) => false,
        Some(Value::String(s)) => !s.is_empty(),
        _ => false,
    };
    if !has_content && !assistant_msg.contains_key("tool_calls") {
        return None;
    }
    Some(Value::Object(assistant_msg))
}

/// 把压缩/分支摘要消息包上前后缀，转成一条 `user` 文本消息（上游不认 summary 角色）。
fn convert_summary_message(msg: &AgentMessage) -> Value {
    let (prefix, suffix) = if msg.role == "compactionSummary" {
        (COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX)
    } else {
        (BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX)
    };
    let text = format!("{}{}{}", prefix, msg.text(), suffix);
    json!({ "role": "user", "content": [{"type": "text", "text": sanitize_surrogates(&text)}] })
}

/// 工具结果消息：返回 `tool` 消息；若含图片，图片块单独返回给调用方延后补发。
///
/// 上游不允许 `tool` 消息直接带图片，也不允许 user 消息插在 `assistant.tool_calls`
/// 与它的 `tool` 结果之间（见 [`finish_tool_batch`]），所以图片不能在这里自行追加消息。
fn convert_tool_result_message(msg: &AgentMessage, model: &ModelConfig) -> ConvertedToolResult {
    let supports_images = model.input.iter().any(|x| x == "image");
    let mut text_result: Vec<String> = Vec::new();
    let mut image_parts: Vec<String> = Vec::new();
    let mut previous_was_placeholder = false;
    for block in &msg.content {
        match block {
            ContentBlock::Text { text, .. } => {
                text_result.push(text.clone());
                previous_was_placeholder =
                    text == "(tool image omitted: model does not support images)";
            }
            ContentBlock::Image { data, mime_type } => {
                if supports_images {
                    image_parts.push(format!("data:{};base64,{}", mime_type, data));
                    previous_was_placeholder = false;
                } else if !previous_was_placeholder {
                    text_result
                        .push("(tool image omitted: model does not support images)".to_string());
                    previous_was_placeholder = true;
                }
            }
            _ => {}
        }
    }
    let text = text_result.join("\n");
    let has_images = !image_parts.is_empty();
    let has_text = !text.is_empty();
    let tool_result_text = if has_text {
        text
    } else if has_images {
        "(see attached image)".to_string()
    } else {
        "(no tool output)".to_string()
    };
    let mut tool_msg = serde_json::Map::new();
    tool_msg.insert("role".into(), json!("tool"));
    tool_msg.insert(
        "content".into(),
        json!(sanitize_surrogates(&tool_result_text)),
    );
    tool_msg.insert(
        "tool_call_id".into(),
        json!(
            msg.tool_call_id
                .clone()
                .map(|id| normalize_tool_call_id(&id))
                .unwrap_or_default()
        ),
    );

    ConvertedToolResult {
        tool: Value::Object(tool_msg),
        image_urls: image_parts,
    }
}

/// 收尾一个 `assistant.tool_calls` 批次：补发工具结果里的图片 + 可选的空 assistant。
///
/// 上游（opencode zen / OpenAI-compatible）要求 `assistant(tool_calls)` 之后紧跟着
/// **全部** `tool` 消息，中间插入 user 消息会被拒：
///
/// ```text
/// provider returned 400 Bad Request: {"model":"deepseek-v4.1-flash"}
/// ```
///
/// 因此一批里若只有部分工具结果带图片，图片消息必须等到批次末尾（`pending` 清空）才能插入。
/// 历史一旦落库带上了这个顺序，该会话之后每次请求都会 400。
fn finish_tool_batch(
    params: &mut Vec<Value>,
    deferred_image_urls: &mut Vec<String>,
    model: &ModelConfig,
) {
    if !deferred_image_urls.is_empty() {
        let mut arr: Vec<Value> = vec![json!({
            "type": "text",
            "text": "Attached image(s) from tool result:"
        })];

        arr.extend(deferred_image_urls.drain(..).map(|url| {
            json!({
                "type": "image_url",
                "image_url": { "url": url }
            })
        }));
        params.push(json!({ "role": "user", "content": arr }));
    }

    // 工具结果（含补发的图片）之后插 assistant 空消息
    if model.requires_assistant_after_tool_result {
        params.push(json!({ "role": "assistant", "content": [] }));
    }
}

/// 关闭一个还没走到末尾的工具批次（例：下一轮 user 消息提前到达、历史从中间截断）：
/// 先给未配对的 tool_calls 补 synthetic 结果，再补发图片。
///
/// `had_results`：本批次是否真的发出过 tool 结果（纯 synthetic 补位不算，与旧行为一致）。
fn close_tool_batch(
    params: &mut Vec<Value>,
    pending: &mut Vec<(String, String)>,
    deferred_image_urls: &mut Vec<String>,
    model: &ModelConfig,
    had_results: bool,
) {
    if pending.is_empty() {
        return; // 已经收尾过（图片也早已补发）
    }

    insert_synthetic_tool_results(params, pending);

    if !had_results {
        return; // 只有 synthetic 补位：不发空 assistant
    }

    finish_tool_batch(params, deferred_image_urls, model);
}

/// 把内部 [`AgentMessage`] 历史整体转成 openai `chat/completions` 的 messages 数组。
///
/// 除逐条转换外还负责修补协议约束：跳过 error/aborted 的 assistant、为孤儿 tool_calls 补
/// synthetic tool result、丢弃对不上 tool_calls 的孤儿 tool 结果，并把工具结果里的图片
/// 延后到批次末尾补发（见 [`finish_tool_batch`]）。
pub fn convert_messages(
    messages: &[AgentMessage],
    system_prompt: &str,
    model: &ModelConfig,
) -> Vec<Value> {
    let mut params: Vec<Value> = Vec::new();

    if !system_prompt.is_empty() {
        let role = if model.reasoning && model.supports_developer_role {
            "developer"
        } else {
            "system"
        };
        params.push(json!({ "role": role, "content": sanitize_surrogates(system_prompt) }));
    }

    // - 跳过 stopReason=error/aborted 的 assistant 消息
    // - 为孤儿 tool_calls 插入 synthetic tool results
    // - 丢弃无法对上 assistant tool_calls 的孤儿 tool 结果
    let mut pending_tool_calls: Vec<(String, String)> = Vec::new(); // (id, name)
    //
    // 本批次工具结果里的图片：必须等 `pending_tool_calls` 清空后才能插进消息流（见 [`finish_tool_batch`]；插在批次中间会 400）。
    let mut deferred_image_urls: Vec<String> = Vec::new();

    // 本批次是否已发出真实 tool 结果（用于决定收尾时是否补空 assistant）
    let mut batch_emitted_results = false;

    for msg in messages {
        match msg.role.as_str() {
            "user" => {
                close_tool_batch(
                    &mut params,
                    &mut pending_tool_calls,
                    &mut deferred_image_urls,
                    model,
                    batch_emitted_results,
                );
                batch_emitted_results = false;
                if let Some(user_msg) = convert_user_message(msg, model) {
                    params.push(user_msg);
                }
            }
            "assistant" => {
                close_tool_batch(
                    &mut params,
                    &mut pending_tool_calls,
                    &mut deferred_image_urls,
                    model,
                    batch_emitted_results,
                );
                batch_emitted_results = false;
                if let Some(assistant_msg) =
                    convert_assistant_message(msg, model, &mut pending_tool_calls)
                {
                    params.push(assistant_msg);
                }
            }
            "compactionSummary" | "branchSummary" => {
                close_tool_batch(
                    &mut params,
                    &mut pending_tool_calls,
                    &mut deferred_image_urls,
                    model,
                    batch_emitted_results,
                );
                batch_emitted_results = false;
                params.push(convert_summary_message(msg));
            }
            // custom 角色转 user（扩展注入消息进上下文）
            "custom" => {
                close_tool_batch(
                    &mut params,
                    &mut pending_tool_calls,
                    &mut deferred_image_urls,
                    model,
                    batch_emitted_results,
                );
                batch_emitted_results = false;
                if let Some(user_msg) = convert_user_message(msg, model) {
                    params.push(user_msg);
                }
            }
            "toolResult" => {
                // 只有能对上前面 assistant tool_calls 的结果才发送；否则丢弃。
                // 历史可能因扩展 transform_context 删除 assistant、或压缩保留尾部从批次中间
                // 截断而留下孤儿 tool 结果，直接发送会触发上游 400：
                // "Messages with role 'tool' must be a response to a preceding message with 'tool_calls'"。
                let norm_id = msg.tool_call_id.as_deref().map(normalize_tool_call_id);
                match norm_id {
                    Some(id) if pending_tool_calls.iter().any(|(p, _)| *p == id) => {
                        pending_tool_calls.retain(|(p, _)| *p != id);

                        let converted = convert_tool_result_message(msg, model);
                        params.push(converted.tool);
                        deferred_image_urls.extend(converted.image_urls);
                        batch_emitted_results = true;

                        // 批次内最后一个 tool 结果已到位 → 现在才能插图片消息
                        if pending_tool_calls.is_empty() {
                            finish_tool_batch(&mut params, &mut deferred_image_urls, model);
                            batch_emitted_results = false;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // 列表尾部若仍有未配对的 tool_calls（中止批次 / 崩溃恢复等半截历史），补 synthetic tool results，
    // 避免上游校验 "assistant message with 'tool_calls' must be followed by tool messages ..." 报 400。
    close_tool_batch(
        &mut params,
        &mut pending_tool_calls,
        &mut deferred_image_urls,
        model,
        batch_emitted_results,
    );

    params
}

/// 把 `(name, description, schema)` 工具定义转成 openai 的 function 工具数组（不启用 strict / grammar）。
pub fn convert_tools(tool_defs: &[(String, String, Value)]) -> Vec<Value> {
    convert_tools_inner(tool_defs, false, false)
}

/// 带 strict mode 的工具转换
pub fn convert_tools_with_strict(
    tool_defs: &[(String, String, Value)],
    strict: bool,
) -> Vec<Value> {
    convert_tools_inner(tool_defs, strict, false)
}

/// 带全部能力的工具转换：`strict` = 模型支持 strict mode，`grammar` = 模型支持 grammar 约束工具。
pub(crate) fn convert_tools_with_capabilities(
    tool_defs: &[(String, String, Value)],
    strict: bool,
    grammar: bool,
) -> Vec<Value> {
    convert_tools_inner(tool_defs, strict, grammar)
}

/// 工具是否声明了 constrainedSampling。内置工具查 ToolDef，扩展工具查 ExtensionTool。
fn tool_has_constrained_sampling(name: &str) -> bool {
    if let Some(def) = tool_defs(std::slice::from_ref(&name.to_string())).first()
        && def.constrained_sampling
    {
        return true;
    }

    core::extensions::registered().iter().any(|ext| {
        ext.tools()
            .iter()
            .any(|t| t.name == name && t.constrained_sampling)
    })
}

/// 工具转换的公共实现：`strict_supported` 时给声明了 constrainedSampling 的工具补 `strict`
/// 字段，并追加 `additionalProperties: false` 与全字段 required；`grammar_supported` 时把
/// 声明了语法约束的工具转成 `custom` 工具（裸源码输入，对齐 pi/OpenAI）。
fn convert_tools_inner(
    tool_defs: &[(String, String, Value)],
    strict_supported: bool,
    grammar_supported: bool,
) -> Vec<Value> {
    tool_defs
        .iter()
        .map(|(name, description, parameters)| {
            if grammar_supported && let Some(grammar) = core::provider::grammar_tool(parameters) {
                return json!({
                    "type": "custom",
                    "custom": {
                        "name": name,
                        "description": description,
                        "format": {
                            "type": "grammar",
                            "grammar": {
                                "syntax": grammar.syntax,
                                "definition": grammar.definition,
                            }
                        }
                    }
                });
            }

            let mut params = parameters.clone();
            // 逐工具 strict（仅声明 constrainedSampling 的工具在模型支持时 strict 化；
            // 其余保留原始 schema）。strict_supported 对应 provider 的 supportsStrictMode。
            let this_strict = strict_supported && tool_has_constrained_sampling(name);
            if this_strict && params.is_object() {
                if !params.get("additionalProperties").is_some() {
                    params["additionalProperties"] = json!(false);
                }

                // required 全字段（strict 模式要求）
                if let Some(props) = params.get("properties").and_then(|p| p.as_object()) {
                    let required: Vec<Value> = props.keys().map(|k| json!(k)).collect();
                    if !params.get("required").is_some() {
                        params["required"] = Value::Array(required);
                    }
                }
            }

            let mut function =
                json!({ "name": name, "description": description, "parameters": params });

            // provider 支持 strict mode 时才带 strict 字段（部分 provider 拒绝未知字段）
            if strict_supported {
                function["strict"] = json!(this_strict);
            }

            json!({ "type": "function", "function": function })
        })
        .collect()
}

/// 以 openai `chat/completions` SSE 流式发起一次请求，边解析边通过 `on_event` 转发事件。
///
/// 返回累积完成的 [`StreamResult`]；HTTP 传输/状态错误返回 `Err`，流内 provider 错误则
/// 由 [`StreamState`] 编码进结果（stopReason=error + errorMessage）。
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
    tool_choice: Option<&str>,
) -> Result<StreamResult> {
    // 按模型能力钳制级别；"off" 归一到 None（避免把 off 当启用级别发给 provider）
    // 级别覆盖项要在归一之前按级别名查出（归一后 off 已丢失）
    let level_sampling_params = model.sampling_params_for_level(reasoning_effort).cloned();
    let reasoning_effort = normalize_reasoning_effort(model, reasoning_effort);
    let client = http::build_client()?;

    let mut openai_messages = convert_messages(messages, system_prompt, model);
    openai_messages.retain(|m| !m.is_null()); // 忽略空消息

    let mut request = build_chat_request(model, &openai_messages, tools, temperature, max_tokens);
    if let Some(tc) = tool_choice {
        request.tool_choice = Some(tc.to_string());
    }
    apply_thinking_config(&mut request, model, reasoning_effort.as_deref());
    let mut body = serialize_request_body(&request, model, level_sampling_params.as_ref())?;
    apply_payload_hook(&mut body, on_payload.as_ref());

    let response = send_completion_request(&client, model, body).await?;
    let mut stream = response.bytes_stream();

    let mut state = StreamState::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut total_bytes: usize = 0;

    // 供应商原始流事件：无扩展订阅时零开销（一次流只查一次）
    let capture_stream_events = core::extensions::has_provider_stream_event_handlers();

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => return Err(Error::msg(format!("stream read failed: {}", e))),
        };
        track_stream_bytes(&mut total_bytes, chunk.len())?;
        // 按行拆分 SSE（可能跨 chunk）
        let (lines, carry) = split_sse_lines(&mut buf, &chunk);
        buf = carry;

        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                break;
            }
            let chunk_val: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if capture_stream_events {
                core::extensions::dispatch_provider_stream_event(
                    &model.provider,
                    &model.api,
                    &model.model_id,
                    &chunk_val,
                );
            }
            state.handle_chunk(&chunk_val, model, &mut on_event)?;
        }
    }

    state.finish(model, &mut on_event)
}

/// 按模型能力钳制级别；"off" 归一到 None
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

/// 组装 openai 兼容请求体：模型 id、messages、stream、max_tokens、usage 流选项与工具定义。
/// thinking 相关字段留空，由 [`apply_thinking_config`] 按模型配置补。
fn build_chat_request(
    model: &ModelConfig,
    openai_messages: &[Value],
    tools: &[(String, String, Value)],
    temperature: Option<f64>,
    max_tokens: Option<u32>,
) -> ChatRequest {
    ChatRequest {
        model: azure::request_model_name(model).into_owned(),
        messages: openai_messages.to_vec(),
        stream: true,
        max_tokens: max_tokens.or(model.max_tokens),
        stream_options: if model.supports_usage_in_streaming {
            Some(json!({ "include_usage": true }))
        } else {
            None
        },
        tools: if tools.is_empty() {
            None
        } else {
            Some(convert_tools_with_capabilities(
                tools,
                model.supports_strict_mode,
                model_resolver::supports_openai_grammar_tools(&model.provider, &model.model_id),
            ))
        },
        thinking: None,
        reasoning_effort: None,
        enable_thinking: None,
        reasoning: None,
        chat_template_kwargs: None,
        temperature,
        tool_choice: None,
    }
}

/// openai 兼容思考参数：按 `thinkingFormat` 构造 thinking / reasoning_effort /
/// reasoning / enable_thinking / chat_template_kwargs。
/// 关闭档（reasoning_effort 为 None）时：`thinkingLevelMap.off` 显式为 null → 什么都不发；
/// 缺失或为字符串 → 发对应关闭值。
fn apply_thinking_config(
    request: &mut ChatRequest,
    model: &ModelConfig,
    reasoning_effort: Option<&str>,
) {
    if !model.reasoning {
        return;
    }

    // thinkingLevelMap 命中值；缺省回退到级别名
    let mapped = |level: &str| -> String {
        model
            .thinking_level_map
            .as_ref()
            .and_then(|m| m.get(level))
            .and_then(|v| v.as_deref())
            .unwrap_or(level)
            .to_string()
    };
    // off 的显式值：None=未声明，Some(None)=显式不支持，Some(Some(v))=关闭值
    let off_value = || model.thinking_level_map.as_ref().and_then(|m| m.get("off"));
    // 缺失或字符串时允许显式关闭
    let can_disable = || off_value().is_none_or(|v| v.is_some());

    match model.thinking_format.as_str() {
        // zai/GLM：thinking 对象 + 可选 reasoning_effort
        "zai" => {
            request.thinking = Some(if reasoning_effort.is_some() {
                json!({ "type": "enabled", "clear_thinking": false })
            } else {
                json!({ "type": "disabled" })
            });
            if let Some(level) = reasoning_effort
                && model.supports_reasoning_effort
            {
                request.reasoning_effort = Some(mapped(level));
            }
        }
        // qwen/DashScope：enable_thinking 布尔 + 可选 reasoning_effort
        "qwen" => {
            request.enable_thinking = Some(reasoning_effort.is_some());
            if let Some(level) = reasoning_effort
                && model.supports_reasoning_effort
            {
                request.reasoning_effort = Some(mapped(level));
            }
        }
        "qwen-chat-template" => {
            request.chat_template_kwargs = Some(json!({
                "enable_thinking": reasoning_effort.is_some(),
                "preserve_thinking": true,
            }));
        }
        "chat-template" => {
            // 由 model_overrides/自定义模板处理；此处不注入
        }
        // baseten：仅 reasoning_effort（关闭时用 off 映射）
        "baseten" => {
            if model.supports_reasoning_effort {
                request.reasoning_effort = match reasoning_effort {
                    Some(level) => Some(mapped(level)),
                    None => off_value().and_then(|v| v.clone()),
                };
            }
        }
        // deepseek：thinking 对象 + reasoning_effort
        "deepseek" => {
            if reasoning_effort.is_some() {
                request.thinking = Some(json!({ "type": "enabled" }));
            } else if can_disable() {
                request.thinking = Some(json!({ "type": "disabled" }));
            }
            if let Some(level) = reasoning_effort
                && model.supports_reasoning_effort
            {
                request.reasoning_effort = Some(mapped(level));
            }
        }
        // openrouter：reasoning.effort（关闭回退 "none"）
        "openrouter" => {
            if let Some(level) = reasoning_effort {
                request.reasoning = Some(json!({ "effort": mapped(level) }));
            } else if can_disable() {
                let off = off_value()
                    .and_then(|v| v.clone())
                    .unwrap_or_else(|| "none".to_string());
                request.reasoning = Some(json!({ "effort": off }));
            }
        }
        // ant-ling：仅显式映射的启用级别才发 reasoning.effort
        "ant-ling" => {
            if let Some(level) = reasoning_effort
                && let Some(Some(effort)) =
                    model.thinking_level_map.as_ref().and_then(|m| m.get(level))
            {
                request.reasoning = Some(json!({ "effort": effort.clone() }));
            }
        }
        // together：reasoning.enabled + 可选 reasoning_effort
        "together" => {
            request.reasoning = Some(json!({ "enabled": reasoning_effort.is_some() }));
            if let Some(level) = reasoning_effort
                && model.supports_reasoning_effort
            {
                request.reasoning_effort = Some(mapped(level));
            }
        }
        // string-thinking：thinking 为字符串
        "string-thinking" => {
            if let Some(level) = reasoning_effort {
                request.thinking = Some(json!(mapped(level)));
            } else if can_disable() {
                let off = off_value()
                    .and_then(|v| v.clone())
                    .unwrap_or_else(|| "none".to_string());
                request.thinking = Some(json!(off));
            }
        }
        // 默认 openai 风格：reasoning_effort（关闭时用 off 映射的字符串值）
        _ => {
            if !model.supports_reasoning_effort {
                return;
            }
            if let Some(level) = reasoning_effort {
                request.reasoning_effort = Some(mapped(level));
            } else if let Some(Some(off)) = off_value() {
                request.reasoning_effort = Some(off.clone());
            }
        }
    }
}

/// 序列化请求；max_tokens 字段名由模型兼容配置决定（max_completion_tokens / max_tokens）。
///
/// `level_sampling_params` 是当前 thinking level 的 `samplingParamsByThinkingLevel`
/// 覆盖项（[`ModelConfig::sampling_params_for_level`]），与模型目录 `samplingParams` 不同：
/// 后写覆盖请求体里已有的同名键。
fn serialize_request_body(
    request: &ChatRequest,
    model: &ModelConfig,
    level_sampling_params: Option<&Value>,
) -> Result<String> {
    let mut v = serde_json::to_value(request).map_err(|source| Error::Json {
        context: "failed to serialize request".to_string(),
        source,
    })?;
    if let Some(mt) = request.max_tokens {
        v[&model.max_tokens_field] = json!(mt);
    }

    // samplingParams 全键合并进请求体（在 pi 自设字段之后，
    // 即合并只补默认未设的键，不覆盖显式 temperature/max_tokens/thinking 等）
    if let Some(sp) = &model.sampling_params
        && let Some(obj) = sp.as_object()
    {
        for (k, val) in obj {
            if !v.as_object().is_some_and(|o| o.contains_key(k)) {
                v[k] = val.clone();
            }
        }
    }

    // compat 的 routing 发到请求体 `provider` 字段
    if let Some(r) = &model.provider_routing
        && !v.get("provider").is_some()
    {
        v["provider"] = r.clone();
    }

    // 默认请求带 store=false（避免 store 语义差异）
    if model.supports_store && !v.get("store").is_some() {
        v["store"] = json!(false);
    }

    // 级别覆盖最后写
    if let Some(obj) = v.as_object_mut() {
        core::provider::apply_level_sampling_params(obj, level_sampling_params);
    }

    serde_json::to_string(&v).map_err(|source| Error::Json {
        context: "failed to serialize request".to_string(),
        source,
    })
}

/// 发送请求；非 2xx 时提取响应体并返回 ProviderStatus 错误
async fn send_completion_request(
    client: &reqwest::Client,
    model: &ModelConfig,
    body: String,
) -> Result<reqwest::Response> {
    let req = client
        .post(model_endpoint(model))
        .header("Content-Type", "application/json")
        .bearer_auth(&model.api_key)
        .headers(provider_extra_headers(model));

    let response = req.body(body).send().await.map_err(Error::from)?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(Error::ProviderStatus {
            status,
            body: truncate_for_error(&text),
        });
    }
    Ok(response)
}

/// 追加 chunk 并按 \n 切出完整 SSE 行；返回 (完整行, 未完成的尾部字节)
fn split_sse_lines(buf: &mut Vec<u8>, chunk: &[u8]) -> (Vec<String>, Vec<u8>) {
    buf.extend_from_slice(chunk);
    let mut lines: Vec<String> = Vec::new();
    let mut start = 0usize;
    for i in 0..buf.len() {
        if buf[i] == b'\n' {
            lines.push(String::from_utf8_lossy(&buf[start..i]).to_string());
            start = i + 1;
        }
    }
    let carry = buf[start..].to_vec();
    (lines, carry)
}

/// 流式累积状态：文本 / 思考 / tool call / 元信息
struct StreamState {
    /// 累积的助手可见文本（正文 delta 依次拼接）。
    assistant_text: String,
    /// 累积的思考/推理文本（reasoning delta 依次拼接）。
    thinking_text: String,
    /// 归一化后的停止原因（stop/length/toolUse/error）。
    stop_reason: Option<String>,
    /// 端点原始 finish_reason，仅供调试保留，当前未被读取。
    _raw_stop_reason: Option<String>,
    /// finish_reason 映射出的错误说明；正常结束时为 None。
    error_message: Option<String>,
    /// 流式累计的用量；端点未返回 usage 时为 None。
    usage: Option<Usage>,
    /// 响应 id（首个 chunk 的 id），用于追踪一次请求。
    response_id: Option<String>,
    /// 端点实际返回的模型名；与请求相同或未给出时为 None。
    response_model: Option<String>,
    tool_calls: Vec<StreamingToolCall>, // 按 index 排序
    /// tool call id → `tool_calls` 下标，用于按 id 归位后续 delta。
    tool_calls_by_id: HashMap<String, usize>,
    // 流式事件状态
    /// 文本块在内容块序列中的下标；未产生文本时为 None。
    text_index: Option<usize>,
    /// 思考块在内容块序列中的下标；未产生思考时为 None。
    thinking_index: Option<usize>,
    /// `tool_calls` 下标 → 内容块下标，供流式事件定位。
    toolcall_indices: HashMap<usize, usize>,
    /// 下一个可用内容块下标，按内容首次出现的顺序递增分配。
    next_content_index: usize,
}

impl StreamState {
    /// 建一个空的流式状态（无文本、无思考、无工具调用、无 usage）。
    fn new() -> Self {
        Self {
            assistant_text: String::new(),
            thinking_text: String::new(),
            stop_reason: None,
            _raw_stop_reason: None,
            error_message: None,
            usage: None,
            response_id: None,
            response_model: None,
            tool_calls: Vec::new(),
            tool_calls_by_id: HashMap::new(),
            text_index: None,
            thinking_index: None,
            toolcall_indices: HashMap::new(),
            next_content_index: 0,
        }
    }

    /// 处理一个 SSE data 块（error / 元信息 / usage / choice delta）
    fn handle_chunk(
        &mut self,
        chunk_val: &Value,
        model: &ModelConfig,
        on_event: &mut Option<&mut PartialTranslator<'_>>,
    ) -> Result<()> {
        // error 字段（OpenAI 错误格式）
        if let Some(err) = chunk_val.get("error") {
            let message = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();
            return Err(Error::ProviderMessage(message));
        }

        if let Some(id) = chunk_val.get("id").and_then(|v| v.as_str())
            && self.response_id.is_none()
        {
            self.response_id = Some(id.to_string());
        }

        if let Some(m) = chunk_val.get("model").and_then(|v| v.as_str())
            && self.response_model.is_none()
            && m != model.model_id
        {
            self.response_model = Some(m.to_string());
        }
        if let Some(u) = chunk_val.get("usage") {
            self.usage = parse_usage(u);
        }
        let choice = chunk_val
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first());
        let Some(choice) = choice else { return Ok(()) };

        if let Some(u) = choice.get("usage")
            && self.usage.is_none()
        {
            self.usage = parse_usage(u);
        }

        if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str())
            && !fr.is_empty()
        {
            self._raw_stop_reason = Some(fr.to_string());
            let mapped = map_stop_reason(fr);
            self.stop_reason = Some(mapped.0.to_string());
            if let Some(em) = mapped.1 {
                self.error_message = Some(em.to_string());
            }
        }

        let delta = choice.get("delta");
        let Some(delta) = delta else { return Ok(()) };
        self.handle_delta(delta, on_event);
        Ok(())
    }

    /// 把 delta 分发给三类内容（文本 / 思考 / tool call）
    fn handle_delta(&mut self, delta: &Value, on_event: &mut Option<&mut PartialTranslator<'_>>) {
        self.handle_content_delta(delta, on_event);
        self.handle_reasoning_delta(delta, on_event);
        self.handle_tool_call_delta(delta, on_event);
    }

    /// 累积 `delta.content` 文本；首次出现时分配内容块下标并发出 TextStart。
    fn handle_content_delta(
        &mut self,
        delta: &Value,
        on_event: &mut Option<&mut PartialTranslator<'_>>,
    ) {
        let Some(content) = delta.get("content").and_then(|v| v.as_str()) else {
            return;
        };
        if content.is_empty() {
            return;
        }
        if self.text_index.is_none() {
            self.text_index = Some(self.next_content_index);
            self.next_content_index += 1;
            if let Some(f) = on_event.as_mut() {
                f.push(RawStreamEvent::TextStart {
                    content_index: self.text_index.unwrap(),
                });
            }
        }
        self.assistant_text.push_str(content);
        if let Some(f) = on_event.as_mut() {
            f.push(RawStreamEvent::TextDelta {
                content_index: self.text_index.unwrap(),
                delta: content.to_string(),
            });
        }
    }

    /// 累积 `delta` 里的思考字段（reasoning_content / reasoning / reasoning_text），
    /// 首次出现时分配内容块下标并发出 ThinkingStart。
    fn handle_reasoning_delta(
        &mut self,
        delta: &Value,
        on_event: &mut Option<&mut PartialTranslator<'_>>,
    ) {
        // reasoning 字段
        let mut found_reasoning: Option<&str> = None;
        for field in ["reasoning_content", "reasoning", "reasoning_text"] {
            if let Some(v) = delta.get(field).and_then(|v| v.as_str())
                && !v.is_empty()
            {
                found_reasoning = Some(field);
                break;
            }
        }
        let Some(field) = found_reasoning else { return };
        let d = delta.get(field).and_then(|v| v.as_str()).unwrap_or("");
        if d.is_empty() {
            return;
        }
        if self.thinking_index.is_none() {
            self.thinking_index = Some(self.next_content_index);
            self.next_content_index += 1;
            if let Some(f) = on_event.as_mut() {
                f.push(RawStreamEvent::ThinkingStart {
                    content_index: self.thinking_index.unwrap(),
                });
            }
        }
        self.thinking_text.push_str(d);
        if let Some(f) = on_event.as_mut() {
            f.push(RawStreamEvent::ThinkingDelta {
                content_index: self.thinking_index.unwrap(),
                delta: d.to_string(),
            });
        }
    }

    /// 按 delta 里的 index（缺失时回退用 id）归位并累积 tool call 的 id / 函数名 / 参数片段；
    /// 首次拿到参数时发出 ToolCallStart，后续片段发 ToolCallDelta。
    fn handle_tool_call_delta(
        &mut self,
        delta: &Value,
        on_event: &mut Option<&mut PartialTranslator<'_>>,
    ) {
        let Some(tool_calls_delta) = delta.get("tool_calls").and_then(|v| v.as_array()) else {
            return;
        };
        for tc in tool_calls_delta {
            let index = tc.get("index").and_then(|v| v.as_u64()).map(|i| i as usize);
            let id = tc.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
            let function = tc.get("function");
            let custom = tc.get("custom");
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .or_else(|| custom.and_then(|c| c.get("name")).and_then(|v| v.as_str()))
                .map(|s| s.to_string());
            let args = function
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            // grammar 约束工具的裸源码：`custom.input` 是**累积值**
            let custom_input = custom
                .and_then(|c| c.get("input"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            // 找到或创建 block
            let slot: Option<usize> = if let Some(i) = index {
                Some(i)
            } else {
                id.as_ref()
                    .and_then(|id| self.tool_calls_by_id.get(id))
                    .copied()
            };

            let position = match slot {
                Some(i) => {
                    while self.tool_calls.len() <= i {
                        self.tool_calls.push(StreamingToolCall {
                            index: self.tool_calls.len(),
                            id: None,
                            name: None,
                            arguments: String::new(),
                            grammar_input: None,
                            grammar_source: String::new(),
                            grammar_started: false,
                            grammar_closed: false,
                        });
                    }
                    i
                }
                None => {
                    let pos = self.tool_calls.len();
                    self.tool_calls.push(StreamingToolCall {
                        index: pos,
                        id: None,
                        name: None,
                        arguments: String::new(),
                        grammar_input: None,
                        grammar_source: String::new(),
                        grammar_started: false,
                        grammar_closed: false,
                    });
                    pos
                }
            };

            if let Some(id) = id
                && self.tool_calls[position].id.is_none()
            {
                self.tool_calls[position].id = Some(id.clone());
                self.tool_calls_by_id.insert(id, position);
            }
            if let Some(name) = name
                && self.tool_calls[position].name.is_none()
            {
                // grammar 工具（`custom` 形状）：记录源码属性名（推不出时回退 `input`）
                if custom.is_some() && function.is_none() {
                    self.tool_calls[position].grammar_input = Some(
                        grammar_input_property_for_tool(&name)
                            .unwrap_or_else(|| "input".to_string()),
                    );
                }
                self.tool_calls[position].name = Some(name);
            }
            if let Some(args) = args {
                // 首次出现该 tool call 时发 toolcall_start
                if !self.toolcall_indices.contains_key(&position) {
                    let ci = self.next_content_index;
                    self.next_content_index += 1;
                    self.toolcall_indices.insert(position, ci);
                    if let Some(f) = on_event.as_mut() {
                        f.push(RawStreamEvent::ToolCallStart { content_index: ci });
                    }
                }
                self.tool_calls[position].arguments.push_str(&args);
                if let Some(f) = on_event.as_mut() {
                    f.push(RawStreamEvent::ToolCallDelta {
                        content_index: self.toolcall_indices[&position],
                        delta: args,
                    });
                }
            }
            // grammar 调用：把裸源码包成 `{"<prop>":"…"}` 的 JSON 参数，后续走同一条路
            if let Some(input) = custom_input {
                if !self.toolcall_indices.contains_key(&position) {
                    let ci = self.next_content_index;
                    self.next_content_index += 1;
                    self.toolcall_indices.insert(position, ci);
                    if let Some(f) = on_event.as_mut() {
                        f.push(RawStreamEvent::ToolCallStart { content_index: ci });
                    }
                }

                let property = self.tool_calls[position]
                    .grammar_input
                    .clone()
                    .unwrap_or_else(|| "input".to_string());

                if !self.tool_calls[position].grammar_started {
                    self.tool_calls[position].grammar_input = Some(property.clone());
                    self.tool_calls[position].grammar_started = true;
                    self.tool_calls[position].arguments.push_str(&format!(
                        "{{\"{}\":\"",
                        core::provider::escape_json_text(&property)
                    ));
                }

                let previous = std::mem::take(&mut self.tool_calls[position].grammar_source);

                // 累积值：只拼新增部分（模型不会回退已发过的前缀）
                let delta_text = input
                    .strip_prefix(previous.as_str())
                    .unwrap_or(input.as_str())
                    .to_string();

                if !delta_text.is_empty() {
                    let escaped = core::provider::escape_json_text(&delta_text);
                    self.tool_calls[position].arguments.push_str(&escaped);
                    self.tool_calls[position].grammar_source = input;
                    if let Some(f) = on_event.as_mut() {
                        f.push(RawStreamEvent::ToolCallDelta {
                            content_index: self.toolcall_indices[&position],
                            delta: escaped,
                        });
                    }
                }
            }
        }
    }

    /// 流结束后组装 assistant 消息与结果
    fn finish(
        mut self,
        model: &ModelConfig,
        on_event: &mut Option<&mut PartialTranslator<'_>>,
    ) -> Result<StreamResult> {
        // grammar 调用的 JSON 包装还可能没闭合：`{"<prop>":"…` → `…"}`
        for tc in self.tool_calls.iter_mut() {
            if tc.grammar_input.is_some() && !tc.grammar_closed {
                if tc.grammar_started {
                    tc.arguments.push_str("\"}");
                } else {
                    tc.arguments = "{}".to_string();
                }
                tc.grammar_closed = true;
            }
        }

        let mut content_blocks: Vec<ContentBlock> = Vec::new();
        if !self.thinking_text.trim().is_empty() {
            content_blocks.push(ContentBlock::Thinking {
                thinking: self.thinking_text.clone(),
                thinking_signature: Some("reasoning_content".to_string()),
                redacted: None,
            });
            if let Some(f) = on_event.as_mut()
                && let Some(ci) = self.thinking_index
            {
                f.push(RawStreamEvent::ThinkingEnd {
                    content_index: ci,
                    content: self.thinking_text.clone(),
                });
            }
        }
        if !self.assistant_text.is_empty() {
            content_blocks.push(ContentBlock::Text {
                text: self.assistant_text.clone(),
                text_signature: None,
            });
            if let Some(f) = on_event.as_mut()
                && let Some(ci) = self.text_index
            {
                f.push(RawStreamEvent::TextEnd {
                    content_index: ci,
                    content: self.assistant_text,
                });
            }
        }
        for tc in &self.tool_calls {
            let id = tc
                .id
                .clone()
                .unwrap_or_else(|| format!("call_{}", tc.index));
            let name = tc.name.clone().unwrap_or_default();
            let arguments: Value = parse_streaming_json(&tc.arguments);
            content_blocks.push(ContentBlock::ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
                thought_signature: None,
                namespace: None,
            });
            if let Some(f) = on_event.as_mut()
                && let Some(ci) = self.toolcall_indices.get(&tc.index)
            {
                f.push(RawStreamEvent::ToolCallEnd {
                    content_index: *ci,
                    id,
                    name,
                    arguments,
                });
            }
        }

        // 流结束时无 finish_reason 帧，按是否存在工具调用推断 stop/toolUse
        let stop_reason = self.stop_reason.or_else(|| {
            if model.supports_finish_reason {
                None
            } else if !self.tool_calls.is_empty() {
                Some("toolUse".to_string())
            } else {
                Some("stop".to_string())
            }
        });

        let mut usage = self.usage;
        if let Some(u) = usage.as_mut() {
            compute_cost(u, model.cost.as_ref());
        }

        let message = AgentMessage {
            role: "assistant".to_string(),
            thinking_level: None,
            content: content_blocks,
            tool_call_id: None,
            tool_name: None,
            is_error: false,
            stop_reason,
            error_message: self.error_message.clone(),
            model: self.response_model.or(Some(model.model_id.clone())),
            provider: Some(model.provider.clone()),
            api: Some("openai-completions".to_string()),
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
            error_message: self.error_message,
        })
    }
}

/// 流式响应里按 index 归位、逐块累积参数的 tool call 缓冲。
#[derive(Debug, Clone)]
struct StreamingToolCall {
    /// 该 tool call 在本次响应中的序号（delta 里的 index）。
    index: usize,
    /// 调用 id；首个 delta 给出前为 None，缺失时回退成 `call_{index}`。
    id: Option<String>,
    /// 函数名；端点未给出时为 None。
    name: Option<String>,
    /// 逐块拼接的 JSON 参数字符串。
    arguments: String,
    /// grammar 约束调用的源码属性名；None = 普通 function 调用。
    grammar_input: Option<String>,
    /// grammar 调用已接收的裸源码（累积值，用来算增量）。
    grammar_source: String,
    /// grammar 调用的 JSON 包装前缀是否已写入（`{"<prop>":"`）。
    grammar_started: bool,
    /// grammar 调用的 JSON 包装是否已闭合（流末补 `"}`）。
    grammar_closed: bool,
}

/// 把上游 `finish_reason` 映射成内部 stopReason，并给出可选的错误说明。
/// 未知值一律归为 `error`，说明里带上原始 reason 便于排查。
fn map_stop_reason(reason: &str) -> (&'static str, Option<String>) {
    match reason {
        "stop" | "end" => ("stop", None),
        "length" => ("length", None),
        "function_call" | "tool_calls" => ("toolUse", None),
        "content_filter" => (
            "error",
            Some("Provider finish_reason: content_filter".to_string()),
        ),
        "network_error" => (
            "error",
            Some("Provider finish_reason: network_error".to_string()),
        ),
        other => ("error", Some(format!("Provider finish_reason: {}", other))),
    }
}

/// cached_tokens 是 cache-read，cache_write_tokens 单独映射；
/// input 需要扣掉 cacheRead 与 cacheWrite。
fn parse_usage(u: &Value) -> Option<Usage> {
    let prompt_tokens = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let cache_read = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .or_else(|| u.get("prompt_cache_hit_tokens").and_then(|v| v.as_u64()))
        .unwrap_or(0) as u32;
    let cache_write = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cache_write_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let output = u
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let reasoning = u
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let input = prompt_tokens
        .saturating_sub(cache_read)
        .saturating_sub(cache_write);
    let total_tokens = input + output + cache_read + cache_write;
    Some(Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: (reasoning > 0).then_some(reasoning),
        total_tokens,
        cost: Cost::default(),
    })
}

/// 本次请求应使用的 chat/completions 端点。
/// github-copilot 从 API key 的 `proxy-ep=` 段推导 API base（个人版/企业版实际端点由 token 决定），
/// 其余情况直接用 [`ModelConfig::endpoint`]。
fn model_endpoint(model: &ModelConfig) -> String {
    if model.provider != "github-copilot" {
        return model.endpoint();
    }

    // GitHub Copilot 专用：从 Copilot token 的 proxy-ep 推导 API base
    // 覆盖目录 base_url（个人版 / 企业版实际端点由 token 决定）。
    let base = model
        .api_key
        .split(';')
        .find_map(|seg| seg.strip_prefix("proxy-ep="))
        .map(|ep| {
            let ep = ep.trim();
            let host = if let Some(rest) = ep.strip_prefix("proxy.") {
                format!("api.{}", rest)
            } else {
                ep.to_string()
            };
            format!("https://{}", host)
        });

    match base {
        Some(b) => azure::with_api_version(
            model,
            format!("{}/chat/completions", b.trim_end_matches('/')),
        ),
        None => model.endpoint(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copilot_model(token: &str) -> ModelConfig {
        ModelConfig {
            model_type: Default::default(),
            image_resize: crate::utils::image::ImageResizeLimits::default(),
            allowed_fallback_models: Vec::new(),
            provider: "github-copilot".into(),
            model_id: "claude-fable-5".into(),
            base_url: "https://api.individual.githubcopilot.com".into(),
            api_key: token.into(),
            api: "openai-completions".into(),
            output: Vec::new(),
            input: vec!["text".into(), "image".into()],
            reasoning: false,
            max_tokens: None,
            temperature: None,
            context_window: 200000,
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
            supports_strict_mode: false,
            send_session_affinity_headers: None,
            session_affinity_format: None,
            max_tokens_field: "max_tokens".into(),
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
    fn convert_messages_custom_role_becomes_user() {
        // 对齐 pi convertToLlm：custom 角色转 user（扩展注入消息进上下文）
        let m = model(false, None);
        let mut custom = AgentMessage::user_text("injected");
        custom.role = "custom".to_string();
        let msgs = convert_messages(&[custom], "", &m);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["text"], "injected");
    }

    #[test]
    fn trailing_orphan_tool_call_gets_synthetic_result() {
        // 对齐 pi transformMessages / responses 转换器：列表尾部未配对的 tool_calls
        // 补 synthetic tool result，否则上游报
        // "assistant message with 'tool_calls' must be followed by tool messages..."。
        let m = model(false, None);
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.content = vec![ContentBlock::ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            arguments: json!({}),
            thought_signature: None,
            namespace: None,
        }];
        let out = convert_messages(&[a], "", &m);
        assert_eq!(out[0]["role"], "assistant");
        assert_eq!(out[0]["tool_calls"][0]["id"], "call_1");
        // 尾部孤儿 → synthetic tool 消息
        assert_eq!(out[1]["role"], "tool");
        assert_eq!(out[1]["tool_call_id"], "call_1");
        assert_eq!(out[1]["content"], "No result provided");
    }

    #[test]
    fn trailing_partial_tool_batch_backfills_missing_results() {
        // 中止/崩溃恢复的历史：assistant(A,B) + tool(A)，以 tool 消息结尾。
        // 缺 B 的结果必须补 synthetic，否则上游报 "insufficient tool messages following tool_calls message"。
        let m = model(false, None);
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.content = vec![
            ContentBlock::ToolCall {
                id: "call_A".into(),
                name: "read".into(),
                arguments: json!({}),
                thought_signature: None,
                namespace: None,
            },
            ContentBlock::ToolCall {
                id: "call_B".into(),
                name: "bash".into(),
                arguments: json!({}),
                thought_signature: None,
                namespace: None,
            },
        ];
        let mut tr = AgentMessage::user_text("");
        tr.role = "toolResult".into();
        tr.tool_call_id = Some("call_A".into());
        tr.content = vec![ContentBlock::Text {
            text: "result A".into(),
            text_signature: None,
        }];
        let out = convert_messages(&[a, tr], "", &m);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["role"], "assistant");
        assert_eq!(out[1]["role"], "tool");
        assert_eq!(out[1]["tool_call_id"], "call_A");
        assert_eq!(out[1]["content"], "result A");
        assert_eq!(out[2]["role"], "tool");
        assert_eq!(out[2]["tool_call_id"], "call_B");
        assert_eq!(out[2]["content"], "No result provided");
    }

    #[test]
    fn orphan_tool_result_is_dropped() {
        // 历史中带 tool_calls 的 assistant 被扩展 transform_context 删除（或压缩保留尾部
        // 从批次中间截断）后，会留下孤儿 tool 结果。直接发送会触发上游 400：
        // "Messages with role 'tool' must be a response to a preceding message with 'tool_calls'"。
        // 转换层必须丢弃它。
        let m = model(false, None);
        let mut user = AgentMessage::user_text("go");
        user.role = "user".into();
        let mut orphan = AgentMessage::user_text("");
        orphan.role = "toolResult".into();
        orphan.tool_call_id = Some("call_missing".into());
        orphan.content = vec![ContentBlock::Text {
            text: "orphan result".into(),
            text_signature: None,
        }];
        let out = convert_messages(&[user, orphan], "", &m);
        assert_eq!(out.len(), 1, "孤儿 tool 结果必须被丢弃: {out:?}");
        assert_eq!(out[0]["role"], "user");
    }

    #[test]
    fn answered_tool_result_still_emitted_once() {
        // pending 在收到结果时即移除：重复结果不会重复发送（去重不依赖外部集合）。
        let m = model(false, None);
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.content = vec![ContentBlock::ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            arguments: json!({}),
            thought_signature: None,
            namespace: None,
        }];
        let mk = |text: &str| {
            let mut tr = AgentMessage::user_text("");
            tr.role = "toolResult".into();
            tr.tool_call_id = Some("call_1".into());
            tr.content = vec![ContentBlock::Text {
                text: text.into(),
                text_signature: None,
            }];
            tr
        };
        let out = convert_messages(&[a, mk("first"), mk("second")], "", &m);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1]["role"], "tool");
        assert_eq!(out[1]["content"], "first");
    }

    /// assistant 消息（带若干 tool_calls）
    fn assistant_with_calls(ids: &[&str]) -> AgentMessage {
        let mut a = AgentMessage::user_text("");
        a.role = "assistant".into();
        a.content = ids
            .iter()
            .map(|id| ContentBlock::ToolCall {
                id: (*id).into(),
                name: "read".into(),
                arguments: json!({}),
                thought_signature: None,
                namespace: None,
            })
            .collect();
        a
    }

    /// toolResult 消息（文本 + 可选图片）
    fn tool_result_with_image(id: &str, text: &str, with_image: bool) -> AgentMessage {
        let mut tr = AgentMessage::user_text("");
        tr.role = "toolResult".into();
        tr.tool_call_id = Some(id.into());
        tr.content = vec![ContentBlock::Text {
            text: text.into(),
            text_signature: None,
        }];
        if with_image {
            tr.content.push(ContentBlock::Image {
                data: "AA==".into(),
                mime_type: "image/png".into(),
            });
        }
        tr
    }

    /// 回归：一批 tool_calls 里只有部分结果带图片时，图片 user 消息必须排在**全部**
    /// tool 消息之后。插在中间会被上游拒绝：
    /// `provider returned 400 Bad Request: {"model":"deepseek-v4.1-flash"}`，
    /// 而且这个顺序一旦落库，整个会话之后每次请求都会 400。
    #[test]
    fn tool_result_images_wait_until_batch_end() {
        let m = model(true, None);
        let out = convert_messages(
            &[
                assistant_with_calls(&["call_read", "call_bash"]),
                tool_result_with_image("call_read", "Read image file [image/png]", true),
                tool_result_with_image("call_bash", "total 0", false),
                AgentMessage::user_text("继续"),
            ],
            "",
            &m,
        );
        let roles: Vec<&str> = out.iter().map(|v| v["role"].as_str().unwrap()).collect();
        assert_eq!(
            roles,
            vec!["assistant", "tool", "tool", "user", "user"],
            "图片消息不得插在两个 tool 消息之间: {out:?}"
        );
        assert_eq!(out[1]["tool_call_id"], "call_read");
        assert_eq!(out[2]["tool_call_id"], "call_bash");
        assert_eq!(
            out[3]["content"][0]["text"],
            "Attached image(s) from tool result:"
        );
        assert_eq!(out[3]["content"][1]["type"], "image_url");
        assert_eq!(out[4]["content"][0]["text"], "继续");
    }

    /// 批次被列表末尾截断时同样要等全部 tool 消息发完。
    #[test]
    fn tool_result_image_at_list_end_follows_all_tool_messages() {
        let m = model(true, None);
        let out = convert_messages(
            &[
                assistant_with_calls(&["call_read", "call_bash"]),
                tool_result_with_image("call_read", "Read image file [image/png]", true),
                tool_result_with_image("call_bash", "total 0", false),
            ],
            "",
            &m,
        );
        let roles: Vec<&str> = out.iter().map(|v| v["role"].as_str().unwrap()).collect();
        assert_eq!(roles, vec!["assistant", "tool", "tool", "user"], "{out:?}");
    }

    /// 只有一个 tool 结果（最常见情况）：tool 后紧跟图片，顺序与旧行为一致。
    #[test]
    fn single_tool_result_image_follows_tool_message() {
        let m = model(true, None);
        let out = convert_messages(
            &[
                assistant_with_calls(&["call_read"]),
                tool_result_with_image("call_read", "Read image file [image/png]", true),
            ],
            "",
            &m,
        );
        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!(out[0]["role"], "assistant");
        assert_eq!(out[1]["role"], "tool");
        assert_eq!(out[1]["content"], "Read image file [image/png]");
        assert_eq!(out[2]["role"], "user");
        assert_eq!(out[2]["content"][1]["type"], "image_url");
        assert_eq!(
            out[2]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AA=="
        );
    }

    /// 一批里多个工具结果都带图片 → 合并成一条 user 消息。
    #[test]
    fn multiple_tool_result_images_merge_into_one_user_message() {
        let m = model(true, None);
        let out = convert_messages(
            &[
                assistant_with_calls(&["call_a", "call_b"]),
                tool_result_with_image("call_a", "a", true),
                tool_result_with_image("call_b", "b", true),
            ],
            "",
            &m,
        );
        let roles: Vec<&str> = out.iter().map(|v| v["role"].as_str().unwrap()).collect();
        assert_eq!(roles, vec!["assistant", "tool", "tool", "user"], "{out:?}");
        let parts = out[3]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 3, "标题 + 两张图: {out:?}");
    }

    /// requiresAssistantAfterToolResult：空 assistant 也只在批次末尾补一次。
    #[test]
    fn empty_assistant_appended_once_per_tool_batch() {
        let mut m = model(true, None);
        m.requires_assistant_after_tool_result = true;
        let out = convert_messages(
            &[
                assistant_with_calls(&["call_read", "call_bash"]),
                tool_result_with_image("call_read", "Read image file [image/png]", true),
                tool_result_with_image("call_bash", "total 0", false),
            ],
            "",
            &m,
        );
        let roles: Vec<&str> = out.iter().map(|v| v["role"].as_str().unwrap()).collect();
        assert_eq!(
            roles,
            vec!["assistant", "tool", "tool", "user", "assistant"],
            "{out:?}"
        );
        assert!(out[4]["content"].as_array().unwrap().is_empty());
    }

    #[test]
    fn tool_choice_serialized_when_set() {
        // 对齐 pi SimpleStreamOptions.toolChoice：\"auto\" | \"none\"
        let m = model(false, None);
        let mut request = build_chat_request(&m, &[], &[], None, None);
        let body = serialize_request_body(&request, &m, None).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert!(
            v.get("tool_choice").is_none(),
            "未设置时不含 tool_choice: {body}"
        );
        request.tool_choice = Some("none".to_string());
        let body2 = serialize_request_body(&request, &m, None).unwrap();
        let v2: Value = serde_json::from_str(&body2).unwrap();
        assert_eq!(v2["tool_choice"], "none");
    }

    #[test]
    fn model_endpoint_forwards_proxy_ep() {
        // 个人版 token：proxy-ep=proxy.individual... → api.individual...
        let m = copilot_model("tid=1;exp=9;proxy-ep=proxy.individual.githubcopilot.com;sku=1");
        assert_eq!(
            model_endpoint(&m),
            "https://api.individual.githubcopilot.com/chat/completions"
        );
        // 无 proxy-ep → 回退目录 base_url
        let m2 = copilot_model("tid=1");
        assert_eq!(
            model_endpoint(&m2),
            "https://api.individual.githubcopilot.com/chat/completions"
        );
        // 非 copilot provider 不受影响
        let m3 = model(false, None);
        assert_eq!(
            model_endpoint(&m3),
            "https://example.com/v1/chat/completions"
        );
    }

    #[test]
    fn provider_extra_headers_follows_provider() {
        // copilot provider：模型条目定义有 IDE 网关头
        let m = copilot_model("x");
        let h = crate::core::provider::provider_extra_headers(&m);
        assert_eq!(
            h.get("User-Agent").and_then(|v| v.to_str().ok()),
            Some("GitHubCopilotChat/0.35.0")
        );
        assert!(h.get("Copilot-Integration-Id").is_some());
        // 未定义 headers 的 provider：空
        let m2 = model(false, None);
        let h2 = crate::core::provider::provider_extra_headers(&m2);
        assert!(h2.is_empty());
    }

    fn model(supports_image: bool, cost: Option<Value>) -> ModelConfig {
        ModelConfig {
            model_type: Default::default(),
            image_resize: crate::utils::image::ImageResizeLimits::default(),
            allowed_fallback_models: Vec::new(),
            provider: "test".into(),
            model_id: "m".into(),
            base_url: "https://example.com/v1".into(),
            api_key: String::new(),
            api: "openai-completions".into(),
            output: Vec::new(),
            input: if supports_image {
                vec!["text".into(), "image".into()]
            } else {
                vec!["text".into()]
            },
            reasoning: false,
            max_tokens: None,
            temperature: None,
            context_window: 1000,
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
            cost,
            session_id: None,
            max_retry_delay_ms: None,
            sampling_params: None,
            sampling_params_by_thinking_level: None,
            provider_routing: None,
            thinking_budgets: None,
        }
    }
    #[test]
    fn non_vision_model_replaces_images() {
        let m = model(false, None);
        let msg = AgentMessage {
            role: "user".into(),
            thinking_level: None,
            content: vec![
                ContentBlock::Text {
                    text: "look".into(),
                    text_signature: None,
                },
                ContentBlock::Image {
                    data: "AA==".into(),
                    mime_type: "image/png".into(),
                },
                ContentBlock::Image {
                    data: "AA==".into(),
                    mime_type: "image/png".into(),
                },
            ],
            ..AgentMessage::user_text("")
        };
        let out = convert_messages(&[msg], "", &m);
        let content = out[0].get("content").unwrap();
        let parts = content.as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "look");
        assert_eq!(
            parts[1]["text"],
            "(image omitted: model does not support images)"
        );
    }

    #[test]
    fn image_only_user_message_omits_empty_text_part() {
        // 回归 #9797：带空 text block 的纯图片 user 消息不得带空 text part。
        let m = model(true, None);
        let msg = AgentMessage {
            role: "user".into(),
            thinking_level: None,
            content: vec![
                ContentBlock::Text {
                    text: String::new(),
                    text_signature: None,
                },
                ContentBlock::Image {
                    data: "AA==".into(),
                    mime_type: "image/png".into(),
                },
            ],
            ..AgentMessage::user_text("")
        };
        let out = convert_messages(&[msg], "", &m);
        let parts = out[0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1, "空 text part 必须被省略: {out:?}");
        assert_eq!(parts[0]["type"], "image_url");
    }

    #[test]
    fn image_block_serializes_pi_compatible() {
        let msg = AgentMessage {
            role: "user".into(),
            thinking_level: None,
            content: vec![ContentBlock::Image {
                data: "AA==".into(),
                mime_type: "image/png".into(),
            }],
            ..AgentMessage::user_text("")
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["content"][0]["mimeType"], "image/png");
    }

    #[test]
    fn summary_roles_become_user_context() {
        let m = model(false, None);
        let mut c = AgentMessage::user_text("sum");
        c.role = "compactionSummary".into();
        let out = convert_messages(&[c], "", &m);
        let text = out[0]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("The conversation history before this point"));
        assert!(text.contains("sum"));
    }

    #[test]
    fn vision_model_emits_image_urls() {
        let m = model(true, None);
        let msg = AgentMessage {
            role: "user".into(),
            thinking_level: None,
            content: vec![ContentBlock::Image {
                data: "AA==".into(),
                mime_type: "image/png".into(),
            }],
            ..AgentMessage::user_text("")
        };
        let out = convert_messages(&[msg], "", &m);
        let part = &out[0]["content"][0];
        assert_eq!(part["type"], "image_url");
        assert_eq!(part["image_url"]["url"], "data:image/png;base64,AA==");
    }

    #[test]
    fn serialize_merges_sampling_params_and_routing() {
        let mut m = copilot_model("x");
        m.sampling_params = Some(json!({ "top_p": 0.9, "min_p": 0.05 }));
        m.provider_routing = Some(json!({ "order": ["openai"], "allow_fallbacks": false }));
        let req = ChatRequest {
            model: m.model_id.clone(),
            messages: vec![json!({ "role": "user", "content": "hi" })],
            stream: true,
            max_tokens: None,
            stream_options: None,
            tools: None,
            thinking: None,
            reasoning_effort: None,
            enable_thinking: None,
            reasoning: None,
            chat_template_kwargs: None,
            temperature: Some(0.7),
            tool_choice: None,
        };
        let body = serialize_request_body(&req, &m, None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        // samplingParams 全键透传（不覆盖显式 temperature）
        assert_eq!(v["top_p"], 0.9);
        assert_eq!(v["min_p"], 0.05);
        assert_eq!(v["temperature"], 0.7);
        // routing → provider 字段（对齐 pi openRouterRouting）
        assert_eq!(v["provider"]["order"][0], "openai");
        assert_eq!(v["provider"]["allow_fallbacks"], false);
    }

    /// 级别覆盖项按 pi 语义后写覆盖（含 temperature），目录 `samplingParams` 只补空键。
    #[test]
    fn level_sampling_params_override_named_fields() {
        let mut m = copilot_model("x");
        m.sampling_params = Some(json!({ "temperature": 1.0, "top_p": 0.95 }));
        let req = ChatRequest {
            model: m.model_id.clone(),
            messages: vec![json!({ "role": "user", "content": "hi" })],
            stream: true,
            max_tokens: None,
            stream_options: None,
            tools: None,
            thinking: None,
            reasoning_effort: None,
            enable_thinking: None,
            reasoning: None,
            chat_template_kwargs: None,
            temperature: Some(1.0),
            tool_choice: None,
        };
        let level = json!({ "temperature": 0.6, "top_k": 64 });
        let body = serialize_request_body(&req, &m, Some(&level)).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["temperature"], 0.6, "级别覆盖应压过显式 temperature");
        assert_eq!(v["top_k"], 64);
        assert_eq!(v["top_p"], 0.95, "目录 samplingParams 仍按补空键合并");

        // 没有级别覆盖项时行为与旧版一致
        let body = serialize_request_body(&req, &m, None).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["temperature"], 1.0);
        assert!(v.get("top_k").is_none());
    }

    #[test]
    fn usage_in_streaming_omits_include_usage_when_unsupported() {
        let mut m = copilot_model("x");
        m.supports_usage_in_streaming = false;
        let req = build_chat_request(&m, &[], &[], None, Some(8));
        assert!(
            req.stream_options.is_none(),
            "supportsUsageInStreaming=false 不发 include_usage"
        );
        let m2 = copilot_model("x");
        let req2 = build_chat_request(&m2, &[], &[], None, Some(8));
        assert!(
            req2.stream_options.is_some(),
            "supportsUsageInStreaming=true 默认发 include_usage"
        );
    }
    #[test]
    fn thinking_format_variants_serialize() {
        let with_map = |mut m: ModelConfig, pairs: &[(&str, Option<&str>)]| {
            m.thinking_level_map = Some(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.map(|s| s.to_string())))
                    .collect(),
            );
            m
        };

        // qwen：enable_thinking 布尔；supportsReasoningEffort 时附 reasoning_effort
        let mut m = copilot_model("x");
        m.reasoning = true;
        m.thinking_format = "qwen".into();
        m.supports_reasoning_effort = true;
        let mut req = build_chat_request(&m, &[], &[], None, None);
        apply_thinking_config(&mut req, &m, Some("high"));
        assert_eq!(req.enable_thinking, Some(true));
        assert_eq!(req.reasoning_effort.as_deref(), Some("high"));
        let body = serialize_request_body(&req, &m, None).unwrap();
        assert!(body.contains("\"enable_thinking\":true"), "{body}");

        // together：开启发 reasoning.enabled=true，关闭发 false（无 off 映射时不发 reasoning_effort）
        let mut m2 = copilot_model("x");
        m2.reasoning = true;
        m2.thinking_format = "together".into();
        let mut req2 = build_chat_request(&m2, &[], &[], None, None);
        apply_thinking_config(&mut req2, &m2, Some("medium"));
        assert_eq!(req2.reasoning, Some(json!({ "enabled": true })));
        let mut req2_off = build_chat_request(&m2, &[], &[], None, None);
        apply_thinking_config(&mut req2_off, &m2, None);
        assert_eq!(req2_off.reasoning, Some(json!({ "enabled": false })));

        // openrouter：关闭回退 effort="none"
        let mut m3 = copilot_model("x");
        m3.reasoning = true;
        m3.thinking_format = "openrouter".into();
        let mut req3 = build_chat_request(&m3, &[], &[], None, None);
        apply_thinking_config(&mut req3, &m3, None);
        assert_eq!(req3.reasoning, Some(json!({ "effort": "none" })));

        // deepseek：无 off 映射 → 关闭发 thinking.disabled
        let mut m4 = copilot_model("x");
        m4.reasoning = true;
        m4.thinking_format = "deepseek".into();
        let mut req4 = build_chat_request(&m4, &[], &[], None, None);
        apply_thinking_config(&mut req4, &m4, None);
        assert_eq!(req4.thinking, Some(json!({ "type": "disabled" })));

        // deepseek：off 显式为 null → 保持 API 默认（不发任何思考字段）
        let m5 = with_map(
            {
                let mut m = copilot_model("x");
                m.reasoning = true;
                m.thinking_format = "deepseek".into();
                m
            },
            &[("off", None), ("high", Some("high"))],
        );
        let mut req5 = build_chat_request(&m5, &[], &[], None, None);
        apply_thinking_config(&mut req5, &m5, None);
        assert_eq!(req5.thinking, None);
        assert_eq!(req5.reasoning_effort, None);

        // zai：关闭发 thinking.disabled
        let mut m6 = copilot_model("x");
        m6.reasoning = true;
        m6.thinking_format = "zai".into();
        let mut req6 = build_chat_request(&m6, &[], &[], None, None);
        apply_thinking_config(&mut req6, &m6, None);
        assert_eq!(req6.thinking, Some(json!({ "type": "disabled" })));

        // 通用 openai 风格（无 thinkingFormat）：off 映射为字符串 → reasoning_effort
        // 这是 opencode-go/hy3、huggingface/tencent/Hy3 等模型 off 失效的修复点
        let m7 = with_map(
            {
                let mut m = copilot_model("x");
                m.reasoning = true;
                m.thinking_format = "openai".into();
                m.supports_reasoning_effort = true;
                m
            },
            &[("off", Some("none")), ("high", Some("high"))],
        );
        let mut req7 = build_chat_request(&m7, &[], &[], None, None);
        apply_thinking_config(&mut req7, &m7, None);
        assert_eq!(req7.reasoning_effort.as_deref(), Some("none"));
        let mut req7_on = build_chat_request(&m7, &[], &[], None, None);
        apply_thinking_config(&mut req7_on, &m7, Some("high"));
        assert_eq!(req7_on.reasoning_effort.as_deref(), Some("high"));

        // 通用分支：supportsReasoningEffort=false 时保持不动（如 github-copilot）
        let mut m8 = copilot_model("x");
        m8.reasoning = true;
        m8.thinking_format = "openai".into();
        m8.supports_reasoning_effort = false;
        let mut req8 = build_chat_request(&m8, &[], &[], None, None);
        apply_thinking_config(&mut req8, &m8, Some("high"));
        assert_eq!(req8.reasoning_effort, None);
    }
    #[test]
    fn strict_mode_follows_tool_constrained_sampling() {
        // 对齐 pi resolveJsonSchemaStrictSampling：仅声明 constrainedSampling 的工具 strict 化
        let defs = vec![
            (
                "read".to_string(),
                "Read".to_string(),
                json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            ),
            (
                "ls".to_string(),
                "Ls".to_string(),
                json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            ),
        ];
        // 不支持 strict 的 provider：不带 strict 字段、不 strict 化
        let plain = convert_tools_with_strict(&defs, false);
        assert!(plain[0]["function"].get("strict").is_none());
        assert!(plain[0]["function"]["parameters"].get("required").is_none());

        // 内置 read/bash/powershell/edit/write 默认声明 constrainedSampling → strict 化；
        // ls 未声明，保持原始 schema。
        let strict = convert_tools_with_strict(&defs, true);
        assert_eq!(strict[0]["function"]["strict"], true);
        assert_eq!(
            strict[0]["function"]["parameters"]["additionalProperties"],
            false
        );
        assert_eq!(strict[0]["function"]["parameters"]["required"][0], "path");
        assert_eq!(strict[1]["function"]["strict"], false);
        assert!(
            strict[1]["function"]["parameters"]
                .get("required")
                .is_none()
        );
    }

    #[test]
    fn finish_message_usage_cost_set_from_model_pricing_before_clone() {
        // 回归：cost 必须先结算再克隆进 message——否则底栏/session 恒为 $0
        let m = model(
            false,
            Some(json!({
                "input": 1.0,
                "output": 2.0,
                "cacheRead": 0.5,
                "cacheWrite": 0.25,
            })),
        );
        let mut st = StreamState::new();
        st.usage = Some(Usage {
            input: 1000,
            output: 2000,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: 3000,
            cost: Cost::default(),
        });
        let res = st.finish(&m, &mut None).unwrap();
        // input 1000*1 + output 2000*2 = 0.005（$/1M tokens）
        let u = res.message.usage.expect("usage attached to message");
        assert!(
            (u.cost.total - 0.005).abs() < 1e-12,
            "message cost.total={} expected 0.005",
            u.cost.total
        );
        assert!((res.usage.unwrap().cost.total - 0.005).abs() < 1e-12);
    }

    /// grammar 能力开启时，带语法约束的工具下发为 `custom` 工具；关闭时仍是 function。
    #[test]
    fn grammar_tool_conversion_emits_custom_tool() {
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

        let with_grammar = convert_tools_with_capabilities(&tools, false, true);
        assert_eq!(with_grammar[0]["type"], "custom");
        assert_eq!(with_grammar[0]["custom"]["name"], "codemode");
        assert_eq!(with_grammar[0]["custom"]["format"]["type"], "grammar");
        assert_eq!(
            with_grammar[0]["custom"]["format"]["grammar"]["syntax"],
            "lark"
        );
        assert_eq!(
            with_grammar[0]["custom"]["format"]["grammar"]["definition"],
            "start: SOURCE"
        );
        // grammar 工具不带 parameters（输入由语法约束）
        assert!(with_grammar[0]["custom"].get("parameters").is_none());

        let without = convert_tools_with_capabilities(&tools, false, false);
        assert_eq!(without[0]["type"], "function");
        assert_eq!(without[0]["function"]["name"], "codemode");
    }

    /// grammar 调用的裸源码（累积的 `custom.input`）被包成 `{"<prop>":"…"}` 的 JSON 参数。
    #[test]
    fn grammar_custom_input_is_wrapped_into_json_arguments() {
        let m = model(false, None);
        let mut st = StreamState::new();
        let mut on_event = None;

        st.handle_tool_call_delta(
            &json!({"tool_calls": [{
                "index": 0,
                "id": "call_1",
                "type": "custom",
                "custom": { "name": "codemode", "input": "" }
            }]}),
            &mut on_event,
        );
        // 累积值：后续 delta 给完整输入，只应拼新增部分
        st.handle_tool_call_delta(
            &json!({"tool_calls": [{
                "index": 0,
                "type": "custom",
                "custom": { "name": "codemode", "input": "return \"a\";" }
            }]}),
            &mut on_event,
        );
        st.handle_tool_call_delta(
            &json!({"tool_calls": [{
                "index": 0,
                "type": "custom",
                "custom": { "name": "codemode", "input": "return \"a\";\n// 换行" }
            }]}),
            &mut on_event,
        );

        let res = st.finish(&m, &mut None).unwrap();
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
        assert_eq!(arguments, json!({ "input": "return \"a\";\n// 换行" }));
    }

    /// 注册一个名为 `codemode` 的 grammar 工具（`code` 是唯一必填 string 参数），
    /// 供回放形状的测试使用；返回后由调用方 `unregister_extension` 清理。
    fn register_codemode_grammar_probe() {
        struct GrammarProbe;
        impl crate::core::extensions::Extension for GrammarProbe {
            fn name(&self) -> &str {
                "zz-codemode-replay-probe"
            }
            fn modes(&self) -> Vec<crate::core::extensions::ExtensionMode> {
                crate::core::extensions::ExtensionMode::ALL.to_vec()
            }
            fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
                let mut tool = crate::core::extensions::ExtensionTool::simple(
                    "codemode",
                    "run js",
                    json!({
                        "type": "object",
                        "properties": { "code": { "type": "string" } },
                        "required": ["code"]
                    }),
                    "run js",
                );
                tool.grammar_sampling = Some(crate::core::extensions::GrammarSampling {
                    openai_lark: Some("start: SOURCE".to_string()),
                    openai_regex: None,
                });
                vec![tool]
            }
        }
        crate::core::extensions::register_extension(GrammarProbe);
        assert_eq!(
            grammar_input_property_for_tool("codemode").as_deref(),
            Some("code"),
            "探针扩展未生效（模式/禁用状态异常）"
        );
    }

    /// 回放历史里的 grammar 工具调用时，`custom` 形状**只在模型声明支持 grammar 工具时**使用。
    ///
    /// 回归：曾经只看工具名，于是不支持该形状的模型（opencode-go 的 deepseek/mimo 等）收到
    /// `{"type":"custom",...}`，上游报
    /// `messages.N.tool_calls.0.type: Input should be 'function'`；这条历史一旦落库，
    /// 之后每次请求都 400，整个会话作废。
    #[test]
    fn grammar_tool_replay_follows_model_capability() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        register_codemode_grammar_probe();

        let mut msg = AgentMessage::user_text("");
        msg.role = "assistant".into();
        msg.stop_reason = Some("toolUse".into());
        msg.content = vec![ContentBlock::ToolCall {
            id: "call_1".into(),
            name: "codemode".into(),
            arguments: json!({ "code": "text(1)" }),
            thought_signature: None,
            namespace: None,
        }];

        // 不支持 grammar 的模型（不在目录里 → false）：必须是 function 形状
        let plain = convert_messages(std::slice::from_ref(&msg), "", &model(false, None));
        let call = &plain[0]["tool_calls"][0];
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "codemode");
        assert_eq!(call["function"]["arguments"], "{\"code\":\"text(1)\"}");

        // 目录里声明 supportsOpenAIGrammarTools 的模型：回放为 custom（裸源码）
        let mut grammar_model = model(false, None);
        grammar_model.provider = "opencode".into();
        grammar_model.model_id = "gpt-5".into();
        assert!(model_resolver::supports_openai_grammar_tools(
            &grammar_model.provider,
            &grammar_model.model_id
        ));
        let custom = convert_messages(&[msg], "", &grammar_model);
        let call = &custom[0]["tool_calls"][0];
        assert_eq!(call["type"], "custom");
        assert_eq!(call["custom"]["input"], "text(1)");

        crate::core::extensions::unregister_extension("zz-codemode-replay-probe");
    }
}
