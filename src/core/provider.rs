//! Provider 层：模型请求的统一入口与公共类型。
//!
//! 协议实现分布在子模块：
//! - [`completions`]: OpenAI chat completions（deepseek 等）
//! - [`responses`]:  OpenAI responses（gpt-5.x 等）
//! - [`anthropic`]:  Anthropic messages（claude 等）
//! - [`google`]:     Google generative ai（gemini 等）
//! - [`images`]：图片生成（一次性非流式，内置 `openrouter-images`）
//! - [`classifier`]：分类器调用（一次性非流式，内置 `typesafe-system-one`）
//! - [`api_impls`]：扩展为自定义 `api` 名注册的图片 / 分类器实现
//!
//! 调用方（agent_session）只与 [`stream_chat`] / [`simple_completion`]
//! 两个入口交互，按 `ModelConfig.api` 内部路由到对应协议实现；
//! 图片生成与分类器是独立入口（[`generate_images`] / [`classify`]），
//! 由扩展直接调用，不经过 agent 循环。

mod convert;
mod retry;

pub mod anthropic;
pub mod api_impls;
pub mod classifier;
pub mod completions;
pub mod google;
pub mod images;
pub mod responses;
pub(crate) mod usage;

#[cfg(test)]
mod mock_tests;

use crate::{
    APP_NAME,
    cli::args::VALID_THINKING_LEVELS,
    core::{self, model_config, model_resolver, settings_manager},
    error::{Error, Result},
    utils::{image::ImageResizeLimits, time::now_ms},
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use strum_macros::{EnumString, IntoStaticStr};

pub use crate::utils::{http::STREAM_READ_TIMEOUT, proxy::proxy_from_env};
pub use classifier::{
    BoolCriteria, ClassifierAnswer, ClassifierContext, ClassifierQuestion, ClassifierResult,
};
pub use convert::{
    BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX, COMPACTION_SUMMARY_PREFIX,
    COMPACTION_SUMMARY_SUFFIX,
};
pub use images::{AssistantImages, ImageContent};
pub use retry::DEFAULT_MAX_RETRIES;

/// onPayload 钩子：provider 发送前检查/替换请求体。
pub type PayloadHook = Arc<Mutex<Box<dyn FnMut(&mut Value) + Send>>>;

/// 单次 provider 请求允许的最长超时（毫秒），用于约束请求超时上限。
pub const MAX_TIMEOUT_MS: u64 = 300_000;

/// 流式响应允许累计的最大字节数（16 MiB）。
///
/// provider 异常时（连接迟迟不结束、同一段内容反复输出）流会一直吐数据，`assistant_text`
/// 与 TUI 的流式缓冲随之无界增长，最终把内存吃光、被系统 OOM killer 杀掉整个进程——
/// 现场没有任何 panic 输出，看起来就是"进程凭空消失"。越界即中止该流，降级成一次普通请求失败。
/// 正常 LLM 回复（< 1 MiB）远达不到此上限。
pub const MAX_STREAM_BYTES: usize = 16 * 1024 * 1024;

/// 工具参数 schema 里承载语法约束的私有键（由 agent 侧注入，见 `compose_tools`）。
pub const GRAMMAR_SCHEMA_KEY: &str = concat!("x-", env!("CARGO_PKG_NAME"), "-grammar");

/// 一个语法约束工具的协议无关形式（两套 OpenAI 协议的 `custom` 工具用它）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrammarToolSpec {
    /// 语法类型：`lark`（优先）或 `regex`。
    pub syntax: &'static str,
    /// 语法定义正文。
    pub definition: String,
    /// 承载源码的属性名（流式包装与历史回放用）。
    pub input: String,
}

/// 读取工具参数 schema 里的语法约束；未声明时返回 `None`。
///
/// `syntax`/`definition` 与 `input` 缺失其一（语法变体都不给、或不是 object schema /
/// 推不出唯一必填 string 属性）都视为未声明：工具退回普通 `function` 形态。
pub(crate) fn grammar_tool(parameters: &Value) -> Option<GrammarToolSpec> {
    let grammar = parameters.get(GRAMMAR_SCHEMA_KEY)?;
    let lark = grammar
        .get("openai_lark")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty());
    let regex = grammar
        .get("openai_regex")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty());
    let (syntax, definition) = match (lark, regex) {
        (Some(lark), _) => ("lark", lark),
        (None, Some(regex)) => ("regex", regex),
        (None, None) => return None,
    };
    let input = grammar
        .get("input")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| grammar_input_property(parameters))?;

    Some(GrammarToolSpec {
        syntax,
        definition: definition.to_string(),
        input,
    })
}

/// 承载源码的属性名：schema 里恰好一个必填的 string 属性
pub(crate) fn grammar_input_property(schema: &Value) -> Option<String> {
    let required = schema.get("required")?.as_array()?;
    if required.len() != 1 {
        return None;
    }

    let name = required[0].as_str()?;
    let property = schema.get("properties")?.get(name)?;
    if property.get("type").and_then(|v| v.as_str()) == Some("string") {
        Some(name.to_string())
    } else {
        None
    }
}

/// JSON 字符串字面量的转义形式（不含首尾引号），用于把裸源码拼进 arguments。
pub(crate) fn escape_json_text(text: &str) -> String {
    let quoted = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    quoted[1..quoted.len() - 1].to_string()
}

/// 工具名 → 语法约束工具的承载属性名（查扩展注册表；非语法工具返回 `None`）。
///
/// 历史消息回放与流式包装只知道工具名，因此按名字回到工具定义上取属性名。
pub(crate) fn grammar_input_property_for_tool(name: &str) -> Option<String> {
    core::extensions::registered().iter().find_map(|ext| {
        ext.tools()
            .iter()
            .find(|t| t.name == name)
            .filter(|t| t.grammar_sampling.is_some())
            .and_then(|t| grammar_input_property(&t.parameters))
    })
}

/// Anthropic 服务端 fallback 模型（目录 `compat.allowedFallbackModels` 条目）。
///
/// `cost` 是该 fallback 模型的计费单价：当响应 `message.model` 与请求模型不一致时，
/// 用它（而不是请求模型的定价）计算本轮成本。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AllowedFallbackModel {
    /// 声明 fallback 所属 provider（只有与请求 provider 相同时才用于计费归属）
    pub provider: String,
    /// 服务端降级时切换到的模型 id（随请求 `fallbacks` 字段下发）。
    pub model: String,
    /// 该 fallback 的每百万 token 计价；None 表示目录未提供单价。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Value>,
}

/// 模型用途（目录条目的 `type` 字段）：决定该条目能不能走 chat 流。
///
/// 目录里同一份文件混装 chat / image / classifier 三种条目（如 openrouter 的
/// FLUX 图片模型、`~typesafe/jev-latest` 分类器）；chat 走 [`super::stream_chat`]，
/// image 走 [`super::generate_images`]，classifier 走 [`super::classify`]。
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Default,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum ModelType {
    /// 对话 / 工具调用模型（`type: "chat"`，目录缺省值）。
    #[default]
    Chat,
    /// 图片生成模型（`type: "image"`）。
    Image,
    /// 分类器模型（`type: "classifier"`，如 Jev）。
    Classifier,
}

impl ModelType {
    /// 解析类型字符串（去首尾空白、大小写不敏感）；无法识别时返回 `None`。
    pub fn parse(s: &str) -> Option<Self> {
        s.trim().parse().ok()
    }

    /// 目录 `type` 字段 → 枚举。
    ///
    /// - 缺字段 / 非字符串 / 空串：`Some(Chat)`，旧目录与用户自写条目按对话模型处理；
    /// - `chat` / `image` / `classifier`（去空白、大小写不敏感）：对应变体；
    /// - 其它非空取值：`None` —— 该条目应被丢弃，不进任何列表（未知模型类型被忽略）。
    pub fn from_catalog(value: Option<&Value>) -> Option<Self> {
        match value.and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => ModelType::parse(s),
            _ => Some(ModelType::Chat),
        }
    }

    /// 目录写回 / 错误文案用的字符串（`chat` / `image` / `classifier`）。
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// 是否为 chat 类型（serde 跳写用：只有非 chat 才序列化该字段）。
    pub fn is_chat(&self) -> bool {
        *self == ModelType::Chat
    }
}

