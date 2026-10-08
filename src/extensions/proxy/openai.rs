//! OpenAI 兼容请求 → prux 内部表示的解析与校验。
//!
//! 参数策略（按语义分桶，见 `assets/docs/extensions.md` 的 proxy 章节）：
//! - **可映射**：`model` / `messages` / `stream` / `stream_options.include_usage` /
//!   `temperature` / `max_tokens`|`max_completion_tokens` / `reasoning_effort` /
//!   `top_p` / `tools` / `tool_choice`；
//! - **会改变输出语义但网关无法如实表达 → `400 unsupported_parameter`**：
//!   `stop`、`n>1`、`logprobs`、`top_logprobs`、`logit_bias`、`seed`、
//!   `response_format`（非 `text`）、`modalities`、`audio`、`prediction`、
//!   `frequency_penalty`/`presence_penalty`（非 0）、`web_search_options`、
//!   `reasoning`、`verbosity`、工具 `strict: true`；
//! - **纯元数据（不影响结果）→ 忽略**：`user`、`metadata`、`store`、`service_tier`、
//!   `parallel_tool_calls`，以及一切未识别字段。
//!
//! 即：**不静默降级的是语义，不是元数据**。

use super::error::GatewayError;
use crate::{
    cli::args::VALID_THINKING_LEVELS,
    core::provider::{AgentMessage, ContentBlock},
};
use serde_json::{Map, Value, json};

/// 内部工具三元组：`(name, description, parameters_schema)`
pub type ToolDefs = Vec<(String, String, Value)>;

/// 语义性参数黑名单：出现即报错（`response_format` 的 `{"type":"text"}` 例外）
const REJECTED_PARAMS: &[&str] = &[
    "logit_bias",
    "seed",
    "modalities",
    "audio",
    "prediction",
    "top_logprobs",
    "web_search_options",
    "reasoning",
    "verbosity",
];

/// 解析后的 chat 请求
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// 客户端请求的 model id（在配置的 provider 上解析）
    pub model: String,
    /// 内部消息（不含 system/developer，那些进 `system_prompt`）
    pub messages: Vec<AgentMessage>,
    /// system/developer 消息拼接结果
    pub system_prompt: String,
    /// `(name, description, schema)`
    pub tools: ToolDefs,
    /// 工具选择策略（`auto` / `none` / `required` / 指定函数名）；`None` = 交给上游默认
    pub tool_choice: Option<String>,
    /// 是否以 SSE 流式返回（`false` 时网关内部仍流式调用、聚合后整块返回）
    pub stream: bool,
    /// `stream_options.include_usage`：是否在流末尾附带 usage 帧
    pub include_usage: bool,
    /// 采样温度
    pub temperature: Option<f64>,
    /// 最大输出 token（`max_tokens` 或 `max_completion_tokens`）
    pub max_tokens: Option<u32>,
    /// 推理强度（映射到内部 thinking level）
    pub reasoning_effort: Option<String>,
    /// 经 `on_payload` 注入上游请求体（仅 openai 兼容协议生效）
    pub top_p: Option<f64>,
    /// 请求里含图片（网关据此校验目标模型是否支持图片）
    pub has_image: bool,
    /// 客户端自带的会话标识，来自入站头 `x-session-affinity` / `x-opencode-session`
    /// （非请求体字段，由 server 层填充）。`None` 时网关按会话稳定前缀自己派生一个。
    pub client_session_id: Option<String>,
}