/// 单个模型的完整配置（来自 models.json/models-store），含端点、凭据、能力开关与计费信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// 目录 `type`：模型用途；缺该字段（旧目录 / 动态缓存）时视为 chat。
    #[serde(default, skip_serializing_if = "ModelType::is_chat")]
    pub model_type: ModelType,
    /// provider 标识（如 anthropic、opencode-go），决定请求头与计费归属。
    pub provider: String,
    /// 供应商侧的模型标识，请求与响应模型比对时使用。
    pub model_id: String,
    /// API 基础地址，endpoint() 在其后拼接协议路径。
    pub base_url: String,
    /// 访问凭据；OAuth 登录时存 access token。
    pub api_key: String,
    /// 协议实现路由键，决定 stream_chat 分发到哪个协议实现。
    #[serde(default = "default_api")]
    pub api: String,
    /// 支持的输入模态列表（"text"/"image"），决定能否内联图片。
    pub input: Vec<String>,
    /// 目录 `output`：输出模态列表（图片模型为 `image`，可带 `text`）。
    /// 图片生成用它决定请求体 `modalities`；chat 模型目录不带该字段。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<String>,
    /// 是否为推理模型，决定是否注入 thinking 参数。
    pub reasoning: bool,
    /// 单次响应输出 token 上限；None 表示不显式限制。
    pub max_tokens: Option<u32>,
    /// 采样温度覆盖值；None 表示沿用供应商默认。
    pub temperature: Option<f64>,
    /// 上下文窗口 token 数，用于占用率与溢出判断。
    pub context_window: u32,
    /// 思考参数的构造格式（如 openai/zai/deepseek/qwen，compat.thinkingFormat）。
    pub thinking_format: String,
    /// provider 是否接受显式 `reasoning_effort`（compat.supportsReasoningEffort，
    /// 缺失时由 provider/baseUrl 探测）。仅用于 openai 兼容的思考参数构造。
    #[serde(default)]
    pub supports_reasoning_effort: bool,
    /// 各级别 → provider 请求参数值的映射（models-store.json 的 thinkingLevelMap）。
    /// null 表示该级别不受支持；xhigh/max 仅在存在映射时可用。
    #[serde(default)]
    pub thinking_level_map: Option<HashMap<String, Option<String>>>,
    /// models.json authHeader：请求层自动加 `Authorization: Bearer <apiKey>`
    #[serde(default)]
    pub auth_header: bool,
    /// 目录 `inputLimits.images.resize`：图片内联限制
    #[serde(default)]
    pub image_resize: ImageResizeLimits,
    /// true 时推理模型的 system 提示改用 developer 角色。
    pub supports_developer_role: bool,
    /// true 时重放 assistant 消息必须补 reasoning_content 字段。
    pub requires_reasoning_content_on_assistant_messages: bool,
    /// 请求体中承载输出上限的字段名（max_tokens/max_completion_tokens/maxOutputTokens）。
    pub max_tokens_field: String,
    /// 每百万 token 计价对象（input/output/cacheRead/cacheWrite）；None 表示不计费。
    pub cost: Option<Value>,
    /// 供应商请求重试延迟上限（ms）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_ms: Option<u64>,
    /// 各级别 thinking token 预算
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budgets: Option<Vec<u32>>,
    /// 模型 samplingParams 全量透传（temperature/top_p/top_k/min_p 等合并进请求体，键优先于默认但被显式参数覆盖）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<Value>,
    /// 目录 `samplingParamsByThinkingLevel`：思考级别（off/minimal/low/medium/high/xhigh/max）
    /// 覆盖 `sampling_params` 的键，取值为该级别对应的采样参数对象。非 openai 兼容协议忽略它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params_by_thinking_level: Option<Value>,
    /// OpenRouter/Vercel Gateway routing（compat.openRouterRouting/vercelGatewayRouting）：openai 兼容请求体发送 `provider` 字段
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_routing: Option<Value>,
    /// 会话标识：Anthropic 系缓存亲和头 `x-session-affinity`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// compat.sendSessionAffinityHeaders：是否下发会话亲和头；`None` 表示按 api + 是否 openrouter 探测。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_session_affinity_headers: Option<bool>,
    /// compat.sessionAffinityFormat：`openai` / `openai-nosession` / `openrouter`；
    /// `None` 表示按 provider / baseUrl 探测。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_affinity_format: Option<String>,
    /// openai 兼容 compat 开关
    pub supports_usage_in_streaming: bool,
    /// true 时请求体显式带 store=false，规避 store 语义差异。
    pub supports_store: bool,
    /// true 表示流式响应带 finish_reason 帧，否则按工具调用推断停止原因。
    pub supports_finish_reason: bool,
    /// true 时工具结果之后需补一条空 assistant 消息。
    pub requires_assistant_after_tool_result: bool,
    /// openai strict JSON schema 工具
    pub supports_strict_mode: bool,
    /// 目录 `compat.allowedFallbackModels`：Anthropic 服务端 fallback 名单
    /// （非空时请求带 `fallbacks` 与 `server-side-fallback` beta，且按返回模型计费）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_fallback_models: Vec<AllowedFallbackModel>,
}

impl ModelConfig {
    /// 该模型的 `chat/completions` 完整 URL：base_url 去尾斜杠后补 `/chat/completions`
    /// （已是该路径则原样返回）。
    pub fn endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{}/chat/completions", base)
        }
    }

    /// 按当前思考级别取 `samplingParamsByThinkingLevel` 的覆盖项。
    ///
    /// `reasoning_effort` 是 pi 级别名（未指定/关闭传 `None`，按 `off` 处理），
    /// 先经 [`Self::clamp_thinking_level`] 归一，钳制后的级别没有覆盖项时返回 `None`。
    pub fn sampling_params_for_level(&self, reasoning_effort: Option<&str>) -> Option<&Value> {
        let level = match reasoning_effort {
            Some(level) => self.clamp_thinking_level(level),
            None => "off".to_string(),
        };

        self.sampling_params_by_thinking_level
            .as_ref()
            .and_then(|m| m.get(&level))
            .filter(|v| v.is_object())
    }

    /// 当前模型支持的 thinking 级别
    /// 不支持推理 → 仅 ["off"]；级别映射为 null → 不支持；
    /// xhigh/max 需要显式映射（minimal~high 默认支持）。
    pub fn supported_thinking_levels(&self) -> Vec<&'static str> {
        if !self.reasoning {
            return vec!["off"];
        }

        VALID_THINKING_LEVELS
            .iter()
            .copied()
            .filter(
                |level| match self.thinking_level_map.as_ref().and_then(|m| m.get(*level)) {
                    Some(Some(_)) => true,
                    Some(None) => false,
                    None => !(*level == "xhigh" || *level == "max"),
                },
            )
            .collect()
    }

    /// 把 thinking 级别钳制到当前模型支持范围
    /// 先向上找最近可用级别，再向下，最后回退到第一个可用级别。
    pub fn clamp_thinking_level(&self, level: &str) -> String {
        let available = self.supported_thinking_levels();
        if available.contains(&level) {
            return level.to_string();
        }

        let levels = VALID_THINKING_LEVELS;
        match levels.iter().position(|l| *l == level) {
            None => available.first().copied().unwrap_or("off").to_string(),
            Some(i) => {
                for l in &levels[i..] {
                    if available.contains(l) {
                        return l.to_string();
                    }
                }
                for l in levels[..i].iter().rev() {
                    if available.contains(l) {
                        return l.to_string();
                    }
                }
                available.first().copied().unwrap_or("off").to_string()
            }
        }
    }
}

/// 引用条目（内容来源引用，provider 层透传）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Citation {
    /// 引用类型，序列化为 JSON 的 "type"（当前为 "citation"）。
    #[serde(rename = "type")]
    pub kind: String,
    /// 引用文本在正文中的起始字符下标；None 表示供应商未提供。
    pub start_index: Option<usize>,
    /// 引用文本在正文中的结束字符下标；None 表示供应商未提供。
    pub end_index: Option<usize>,
    /// 引用来源链接；None 表示无外部链接来源。
    pub url: Option<String>,
    /// 引用来源标题；None 表示无标题。
    pub title: Option<String>,
    /// 被引用的原文片段；None 表示仅给出位置。
    pub text: Option<String>,
}

/// 被推迟执行（deferred）的工具调用句柄，模型可稍后 fetch 结果
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredHandle {
    /// 发起该延迟调用的 provider，恢复时用于匹配。
    pub provider: String,
    /// 发起该延迟调用的模型 id。
    pub model_id: String,
    /// 供应商侧的延迟任务标识（如 `resp_1|call_2`）。
    pub id: String,
    /// 协议类型，决定用哪个实现去取回结果。
    pub api: String,
    /// 句柄失效时刻（epoch 毫秒）；None 表示不过期。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// 建议的轮询间隔（毫秒）；None 表示无建议间隔。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<u64>,
    /// 供应商附加的不透明数据，取回结果时原样回传；None 表示无。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// 消息内容块：文本、思考、工具调用、图片，是 agent 消息与各协议互转的公共表示。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// 纯文本内容块（用户输入、模型回复正文等）。
    #[serde(rename = "text")]
    Text {
        /// 文本正文内容。
        text: String,
        /// 文本块的供应商签名，校验/回传时原样带回；无签名为 None。
        #[serde(
            rename = "textSignature",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        text_signature: Option<String>,
    },
    /// 模型的思考（reasoning）内容块。
    #[serde(rename = "thinking")]
    Thinking {
        /// 思考正文内容。
        thinking: String,
        /// 思考块的供应商签名，回传时需原样带回；无签名为 None。
        #[serde(
            rename = "thinkingSignature",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        thinking_signature: Option<String>,
        /// 是否被打码（redacted）的 thinking 块
        #[serde(default, skip_serializing_if = "Option::is_none")]
        redacted: Option<bool>,
    },
    /// 模型发起的工具调用请求块。
    #[serde(rename = "toolCall")]
    ToolCall {
        /// 工具调用的唯一 id，toolResult 消息据此关联。
        id: String,
        /// 被调用的工具名。
        name: String,
        /// 工具入参 JSON，缺省时反序列化为空对象。
        #[serde(default)]
        arguments: Value,
        /// 工具调用附带的思考签名；无签名为 None。
        #[serde(
            rename = "thoughtSignature",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        thought_signature: Option<String>,
        /// OpenAI Responses 动态/命名空间工具的附加标识
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
    },
    /// 图片内容块（用户附件或工具返回的图片）。
    #[serde(rename = "image")]
    Image {
        /// 图片数据，base64 编码（不含 data URI 前缀）。
        data: String,
        /// 图片 MIME 类型，如 image/png。
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

// 消息模型（agent 内部表示）
/// agent 内部的消息表示：role + 内容块，并携带工具调用、用量、停止原因等元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessage {
    pub role: String, // "user" | "assistant" | "toolResult"
    /// 消息内容块列表，按顺序排列文本、思考、工具调用或图片。
    pub content: Vec<ContentBlock>,
    /// toolResult 消息关联的工具调用 id；非工具结果消息为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// toolResult 消息对应的工具名；非工具结果消息为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// 工具执行是否失败：true 表示返回的是错误结果。
    #[serde(default)]
    pub is_error: bool,
    /// 归一化后的停止原因（如 stop/length/error/aborted）；非 assistant 消息为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// 本轮出错时的错误描述；无错误为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// 本轮请求使用的模型 id；非 assistant 消息为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 本轮请求使用的供应商名；非 assistant 消息为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// 本轮请求使用的协议实现标识（如 anthropic-messages）；无则为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// 供应商返回的实际响应模型
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    /// 供应商响应 id
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// 本轮请求使用的 thinking 级别（assistant 消息携带；缺失 = 旧会话/非 agent 循环产生的响应）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    /// 供应商诊断信息
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Value>,
    /// 本轮的 token 用量与费用统计；无则为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// 被推迟执行的工具调用句柄（stop_reason == "deferred" 时携带）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredHandle>,
    /// 供应商原始 stop reason（未归一化）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_stop_reason: Option<String>,
    /// 模型是否显式结束本轮
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<bool>,
    /// 消息创建时间戳，Unix 毫秒。
    #[serde(default)]
    pub timestamp: u64,
    /// 工具执行耗时（毫秒；toolResult 消息携带，渲染 Took X.Xs）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 工具执行附加元数据
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// 供应商返回的内容来源引用（当前 anthropic 最小解析）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citations: Option<Vec<Citation>>,
    /// 该消息在会话存储中的条目 id；未落库（如临时注入消息）为 None。
    #[serde(skip)]
    pub entry_id: Option<String>,
}

impl AgentMessage {
    /// 构造一条只含单个文本块的 `user` 消息（时间戳为当前毫秒）。
    pub fn user_text(text: &str) -> Self {
        AgentMessage {
            role: "user".to_string(),
            thinking_level: None,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                text_signature: None,
            }],
            tool_call_id: None,
            tool_name: None,
            is_error: false,
            stop_reason: None,
            error_message: None,
            model: None,
            provider: None,
            api: None,
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: None,
            deferred: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: now_ms(),
            duration_ms: None,
            details: None,
            citations: None,
            entry_id: None,
        }
    }
    /// 拼接所有文本块的内容（忽略思考与工具调用块）。
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
    /// 拼接所有思考块的内容（换行分隔）；无思考块时为空串。
    pub fn thinking(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    /// 收集内容里的工具调用块（保持原有顺序）。
    pub fn tool_calls(&self) -> Vec<&ContentBlock> {
        self.content
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
            .collect()
    }
}

/// 协议类型：openai-completions / openai-responses / anthropic-messages / google-generative-ai。
fn default_api() -> String {
    "openai-completions".to_string()
}

/// 按模型 provider 合并的额外请求头：
/// - 模型目录（静态基线 + models-store.json 动态覆盖）模型条目 headers 字段的合并视图；
///   github-copilot 的 IDE 网关头即由此注入。
/// - models.json 配置的 provider 级 / 模型级 headers（值解析后）。
/// - models.json authHeader → `Authorization: Bearer <api_key>`。
///
///   opencode/opencode-go（baseUrl 为 opencode.ai 的 Console Go）请求需要带
///   会话标识头用于路由与提示缓存（见 https://opencode.ai/docs/go）。
///   会话 id 来自 Agent 会话（agent_session 构建 model 时写入 ModelConfig.session_id）。
pub(crate) fn provider_session_headers(model: &ModelConfig) -> Vec<(String, String)> {
    let is_opencode = model.provider == "opencode"
        || model.provider == "opencode-go"
        || model.base_url.contains("opencode.ai");

    if !is_opencode {
        return Vec::new();
    }

    let mut out = Vec::new();
    if let Some(sid) = &model.session_id
        && !sid.is_empty()
    {
        out.push(("x-opencode-session".into(), sid.clone()));
        out.push(("x-opencode-client".into(), APP_NAME.into()));
    }
    out
}