/// 把 OpenAI 兼容 chat 请求体解析为内部 [`ChatRequest`]。
///
/// 逐字段校验并归一（system/developer 拼进 `system_prompt`，`reasoning_effort` 映射 thinking 级别）；
/// 请求体非对象、缺 `model`、字段类型/取值非法，或出现网关无法如实表达语义的参数时返回错误。
pub fn parse_chat_request(body: &Value) -> Result<ChatRequest, GatewayError> {
    let obj = body
        .as_object()
        .ok_or_else(|| GatewayError::invalid("request body must be a JSON object", None))?;

    let model = obj
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            GatewayError::invalid(
                "`model` is required: it selects the model id on the configured provider",
                Some("model"),
            )
        })?
        .to_string();

    let stream = match obj.get("stream") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        _ => {
            return Err(GatewayError::invalid(
                "`stream` must be a boolean",
                Some("stream"),
            ));
        }
    };

    let include_usage = parse_stream_options(obj)?;
    reject_unsupported_semantics(obj)?;

    let temperature = parse_temperature(obj)?;
    let max_tokens = parse_max_tokens(obj)?;
    let reasoning_effort = parse_reasoning_effort(obj)?;
    let top_p = parse_top_p(obj)?;

    let (system_prompt, messages, has_image) = parse_messages(obj)?;
    let (tools, tool_choice) = parse_tools(obj)?;

    Ok(ChatRequest {
        model,
        messages,
        system_prompt,
        tools,
        tool_choice,
        stream,
        include_usage,
        temperature,
        max_tokens,
        reasoning_effort,
        top_p,
        has_image,
        client_session_id: None, // 来自请求头，由 server 层填
    })
}

/// 解析 `stream_options.include_usage`（缺失/`null` 视为 false）；只允许这一个子键，多余键或类型错误报错。
fn parse_stream_options(obj: &Map<String, Value>) -> Result<bool, GatewayError> {
    match obj.get("stream_options") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Object(m)) => {
            if let Some(key) = m.keys().find(|k| k.as_str() != "include_usage") {
                return Err(GatewayError::unsupported(
                    key,
                    "only `stream_options.include_usage` is supported",
                ));
            }
            match m.get("include_usage") {
                None | Some(Value::Null) => Ok(false),
                Some(Value::Bool(b)) => Ok(*b),
                _ => Err(GatewayError::invalid(
                    "`stream_options.include_usage` must be a boolean",
                    Some("stream_options.include_usage"),
                )),
            }
        }
        _ => Err(GatewayError::invalid(
            "`stream_options` must be an object",
            Some("stream_options"),
        )),
    }
}

/// 拒绝网关无法如实表达的语义性参数（`n`/`stop`/惩罚项/`logprobs`/非 text 的 `response_format` 及 `REJECTED_PARAMS`）。
/// 默认值（n=1、空 stop、0 惩罚、logprobs=false、`{"type":"text"}`）放行；命中即返回 unsupported 错误。
fn reject_unsupported_semantics(obj: &Map<String, Value>) -> Result<(), GatewayError> {
    // n：只支持 1（默认值放行）
    if let Some(v) = obj.get("n")
        && !v.is_null()
        && v.as_u64() != Some(1)
    {
        return Err(GatewayError::unsupported("n", "only n=1 is supported"));
    }

    // stop：空/缺失放行（无 stop 序列）
    if let Some(v) = obj.get("stop")
        && !(v.is_null()
            || v.as_str().is_some_and(|s| s.is_empty())
            || v.as_array().is_some_and(|a| a.is_empty()))
    {
        return Err(GatewayError::unsupported(
            "stop",
            "stop sequences are not supported",
        ));
    }

    // 惩罚项：0 等价于不设置
    for p in ["frequency_penalty", "presence_penalty"] {
        if let Some(v) = obj.get(p)
            && !(v.is_null() || v.as_f64() == Some(0.0))
        {
            return Err(GatewayError::unsupported(p, "penalties are not supported"));
        }
    }

    // logprobs：false/缺失放行
    if let Some(v) = obj.get("logprobs")
        && !(v.is_null() || v.as_bool() == Some(false))
    {
        return Err(GatewayError::unsupported(
            "logprobs",
            "log probabilities are not supported",
        ));
    }

    // response_format：仅放行默认的 {"type":"text"}
    if let Some(v) = obj.get("response_format")
        && !v.is_null()
        && v.get("type").and_then(|t| t.as_str()) != Some("text")
    {
        return Err(GatewayError::unsupported(
            "response_format",
            "only the default `{\"type\":\"text\"}` is supported",
        ));
    }

    for p in REJECTED_PARAMS {
        if obj.get(*p).is_some_and(|v| !v.is_null()) {
            return Err(GatewayError::unsupported(
                p,
                "not supported by this gateway",
            ));
        }
    }

    Ok(())
}