/// 会话亲和请求头：按协议与能力开关决定发哪些头，而不是不加区分地发。
///
/// 格式：
/// - `openrouter`（模型声明或 provider/baseUrl 命中 openrouter）→ `x-session-id`；
/// - `anthropic-messages` → `x-session-affinity`；
/// - `openai` → `session_id`，并补 `x-client-request-id`（completions 再补 `x-session-affinity`）；
/// - `openai-nosession` → 只发 `x-client-request-id`（responses）/ `x-client-request-id` +
///   `x-session-affinity`（completions），**不带** `session_id`。
///
/// 开关（`compat.sendSessionAffinityHeaders`，缺省值为「是否 openrouter」）：
/// - `anthropic-messages` / `openai-completions`：关着就一个头也不发；
/// - `openai-responses`：只要有会话 id 就发。
///
/// 无会话 id / 空 id / 其他协议（图片、分类器、google 系）返回空。
pub(crate) fn session_affinity_headers(model: &ModelConfig) -> Vec<(&'static str, String)> {
    let Some(sid) = model.session_id.as_deref().filter(|s| !s.is_empty()) else {
        return Vec::new();
    };

    let is_openrouter = model.provider == "openrouter" || model.base_url.contains("openrouter.ai");
    let format = model
        .session_affinity_format
        .as_deref()
        .unwrap_or(if is_openrouter {
            "openrouter"
        } else {
            "openai"
        });

    let enabled = match model.api.as_str() {
        "anthropic-messages" | "openai-completions" => {
            model.send_session_affinity_headers.unwrap_or(is_openrouter)
        }
        "openai-responses" => true,
        _ => return Vec::new(),
    };
    if !enabled {
        return Vec::new();
    }

    if format == "openrouter" {
        return vec![("x-session-id", sid.to_string())];
    }

    // anthropic-messages 不参与 `session_id` / `x-client-request-id` 组合，只用 `x-session-affinity`
    if model.api == "anthropic-messages" {
        return vec![("x-session-affinity", sid.to_string())];
    }

    let mut out = Vec::new();
    if format == "openai" {
        out.push(("session_id", sid.to_string()));
    }
    out.push(("x-client-request-id", sid.to_string()));

    if model.api == "openai-completions" {
        out.push(("x-session-affinity", sid.to_string()));
    }
    out
}

/// 组装该模型请求要带的额外请求头（目录基线 + opencode 会话头 + models.json 配置），
/// 后写入的覆盖先写入的；非法 header 名/值直接跳过。
pub(crate) fn provider_extra_headers(model: &ModelConfig) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (k, v) in model_resolver::provider_headers(&model.provider) {
        if let (Ok(k), Ok(v)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(&v),
        ) {
            headers.insert(k, v);
        }
    }

    // 会话亲和头（按协议与 compat 格式规则，见 session_affinity_headers）
    for (k, v) in session_affinity_headers(model) {
        if let (Ok(k), Ok(v)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(&v),
        ) {
            headers.insert(k, v);
        }
    }

    // opencode 会话标识头（该 provider 的目录基线）：所有协议共用（models.json 配置仍可覆盖）
    for (k, v) in provider_session_headers(model) {
        if let (Ok(k), Ok(v)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(&v),
        ) {
            headers.insert(k, v);
        }
    }

    // 使用model.json配置覆盖
    for (k, v) in model_config::configured_request_headers(&model.provider, &model.model_id) {
        if let (Ok(k), Ok(v)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(&v),
        ) {
            headers.insert(k, v);
        }
    }

    if model.auth_header
        && !model.api_key.is_empty()
        && let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", model.api_key))
    {
        headers.insert(reqwest::header::AUTHORIZATION, v);
    }

    headers
}

/// 非流式简单调用：用于压缩摘要等一次性请求。
/// 实现为「无事件回调的流式请求」，四协议统一。
pub async fn simple_completion(
    model: &ModelConfig,
    system_prompt: &str,
    user_prompt: &str,
    max_tokens: Option<u32>,
) -> Result<(String, Usage)> {
    let user_msg = AgentMessage::user_text(user_prompt);
    let result = stream_chat(
        model,
        &[user_msg],
        system_prompt,
        &[],
        None,
        None,
        max_tokens,
        None,
        None,
        None,
    )
    .await;

    // stream_chat 永不 Err：错误编码为 stopReason=error 的消息，此处还原为 Err 供调用方处理
    if result.message.stop_reason.as_deref() == Some("error") {
        return Err(Error::msg(
            result
                .error_message
                .clone()
                .unwrap_or_else(|| "stream error".to_string()),
        ));
    }

    // 截断：输出达到 token 上限，结果不完整，拒绝持久化（stopReason === "length" 时摘要作废）。
    // 适用调用方为 compaction / branch summary 等一次性摘要生成。
    if result.message.stop_reason.as_deref() == Some("length") {
        return Err(Error::msg(
            "summarization failed: generation hit the token cap and the summary is incomplete",
        ));
    }
    Ok((result.message.text(), result.usage.unwrap_or_default()))
}

/// 图片生成入口（一次性非流式）：按 [`ModelConfig::api`] 路由到图片协议实现。
///
/// 分发顺序：**扩展经 [`api_impls`] 注册的实现** → 内置实现（`openrouter-images`）。
/// **失败永不抛错**——模型类型不对、协议未实现、缺凭据、网络/HTTP/解析失败，
/// 都返回 `stop_reason == "error"` + `error_message` 的结果。
/// 图片模型取自目录的 `type: image` 条目（如 openrouter 的 FLUX / gemini-image），
/// 或扩展注册的 image 条目；可用 [`crate::core::model_resolver::find_model_of_type`] 按类型取到。
pub async fn generate_images(model: &ModelConfig, input: &[ImageContent]) -> AssistantImages {
    if model.model_type != ModelType::Image {
        return images::error_result(
            model,
            format!(
                "model {} is not an image model (type: {}, provider: {})",
                model.model_id,
                model.model_type.as_str(),
                model.provider
            ),
        );
    }

    let result = if let Some(implementation) = api_impls::image_impl(&model.api) {
        implementation.generate(model, input).await
    } else {
        match model.api.as_str() {
            "openrouter-images" => images::generate_openrouter_images(model, input).await,
            other => Err(Error::msg(format!(
                "provider {} does not support image generation (api: {})",
                model.provider, other
            ))),
        }
    };

    match result {
        Ok(images) => images,
        Err(err) => images::error_result(model, err.to_string()),
    }
}

/// 分类器调用入口（一次性非流式）：按 [`ModelConfig::api`] 路由到分类协议实现。
///
/// 分发顺序：**扩展经 [`api_impls`] 注册的实现** → 内置实现（`typesafe-system-one`）。
/// **失败永不抛错**——模型类型不对、协议未实现、缺凭据、网络/HTTP/解析失败、
/// 答案格式不对，都返回 `stop_reason == "error"` + `error_message` 的结果
/// （答案格式不对但请求已发出去的情况下，`usage` 仍会带上）。
/// 分类器模型取自目录的 `type: classifier` 条目（如 `jev-latest`），
/// 或扩展注册的 classifier 条目；可用 [`crate::core::model_resolver::find_model_of_type`] 按类型取到。
pub async fn classify(model: &ModelConfig, context: &ClassifierContext) -> ClassifierResult {
    if model.model_type != ModelType::Classifier {
        return classifier::error_result(
            model,
            format!(
                "model {} is not a classifier model (type: {}, provider: {})",
                model.model_id,
                model.model_type.as_str(),
                model.provider
            ),
        );
    }

    if let Some(implementation) = api_impls::classifier_impl(&model.api) {
        return match implementation.classify(model, context).await {
            Ok(result) => result,
            Err(err) => classifier::error_result(model, err.to_string()),
        };
    }

    match model.api.as_str() {
        "typesafe-system-one" => classifier::classify_typesafe_system_one(model, context).await,
        other => classifier::error_result(
            model,
            format!(
                "provider {} does not support classification (api: {})",
                model.provider, other
            ),
        ),
    }
}

/// 一次流式请求的最终结果：完整消息、token 用量与可选的错误信息。
#[derive(Debug, Clone)]
pub struct StreamResult {
    /// 本次流式请求组装出的完整 assistant 消息。
    pub message: AgentMessage,
    /// token 用量；供应商未返回用量或流提前失败时为 None。
    pub usage: Option<Usage>,
    /// 流式过程中的错误描述；正常结束时为 None。
    pub error_message: Option<String>,
}

/// 一次请求的费用明细：输入、输出、缓存读写及合计金额。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    /// 未命中缓存的输入 token 费用，单位美元。
    pub input: f64,
    /// 输出 token 费用，单位美元。
    pub output: f64,
    /// 缓存读取 token 费用，单位美元。
    pub cache_read: f64,
    /// 缓存写入 token 费用，单位美元（1h 缓存按加价计价）。
    pub cache_write: f64,
    /// 上述四项之和，单位美元。
    pub total: f64,
}

/// 一次请求的 token 用量统计：输入输出、缓存读写、推理 token 与费用。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// 未命中缓存的输入 prompt token 数。
    pub input: u32,
    /// 模型生成的输出 token 数。
    pub output: u32,
    /// 命中提示缓存的输入 token 数。
    pub cache_read: u32,
    /// 写入提示缓存的 token 数（含 1h 缓存部分）。
    pub cache_write: u32,
    /// 其中 1 小时有效期缓存的 token 数；无 1h 缓存或供应商不区分时为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<u32>,
    /// 推理/思考 token 数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u32>,
    /// 输入、输出与缓存读写 token 的合计。
    pub total_tokens: u32,
    /// 按模型定价与缓存策略算出的本次费用明细。
    #[serde(default)]
    pub cost: Cost,
}

/// 在发送点应用 onPayload 钩子：把请求体解析为 Value、交给钩子修改、回写序列化。
pub(crate) fn apply_payload_hook(body: &mut String, hook: Option<&PayloadHook>) {
    if let Some(h) = hook
        && let Ok(mut g) = h.lock()
        && let Ok(mut v) = serde_json::from_str::<Value>(body)
    {
        let f: &mut (dyn FnMut(&mut Value) + Send) = g.as_mut();
        f(&mut v);
        if let Ok(s) = serde_json::to_string(&v) {
            *body = s;
        }
    }
}

/// 把按 thinking level 解析出的采样参数写进请求体。
///
/// `level_params` 来自 [`ModelConfig::sampling_params_for_level`]，
/// 语义：同名键一律覆盖请求体里已有的值（模型目录 `samplingParams` 的「只补未设键」
/// 行为在各自协议实现里单独处理，不受这里影响）。
/// `settings.json` 的 `temperature` 是用户级覆盖、优先级高于模型目录，
/// 因此它存在时 `temperature` 键不被级别覆盖。传入 `None`（该级别没有覆盖项）时不做任何事。
pub(crate) fn apply_level_sampling_params(
    body: &mut serde_json::Map<String, Value>,
    level_params: Option<&Value>,
) {
    let Some(obj) = level_params.and_then(|v| v.as_object()) else {
        return;
    };

    let settings_temperature = settings_manager::read_settings_temperature().is_some();
    for (k, v) in obj {
        if k == "temperature" && settings_temperature {
            continue;
        }
        body.insert(k.clone(), v.clone());
    }
}

// 协议内部使用的“原始事件”——不含 partial 快照。
// 供 5 个协议在解析 SSE/WS 时直接 emit；
// 由 [`PartialTranslator`] 翻译成带 partial 的 [`StreamEvent`] 交给 agent_session。
/// 各协议解析 SSE/WS 时直接产出的原始增量事件，不含 partial 快照。
#[derive(Debug, Clone)]
pub enum RawStreamEvent {
    /// 文本内容块开始，此后该块陆续收到文本增量。
    TextStart {
        /// 内容块在消息 content 数组中的下标，同一块的各事件保持一致。
        content_index: usize,
    },
    /// 文本内容块的增量片段。
    TextDelta {
        /// 内容块在消息 content 数组中的下标，同一块的各事件保持一致。
        content_index: usize,
        /// 本次新增的文本片段，需追加到该块已有文本之后。
        delta: String,
    },
    /// 文本内容块结束，携带协议给出的完整文本。
    TextEnd {
        /// 内容块在消息 content 数组中的下标，同一块的各事件保持一致。
        content_index: usize,
        /// 该文本块累计出的完整文本，用于覆盖增量拼接结果。
        content: String,
    },
    /// 思考/推理内容块开始，结构同文本块。
    ThinkingStart {
        /// 内容块在消息 content 数组中的下标，同一块的各事件保持一致。
        content_index: usize,
    },
    /// 思考内容块的增量片段。
    ThinkingDelta {
        /// 内容块在消息 content 数组中的下标，同一块的各事件保持一致。
        content_index: usize,
        /// 本次新增的思考片段，需追加到该块已有内容之后。
        delta: String,
    },
    /// 思考内容块结束，携带协议给出的完整推理文本。
    ThinkingEnd {
        /// 内容块在消息 content 数组中的下标，同一块的各事件保持一致。
        content_index: usize,
        /// 该思考块累计出的完整文本。
        content: String,
    },
    /// 工具调用块开始，参数随后以 JSON 增量片段到达。
    ToolCallStart {
        /// 内容块在消息 content 数组中的下标，同一块的各事件保持一致。
        content_index: usize,
    },
    /// 工具调用参数的 JSON 增量片段。
    ToolCallDelta {
        /// 内容块在消息 content 数组中的下标，同一块的各事件保持一致。
        content_index: usize,
        /// 本次新增的参数 JSON 字符串片段，拼接后再整体解析。
        delta: String,
    },
    /// 工具调用块结束，携带已解析完整的调用信息。
    ToolCallEnd {
        /// 内容块在消息 content 数组中的下标，同一块的各事件保持一致。
        content_index: usize,
        /// 提供方为该次调用分配的唯一 id，回传结果时需原样带上。
        id: String,
        /// 被调用工具的名称。
        name: String,
        /// 已解析为 JSON 对象的调用参数。
        arguments: Value,
    },
}

// 流式事件（agent_session 消费）。
// 流事件集：每个增量事件都携带 `partial`（当前 assistant 消息的完整快照），
// 并新增 `Start`（流开始时的初始快照）。`Done` 不在此枚举——stream_chat 的返回值即最终消息。
/// 交给 agent_session 消费的流式事件，每个增量事件都附带当前 assistant 消息快照。
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// 流开始，携带尚未累积任何内容的初始 assistant 消息快照。
    Start {
        /// 当前 assistant 消息的完整快照，每个流事件都会附带。
        partial: AgentMessage,
    },
    /// 模型开始输出一段思维链内容块。
    ThinkingStart {
        /// provider 分配的内容块序号，用于定位消息中对应的块。
        content_index: usize,
        /// 当前 assistant 消息的完整快照，每个流事件都会附带。
        partial: AgentMessage,
    },
    /// 思维链内容块收到一段增量文本。
    ThinkingDelta {
        /// provider 分配的内容块序号，用于定位消息中对应的块。
        content_index: usize,
        /// 本次新增的文本片段，需追加到对应内容块。
        delta: String,
        /// 当前 assistant 消息的完整快照，每个流事件都会附带。
        partial: AgentMessage,
    },
    /// 思维链内容块输出结束，content 为完整文本。
    ThinkingEnd {
        /// provider 分配的内容块序号，用于定位消息中对应的块。
        content_index: usize,
        /// 该内容块结束时的完整文本，覆盖增量累积结果。
        content: String,
        /// 当前 assistant 消息的完整快照，每个流事件都会附带。
        partial: AgentMessage,
    },
    /// 模型开始输出一段可见文本内容块。
    TextStart {
        /// provider 分配的内容块序号，用于定位消息中对应的块。
        content_index: usize,
        /// 当前 assistant 消息的完整快照，每个流事件都会附带。
        partial: AgentMessage,
    },
    /// 可见文本内容块收到一段增量文本。
    TextDelta {
        /// provider 分配的内容块序号，用于定位消息中对应的块。
        content_index: usize,
        /// 本次新增的文本片段，需追加到对应内容块。
        delta: String,
        /// 当前 assistant 消息的完整快照，每个流事件都会附带。
        partial: AgentMessage,
    },
    /// 可见文本内容块输出结束，content 为完整文本。
    TextEnd {
        /// provider 分配的内容块序号，用于定位消息中对应的块。
        content_index: usize,
        /// 该内容块结束时的完整文本，覆盖增量累积结果。
        content: String,
        /// 当前 assistant 消息的完整快照，每个流事件都会附带。
        partial: AgentMessage,
    },
    /// 模型开始输出一个工具调用内容块。
    ToolCallStart {
        /// provider 分配的内容块序号，用于定位消息中对应的块。
        content_index: usize,
        /// 当前 assistant 消息的完整快照，每个流事件都会附带。
        partial: AgentMessage,
    },
    /// 工具调用的参数 JSON 收到一段增量片段。
    ToolCallDelta {
        /// provider 分配的内容块序号，用于定位消息中对应的块。
        content_index: usize,
        /// 本次新增的参数 JSON 片段，拼接后解析为调用参数。
        delta: String,
        /// 当前 assistant 消息的完整快照，每个流事件都会附带。
        partial: AgentMessage,
    },
    /// 工具调用输出结束，tool_call 为完整的调用块。
    ToolCallEnd {
        /// provider 分配的内容块序号，用于定位消息中对应的块。
        content_index: usize,
        /// 组装完成的工具调用块，含 id、名称与参数。
        tool_call: ContentBlock,
        /// 当前 assistant 消息的完整快照，每个流事件都会附带。
        partial: AgentMessage,
    },
}