/// 解析 `temperature`（缺失/`null` 为 `None`）；非数字或超出 [0,2] 报错。
fn parse_temperature(obj: &Map<String, Value>) -> Result<Option<f64>, GatewayError> {
    match obj.get("temperature") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let t = v.as_f64().ok_or_else(|| {
                GatewayError::invalid("`temperature` must be a number", Some("temperature"))
            })?;

            if !(0.0..=2.0).contains(&t) {
                return Err(GatewayError::invalid(
                    "`temperature` must be between 0 and 2",
                    Some("temperature"),
                ));
            }
            Ok(Some(t))
        }
    }
}

/// 解析 `top_p`（缺失/`null` 为 `None`）；非数字或不在 (0,1] 报错。
fn parse_top_p(obj: &Map<String, Value>) -> Result<Option<f64>, GatewayError> {
    match obj.get("top_p") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let p = v
                .as_f64()
                .ok_or_else(|| GatewayError::invalid("`top_p` must be a number", Some("top_p")))?;

            if !(p > 0.0 && p <= 1.0) {
                return Err(GatewayError::invalid(
                    "`top_p` must be in (0, 1]",
                    Some("top_p"),
                ));
            }
            Ok(Some(p))
        }
    }
}

/// 合并 `max_tokens` 与 `max_completion_tokens`：两者同时给出且不等报错，否则取其一；都缺为 `None`。
fn parse_max_tokens(obj: &Map<String, Value>) -> Result<Option<u32>, GatewayError> {
    let a = parse_token_field(obj, "max_tokens")?;
    let b = parse_token_field(obj, "max_completion_tokens")?;
    match (a, b) {
        (Some(x), Some(y)) if x != y => Err(GatewayError::invalid(
            "`max_tokens` and `max_completion_tokens` disagree; send only one",
            Some("max_tokens"),
        )),
        (Some(x), _) | (_, Some(x)) => Ok(Some(x)),
        (None, None) => Ok(None),
    }
}

/// 解析单个 token 上限字段：须为正整数且不超 u32 上限，否则报错；缺失/`null` 为 `None`。
fn parse_token_field(obj: &Map<String, Value>, field: &str) -> Result<Option<u32>, GatewayError> {
    match obj.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v.as_u64().filter(|n| *n > 0).ok_or_else(|| {
                GatewayError::invalid(format!("`{field}` must be a positive integer"), Some(field))
            })?;
            u32::try_from(n)
                .map(Some)
                .map_err(|_| GatewayError::invalid(format!("`{field}` is too large"), Some(field)))
        }
    }
}

/// `reasoning_effort` → thinking 级别（`none` 归一为 `off`）
fn parse_reasoning_effort(obj: &Map<String, Value>) -> Result<Option<String>, GatewayError> {
    let Some(v) = obj.get("reasoning_effort") else {
        return Ok(None);
    };

    if v.is_null() {
        return Ok(None);
    }

    let raw = v.as_str().ok_or_else(|| {
        GatewayError::invalid(
            "`reasoning_effort` must be a string",
            Some("reasoning_effort"),
        )
    })?;

    let level = match raw {
        "none" => "off",
        other => other,
    };

    if !VALID_THINKING_LEVELS.contains(&level) {
        return Err(GatewayError::invalid(
            format!(
                "`reasoning_effort` must be one of {} (or `none`)",
                VALID_THINKING_LEVELS.join(", ")
            ),
            Some("reasoning_effort"),
        ));
    }
    Ok(Some(level.to_string()))
}

/// 解析 `messages`：system/developer 拼进系统提示，其余转内部消息。
fn parse_messages(
    obj: &Map<String, Value>,
) -> Result<(String, Vec<AgentMessage>, bool), GatewayError> {
    let Some(raw) = obj.get("messages") else {
        return Err(GatewayError::invalid(
            "`messages` is required",
            Some("messages"),
        ));
    };

    let list = raw
        .as_array()
        .ok_or_else(|| GatewayError::invalid("`messages` must be an array", Some("messages")))?;

    if list.is_empty() {
        return Err(GatewayError::invalid(
            "`messages` must not be empty",
            Some("messages"),
        ));
    }

    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<AgentMessage> = Vec::new();
    let mut has_image = false;

    for (i, msg) in list.iter().enumerate() {
        let role = msg.get("role").and_then(|v| v.as_str()).ok_or_else(|| {
            GatewayError::invalid(format!("messages[{i}].role is required"), Some("messages"))
        })?;

        match role {
            "system" | "developer" => {
                let text = content_text(msg, i)?;
                if !text.trim().is_empty() {
                    system_parts.push(text);
                }
            }
            "user" => {
                let (blocks, image) = parse_content_blocks(msg, i, true)?;
                has_image |= image;
                messages.push(AgentMessage {
                    role: "user".to_string(),
                    thinking_level: None,
                    content: blocks,
                    ..AgentMessage::user_text("")
                });
            }
            "assistant" => {
                // 内部约定：思考块在正文之前（见 provider 层的 finish 组装）
                let mut blocks = Vec::new();
                if let Some(t) = msg
                    .get("reasoning_content")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    blocks.push(ContentBlock::Thinking {
                        thinking: t.to_string(),
                        thinking_signature: None,
                        redacted: None,
                    });
                }

                if msg.get("content").is_some_and(|c| !c.is_null()) {
                    let (b, image) = parse_content_blocks(msg, i, false)?;
                    has_image |= image;
                    blocks.extend(b);
                }
                blocks.extend(parse_assistant_tool_calls(msg, i)?);

                if blocks.is_empty() {
                    return Err(GatewayError::invalid(
                        format!("messages[{i}] (assistant) has neither content nor tool_calls"),
                        Some("messages"),
                    ));
                }

                messages.push(AgentMessage {
                    role: "assistant".to_string(),
                    thinking_level: None,
                    content: blocks,
                    ..AgentMessage::user_text("")
                });
            }
            "tool" => {
                let id = msg
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        GatewayError::invalid(
                            format!("messages[{i}] (tool) requires `tool_call_id`"),
                            Some("messages"),
                        )
                    })?;

                let text = content_text(msg, i)?;

                messages.push(AgentMessage {
                    role: "toolResult".to_string(),
                    thinking_level: None,
                    content: vec![ContentBlock::Text {
                        text,
                        text_signature: None,
                    }],
                    tool_call_id: Some(id.to_string()),
                    ..AgentMessage::user_text("")
                });
            }

            // legacy `function` 角色（2023 年的协议）不支持
            other => {
                return Err(GatewayError::invalid(
                    format!("messages[{i}].role `{other}` is not supported"),
                    Some("messages"),
                ));
            }
        }
    }

    Ok((system_parts.join("\n\n"), messages, has_image))
}

/// 取消息文本内容（字符串 content，或 content 数组里的 text 部分）。
fn content_text(msg: &Value, i: usize) -> Result<String, GatewayError> {
    match msg.get("content") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for (j, part) in parts.iter().enumerate() {
                match part.get("type").and_then(|t| t.as_str()) {
                    Some("text") | Some("input_text") => {
                        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                            out.push_str(t);
                        }
                    }
                    other => {
                        return Err(GatewayError::invalid(
                            format!(
                                "messages[{i}].content[{j}] type `{}` is not supported here",
                                other.unwrap_or("(missing)")
                            ),
                            Some("messages"),
                        ));
                    }
                }
            }
            Ok(out)
        }
        _ => Err(GatewayError::invalid(
            format!("messages[{i}].content must be a string or an array"),
            Some("messages"),
        )),
    }
}