impl StreamEvent {
    /// 当前 partial 快照（每个流事件都携带 assistant 消息的增量快照）
    pub fn partial(&self) -> Option<&AgentMessage> {
        match self {
            StreamEvent::Start { partial }
            | StreamEvent::ThinkingStart { partial, .. }
            | StreamEvent::ThinkingDelta { partial, .. }
            | StreamEvent::ThinkingEnd { partial, .. }
            | StreamEvent::TextStart { partial, .. }
            | StreamEvent::TextDelta { partial, .. }
            | StreamEvent::TextEnd { partial, .. }
            | StreamEvent::ToolCallStart { partial, .. }
            | StreamEvent::ToolCallDelta { partial, .. }
            | StreamEvent::ToolCallEnd { partial, .. } => Some(partial),
        }
    }
}

/// 把协议层的 [`RawStreamEvent`] 序列翻译成带 partial 快照的 [`StreamEvent`]。
///
/// 维护一份按 content 顺序（== content_index 顺序）增量构建的 assistant 消息快照，
/// 每个增量事件都把该快照作为 `partial` 附加后转发——`message_update` 携带完整快照。
pub(crate) struct PartialTranslator<'a> {
    sink: &'a mut (dyn FnMut(StreamEvent) + Send),
    content: Vec<ContentBlock>,
    index_pos: HashMap<usize, usize>,
    pending_tool_args: HashMap<usize, String>,
    started: bool,
    api: String,
    provider: String,
    model: String,
}

impl<'a> PartialTranslator<'a> {
    /// 建一个翻译器：绑定事件 sink，并从模型配置里记下 api / provider / model 供快照使用。
    fn new(sink: &'a mut (dyn FnMut(StreamEvent) + Send), model: &ModelConfig) -> Self {
        Self {
            sink,
            content: Vec::new(),
            index_pos: HashMap::new(),
            pending_tool_args: HashMap::new(),
            started: false,
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.model_id.clone(),
        }
    }

    /// 用当前已累积的内容块构造一份 assistant 消息快照（每个流事件都附带的 `partial`）。
    fn snapshot(&self) -> AgentMessage {
        let mut m = AgentMessage::user_text("");
        m.role = "assistant".to_string();
        m.content = self.content.clone();
        m.api = Some(self.api.clone());
        m.provider = Some(self.provider.clone());
        m.model = Some(self.model.clone());
        m
    }

    /// 先取当前快照，再交给 `ev_builder` 构造事件并写出到 sink。
    fn emit(&mut self, ev_builder: impl FnOnce(AgentMessage) -> StreamEvent) {
        let partial = self.snapshot();
        (self.sink)(ev_builder(partial));
    }

    /// 消费一个协议层原始事件：按 content_index 累积到快照并转发对应的 [`StreamEvent`]。
    /// 首个事件之前会先补发一次 `Start`。
    fn push(&mut self, ev: RawStreamEvent) {
        if !self.started {
            self.started = true;
            self.emit(|p| StreamEvent::Start { partial: p });
        }

        match ev {
            RawStreamEvent::TextStart { content_index } => {
                self.index_pos.insert(content_index, self.content.len());
                self.content.push(ContentBlock::Text {
                    text: String::new(),
                    text_signature: None,
                });
                self.emit(|p| StreamEvent::TextStart {
                    content_index,
                    partial: p,
                });
            }
            RawStreamEvent::TextDelta {
                content_index,
                delta,
            } => {
                if let Some(&pos) = self.index_pos.get(&content_index)
                    && let ContentBlock::Text { text, .. } = &mut self.content[pos]
                {
                    text.push_str(&delta);
                }

                self.emit(|p| StreamEvent::TextDelta {
                    content_index,
                    delta,
                    partial: p,
                });
            }
            RawStreamEvent::TextEnd {
                content_index,
                content,
            } => {
                if let Some(&pos) = self.index_pos.get(&content_index) {
                    self.content[pos] = ContentBlock::Text {
                        text: content.clone(),
                        text_signature: None,
                    };
                }
                self.emit(|p| StreamEvent::TextEnd {
                    content_index,
                    content,
                    partial: p,
                });
            }
            RawStreamEvent::ThinkingStart { content_index } => {
                self.index_pos.insert(content_index, self.content.len());
                self.content.push(ContentBlock::Thinking {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: None,
                });
                self.emit(|p| StreamEvent::ThinkingStart {
                    content_index,
                    partial: p,
                });
            }
            RawStreamEvent::ThinkingDelta {
                content_index,
                delta,
            } => {
                if let Some(&pos) = self.index_pos.get(&content_index)
                    && let ContentBlock::Thinking { thinking, .. } = &mut self.content[pos]
                {
                    thinking.push_str(&delta);
                }

                self.emit(|p| StreamEvent::ThinkingDelta {
                    content_index,
                    delta,
                    partial: p,
                });
            }
            RawStreamEvent::ThinkingEnd {
                content_index,
                content,
            } => {
                if let Some(&pos) = self.index_pos.get(&content_index) {
                    self.content[pos] = ContentBlock::Thinking {
                        thinking: content.clone(),
                        thinking_signature: None,
                        redacted: None,
                    };
                }
                self.emit(|p| StreamEvent::ThinkingEnd {
                    content_index,
                    content,
                    partial: p,
                });
            }
            RawStreamEvent::ToolCallStart { content_index } => {
                self.index_pos.insert(content_index, self.content.len());
                self.content.push(ContentBlock::ToolCall {
                    id: String::new(),
                    name: String::new(),
                    arguments: Value::Null,
                    thought_signature: None,
                    namespace: None,
                });
                self.emit(|p| StreamEvent::ToolCallStart {
                    content_index,
                    partial: p,
                });
            }
            RawStreamEvent::ToolCallDelta {
                content_index,
                delta,
            } => {
                self.pending_tool_args
                    .entry(content_index)
                    .or_default()
                    .push_str(&delta);
                if let Some(&pos) = self.index_pos.get(&content_index)
                    && let ContentBlock::ToolCall { arguments, .. } = &mut self.content[pos]
                    && let Ok(v) = serde_json::from_str::<Value>(
                        self.pending_tool_args
                            .get(&content_index)
                            .unwrap_or(&String::new()),
                    )
                {
                    *arguments = v;
                }

                self.emit(|p| StreamEvent::ToolCallDelta {
                    content_index,
                    delta,
                    partial: p,
                });
            }
            RawStreamEvent::ToolCallEnd {
                content_index,
                id,
                name,
                arguments,
            } => {
                if let Some(&pos) = self.index_pos.get(&content_index) {
                    self.content[pos] = ContentBlock::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                        thought_signature: None,
                        namespace: None,
                    };
                }
                let tool_call = ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                    thought_signature: None,
                    namespace: None,
                };
                self.emit(|p| StreamEvent::ToolCallEnd {
                    content_index,
                    tool_call,
                    partial: p,
                });
            }
        }
    }
}