/// 解析 user/assistant 的 content 为内容块；`allow_image` 为假时拒绝图片部分。
///
/// 图片只接受 **data URI**（`data:image/png;base64,<...>`）：远程 `http(s)` 图片需要网关
/// 代抓（SSRF 面 + 内存放大），本阶段明确不支持。
fn parse_content_blocks(
    msg: &Value,
    i: usize,
    allow_image: bool,
) -> Result<(Vec<ContentBlock>, bool), GatewayError> {
    let Some(content) = msg.get("content") else {
        return Ok((Vec::new(), false));
    };

    match content {
        Value::Null => Ok((Vec::new(), false)),
        Value::String(s) => Ok((
            if s.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::Text {
                    text: s.clone(),
                    text_signature: None,
                }]
            },
            false,
        )),
        Value::Array(parts) => {
            let mut blocks = Vec::new();
            let mut has_image = false;

            for (j, part) in parts.iter().enumerate() {
                match part.get("type").and_then(|t| t.as_str()) {
                    Some("text") | Some("input_text") => {
                        if let Some(t) = part.get("text").and_then(|v| v.as_str())
                            && !t.is_empty()
                        {
                            blocks.push(ContentBlock::Text {
                                text: t.to_string(),
                                text_signature: None,
                            });
                        }
                    }
                    Some("image_url") | Some("input_image") => {
                        let url = part
                            .get("image_url")
                            .and_then(|v| {
                                v.as_str().map(|s| s.to_string()).or_else(|| {
                                    v.get("url").and_then(|u| u.as_str()).map(|s| s.to_string())
                                })
                            })
                            .filter(|s| !s.is_empty())
                            .ok_or_else(|| {
                                GatewayError::invalid(
                                    format!("messages[{i}].content[{j}].image_url.url is required"),
                                    Some("messages"),
                                )
                            })?;

                        let (mime, data) = parse_data_uri(&url).ok_or_else(|| {
                            GatewayError::unsupported(
                                "messages",
                                "images must be sent as `data:image/<type>;base64,<...>` data URIs (remote image URLs are not fetched)",
                            )
                        })?;

                        if !allow_image {
                            return Err(GatewayError::unsupported(
                                "messages",
                                "images are only accepted on user messages",
                            ));
                        }

                        blocks.push(ContentBlock::Image {
                            data,
                            mime_type: mime,
                        });
                        has_image = true;
                    }
                    other => {
                        return Err(GatewayError::unsupported(
                            "messages",
                            &format!(
                                "content part type `{}` is not supported",
                                other.unwrap_or("(missing)")
                            ),
                        ));
                    }
                }
            }
            Ok((blocks, has_image))
        }
        _ => Err(GatewayError::invalid(
            format!("messages[{i}].content must be a string, an array, or null"),
            Some("messages"),
        )),
    }
}

/// assistant 消息的 `tool_calls` → `ContentBlock::ToolCall`
fn parse_assistant_tool_calls(msg: &Value, i: usize) -> Result<Vec<ContentBlock>, GatewayError> {
    let Some(raw) = msg.get("tool_calls") else {
        return Ok(Vec::new());
    };

    if raw.is_null() {
        return Ok(Vec::new());
    }

    let calls = raw.as_array().ok_or_else(|| {
        GatewayError::invalid(
            format!("messages[{i}].tool_calls must be an array"),
            Some("messages"),
        )
    })?;

    let mut out = Vec::new();
    for (j, call) in calls.iter().enumerate() {
        let id = call
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                GatewayError::invalid(
                    format!("messages[{i}].tool_calls[{j}].id is required"),
                    Some("messages"),
                )
            })?;

        let function = call.get("function").ok_or_else(|| {
            GatewayError::invalid(
                format!("messages[{i}].tool_calls[{j}].function is required"),
                Some("messages"),
            )
        })?;

        let name = function
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                GatewayError::invalid(
                    format!("messages[{i}].tool_calls[{j}].function.name is required"),
                    Some("messages"),
                )
            })?;

        let arguments = match function.get("arguments") {
            None | Some(Value::Null) => json!({}),
            // 规范是 JSON 字符串；部分客户端直接给对象，一并接受
            Some(Value::String(s)) if s.trim().is_empty() => json!({}),
            Some(Value::String(s)) => serde_json::from_str::<Value>(s).map_err(|e| {
                GatewayError::invalid(
                    format!(
                        "messages[{i}].tool_calls[{j}].function.arguments is not valid JSON: {e}"
                    ),
                    Some("messages"),
                )
            })?,
            Some(v) if v.is_object() => v.clone(),
            Some(_) => {
                return Err(GatewayError::invalid(
                    format!(
                        "messages[{i}].tool_calls[{j}].function.arguments must be a JSON string"
                    ),
                    Some("messages"),
                ));
            }
        };

        out.push(ContentBlock::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments,
            thought_signature: None,
            namespace: None,
        });
    }
    Ok(out)
}

/// `tools` / `tool_choice` → 内部工具三元组与 tool_choice 字符串
fn parse_tools(obj: &Map<String, Value>) -> Result<(ToolDefs, Option<String>), GatewayError> {
    let mut tools = Vec::new();
    match obj.get("tools") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) => {
            for (i, tool) in list.iter().enumerate() {
                if tool.get("type").and_then(|t| t.as_str()) != Some("function") {
                    return Err(GatewayError::unsupported(
                        "tools",
                        "only `type: \"function\"` tools are supported",
                    ));
                }

                let f = tool.get("function").ok_or_else(|| {
                    GatewayError::invalid(format!("tools[{i}].function is required"), Some("tools"))
                })?;

                let name = f
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        GatewayError::invalid(
                            format!("tools[{i}].function.name is required"),
                            Some("tools"),
                        )
                    })?;

                // strict 改变的是「schema 必须被强制满足」的语义，网关无法如实表达 → 拒绝
                if f.get("strict").and_then(|v| v.as_bool()) == Some(true) {
                    return Err(GatewayError::unsupported(
                        "tools",
                        "`strict: true` cannot be honored by this gateway",
                    ));
                }

                let description = f
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                let parameters = match f.get("parameters") {
                    None | Some(Value::Null) => json!({"type": "object", "properties": {}}),
                    Some(v) if v.is_object() => v.clone(),
                    Some(_) => {
                        return Err(GatewayError::invalid(
                            format!("tools[{i}].function.parameters must be an object"),
                            Some("tools"),
                        ));
                    }
                };

                tools.push((name.to_string(), description, parameters));
            }
        }
        Some(_) => {
            return Err(GatewayError::invalid(
                "`tools` must be an array",
                Some("tools"),
            ));
        }
    }

    let tool_choice = match obj.get("tool_choice") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => match s.as_str() {
            "auto" | "none" => Some(s.clone()),
            // `required` 与「指定函数」在统一接口里没有跨协议等价表达（anthropic 的
            // any/tool 与 openai 的 required/named 语义并不相通），拒绝而不静默降级
            other => {
                return Err(GatewayError::unsupported(
                    "tool_choice",
                    &format!("`{other}` is not supported; use `auto` or `none`"),
                ));
            }
        },
        Some(Value::Object(_)) => {
            return Err(GatewayError::unsupported(
                "tool_choice",
                "naming a specific function is not supported; use `auto` or `none`",
            ));
        }
        Some(_) => {
            return Err(GatewayError::invalid(
                "`tool_choice` must be a string or an object",
                Some("tool_choice"),
            ));
        }
    };

    Ok((tools, tool_choice))
}