// 统一入口：按 ModelConfig.api 路由到各协议实现
/// 统一流式入口：按 [`ModelConfig::api`] 路由到对应协议实现（completions / responses / anthropic / google）。
///
/// 纯流契约：请求/模型/运行时失败不抛错，而是把失败编码进流（先发 Start 的失败快照），
/// 最终返回 stopReason=error + errorMessage 的 assistant 消息。
/// 该 `api` 是否是内置的 chat 协议实现（需要 provider 凭据的四个协议）。
///
/// 用于缺失凭据时区分「provider 未配置」与「api 类型不支持」：`prux-virtual` 这类路由哨兵
/// 不属于协议实现，其错误应是 `unsupported api type`。
fn is_chat_protocol_api(api: &str) -> bool {
    matches!(
        api,
        "openai-completions" | "openai-responses" | "anthropic-messages" | "google-generative-ai"
    )
}

#[allow(clippy::too_many_arguments)]
pub async fn stream_chat(
    model: &ModelConfig,
    messages: &[AgentMessage],
    system_prompt: &str,
    tools: &[(String, String, Value)],
    reasoning_effort: Option<&str>,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    mut on_event: Option<&mut (dyn FnMut(StreamEvent) + Send)>,
    on_payload: Option<PayloadHook>,
    tool_choice: Option<&str>,
) -> StreamResult {
    // 纯流契约：对请求/模型/运行时失败不抛错，
    // 而是把失败编码进流（发 Start(partial=失败消息) 供 agent_session collector 发 message_start），
    // 并返回 stopReason=error + errorMessage 的 assistant 消息。
    let inner = if model.model_type != ModelType::Chat {
        // 目录里的图片 / 分类器条目没有 chat 协议实现：在分发前拦住，
        // 给出比“unsupported api type: openrouter-images”更直白的错误。
        Err(Error::msg(format!(
            "model {} is not a chat model (type: {}, provider: {})",
            model.model_id,
            model.model_type.as_str(),
            model.provider
        )))
    } else if model.api_key.trim().is_empty() && is_chat_protocol_api(&model.api) {
        // 解析不到任何凭据 = provider 未配置：包括 models.json 声明了 apiKey 但值不可解析（不回退 env）的情形。
        Err(Error::msg(format!(
            "Provider is not configured: {}",
            model.provider
        )))
    } else {
        // 把外部（带 partial 的事件）sink 包装成 translator，供协议层以 RawStreamEvent 消费。
        let mut translator_owner: Option<PartialTranslator>;
        let mut raw: Option<&mut PartialTranslator> = None;
        if let Some(sink) = on_event.as_deref_mut() {
            translator_owner = Some(PartialTranslator::new(sink, model));
            raw = translator_owner.as_mut();
        }

        match model.api.as_str() {
            "openai-completions" => {
                completions::stream(
                    model,
                    messages,
                    system_prompt,
                    tools,
                    reasoning_effort,
                    temperature,
                    max_tokens,
                    raw,
                    on_payload,
                    tool_choice,
                )
                .await
            }
            "openai-responses" => {
                responses::stream(
                    model,
                    messages,
                    system_prompt,
                    tools,
                    reasoning_effort,
                    temperature,
                    max_tokens,
                    raw,
                    on_payload,
                    tool_choice,
                )
                .await
            }
            "anthropic-messages" => {
                anthropic::stream(
                    model,
                    messages,
                    system_prompt,
                    tools,
                    reasoning_effort,
                    temperature,
                    max_tokens,
                    raw,
                    on_payload,
                    tool_choice,
                )
                .await
            }
            "google-generative-ai" => {
                google::stream(
                    model,
                    messages,
                    system_prompt,
                    tools,
                    reasoning_effort,
                    temperature,
                    max_tokens,
                    raw,
                    on_payload,
                )
                .await
            }
            other => Err(Error::UnsupportedApi {
                api: format!(
                    "unsupported api type: {} (provider: {})",
                    other, model.provider
                ),
            }),
        }
    };

    match inner {
        Ok(r) => r,
        Err(e) => {
            let failure = failure_message(model, &e);
            if let Some(sink) = on_event {
                sink(StreamEvent::Start {
                    partial: failure.clone(),
                });
            }
            StreamResult {
                message: failure,
                usage: None,
                error_message: Some(e.display_full()),
            }
        }
    }
}

/// 构造 provider 失败消息（StreamFn 失败契约：stopReason=error + errorMessage）。
pub(crate) fn failure_message(model: &ModelConfig, e: &Error) -> AgentMessage {
    AgentMessage {
        role: "assistant".to_string(),
        thinking_level: None,
        content: vec![ContentBlock::Text {
            text: String::new(),
            text_signature: None,
        }],
        tool_call_id: None,
        tool_name: None,
        is_error: false,
        stop_reason: Some("error".to_string()),
        error_message: Some(e.display_full()),
        model: Some(model.model_id.clone()),
        provider: Some(model.provider.clone()),
        api: Some(model.api.clone()),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: None,
        deferred: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: now_ms(),
        duration_ms: None,
        details: None,
        citations: None,
        entry_id: None,
    }
}