/// 解析 `data:<mime>;base64,<payload>` → `(mime, base64_payload)`。
///
/// 只接受 `image/*`；base64 载荷做轻量校验（字符集 + 长度对齐），
/// 避免把明显非法的东西塞给上游换取一个难读的 400。
fn parse_data_uri(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let mut parts = meta.split(';');
    let mime = parts.next()?.trim();

    if !mime.starts_with("image/") || !parts.any(|p| p.eq_ignore_ascii_case("base64")) {
        return None;
    }

    if payload.is_empty() || payload.len() % 4 != 0 {
        return None;
    }

    if !payload
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
    {
        return None;
    }

    Some((mime.to_string(), payload.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(v: Value) -> Value {
        v
    }

    fn simple(model: &str) -> Value {
        body(json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}]
        }))
    }

    #[test]
    fn parses_minimal_request() {
        let req = parse_chat_request(&simple("deepseek-chat")).unwrap();
        assert_eq!(req.model, "deepseek-chat");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
        assert!(req.system_prompt.is_empty());
        assert!(!req.stream);
        assert!(!req.include_usage);
        assert!(req.tools.is_empty());
        assert_eq!(req.tool_choice, None);
        assert!(req.temperature.is_none());
        assert!(req.max_tokens.is_none());
    }

    #[test]
    fn model_is_required() {
        let err = parse_chat_request(&body(json!({"messages": []}))).unwrap_err();
        assert_eq!(err.param.as_deref(), Some("model"));
        let err = parse_chat_request(&body(json!({"model": "  ", "messages": []}))).unwrap_err();
        assert_eq!(err.param.as_deref(), Some("model"));
    }

    #[test]
    fn messages_must_be_non_empty_array() {
        let err = parse_chat_request(&body(json!({"model": "m", "messages": []}))).unwrap_err();
        assert_eq!(err.param.as_deref(), Some("messages"));
        let err = parse_chat_request(&body(json!({"model": "m", "messages": "hi"}))).unwrap_err();
        assert_eq!(err.param.as_deref(), Some("messages"));
    }

    #[test]
    fn system_and_developer_join_into_system_prompt() {
        let req = parse_chat_request(&body(json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "developer", "content": [{"type": "text", "text": "be kind"}]},
                {"role": "user", "content": "hi"}
            ]
        })))
        .unwrap();
        assert_eq!(req.system_prompt, "be terse\n\nbe kind");
        assert_eq!(req.messages.len(), 1, "system/developer 不进内部消息");
    }

    #[test]
    fn tool_role_becomes_tool_result_message() {
        let req = parse_chat_request(&body(json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function",
                    "function": {"name": "read", "arguments": "{\"path\":\"a\"}"}}]},
                {"role": "tool", "tool_call_id": "call_1", "content": "file body"}
            ]
        })))
        .unwrap();
        assert_eq!(req.messages[0].role, "assistant");
        match &req.messages[0].content[0] {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
                ..
            } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "read");
                assert_eq!(arguments["path"], "a");
            }
            other => panic!("expected tool call, got {other:?}"),
        }
        assert_eq!(req.messages[1].role, "toolResult");
        assert_eq!(req.messages[1].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(req.messages[1].text(), "file body");
    }

    #[test]
    fn tool_call_arguments_must_be_json() {
        let err = parse_chat_request(&body(json!({
            "model": "m",
            "messages": [{"role": "assistant", "tool_calls": [
                {"id": "c", "function": {"name": "f", "arguments": "{oops"}}]}]
        })))
        .unwrap_err();
        assert!(err.message.contains("not valid JSON"), "{}", err.message);
    }

    #[test]
    fn legacy_function_role_is_rejected() {
        let err = parse_chat_request(&body(json!({
            "model": "m",
            "messages": [{"role": "function", "name": "f", "content": "x"}]
        })))
        .unwrap_err();
        assert!(err.message.contains("`function`"), "{}", err.message);
    }

    #[test]
    fn assistant_without_content_or_tool_calls_is_rejected() {
        let err = parse_chat_request(&body(json!({
            "model": "m",
            "messages": [{"role": "assistant"}]
        })))
        .unwrap_err();
        assert_eq!(err.param.as_deref(), Some("messages"));
    }

    #[test]
    fn reasoning_content_is_accepted_on_assistant_messages() {
        let req = parse_chat_request(&body(json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "content": "answer", "reasoning_content": "thought"},
                {"role": "user", "content": "next"}
            ]
        })))
        .unwrap();
        assert!(matches!(
            req.messages[0].content[0],
            ContentBlock::Thinking { .. }
        ));
    }

    #[test]
    fn data_uri_images_map_to_image_blocks() {
        let req = parse_chat_request(&body(json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "what is this"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]}]
        })))
        .unwrap();
        assert!(req.has_image);
        assert!(matches!(
            req.messages[0].content[1],
            ContentBlock::Image { .. }
        ));
    }

    #[test]
    fn remote_image_urls_are_rejected_with_unsupported_parameter() {
        let err = parse_chat_request(&body(json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
            ]}]
        })))
        .unwrap_err();
        assert_eq!(err.code.as_deref(), Some("unsupported_parameter"));
        assert!(err.message.contains("data URI"), "{}", err.message);
    }

    #[test]
    fn garbage_data_uri_is_rejected() {
        for url in [
            "data:image/png;base64,!!!!",
            "data:text/plain;base64,AAAA",
            "data:image/png,AAAA",
            "data:image/png;base64,AAA",
        ] {
            assert!(parse_data_uri(url).is_none(), "{url} 应被拒绝");
        }
        assert_eq!(
            parse_data_uri("data:image/png;base64,AAAA"),
            Some(("image/png".into(), "AAAA".into()))
        );
    }

    #[test]
    fn tools_map_to_internal_triples() {
        let req = parse_chat_request(&body(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {
                "name": "read", "description": "read a file",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
            }}],
            "tool_choice": "auto"
        })))
        .unwrap();
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].0, "read");
        assert_eq!(req.tools[0].1, "read a file");
        assert_eq!(req.tools[0].2["type"], "object");
        assert_eq!(req.tool_choice.as_deref(), Some("auto"));
    }

    #[test]
    fn named_tool_choice_and_strict_tools_are_rejected() {
        let err = parse_chat_request(&body(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "tool_choice": {"type": "function", "function": {"name": "read"}}
        })))
        .unwrap_err();
        assert_eq!(err.code.as_deref(), Some("unsupported_parameter"));

        let err = parse_chat_request(&body(json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": "read", "strict": true}}]
        })))
        .unwrap_err();
        assert_eq!(err.code.as_deref(), Some("unsupported_parameter"));
    }

    #[test]
    fn semantic_params_are_rejected_but_metadata_is_ignored() {
        for (key, value) in [
            ("stop", json!(["\n\n"])),
            ("n", json!(3)),
            ("logprobs", json!(true)),
            ("top_logprobs", json!(3)),
            ("logit_bias", json!({"1": 2})),
            ("seed", json!(7)),
            ("response_format", json!({"type": "json_object"})),
            ("modalities", json!(["text", "audio"])),
            ("frequency_penalty", json!(0.5)),
            ("presence_penalty", json!(-0.5)),
            ("web_search_options", json!({})),
            ("verbosity", json!("low")),
        ] {
            let mut b = simple("m");
            b[key] = value;
            let err = parse_chat_request(&b).unwrap_err();
            assert_eq!(
                err.code.as_deref(),
                Some("unsupported_parameter"),
                "{key} 应被拒绝"
            );
        }

        // 等价于「不设置」的取值放行
        for (key, value) in [
            ("n", json!(1)),
            ("stop", json!([])),
            ("stop", Value::Null),
            ("logprobs", json!(false)),
            ("response_format", json!({"type": "text"})),
            ("frequency_penalty", json!(0)),
        ] {
            let mut b = simple("m");
            b[key] = value;
            assert!(parse_chat_request(&b).is_ok(), "{key} 应放行");
        }

        // 纯元数据忽略
        let mut b = simple("m");
        b["user"] = json!("u-1");
        b["metadata"] = json!({"trace": "x"});
        b["store"] = json!(true);
        b["service_tier"] = json!("auto");
        b["parallel_tool_calls"] = json!(true);
        assert!(parse_chat_request(&b).is_ok());
    }

    #[test]
    fn token_fields_and_temperature_are_validated() {
        let mut b = simple("m");
        b["max_tokens"] = json!(100);
        b["max_completion_tokens"] = json!(200);
        assert!(
            parse_chat_request(&b)
                .unwrap_err()
                .message
                .contains("disagree")
        );

        let mut b = simple("m");
        b["max_tokens"] = json!(0);
        assert!(parse_chat_request(&b).is_err());

        let mut b = simple("m");
        b["temperature"] = json!(3);
        assert!(parse_chat_request(&b).is_err());

        let mut b = simple("m");
        b["top_p"] = json!(0);
        assert!(parse_chat_request(&b).is_err());

        let mut b = simple("m");
        b["max_completion_tokens"] = json!(64);
        b["temperature"] = json!(0.2);
        b["top_p"] = json!(0.9);
        let req = parse_chat_request(&b).unwrap();
        assert_eq!(req.max_tokens, Some(64));
        assert_eq!(req.temperature, Some(0.2));
        assert_eq!(req.top_p, Some(0.9));
    }

    #[test]
    fn reasoning_effort_maps_to_thinking_levels() {
        let mut b = simple("m");
        b["reasoning_effort"] = json!("high");
        assert_eq!(
            parse_chat_request(&b).unwrap().reasoning_effort.as_deref(),
            Some("high")
        );

        b["reasoning_effort"] = json!("none");
        assert_eq!(
            parse_chat_request(&b).unwrap().reasoning_effort.as_deref(),
            Some("off")
        );

        b["reasoning_effort"] = json!("turbo");
        assert!(parse_chat_request(&b).is_err());
    }

    #[test]
    fn stream_options_only_allows_include_usage() {
        let mut b = simple("m");
        b["stream"] = json!(true);
        b["stream_options"] = json!({"include_usage": true});
        let req = parse_chat_request(&b).unwrap();
        assert!(req.stream);
        assert!(req.include_usage);

        b["stream_options"] = json!({"include_usage": true, "foo": 1});
        assert_eq!(
            parse_chat_request(&b).unwrap_err().code.as_deref(),
            Some("unsupported_parameter")
        );
    }
}