/// 累计流式响应已收到的字节数，超过 [`MAX_STREAM_BYTES`] 时报错中止该流。
///
/// `chunk_len` 是本次读到的分块字节数。返回 `Err` 表示总量已越界——继续读下去只会把内存耗尽，
/// 由调用方立刻结束本次请求。
pub(crate) fn track_stream_bytes(total: &mut usize, chunk_len: usize) -> Result<()> {
    *total += chunk_len;
    if *total > MAX_STREAM_BYTES {
        return Err(Error::msg(format!(
            "provider stream aborted: received over {MAX_STREAM_BYTES} bytes without finishing \
             (the connection kept sending data; refusing to grow memory further)"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 恰好到上限应放行，多 1 字节就必须中止：这是防 OOM 的闸门。
    #[test]
    fn track_stream_bytes_enforces_cap() {
        let mut total = 0usize;
        assert!(track_stream_bytes(&mut total, MAX_STREAM_BYTES).is_ok());
        let err = track_stream_bytes(&mut total, 1).expect_err("越界必须报错");
        assert!(err.to_string().contains("aborted"), "got: {err}");
    }

    fn model(supports_image: bool, cost: Option<Value>) -> ModelConfig {
        ModelConfig {
            model_type: Default::default(),
            image_resize: crate::utils::image::ImageResizeLimits::default(),
            allowed_fallback_models: Vec::new(),
            supports_usage_in_streaming: true,
            supports_store: true,
            supports_finish_reason: true,
            requires_assistant_after_tool_result: false,
            supports_strict_mode: false,
            send_session_affinity_headers: None,
            session_affinity_format: None,
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
            max_tokens_field: "max_tokens".into(),
            cost,
            session_id: None,
            max_retry_delay_ms: None,
            sampling_params: None,
            sampling_params_by_thinking_level: None,
            provider_routing: None,
            thinking_budgets: None,
        }
    }

    /// 构造带 thinkingLevelMap 的推理模型（数据取自实际 models-store.json）
    fn thinking_model(levels: &[(&str, Option<&str>)]) -> ModelConfig {
        let mut m = model(false, None);
        m.reasoning = true;
        m.thinking_level_map = Some(
            levels
                .iter()
                .map(|(k, v)| (k.to_string(), v.map(|s| s.to_string())))
                .collect(),
        );
        m
    }

    #[test]
    fn provider_session_headers_only_for_opencode() {
        let mut m = model(false, None);
        // 非 opencode：无会话头
        assert!(provider_session_headers(&m).is_empty());
        // opencode-go 但无会话 id：无会话头
        m.provider = "opencode-go".into();
        m.base_url = "https://opencode.ai/zen/go".into();
        assert!(provider_session_headers(&m).is_empty());
        // 有会话 id：发 x-opencode-session + x-opencode-client
        m.session_id = Some("sess-1".into());
        let h = provider_session_headers(&m);
        assert_eq!(
            h.iter()
                .find(|(k, _)| k == "x-opencode-session")
                .map(|(_, v)| v.as_str()),
            Some("sess-1")
        );
        assert_eq!(
            h.iter()
                .find(|(k, _)| k == "x-opencode-client")
                .map(|(_, v)| v.as_str()),
            Some("prux")
        );
    }

    /// pi 1.0.2：会话亲和头按「协议 + compat 开关 + 格式」决定，不是不分协议一律发 x-session-affinity。
    #[test]
    fn session_affinity_headers_follow_api_and_compat() {
        fn names(m: &ModelConfig) -> Vec<String> {
            session_affinity_headers(m)
                .into_iter()
                .map(|(k, _)| k.to_string())
                .collect()
        }

        // 无会话 id：一个头也不发
        let m = model(false, None);
        assert!(names(&m).is_empty());

        // openai-completions 且未声明 sendSessionAffinityHeaders：默认关（非 openrouter）
        let mut m = model(false, None);
        m.session_id = Some("sess-1".into());
        assert!(names(&m).is_empty());

        // 显式开启：session_id + x-client-request-id + x-session-affinity
        m.send_session_affinity_headers = Some(true);
        assert_eq!(
            names(&m),
            vec!["session_id", "x-client-request-id", "x-session-affinity"]
        );

        // openrouter：只发 x-session-id（开关默认开）
        let mut m = model(false, None);
        m.provider = "openrouter".into();
        m.session_id = Some("sess-1".into());
        assert_eq!(names(&m), vec!["x-session-id"]);

        // openai-responses + openai-nosession（opencode 目录）：只发 x-client-request-id
        let mut m = model(false, None);
        m.api = "openai-responses".into();
        m.provider = "opencode".into();
        m.session_id = Some("sess-1".into());
        m.session_affinity_format = Some("openai-nosession".into());
        assert_eq!(names(&m), vec!["x-client-request-id"]);

        // anthropic-messages：同样受开关限制；格式 openrouter 时换 x-session-id
        let mut m = model(false, None);
        m.api = "anthropic-messages".into();
        m.provider = "anthropic".into();
        m.session_id = Some("sess-1".into());
        assert!(names(&m).is_empty());
        m.send_session_affinity_headers = Some(true);
        assert_eq!(names(&m), vec!["x-session-affinity"]);
        m.session_affinity_format = Some("openrouter".into());
        assert_eq!(names(&m), vec!["x-session-id"]);

        // 其他协议（图片等）：不发亲和头
        let mut m = model(false, None);
        m.api = "google-generative-ai".into();
        m.session_id = Some("sess-1".into());
        assert!(names(&m).is_empty());
    }

    #[test]
    fn assistant_message_records_thinking_level() {
        // assistant 消息携带本轮 thinking 级别（对齐 pi AssistantMessage.thinkingLevel）
        let mut m = AgentMessage::user_text("hi");
        m.role = "assistant".to_string();
        m.thinking_level = Some("high".to_string());
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(
            v.get("thinkingLevel").and_then(|x| x.as_str()),
            Some("high")
        );
        let back: AgentMessage = serde_json::from_value(v).unwrap();
        assert_eq!(back.thinking_level.as_deref(), Some("high"));

        // None 不落盘（旧会话/非 agent 循环的响应）
        let none = AgentMessage::user_text("hi");
        let v = serde_json::to_value(&none).unwrap();
        assert!(v.get("thinkingLevel").is_none());

        // 旧会话缺字段仍可解析（向后兼容）
        let legacy: AgentMessage = serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "content": []
        }))
        .unwrap();
        assert_eq!(legacy.thinking_level, None);
    }

    #[test]
    fn provider_extra_headers_include_opencode_session() {
        let mut m = model(false, None);
        m.provider = "opencode-go".into();
        m.base_url = "https://opencode.ai/zen/go".into();
        m.session_id = Some("sess-1".into());
        let map = provider_extra_headers(&m);
        assert_eq!(
            map.get("x-opencode-session").and_then(|v| v.to_str().ok()),
            Some("sess-1")
        );
        assert_eq!(
            map.get("x-opencode-client").and_then(|v| v.to_str().ok()),
            Some("prux")
        );
        // 非 opencode provider：不受会话影响
        let m2 = model(false, None);
        assert!(provider_extra_headers(&m2).is_empty());
    }

    #[test]
    fn supported_levels_from_thinking_level_map() {
        // deepseek-flash: minimal/medium 显式 null，xhigh 无映射
        let m = thinking_model(&[
            ("minimal", None),
            ("low", Some("low")),
            ("medium", None),
            ("high", Some("high")),
            ("max", Some("max")),
        ]);
        assert_eq!(
            m.supported_thinking_levels(),
            vec!["off", "low", "high", "max"]
        );
        // glm-5.2: 全部 null，仅 high/max 可用（off 也被显式禁用）
        let m = thinking_model(&[
            ("off", None),
            ("minimal", None),
            ("low", None),
            ("medium", None),
            ("high", Some("high")),
            ("xhigh", None),
            ("max", Some("max")),
        ]);
        assert_eq!(m.supported_thinking_levels(), vec!["high", "max"]);
        // kimi-k2.6: minimal/low/medium 显式 null；high 无键 → 默认支持；xhigh/max 无映射 → 不支持
        let m = thinking_model(&[("minimal", None), ("low", None), ("medium", None)]);
        assert_eq!(m.supported_thinking_levels(), vec!["off", "high"]);
        // hy3: off 映射为 "none" 而非 null → off 可用
        let m = thinking_model(&[
            ("off", Some("none")),
            ("minimal", None),
            ("low", Some("low")),
            ("medium", None),
            ("high", Some("high")),
            ("xhigh", None),
            ("max", None),
        ]);
        assert_eq!(m.supported_thinking_levels(), vec!["off", "low", "high"]);
        // 非推理模型只有 off
        let plain = model(false, None);
        assert_eq!(plain.supported_thinking_levels(), vec!["off"]);
    }

    #[test]
    fn clamp_rounds_to_supported_levels() {
        let m = thinking_model(&[
            ("minimal", None),
            ("low", Some("low")),
            ("medium", None),
            ("high", Some("high")),
            ("max", Some("max")),
        ]);
        // 已支持：原样返回
        assert_eq!(m.clamp_thinking_level("high"), "high");
        // 向上取最近（medium → high）
        assert_eq!(m.clamp_thinking_level("medium"), "high");
        // 向上无可用 → 向下取最近（xhigh → max）
        assert_eq!(m.clamp_thinking_level("xhigh"), "max");
        // 未知级别 → 第一个可用
        assert_eq!(m.clamp_thinking_level("bogus"), "off");
        // glm-5.2 式：off/minimal/low/medium 全部不可用，向上升级
        let m = thinking_model(&[
            ("off", None),
            ("minimal", None),
            ("low", None),
            ("medium", None),
            ("high", Some("high")),
            ("max", Some("max")),
        ]);
        assert_eq!(m.clamp_thinking_level("off"), "high");
        assert_eq!(m.clamp_thinking_level("xhigh"), "max");
        // 非推理模型：一切钳制到 off
        let plain = model(false, None);
        assert_eq!(plain.clamp_thinking_level("max"), "off");
    }

    #[test]
    fn dispatch_rejects_unknown_api() {
        let mut m = model(false, None);
        m.api = "bogus-protocol".into();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(simple_completion(&m, "", "hi", None))
            .unwrap_err();
        assert!(err.to_string().contains("bogus-protocol"));
    }
}
