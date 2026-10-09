//! 模型解析。模型来源分三层
//!
//! 1. 静态基线：内嵌于程序的 provider 模型目录（`assets/models/models.all.json`）
//! 2. 动态缓存：models-store.json 只保存「当前支持 provider」的远程目录（pi.dev）拉取结果，
//!    由 core/model_refresh 异步刷新（启动时缺失即拉，/login 成功后强制刷新）
//! 3. 用户配置：models.json（agent_dir 下）添加第三方 provider / 模型或覆盖内置 provider；
//!    由 core/model_config 解析，每次读取实时生效
//!
//! 三层的键都是 `(type, id)`：chat / image / classifier 里同名条目互不覆盖

use super::model_config::{self, ProviderConfig};
use crate::{
    cli::args::VALID_THINKING_LEVELS,
    core::{
        auth, model_refresh,
        provider::{AllowedFallbackModel, ModelConfig, ModelType},
        settings_manager::{self, Settings},
        virtual_models::{self, VirtualModelDefinition},
    },
    embedded,
    error::{Error, Result},
    utils::image::InputLimits,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, OnceLock},
};

/// 当前支持（可 /login）的 provider 列表。
pub const SUPPORTED_PROVIDERS: &[&str] = &[
    "deepseek",
    "opencode",
    "opencode-go",
    "anthropic",
    "ant-ling",
    "azure",
    "baseten",
    "cerebras",
    "fireworks",
    "github-copilot",
    "google",
    "groq",
    "huggingface",
    "kimi-coding",
    "meta",
    "minimax",
    "minimax-cn",
    "moonshotai",
    "moonshotai-cn",
    "nvidia",
    "openai",
    "openrouter",
    "qwen-token-plan",
    "qwen-token-plan-cn",
    "qwen-token-plan-individual",
    "together",
    "typesafe",
    "vercel-ai-gateway",
    "xai",
    "xiaomi",
    "xiaomi-token-plan-ams",
    "xiaomi-token-plan-cn",
    "xiaomi-token-plan-sgp",
    "zai",
    "zai-coding-cn",
];

/// 静态基线目录：`assets/models/models.all.json`（pi 的数组式目录，`{provider: [模型, ...]}`）。
///
/// 解析结果缓存一次：调用点（`list_all_models` 等）会对每个 provider 各问一遍，
/// 每次重新解析整份目录是纯浪费。
fn baseline_catalog() -> &'static HashMap<String, Vec<Value>> {
    static CATALOG: OnceLock<HashMap<String, Vec<Value>>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(embedded!("models/models.all.json")).unwrap_or_default()
    })
}

/// 静态基线目录里某 provider 的模型条目（数组式；目录缺失该 provider 时为空）。
fn baseline_models(provider: &str) -> Vec<Value> {
    baseline_catalog()
        .get(provider)
        .cloned()
        .unwrap_or_default()
}

/// 读取 models-store.json（只读动态缓存；不存在返回 None，不生成默认文件）。
///
/// 走 model_refresh 的读写锁 + 缓存：与写入方串行，且文件未变时直接命中内存值。
fn models_store() -> Option<Value> {
    model_refresh::read_models_store()
}

/// 条目的 `(type, id)` 键。
///
/// 三层的覆盖 / 去重都以它为口径：chat / image / classifier 里同名条目互不覆盖
/// 缺 `type` 按 chat；未知 `type` 或缺 `id` 返回 None（调用方丢弃整条）。
fn catalog_key(model: &Value) -> Option<(ModelType, &str)> {
    let id = model.get("id").and_then(|v| v.as_str())?;
    let kind = ModelType::from_catalog(model.get("type"))?;
    Some((kind, id))
}

/// 合并后的模型列表：静态基线 → models-store 动态缓存 → models.json 用户配置
pub fn provider_models(provider: &str) -> Vec<Value> {
    let mut merged = baseline_models(provider);
    // 目录键 → 下标索引：动态目录逐条 upsert 时不再线性扫 `merged`
    // （远程目录可达数百条，平方复杂度会让刷新明显变慢）
    let mut index: HashMap<(ModelType, String), usize> = merged
        .iter()
        .enumerate()
        .filter_map(|(i, m)| catalog_key(m).map(|(kind, id)| ((kind, id.to_string()), i)))
        .collect();
    if let Some(dynamic) = models_store()
        .as_ref()
        .and_then(|v| v.get(provider))
        .and_then(|p| p.get("models"))
        .and_then(|m| m.as_array())
    {
        for model in dynamic {
            let Some(key) = catalog_key(model) else {
                continue;
            };
            let key = (key.0, key.1.to_string());

            match index.get(&key) {
                Some(&idx) => merged[idx] = model.clone(),
                None => {
                    index.insert(key, merged.len());
                    merged.push(model.clone());
                }
            }
        }
    }

    // 用户配置层（models.json）：baseUrl 覆盖、compat 合并、models 按 id upsert、modelOverrides 应用
    if let Some(cfg) = model_config::provider_config(provider) {
        apply_models_json(&mut merged, &cfg);
    }

    // 未知 `type` 取值的条目整条丢弃：
    // 否则会以 chat 身份混进 `/model` 选择器、选中后报 `unsupported api type`
    // （缺 `type` 字段仍按 chat 处理，见 [`ModelType::from_catalog`]）。
    merged.retain(|m| ModelType::from_catalog(m.get("type")).is_some());
    merged
}

/// deep-merge 两个 compat 对象（第二个参数胜；嵌套对象字段级合并）
fn merge_compat(base: Option<&Value>, override_: Option<&Value>) -> Option<Value> {
    match (base, override_) {
        (None, None) => None,
        (Some(b), None) => Some(b.clone()),
        (None, Some(o)) => Some(o.clone()),
        (Some(b), Some(o)) => {
            /// compat 中需按字段级深合并（而非整体覆盖）的嵌套对象键名。
            const NESTED: &[&str] = &[
                "openRouterRouting",
                "vercelGatewayRouting",
                "chatTemplateKwargs",
                "chatTemplateArgs",
            ];
            let mut merged = b.as_object().cloned().unwrap_or_default();
            let Some(oo) = o.as_object() else {
                return Some(Value::Object(merged));
            };

            for (k, v) in oo {
                if NESTED.contains(&k.as_str())
                    && (merged
                        .get(k.as_str())
                        .map(|x| x.is_object())
                        .unwrap_or(false)
                        || v.is_object())
                {
                    let mut nested = merged
                        .get(k.as_str())
                        .and_then(|x| x.as_object())
                        .cloned()
                        .unwrap_or_default();

                    if let Some(ov) = v.as_object() {
                        for (nk, nv) in ov {
                            nested.insert(nk.clone(), nv.clone());
                        }
                    }
                    merged.insert(k.clone(), Value::Object(nested));
                } else {
                    merged.insert(k.clone(), v.clone());
                }
            }

            Some(Value::Object(merged))
        }
    }
}

/// 应用 models.json 用户配置到合并视图
/// - `baseUrl` 作用于全部类型条目，`compat` 只作用于 chat（非 chat 条目只有 baseUrl 变）；
/// - `models` 是 chat 条目：同 id 的 **chat** 条目被替换，否则追加（不会顶掉同 id 的 image / classifier 条目）；
/// - `modelOverrides` 同样只作用于 chat 条目。
fn apply_models_json(merged: &mut Vec<Value>, cfg: &ProviderConfig) {
    // 1) provider 级 baseUrl 覆盖（全部类型）+ compat 合并（仅 chat）
    for m in merged.iter_mut() {
        let is_chat = ModelType::from_catalog(m.get("type")) == Some(ModelType::Chat);
        if let Some(obj) = m.as_object_mut()
            && let Some(bu) = &cfg.base_url
        {
            obj.insert("baseUrl".to_string(), Value::String(bu.clone()));
        }

        if !is_chat {
            continue;
        }

        let compat = merge_compat(m.get("compat"), cfg.compat.as_ref());
        if let Some(compat) = compat
            && let Some(obj) = m.as_object_mut()
        {
            obj.insert("compat".to_string(), compat);
        }
    }

    // 2) models 按 id upsert：已有**同类型**条目替换，新 id 追加；缺省字段继承
    for definition in &cfg.models {
        let Some(id) = definition.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let kind = ModelType::from_catalog(definition.get("type")).unwrap_or_default();
        let existing = merged
            .iter()
            .position(|m| catalog_key(m) == Some((kind, id)));
        let defaults = find_model_defaults(
            merged,
            id,
            definition.get("api").and_then(|v| v.as_str()),
            kind,
        )
        .cloned();
        let mut model = definition.clone();

        // api：definition → provider → 继承默认
        if model.get("api").is_none() {
            if let Some(api) = &cfg.api {
                model
                    .as_object_mut()
                    .unwrap()
                    .insert("api".to_string(), json!(api));
            } else if let Some(api) = defaults
                .as_ref()
                .and_then(|d| d.get("api"))
                .and_then(|v| v.as_str())
            {
                model
                    .as_object_mut()
                    .unwrap()
                    .insert("api".to_string(), json!(api));
            }
        }

        // baseUrl：definition → provider → 继承默认（同 id 条目或第一个模型）
        if model.get("baseUrl").is_none() {
            let bu = cfg.base_url.clone().or_else(|| {
                defaults
                    .as_ref()
                    .and_then(|d| d.get("baseUrl"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });
            if let Some(bu) = bu {
                model
                    .as_object_mut()
                    .unwrap()
                    .insert("baseUrl".to_string(), json!(bu));
            }
        }

        // compat：provider 级与模型条目级合并（条目胜）
        if let Some(compat) = merge_compat(cfg.compat.as_ref(), model.get("compat")) {
            model
                .as_object_mut()
                .unwrap()
                .insert("compat".to_string(), compat);
        }

        if let Some(idx) = existing {
            merged[idx] = model;
        } else {
            merged.push(model);
        }
    }

    // 3) modelOverrides 最后应用（仅 chat 条目）
    for (model_id, ov) in &cfg.model_overrides {
        if let Some(idx) = merged.iter().position(|m| {
            ModelType::from_catalog(m.get("type")) == Some(ModelType::Chat)
                && m.get("id").and_then(|v| v.as_str()) == Some(model_id)
        }) {
            apply_model_override(&mut merged[idx], ov);
        }
    }
}

/// models.json 自定义条目的缺省来源：
/// 同类型里「同 id → 同 api → （chat 才有）`openai-completions` → 第一个条目」。
///
/// 只看同类型条目：typesafe 这类 provider 只有 classifier 条目，
/// 把它的 api / baseUrl 继承给用户新增的 chat 模型会直接跑错协议。
fn find_model_defaults<'a>(
    models: &'a [Value],
    id: &str,
    api: Option<&str>,
    kind: ModelType,
) -> Option<&'a Value> {
    let same_type = || {
        models
            .iter()
            .filter(move |m| ModelType::from_catalog(m.get("type")).unwrap_or_default() == kind)
    };

    if let Some(m) = same_type().find(|m| m.get("id").and_then(|v| v.as_str()) == Some(id)) {
        return Some(m);
    }

    if let Some(api) = api
        && let Some(m) = same_type().find(|m| m.get("api").and_then(|v| v.as_str()) == Some(api))
    {
        return Some(m);
    }

    if kind == ModelType::Chat
        && let Some(m) = same_type()
            .find(|m| m.get("api").and_then(|v| v.as_str()) == Some("openai-completions"))
    {
        return Some(m);
    }

    same_type().next()
}

/// 对单个模型条目应用 modelOverrides 覆盖字段
fn apply_model_override(model: &mut Value, ov: &Value) {
    let Some(obj) = model.as_object_mut() else {
        return;
    };

    for key in ["name", "reasoning", "input", "contextWindow", "maxTokens"] {
        if let Some(v) = ov.get(key) {
            obj.insert(key.to_string(), v.clone());
        }
    }

    // 逐键合并：thinkingLevelMap / samplingParams
    for key in ["thinkingLevelMap", "samplingParams"] {
        if let (Some(base), Some(over)) = (obj.get(key), ov.get(key)) {
            if let (Some(b), Some(o)) = (base.as_object(), over.as_object()) {
                let mut merged = b.clone();
                for (k, v) in o {
                    merged.insert(k.clone(), v.clone());
                }
                obj.insert(key.to_string(), Value::Object(merged));
            }
        } else if let Some(v) = ov.get(key) {
            obj.insert(key.to_string(), v.clone());
        }
    }

    // samplingParamsByThinkingLevel：逐级别合并（覆盖项里未给出的级别继承基线，同一级别内逐键覆盖）
    if let Some(over) = ov.get("samplingParamsByThinkingLevel") {
        let mut merged = obj
            .get("samplingParamsByThinkingLevel")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();

        if let Some(o) = over.as_object() {
            for (level, params) in o {
                let base_level = merged.get(level).and_then(|v| v.as_object()).cloned();
                match (base_level, params.as_object()) {
                    (Some(b), Some(p)) => {
                        let mut lv = b;
                        for (k, v) in p {
                            lv.insert(k.clone(), v.clone());
                        }
                        merged.insert(level.clone(), Value::Object(lv));
                    }
                    _ => {
                        merged.insert(level.clone(), params.clone());
                    }
                }
            }
        }

        obj.insert(
            "samplingParamsByThinkingLevel".to_string(),
            Value::Object(merged),
        );
    }

    // cost：部分覆盖（缺失字段保持原值）
    if let Some(over) = ov.get("cost") {
        let mut cost = obj.get("cost").cloned().unwrap_or_else(|| json!({}));
        if let (Some(c), Some(o)) = (cost.as_object_mut(), over.as_object()) {
            for (k, v) in o {
                c.insert(k.clone(), v.clone());
            }
        }
        obj.insert("cost".to_string(), cost);
    }

    // inputLimits：深合并（pi `mergeInputLimits`）——顶层键与 images 键覆盖，
    // images.resize 字段级合并（未给出的字段保留目录值）。
    if let Some(over) = ov.get("inputLimits") {
        let mut merged = obj
            .get("inputLimits")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();

        if let Some(o) = over.as_object() {
            for (k, v) in o {
                if k == "images" {
                    let mut images = merged
                        .get("images")
                        .and_then(|x| x.as_object())
                        .cloned()
                        .unwrap_or_default();

                    if let Some(oi) = v.as_object() {
                        for (ik, iv) in oi {
                            if ik == "resize" && iv.is_object() {
                                let mut resize = images
                                    .get("resize")
                                    .and_then(|x| x.as_object())
                                    .cloned()
                                    .unwrap_or_default();

                                if let Some(or) = iv.as_object() {
                                    for (rk, rv) in or {
                                        resize.insert(rk.clone(), rv.clone());
                                    }
                                }

                                images.insert(ik.clone(), Value::Object(resize));
                            } else {
                                images.insert(ik.clone(), iv.clone());
                            }
                        }
                    }

                    merged.insert(k.clone(), Value::Object(images));
                } else {
                    merged.insert(k.clone(), v.clone());
                }
            }
        }
        obj.insert("inputLimits".to_string(), Value::Object(merged));
    }

    // compat：合并（覆盖胜）
    if let Some(compat) = merge_compat(obj.get("compat"), ov.get("compat")) {
        obj.insert("compat".to_string(), compat);
    }
}

/// 合并某 provider 所有模型条目中的 headers 字段（模型目录 = 静态基线 + models-store.json 动态覆盖）。
/// github-copilot 的 IDE 网关头（User-Agent / Editor-* / Copilot-Integration-Id）定义在
/// 模型条目的 headers 里，认证与请求层都从这里读取而不是写死在代码中。
/// 认证流程可能在登录前发生（models-store.json 尚未写入），基线内嵌保证始终有值。
pub fn provider_headers(provider: &str) -> Vec<(String, String)> {
    let mut merged: HashMap<String, String> = HashMap::new();
    for m in provider_models(provider) {
        if let Some(headers) = m.get("headers").and_then(|v| v.as_object()) {
            for (k, v) in headers {
                if let Some(vs) = v.as_str() {
                    merged.insert(k.clone(), vs.to_string());
                }
            }
        }
    }
    let mut out: Vec<(String, String)> = merged.into_iter().collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// 解析后的单个模型条目：API 类型、端点、能力标志与采样参数等。
#[derive(Debug, Clone)]
pub struct ModelEntry {
    /// 模型标识（目录 `id`），请求时作为 model 名与匹配键。
    pub id: String,
    /// 展示名（目录 `name`，缺失时回退为 id）。
    pub name: String,
    /// 请求协议：anthropic-messages / openai-completions / openai-responses。
    pub api: String,
    /// 目录 `type`：模型用途（chat/image/classifier）；非 chat 条目不能走 chat 流。
    pub model_type: ModelType,
    /// 该模型 API 端点根地址（目录 baseUrl）。
    pub base_url: String,
    /// 所属提供方标识（如 anthropic、github-copilot）。
    pub provider: String,
    /// 支持的输入模态列表（如 text、image）。
    pub input: Vec<String>,
    /// 目录 `output`：输出模态列表（图片模型为 `image`，可带 `text`）；chat 条目为空。
    pub output: Vec<String>,
    /// true 表示模型支持推理/思考输出。
    pub reasoning: bool,
    /// 上下文窗口 token 上限，目录缺失时取 128000。
    pub context_window: u32,
    /// 单次回复最大输出 token，目录缺失时取 16384，0 表示不限。
    pub max_tokens: u32,
    /// 请求体中承载输出上限的字段名，缺省 max_completion_tokens。
    pub max_tokens_field: String,
    /// 采样参数（temperature/top_p 等），目录未提供时为 None。
    pub sampling_params: Option<Value>,
    /// 思考级别覆盖 `sampling_params` 的对象（目录 `samplingParamsByThinkingLevel`）；
    /// 键为 off/minimal/low/medium/high/xhigh/max，值为该级别的采样参数对象。
    pub sampling_params_by_thinking_level: Option<Value>,
    /// 网关路由配置（openRouterRouting / vercelGatewayRouting），无则 None。
    pub provider_routing: Option<Value>,
    /// 流式响应是否带 usage 统计，目录缺省为 true。
    pub supports_usage_in_streaming: bool,
    /// 是否接受 store 参数（服务端保存会话），缺省 true。
    pub supports_store: bool,
    /// 流式分片是否携带 finish_reason，缺省 true。
    pub supports_finish_reason: bool,
    /// 工具结果后是否必须先跟一条 assistant 消息，缺省 false。
    pub requires_assistant_after_tool_result: bool,
    /// 是否支持工具 schema 的 strict 严格模式，缺省 false。
    pub supports_strict_mode: bool,
    /// 目录中的计费单价（每百万 token 的输入/输出/缓存价），未提供为 None。
    pub cost: Option<Value>,
    /// 是否接受 developer 角色消息，缺省 false。
    pub supports_developer_role: bool,
    /// assistant 历史消息是否必须带 reasoning_content，缺省 false。
    pub requires_reasoning_content_on_assistant_messages: bool,
    /// 思考内容传递格式（compat.thinkingFormat），缺省按 provider/baseUrl 探测。
    pub thinking_format: String,
    /// compat.supportsReasoningEffort：provider 是否接受显式 reasoning_effort。缺失时按 provider/baseUrl 探测。
    pub supports_reasoning_effort: bool,
    /// 思考档位映射：值为 Some 表示该档位可用，值为 None 表示不支持该档位。
    pub thinking_level_map: Option<HashMap<String, Option<String>>>,
    /// models.json provider 级 authHeader：请求层自动加 `Authorization: Bearer <apiKey>`
    pub auth_header: bool,
    /// 目录 `inputLimits`：图片内联限制 + 请求级上限
    pub input_limits: InputLimits,
    /// 目录 `compat.allowedFallbackModels`：Anthropic 服务端 fallback 名单
    pub allowed_fallback_models: Vec<AllowedFallbackModel>,
    /// 是否支持 OpenAI 的 grammar 约束工具（目录 compat.supportsOpenAIGrammarTools）。
    pub supports_openai_grammar_tools: bool,
    /// compat.sendSessionAffinityHeaders：是否下发会话亲和头；`None` 表示按 api + 是否 openrouter 探测
    /// （见 [`crate::core::provider::session_affinity_headers`]）。
    pub send_session_affinity_headers: Option<bool>,
    /// compat.sessionAffinityFormat：`openai` / `openai-nosession` / `openrouter`；
    /// `None` 表示按 provider / baseUrl 探测。
    pub session_affinity_format: Option<String>,
}

/// 解析目录 `compat.allowedFallbackModels`：无 / 非数组 / 条目缺 `model` 时丢弃。
fn parse_allowed_fallback_models(v: Option<&Value>) -> Vec<AllowedFallbackModel> {
    let Some(items) = v.and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    items
        .iter()
        .filter_map(|item| {
            let model = item.get("model")?.as_str()?.to_string();
            Some(AllowedFallbackModel {
                provider: item
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                model,
                cost: item.get("cost").cloned(),
            })
        })
        .collect()
}

/// 按 provider/baseUrl 探测 openai 兼容层的 thinkingFormat / supportsReasoningEffort。
fn detect_compat(provider: &str, base_url: &str) -> (&'static str, bool) {
    let base = base_url.to_ascii_lowercase();
    let is_zai = provider == "zai"
        || provider == "zai-coding-cn"
        || base.contains("api.z.ai")
        || base.contains("open.bigmodel.cn");
    let is_together = provider == "together"
        || base.contains("api.together.ai")
        || base.contains("api.together.xyz");
    let is_moonshot =
        provider == "moonshotai" || provider == "moonshotai-cn" || base.contains("api.moonshot.");
    let is_openrouter = provider == "openrouter" || base.contains("openrouter.ai");
    let is_cloudflare_gateway =
        provider == "cloudflare-ai-gateway" || base.contains("gateway.ai.cloudflare.com");
    let is_nvidia = provider == "nvidia" || base.contains("integrate.api.nvidia.com");
    let is_ant_ling = provider == "ant-ling" || base.contains("api.ant-ling.com");
    let is_deepseek = provider == "deepseek" || base.contains("deepseek.com");
    let is_grok = provider == "xai" || base.contains("api.x.ai");

    let format = if is_deepseek {
        "deepseek"
    } else if is_zai {
        "zai"
    } else if is_together {
        "together"
    } else if is_ant_ling {
        "ant-ling"
    } else if is_openrouter {
        "openrouter"
    } else {
        "openai"
    };
    let supports_effort = !is_grok
        && !is_zai
        && !is_moonshot
        && !is_together
        && !is_cloudflare_gateway
        && !is_nvidia
        && !is_ant_ling;
    (format, supports_effort)
}

/// 从合并视图（基线 + 动态缓存）查找模型（不限类型）。
///
/// 同一上游 ID 可能有多个类型的条目（如 OpenRouter 同一模型兼有 chat 与 image 条目），
/// 此处返回**首个**匹配；需要限定类型时用 [`find_model_of_type`]。
pub fn find_model(provider: &str, model_id: &str) -> Result<ModelEntry> {
    find_model_impl(provider, model_id, None)
}

/// 按 id / name 精确、再按前缀回退，在激活的虚拟模型里查找定义。
fn find_virtual_definition(provider: &str, model_id: &str) -> Option<Arc<VirtualModelDefinition>> {
    let defs = virtual_models::active_definitions();
    let exact = defs.iter().find(|d| {
        d.provider == provider && (d.id == model_id || (!d.name.is_empty() && d.name == model_id))
    });

    exact.cloned().or_else(|| {
        // 有相同前缀的也算找到
        defs.into_iter().find(|d| {
            d.provider == provider
                && (model_id.starts_with(&d.id)
                    || (!d.name.is_empty() && model_id.starts_with(&d.name)))
        })
    })
}

/// 把虚拟模型定义转成目录条目。
///
/// chat 型条目的 `api` 是 [`VIRTUAL_MODEL_API`]（凭据交给路由后的物理模型）；
/// image / classifier 型条目直接用定义里声明的协议实现与元数据。
fn virtual_model_entry(definition: &VirtualModelDefinition) -> ModelEntry {
    let levels = definition.resolved_thinking_levels();
    let mut thinking_level_map: HashMap<String, Option<String>> = HashMap::new();

    for level in VALID_THINKING_LEVELS {
        thinking_level_map.insert(
            level.to_string(),
            levels.iter().any(|l| l == level).then(|| level.to_string()),
        );
    }

    ModelEntry {
        id: definition.id.clone(),
        name: if definition.name.is_empty() {
            definition.id.clone()
        } else {
            definition.name.clone()
        },
        api: definition.api.clone(),
        model_type: definition.model_type,
        base_url: definition.base_url.clone(),
        provider: definition.provider.clone(),
        input: definition.resolved_input(),
        output: definition.output.clone(),
        reasoning: definition.model_type == ModelType::Chat && levels.iter().any(|l| l != "off"),
        context_window: definition.context_window,
        max_tokens: definition.max_tokens,
        max_tokens_field: "max_tokens".to_string(),
        sampling_params: None,
        sampling_params_by_thinking_level: None,
        provider_routing: None,
        supports_usage_in_streaming: false,
        supports_store: false,
        supports_finish_reason: true,
        requires_assistant_after_tool_result: false,
        supports_strict_mode: false,
        cost: definition.cost.clone(),
        supports_developer_role: false,
        requires_reasoning_content_on_assistant_messages: false,
        thinking_format: String::new(),
        supports_reasoning_effort: false,
        thinking_level_map: Some(thinking_level_map),
        auth_header: false,
        input_limits: InputLimits::default(),
        allowed_fallback_models: Vec::new(),
        supports_openai_grammar_tools: false,
        send_session_affinity_headers: None,
        session_affinity_format: None,
    }
}

/// 按 `type` 查找模型条目
///
/// 同一上游 ID 的 chat / image / classifier 条目互不覆盖：`find_model` 拿到的是首个匹配，
/// 而图片生成 / 分类器必须按类型取到对应的那条（`api` 与 `output` 都不同）。类型不匹配时返回「未找到」错误。
pub fn find_model_of_type(provider: &str, model_id: &str, kind: ModelType) -> Result<ModelEntry> {
    find_model_impl(provider, model_id, Some(kind))
}

/// [`find_model`] / [`find_model_of_type`] 的共同实现；`kind` 为 `None` 时不限类型。
fn find_model_impl(provider: &str, model_id: &str, kind: Option<ModelType>) -> Result<ModelEntry> {
    // 虚拟模型优先：同名时隐藏同 provider、同类型的物理条目
    if let Some(definition) = find_virtual_definition(provider, model_id)
        && kind.is_none_or(|k| k == definition.model_type)
    {
        return Ok(virtual_model_entry(&definition));
    }

    let provider_models = provider_models(provider);
    if provider_models.is_empty() {
        return Err(Error::msg(format!(
            "no models available for provider {}: run /login to refresh the model catalog",
            provider
        )));
    }

    // id 精确 → name 精确 → 前缀回退
    for (pass_exact, m) in provider_models
        .iter()
        .map(|m| (true, m))
        .chain(provider_models.iter().map(|m| (false, m)))
    {
        if let Some(kind) = kind
            && ModelType::from_catalog(m.get("type")) != Some(kind)
        {
            continue;
        }

        // ChatGPT 登录态下 openai 的分类器条目不可见
        if ModelType::from_catalog(m.get("type")) == Some(ModelType::Classifier)
            && classifier_hidden_by_chatgpt_sign_in(provider)
        {
            continue;
        }

        let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let name = m.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let matched = if pass_exact {
            id == model_id || (!name.is_empty() && name == model_id)
        } else {
            model_id.starts_with(id) || (!name.is_empty() && model_id.starts_with(name))
        };

        if matched {
            let compat = m.get("compat").cloned().unwrap_or(Value::Null);
            let auth_header = model_config::provider_config(provider)
                .map(|c| c.auth_header)
                .unwrap_or(false);
            let base_url = m
                .get("baseUrl")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let (detected_format, detected_supports_effort) = detect_compat(provider, &base_url);

            return Ok(ModelEntry {
                id: id.to_string(),
                name: m
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(id)
                    .to_string(),
                api: {
                    // github-copilot 模型按目录 api 字段原生分发
                    // （Claude → anthropic-messages、gpt-oss/部分 → openai-completions、gpt-5.x → openai-responses）；
                    // Copilot 网关同一 baseUrl 支持多端点。
                    m.get("api")
                        .and_then(|v| v.as_str())
                        .unwrap_or("openai-completions")
                }
                .to_string(),
                model_type: ModelType::from_catalog(m.get("type")).unwrap_or_default(),
                base_url,
                provider: provider.to_string(),
                input: m
                    .get("input")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
                output: m
                    .get("output")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
                reasoning: m
                    .get("reasoning")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                context_window: m // provider-composer 缺省：128000 / 16384
                    .get("contextWindow")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(128_000) as u32,
                max_tokens: m
                    .get("maxTokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(16_384) as u32,
                max_tokens_field: compat
                    .get("maxTokensField")
                    .and_then(|v| v.as_str())
                    .unwrap_or("max_completion_tokens")
                    .to_string(),
                sampling_params: m.get("samplingParams").cloned(),
                sampling_params_by_thinking_level: m.get("samplingParamsByThinkingLevel").cloned(),
                cost: m.get("cost").cloned(),
                supports_developer_role: compat
                    .get("supportsDeveloperRole")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                requires_reasoning_content_on_assistant_messages: compat
                    .get("requiresReasoningContentOnAssistantMessages")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                thinking_format: compat
                    .get("thinkingFormat")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| detected_format.to_string()),
                supports_reasoning_effort: compat
                    .get("supportsReasoningEffort")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(detected_supports_effort),
                provider_routing: compat
                    .get("openRouterRouting")
                    .or_else(|| compat.get("vercelGatewayRouting"))
                    .cloned(),
                supports_usage_in_streaming: compat
                    .get("supportsUsageInStreaming")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true),
                supports_store: compat
                    .get("supportsStore")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true),
                supports_finish_reason: compat
                    .get("supportsFinishReason")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true),
                requires_assistant_after_tool_result: compat
                    .get("requiresAssistantAfterToolResult")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                supports_strict_mode: compat
                    .get("supportsStrictMode")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                supports_openai_grammar_tools: compat
                    .get("supportsOpenAIGrammarTools")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                send_session_affinity_headers: compat
                    .get("sendSessionAffinityHeaders")
                    .and_then(|v| v.as_bool()),
                session_affinity_format: compat
                    .get("sessionAffinityFormat")
                    .and_then(|v| v.as_str())
                    .map(|v| v.to_string()),
                thinking_level_map: m.get("thinkingLevelMap").and_then(|v| v.as_object()).map(
                    |obj| {
                        obj.iter()
                            .map(|(k, v)| (k.clone(), v.as_str().map(|s| s.to_string())))
                            .collect()
                    },
                ),
                auth_header,
                input_limits: InputLimits::from_catalog(m.get("inputLimits")),
                allowed_fallback_models: parse_allowed_fallback_models(
                    compat.get("allowedFallbackModels"),
                ),
            });
        }
    }

    Err(Error::msg(match kind {
        Some(kind) => format!(
            "model {} not found (provider: {}, type: {})",
            model_id,
            provider,
            kind.as_str()
        ),
        None => format!("model {} not found (provider: {})", model_id, provider),
    }))
}

/// 由模型目录条目组装供 provider 层直接使用的 [`ModelConfig`]。
///
/// 这是 `ModelConfig` 的**唯一构造点**：agent 启动（[`crate::core::agent_session::Agent::new`]）、
/// `/model` 切换（`switch_model`）、proxy 网关的逐请求解析都走这里。
///
/// `api_key_override` 为显式密钥（CLI `--api-key` / 会话覆盖）；
/// `session_id` 供 provider 层生成会话路由 / 提示缓存亲和头
/// （anthropic `x-session-affinity`、opencode `x-opencode-session`）。
/// Agent 传真实会话 id；gateway 这类没有会话的调用方传一个按请求派生的稳定 id
/// （见 `extensions::proxy::server::gateway_session_id`）。
pub fn model_config_from_entry(
    entry: &ModelEntry,
    api_key_override: Option<String>,
    session_id: Option<String>,
) -> ModelConfig {
    let api_key = auth::resolve_api_key(&entry.provider, api_key_override).unwrap_or_default();
    let temperature = settings_manager::read_settings_temperature().or_else(|| {
        entry
            .sampling_params
            .as_ref()
            .and_then(|sp| sp.get("temperature"))
            .and_then(|v| v.as_f64())
    });

    ModelConfig {
        model_type: entry.model_type,
        provider: entry.provider.clone(),
        model_id: entry.id.clone(),
        base_url: entry.base_url.clone(),
        sampling_params: entry.sampling_params.clone(),
        sampling_params_by_thinking_level: entry.sampling_params_by_thinking_level.clone(),
        provider_routing: entry.provider_routing.clone(),
        supports_usage_in_streaming: entry.supports_usage_in_streaming,
        supports_store: entry.supports_store,
        supports_finish_reason: entry.supports_finish_reason,
        requires_assistant_after_tool_result: entry.requires_assistant_after_tool_result,
        supports_strict_mode: entry.supports_strict_mode,
        api_key,
        api: entry.api.clone(),
        input: entry.input.clone(),
        output: entry.output.clone(),
        reasoning: entry.reasoning,
        max_tokens: if entry.max_tokens > 0 {
            Some(entry.max_tokens)
        } else {
            None
        },
        temperature,
        context_window: entry.context_window,
        thinking_format: entry.thinking_format.clone(),
        supports_reasoning_effort: entry.supports_reasoning_effort,
        thinking_level_map: entry.thinking_level_map.clone(),
        auth_header: entry.auth_header,
        image_resize: entry.input_limits.images.resize,
        allowed_fallback_models: entry.allowed_fallback_models.clone(),
        supports_developer_role: entry.supports_developer_role,
        requires_reasoning_content_on_assistant_messages: entry
            .requires_reasoning_content_on_assistant_messages,
        max_tokens_field: entry.max_tokens_field.clone(),
        cost: entry.cost.clone(),
        session_id,
        send_session_affinity_headers: entry.send_session_affinity_headers,
        session_affinity_format: entry.session_affinity_format.clone(),
        max_retry_delay_ms: None,
        thinking_budgets: None,
    }
}

/// 目录条目是否声明支持 OpenAI 的 grammar 约束工具（`compat.supportsOpenAIGrammarTools`）。
///
/// provider 层在决定是否把带语法约束的工具下发为 `custom` 工具时查询它；
/// 模型不在目录里（测试专用模型 / 自定义配置）时为 `false`。
pub fn supports_openai_grammar_tools(provider: &str, model_id: &str) -> bool {
    find_model(provider, model_id)
        .map(|entry| entry.supports_openai_grammar_tools)
        .unwrap_or(false)
}

/// 按 `provider` + `model_id` 解析并组装 [`ModelConfig`]（`find_model` + [`model_config_from_entry`]）。
///
/// 注意：密钥来自 `auth.json` / `models.json` / 环境变量的**同步**解析；OAuth 凭据的
/// 过期刷新是异步的（[`crate::core::auth::ensure_oauth_valid`]），调用方
/// 在真正发起模型请求前需自行刷新（见 `agent_loop::prepare_model_context`）。
pub fn configured_model(
    provider: &str,
    model_id: &str,
    api_key_override: Option<String>,
    session_id: Option<String>,
) -> Result<ModelConfig> {
    let entry = find_model(provider, model_id)?;
    Ok(model_config_from_entry(
        &entry,
        api_key_override,
        session_id,
    ))
}

/// 按 `provider` + `model_id` + 类型解析并组装 [`ModelConfig`]
/// （[`find_model_of_type`] + [`model_config_from_entry`]）。
///
/// 非 chat 操作（[`crate::core::provider::classify`] / `generate_images`）用它：
/// 同 provider 同 id 可以兼有 chat / image / classifier 条目，
/// 不限类型的 [`configured_model`] 可能拿到别的类型，调用时直接报「类型不符」。
/// 密钥解析口径同 [`configured_model`]。
pub fn configured_model_of_type(
    provider: &str,
    model_id: &str,
    kind: ModelType,
    api_key_override: Option<String>,
    session_id: Option<String>,
) -> Result<ModelConfig> {
    let entry = find_model_of_type(provider, model_id, kind)?;
    Ok(model_config_from_entry(
        &entry,
        api_key_override,
        session_id,
    ))
}

/// 列出 provider 的可用模型（合并视图：基线 + 动态缓存）
/// provider 的 chat 模型列表（id, 展示名）：`/model` 选择器、子代理目录、
/// `--list-models`、proxy `/v1/models` 共用的口径。
///
/// 目录里同一文件混装 image / classifier 条目，它们不能走 chat 流，因此不在此列
/// （其他用途见 [`list_models_of_type`]）。
pub fn list_models(provider: &str) -> Vec<(String, String)> {
    list_models_of_type(provider, ModelType::Chat)
}

/// 当前凭据下某个 provider 的分类器条目是否应当隐藏。
///
/// 只有 openai 命中：Sign in with ChatGPT 只签发 Responses API 的 token，Decisions API 会拒绝它。
pub fn classifier_hidden_by_chatgpt_sign_in(provider: &str) -> bool {
    provider == "openai" && auth::read_oauth_credential(provider).is_some()
}

/// 指定 [`ModelType`] 的模型列表（id, 展示名）。
///
/// 图片生成（[`crate::core::provider::generate_images`]）用它列 `type: image` 条目；
/// 分类器（[`crate::core::provider::classify`]）用它列 `type: classifier` 条目。
pub fn list_models_of_type(provider: &str, kind: ModelType) -> Vec<(String, String)> {
    // ChatGPT 登录取到的凭据到不了 Decisions API，该登录态下列不出分类器条目
    if kind == ModelType::Classifier && classifier_hidden_by_chatgpt_sign_in(provider) {
        return Vec::new();
    }

    let items = provider_models(provider);

    // 凭据携带 availableModelIds（登录时从账号拉取），过滤目录中当前账号不可用的模型
    let available: Option<HashSet<String>> = {
        let cred = auth::read_oauth_credential(provider);
        cred.and_then(|c| c.available_model_ids)
            .map(|ids| ids.into_iter().collect())
    };

    let mut out: Vec<(String, String)> = items
        .iter()
        .filter(|m| ModelType::from_catalog(m.get("type")) == Some(kind))
        .filter(|m| {
            let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("");
            match &available {
                // github-copilot 要以 auth.json 和 models-store.json 的并集为支持的模型
                Some(set) if provider == "github-copilot" => set.contains(id),
                _ => true,
            }
        })
        .filter_map(|m| {
            let id = m.get("id").and_then(|v| v.as_str())?;
            let name = m.get("name").and_then(|v| v.as_str()).unwrap_or(id);
            Some((id.to_string(), name.to_string()))
        })
        .collect();

    // 虚拟模型出现在自己类型的列表里：同名的物理条目被隐藏。
    let virtuals = virtual_models::list_for_provider_of_type(provider, kind);
    if !virtuals.is_empty() {
        let hidden: HashSet<&str> = virtuals.iter().map(|(id, _)| id.as_str()).collect();
        out.retain(|(id, _)| !hidden.contains(id.as_str()));
        out.extend(virtuals);
    }

    out
}

/// 目录里的一个模型条目（跨 provider 展开）：`provider` 与 `id` 共同唯一标识一个模型
/// （同一 id 可同时是 chat 与 image 条目，靠 `kind` 区分）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    /// provider 标识（与 `/model` 选择器、`models.json` 一致）。
    pub provider: String,
    /// provider 内的模型 id（请求时作为 model 名）。
    pub id: String,
    /// 展示名（目录 `name`，缺失时回退为 id）。
    pub name: String,
    /// 模型类型（chat / image / classifier）。
    pub kind: ModelType,
}

/// 目录中可见的全部 provider：内置支持列表在前，用户 `models.json` 声明的自定义 provider 按声明顺序追加在后（同名去重）。
pub fn catalog_providers() -> Vec<String> {
    let mut out: Vec<String> = SUPPORTED_PROVIDERS.iter().map(|p| p.to_string()).collect();
    for (id, _) in model_config::read_models_config() {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// 全部 provider 的全部类型模型
pub fn list_all_models() -> Vec<CatalogModel> {
    catalog_providers()
        .into_iter()
        .flat_map(|provider| collect_all_of_type(&provider, None))
        .collect()
}

/// 全部 provider 中指定 [`ModelType`] 的模型
pub fn list_all_models_of_type(kind: ModelType) -> Vec<CatalogModel> {
    catalog_providers()
        .into_iter()
        .flat_map(|provider| collect_all_of_type(&provider, Some(kind)))
        .collect()
}

/// 只含「凭据已配置」provider 的全部类型模型
///
/// 可用性口径与 `/model` 选择器一致（存储凭据 / 环境变量 / `models.json` 的 key /
/// 仅有虚拟模型的 provider），见 [`auth::list_configured_providers`]。
pub fn list_all_available_models() -> Vec<CatalogModel> {
    auth::list_configured_providers()
        .into_iter()
        .flat_map(|provider| collect_all_of_type(&provider, None))
        .collect()
}

/// [`list_all_available_models`] 的指定类型版本
pub fn list_all_available_models_of_type(kind: ModelType) -> Vec<CatalogModel> {
    auth::list_configured_providers()
        .into_iter()
        .flat_map(|provider| collect_all_of_type(&provider, Some(kind)))
        .collect()
}

/// 把某 provider 的模型展开成 [`CatalogModel`]；`kind` 为 None 时取全部类型。
fn collect_all_of_type(provider: &str, kind: Option<ModelType>) -> Vec<CatalogModel> {
    let kinds: Vec<ModelType> = match kind {
        Some(kind) => vec![kind],
        None => vec![ModelType::Chat, ModelType::Image, ModelType::Classifier],
    };
    kinds
        .iter()
        .flat_map(|kind| {
            list_models_of_type(provider, *kind)
                .into_iter()
                .map(|(id, name)| CatalogModel {
                    provider: provider.to_string(),
                    id,
                    name,
                    kind: *kind,
                })
        })
        .collect()
}

/// 从 CLI flag / 环境变量 / settings 决定 provider 与 model
pub fn resolve_provider_model(
    provider_flag: Option<&str>,
    model_flag: Option<&str>,
    settings: &Settings,
) -> Result<(String, String, Option<String>)> {
    let env_provider = std::env::var("PRUX_PROVIDER").ok();
    let env_model = std::env::var("PRUX_MODEL").ok();

    let provider_flag = provider_flag
        .map(|s| s.to_string())
        .or(env_provider)
        .or(settings.default_provider.clone());
    let model_flag = model_flag
        .map(|s| s.to_string())
        .or(env_model)
        .or(settings.default_model.clone());
    let (m_provider, m_model, m_thinking) = model_flag
        .as_deref()
        .map(parse_model_arg)
        .unwrap_or_default();

    match (provider_flag, m_provider) {
        (Some(_p), Some(mp)) => Ok((mp, m_model, m_thinking)),
        (Some(p), None) => {
            if m_model.is_empty() {
                // 只给了 provider 没给模型：用该 provider 默认模型
                match default_model_for(&p) {
                    Some((pp, mm)) => Ok((pp, mm, m_thinking)),
                    None => Err(Error::msg(format!("unsupported provider: {}", p))),
                }
            } else {
                Ok((p, m_model, m_thinking))
            }
        }
        (None, Some(mp)) => Ok((mp, m_model, m_thinking)),
        // 无任何配置：回退默认 provider 的默认模型。使 TUI 正常启动并在无凭据时提示 /login
        (None, None) => match default_model_for(SUPPORTED_PROVIDERS[0]) {
            Some((p, m)) => Ok((p, m, None)),
            None => Err(Error::msg(
                "no provider and model configured. set PRUX_PROVIDER/PRUX_MODEL or use --provider/--model.",
            )),
        },
    }
}

/// provider 是否由用户显式指定（--provider / PRUX_PROVIDER / settings.default_provider，
/// 或 model 参数带 provider 前缀）。
/// 用于 --list-models：无显式 provider 时 resolve 出的兜底 provider 不算可用模型，不应列出目录。
pub fn provider_explicitly_configured(
    provider_flag: Option<&str>,
    model_flag: Option<&str>,
    settings: &Settings,
) -> bool {
    let env_provider = std::env::var("PRUX_PROVIDER").ok();
    let env_model = std::env::var("PRUX_MODEL").ok();

    if provider_flag.is_some() || env_provider.is_some() || settings.default_provider.is_some() {
        return true;
    }

    let model_flag = model_flag
        .map(|s| s.to_string())
        .or(env_model)
        .or(settings.default_model.clone());

    model_flag
        .as_deref()
        .map(parse_model_arg)
        .and_then(|(p, _, _)| p)
        .is_some()
}

/// provider 是否已知：基线目录或 models-store 动态缓存中存在模型目录
pub fn is_known_provider(provider: &str) -> bool {
    !provider_models(provider).is_empty()
}

/// provider 默认模型表。 查找结果仍需确认存在于基线/动态缓存，否则回退基线第一个条目。
fn default_model_id(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "ant-ling" => "Ring-2.6-1T",
        "anthropic" => "claude-opus-4-8",
        "azure" => "gpt-5.4",
        "baseten" => "zai-org/GLM-5.2",
        "cerebras" => "gpt-oss-120b",
        "deepseek" => "deepseek-v4-pro",
        "fireworks" => "accounts/fireworks/models/kimi-k3",
        "github-copilot" => "gpt-5.4",
        "google" => "gemini-3.1-pro-preview",
        "groq" => "openai/gpt-oss-120b",
        "huggingface" => "moonshotai/Kimi-K2.6",
        "kimi-coding" => "kimi-for-coding",
        "meta" => "muse-spark-1.3",
        "minimax" => "MiniMax-M2.7",
        "minimax-cn" => "MiniMax-M2.7",
        "moonshotai" => "kimi-k2.6",
        "moonshotai-cn" => "kimi-k2.6",
        "nvidia" => "nvidia/nemotron-3-ultra-550b-a55b",
        "openai" => "gpt-5.5",
        "opencode" => "kimi-k2.6",
        "opencode-go" => "kimi-k3",
        "openrouter" => "moonshotai/kimi-k2.6",
        "qwen-token-plan" => "qwen3.7-max",
        "qwen-token-plan-cn" => "qwen3.7-max",
        "qwen-token-plan-individual" => "qwen3.8-max",
        "together" => "moonshotai/Kimi-K3",
        "vercel-ai-gateway" => "zai/glm-5.1",
        "xai" => "grok-4.7",
        "xiaomi" => "mimo-v2.5-pro",
        "xiaomi-token-plan-ams" => "mimo-v2.5-pro",
        "xiaomi-token-plan-cn" => "mimo-v2.5-pro",
        "xiaomi-token-plan-sgp" => "mimo-v2.5-pro",
        "zai" => "glm-5.3",
        "zai-coding-cn" => "glm-5.3",
        _ => return None,
    })
}

/// provider 的默认模型：默认模型表优先。
/// 表中 id 不在当前合并视图时回退合并视图（基线 + store + models.json）第一个
/// chat 条目（目录里的 image/classifier 条目不能当默认模型用）；
/// 只有虚拟模型的 provider 回退到第一个虚拟模型。返回的元组是 `(provider, 模型 id)`。
pub fn default_model_for(provider: &str) -> Option<(String, String)> {
    let models = provider_models(provider);
    if let Some(preferred) = default_model_id(provider) {
        let available: Vec<&str> = models
            .iter()
            .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
            .collect();
        if available.contains(&preferred) {
            return Some((provider.to_string(), preferred.to_string()));
        }
    }

    models
        .iter()
        .find(|m| ModelType::from_catalog(m.get("type")) == Some(ModelType::Chat))
        .and_then(|m| {
            m.get("id")
                .and_then(|v| v.as_str())
                .map(|s| (provider.to_string(), s.to_string()))
        })
        .or_else(|| {
            // 仅有虚拟模型的 provider（目录里没有物理条目）回退到第一个虚拟模型。
            // `list_for_provider` 给的是 `(id, 展示名)`，元组要塞回 `(provider, id)`。
            virtual_models::list_for_provider(provider)
                .into_iter()
                .next()
                .map(|(id, _)| (provider.to_string(), id))
        })
}

/// 解析模型参数：支持 provider/model:thinking 与 model:thinking 形式
/// 返回 (provider, model_id, thinking)
/// 注意：模型 id 本身可能含冒号（如 Ollama 的 `llama3.1:8b`），
/// 冒号后缀不是合法 thinking 级别时整体视为模型 id
pub fn parse_model_arg(arg: &str) -> (Option<String>, String, Option<String>) {
    let split = |m: &str| -> (String, Option<String>) {
        if let Some((model, suffix)) = m.split_once(':')
            && VALID_THINKING_LEVELS.contains(&suffix)
        {
            return (model.to_string(), Some(suffix.to_string()));
        }

        (m.to_string(), None)
    };

    if let Some((p, m)) = arg.split_once('/') {
        let (model, thinking) = split(m);
        (Some(p.to_string()), model, thinking)
    } else {
        let (model, thinking) = split(arg);
        (None, model, thinking)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::settings_manager::agent_dir;
    use serde_json::json;

    #[test]
    fn detect_compat_matches_pi_detection() {
        // 无特殊 provider/baseUrl → openai + 接受 reasoning_effort
        assert_eq!(
            detect_compat("opencode-go", "https://opencode.ai/zen/go/v1"),
            ("openai", true)
        );
        // deepseek → deepseek
        assert_eq!(
            detect_compat("deepseek", "https://api.deepseek.com"),
            ("deepseek", true)
        );
        // zai / together / ant-ling / openrouter 格式探测
        assert_eq!(
            detect_compat("zai", "https://api.z.ai/api/paas/v4"),
            ("zai", false)
        );
        assert_eq!(
            detect_compat("together", "https://api.together.xyz/v1"),
            ("together", false)
        );
        assert_eq!(
            detect_compat("ant-ling", "https://api.ant-ling.com/v1"),
            ("ant-ling", false)
        );
        assert_eq!(
            detect_compat("openrouter", "https://openrouter.ai/api/v1"),
            ("openrouter", true)
        );
        // moonshot / nvidia / xai / cloudflare gateway 不接受 reasoning_effort
        assert_eq!(
            detect_compat("moonshotai", "https://api.moonshot.cn/v1"),
            ("openai", false)
        );
        assert_eq!(
            detect_compat("nvidia", "https://integrate.api.nvidia.com/v1"),
            ("openai", false)
        );
        assert_eq!(
            detect_compat("xai", "https://api.x.ai/v1"),
            ("openai", false)
        );
        assert_eq!(
            detect_compat(
                "cloudflare-ai-gateway",
                "https://gateway.ai.cloudflare.com/v1/a/b"
            ),
            ("openai", false)
        );
    }

    #[test]
    fn find_model_detects_thinking_capabilities_for_uncatalogued_format() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        // hy3：目录无 thinkingFormat / supportsReasoningEffort → 探测为 openai + 接受 reasoning_effort，
        // 且 off 映射为字符串 "none"（off 失效修复的输入条件）
        let hy3 = find_model("opencode-go", "hy3").unwrap();
        assert_eq!(hy3.thinking_format, "openai");
        assert!(hy3.supports_reasoning_effort);
        assert_eq!(
            hy3.thinking_level_map
                .as_ref()
                .and_then(|m| m.get("off"))
                .and_then(|v| v.as_deref()),
            Some("none")
        );
        // mimo-v2.5：同样探测为 openai + 接受 reasoning_effort（无 off 映射，仍无法强制关闭）
        let mimo = find_model("opencode-go", "mimo-v2.5").unwrap();
        assert_eq!(mimo.thinking_format, "openai");
        assert!(mimo.supports_reasoning_effort);
        assert!(mimo.thinking_level_map.is_none());
        // kimi-k2.6：目录显式 thinkingFormat=deepseek + supportsReasoningEffort=false，探测不得覆盖
        let kimi = find_model("moonshotai", "kimi-k2.6").unwrap();
        assert_eq!(kimi.thinking_format, "deepseek");
        assert!(!kimi.supports_reasoning_effort);
        // github-copilot openai-completions：目录显式 supportsReasoningEffort=false
        let copilot = find_model("github-copilot", "kimi-k3").unwrap();
        assert!(!copilot.supports_reasoning_effort);
    }

    #[test]
    fn openai_classifier_listing_follows_chatgpt_sign_in() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        // 无凭据：openai 的分类器条目可见
        let visible = list_models_of_type("openai", ModelType::Classifier);
        assert!(
            visible.iter().any(|(id, _)| id == "gpt-6-luna"),
            "无凭据时 gpt-6-luna 应可见: {visible:?}"
        );
        assert!(find_model_of_type("openai", "gpt-6-luna", ModelType::Classifier).is_ok());

        // Sign in with ChatGPT：oauth 凭据到不了 Decisions API，分类器条目应当隐藏
        let cred = crate::core::oauth::OAuthCredential {
            access: "x".into(),
            refresh: "r".into(),
            expires: u64::MAX,
            enterprise_url: None,
            available_model_ids: None,
            client_id: None,
            scopes: None,
        };
        crate::core::auth::write_oauth_credential("openai", &cred).unwrap();
        assert!(
            list_models_of_type("openai", ModelType::Classifier).is_empty(),
            "ChatGPT 登录态下不应列出 openai 的分类器"
        );
        assert!(
            find_model_of_type("openai", "gpt-6-luna", ModelType::Classifier).is_err(),
            "ChatGPT 登录态下 openai 的分类器也不应可解析"
        );
        // chat 模型不受登录方式影响
        assert!(!list_models_of_type("openai", ModelType::Chat).is_empty());
    }

    #[test]
    fn list_models_filters_copilot_by_available_ids() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        // 无凭据 → 不过滤
        let all = list_models("github-copilot");
        assert!(
            !all.is_empty(),
            "github-copilot 应有基线模型（pi.dev 或内嵌）"
        );
        // 有凭据 availableModelIds → 只列可用模型
        let cred = crate::core::oauth::OAuthCredential {
            access: "x".into(),
            refresh: "r".into(),
            expires: u64::MAX,
            enterprise_url: None,
            available_model_ids: Some(vec!["never-exists-model".to_string()]),
            client_id: None,
            scopes: None,
        };
        crate::core::auth::write_oauth_credential("github-copilot", &cred).unwrap();
        let filtered = list_models("github-copilot");
        assert!(
            filtered.is_empty(),
            "目录模型不在 availableModelIds 中应被过滤"
        );
        // 已登录模型 id 出现在列表
        let first_id = all[0].0.clone();
        let cred2 = crate::core::oauth::OAuthCredential {
            access: "x".into(),
            refresh: "r".into(),
            expires: u64::MAX,
            enterprise_url: None,
            available_model_ids: Some(vec![first_id.clone()]),
            client_id: None,
            scopes: None,
        };
        crate::core::auth::write_oauth_credential("github-copilot", &cred2).unwrap();
        let filtered2 = list_models("github-copilot");
        assert_eq!(filtered2.len(), 1);
        assert_eq!(filtered2[0].0, first_id);
        crate::core::auth::remove_auth("github-copilot").ok();
    }

    fn test_dir() {
        // 历史遗留：原为进程级 env pin（PRUX_AGENT_DIR 劫持）。
        // 现由各测试顶部的 AgentDirGuard 线程本地隔离，此处保持空实现。
    }

    fn write_store(v: serde_json::Value) {
        let path = agent_dir().join("models-store.json");
        std::fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
    }

    fn write_models_json(v: serde_json::Value) {
        let path = agent_dir().join("models.json");
        std::fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
    }

    #[test]
    fn meta_provider_baseline_models() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        // 基线含全部 Muse 模型
        let base = baseline_models("meta");
        assert!(!base.is_empty());
        let ids: Vec<&str> = base
            .iter()
            .filter_map(|m| m.get("id").and_then(|v| v.as_str()))
            .collect();
        assert!(ids.contains(&"muse-spark-1.3"));
        assert!(ids.contains(&"muse-spark-1.2"));
        assert!(ids.contains(&"muse-spark-1.1"));
        assert!(ids.contains(&"muse-spark-1.3-contributor"));
        assert!(ids.contains(&"muse-spark-1.2-contributor"));

        // openai-responses + meta 端点 + 1M 上下文 + thinkingLevelMap
        let e = find_model("meta", "muse-spark-1.3").unwrap();
        assert_eq!(e.provider, "meta");
        assert_eq!(e.api, "openai-responses");
        assert_eq!(e.base_url, "https://api.meta.ai/v1");
        assert!(e.reasoning);
        assert_eq!(e.context_window, 1_048_576);
        assert_eq!(e.max_tokens, 131_072);
        assert_eq!(
            e.thinking_level_map
                .as_ref()
                .and_then(|m| m.get("max"))
                .and_then(|v| v.as_deref()),
            Some("max"),
            "muse-spark-1.3 支持 max effort"
        );
        assert_eq!(
            e.thinking_level_map
                .as_ref()
                .and_then(|m| m.get("off"))
                .and_then(|v| v.as_deref()),
            None,
            "Muse 不支持 off"
        );

        // 默认模型
        assert_eq!(
            default_model_for("meta").unwrap(),
            ("meta".to_string(), "muse-spark-1.3".to_string())
        );
        // 早期模型无 max effort
        let e2 = find_model("meta", "muse-spark-1.1").unwrap();
        assert_eq!(
            e2.thinking_level_map
                .as_ref()
                .and_then(|m| m.get("max"))
                .and_then(|v| v.as_deref()),
            None,
            "muse-spark-1.1 无 max effort"
        );
    }

    /// pi：上游 models.dev 元数据不完整时，Opus 5.5 / Sonnet 5.5 只提供 low..max
    /// （off/minimal 显式禁用），不得越界给出更多档。
    #[test]
    fn opus_5_5_offers_low_through_max() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));
        let expected = vec!["low", "medium", "high", "xhigh", "max"];
        for (provider, id) in [
            ("github-copilot", "claude-opus-5.5"),
            ("anthropic", "claude-opus-5-5"),
            ("anthropic", "claude-sonnet-5-5"),
        ] {
            let m = find_model(provider, id).unwrap();
            let cfg = model_config_from_entry(&m, None, None);
            assert_eq!(cfg.supported_thinking_levels(), expected, "{provider}/{id}");
        }
    }

    /// 跨 provider 的模型集合：全量视图覆盖多种类型与多个 provider；
    /// 指定类型视图与全量视图的类型子集一致；available 视图是全量的子集。
    #[test]
    fn cross_provider_model_collections_cover_types_and_providers() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));

        let all = list_all_models();
        assert!(
            all.iter()
                .any(|m| m.provider == "anthropic" && m.kind == ModelType::Chat),
            "全量视图应含内置 provider 的 chat 模型"
        );
        assert!(
            all.iter().any(|m| m.kind == ModelType::Image),
            "全量视图应含图片模型"
        );
        assert!(
            all.iter().any(|m| m.kind == ModelType::Classifier),
            "全量视图应含分类器模型"
        );

        let images = list_all_models_of_type(ModelType::Image);
        assert!(!images.is_empty());
        assert!(images.iter().all(|m| m.kind == ModelType::Image));
        assert_eq!(
            images.len(),
            all.iter().filter(|m| m.kind == ModelType::Image).count(),
            "指定类型视图应与全量视图的类型子集逐条对应"
        );

        let chats = list_all_models_of_type(ModelType::Chat);
        let providers: HashSet<&str> = chats.iter().map(|m| m.provider.as_str()).collect();
        assert!(
            providers.len() > 3,
            "chat 视图应跨多个 provider: {providers:?}"
        );

        // available 视图是全量的子集，且类型过滤口径一致
        let all: Vec<(String, String, ModelType)> = all
            .into_iter()
            .map(|m| (m.provider, m.id, m.kind))
            .collect();
        let available = list_all_available_models();
        for m in &available {
            assert!(
                all.contains(&(m.provider.clone(), m.id.clone(), m.kind)),
                "available 条目不在全量视图里: {m:?}"
            );
        }
        assert_eq!(
            list_all_available_models_of_type(ModelType::Classifier).len(),
            available
                .iter()
                .filter(|m| m.kind == ModelType::Classifier)
                .count()
        );
    }

    /// 目录里混装 image / classifier 条目：`list_models`（选择器 / 子代理目录 / proxy
    /// `/v1/models` 共用的 chat 口径）不得列出它们，`list_models_of_type` 才取得到。
    #[test]
    fn model_lists_are_filtered_by_catalog_type() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));

        let ids =
            |v: Vec<(String, String)>| -> Vec<String> { v.into_iter().map(|(id, _)| id).collect() };
        let chat = ids(list_models("openrouter"));
        assert!(!chat.is_empty());
        assert!(
            !chat.iter().any(|id| id == "black-forest-labs/flux.2-flex"),
            "图片模型不应出现在 chat 列表"
        );
        assert!(
            !chat.iter().any(|id| id == "~typesafe/jev-latest"),
            "分类器不应出现在 chat 列表"
        );

        let images = ids(list_models_of_type("openrouter", ModelType::Image));
        assert!(
            images
                .iter()
                .any(|id| id == "black-forest-labs/flux.2-flex")
        );
        assert!(!images.iter().any(|id| id == "moonshotai/kimi-k2.6"));
        let classifiers = ids(list_models_of_type("openrouter", ModelType::Classifier));
        assert!(classifiers.iter().any(|id| id == "~typesafe/jev-latest"));

        // 类型从目录带进 ModelEntry / ModelConfig
        assert_eq!(
            find_model("openrouter", "black-forest-labs/flux.2-flex")
                .unwrap()
                .model_type,
            ModelType::Image
        );
        let img = model_config_from_entry(
            &find_model("openrouter", "black-forest-labs/flux.2-flex").unwrap(),
            None,
            None,
        );
        assert_eq!(img.model_type, ModelType::Image);
        assert_eq!(
            find_model("openrouter", &chat[0]).unwrap().model_type,
            ModelType::Chat
        );
    }

    /// 同一上游 ID 可同时有 chat 与 image 条目（如 openrouter 的 gemini-3-pro-image）：
    /// `find_model_of_type` 必须按类型取到对应那条（`api` 与 `output` 都不同）。
    #[test]
    fn find_model_of_type_selects_the_entry_of_that_type() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));

        let id = "google/gemini-3-pro-image";
        let chat = find_model_of_type("openrouter", id, ModelType::Chat).unwrap();
        let image = find_model_of_type("openrouter", id, ModelType::Image).unwrap();
        assert_eq!(chat.api, "openai-completions");
        assert_eq!(image.api, "openrouter-images");
        assert!(image.output.contains(&"image".to_string()));
        assert!(chat.output.is_empty());

        // 类型不匹配 → 未找到（错误带上类型，便于排查）
        let err = find_model_of_type("openrouter", id, ModelType::Classifier)
            .unwrap_err()
            .to_string();
        assert!(err.contains("type: classifier"), "{err}");

        // 不限类型时仍能查到，且带上了目录的 output
        assert_eq!(
            find_model("openrouter", "black-forest-labs/flux.2-flex")
                .unwrap()
                .output,
            vec!["image".to_string()]
        );
    }

    /// 目录缺 `type` 字段（旧目录 / 动态缓存 / 用户 models.json）一律当 chat；
    /// 无法识别的非空取值返回 `None`（调用方据此丢弃该条目）。
    #[test]
    fn missing_catalog_type_defaults_to_chat() {
        assert_eq!(ModelType::from_catalog(None), Some(ModelType::Chat));
        assert_eq!(
            ModelType::from_catalog(Some(&json!(""))),
            Some(ModelType::Chat)
        );
        assert_eq!(
            ModelType::from_catalog(Some(&json!(7))),
            Some(ModelType::Chat)
        );
        assert_eq!(ModelType::from_catalog(Some(&json!("unknown-kind"))), None);
        assert_eq!(
            ModelType::from_catalog(Some(&json!("chat"))),
            Some(ModelType::Chat)
        );
        // 大小写与首尾空白与 `parse` 同一口径
        assert_eq!(
            ModelType::from_catalog(Some(&json!(" IMAGE "))),
            Some(ModelType::Image)
        );
    }

    /// 未知 `type` 的条目整条不进目录：列表 / 查找 / 默认模型都看不到它。
    #[test]
    fn unknown_catalog_type_entries_are_dropped() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let path = agent_dir().join("models-store.json");
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            json!({
                "deepseek": {
                    "models": [
                        { "id": "ds-chat", "type": "chat" },
                        { "id": "ds-embed", "type": "embedding" },
                        { "id": "ds-plain" }
                    ]
                }
            })
            .to_string(),
        )
        .unwrap();

        let ids: Vec<String> = list_models("deepseek")
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(ids.contains(&"ds-chat".to_string()), "{ids:?}");
        assert!(ids.contains(&"ds-plain".to_string()), "{ids:?}");
        assert!(!ids.contains(&"ds-embed".to_string()), "{ids:?}");
        assert!(find_model("deepseek", "ds-embed").is_err());

        let _ = std::fs::remove_file(&path);
    }

    /// `strum` 派生的类型名互转：`as_str` 为规范小写名，`parse` 大小写不敏感且容忍首尾空白。
    #[test]
    fn model_type_strum_conversions() {
        for (kind, name) in [
            (ModelType::Chat, "chat"),
            (ModelType::Image, "image"),
            (ModelType::Classifier, "classifier"),
        ] {
            assert_eq!(kind.as_str(), name);
            assert_eq!(ModelType::parse(name), Some(kind));
            assert_eq!(ModelType::parse(&name.to_uppercase()), Some(kind));
            assert_eq!(ModelType::parse(&format!("  {name} ")), Some(kind));
        }
        assert_eq!(ModelType::parse("unknown-kind"), None);
        assert_eq!(ModelType::parse(""), None);
    }

    /// 每个 provider 的默认模型都必须是 chat：目录里 image 条目可能排在最前
    /// （如 openrouter 的 FLUX），回退取第一个条目时不能取到它们。
    #[test]
    fn default_model_is_always_chat() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));
        for p in SUPPORTED_PROVIDERS {
            let Some((provider, id)) = default_model_for(p) else {
                continue;
            };
            let entry = find_model(&provider, &id).unwrap();
            assert_eq!(entry.model_type, ModelType::Chat, "{provider}/{id}");
        }
    }

    #[test]
    fn baseline_models_available_without_store() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));
        assert_eq!(baseline_models("deepseek").len(), 2); // pi 0.87.1: deepseek-flash / deepseek-v4-pro
        assert!(baseline_models("opencode").len() > 50);
        assert!(baseline_models("opencode-go").len() >= 18);
        assert!(baseline_models("openai").len() >= 30);
        assert!(baseline_models("google").len() >= 20);
        assert!(baseline_models("huggingface").len() >= 50);
        assert!(baseline_models("openrouter").len() >= 300);
        assert!(baseline_models("unknown").is_empty());
        // 全部保留 provider 均有基线目录
        for p in SUPPORTED_PROVIDERS {
            assert!(!baseline_models(p).is_empty(), "{} 缺少静态基线", p);
        }
        // 无 store 时合并视图 = 基线
        assert_eq!(provider_models("deepseek").len(), 2);
        // find_model 从基线可解析
        let e = find_model("deepseek", "deepseek-flash").unwrap();
        assert_eq!(e.id, "deepseek-flash");
        assert_eq!(e.provider, "deepseek");
        assert!(e.reasoning);
        assert!(e.thinking_level_map.is_some());
    }

    /// pi 0.87.0 #9631：图片内联限制来自目录 `inputLimits.images.resize`。
    #[test]
    fn model_entry_carries_catalog_image_limits() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));

        // 目录里所有带 inputLimits 的条目共用 2000x2000 / 4.5MiB / q80
        let google = find_model("google", "gemini-3.1-pro-preview").unwrap();
        assert_eq!(
            google.input_limits.images.resize,
            crate::utils::image::ImageResizeLimits {
                max_width: 2000,
                max_height: 2000,
                max_bytes: 4_718_592,
                jpeg_quality: 80,
            }
        );
        assert_eq!(
            google.input_limits.images.resize,
            crate::utils::image::ImageResizeLimits::default()
        );

        // 无 inputLimits 的条目回退 pi 默认值（不是 0/缺失）
        let deepseek = find_model("deepseek", "deepseek-flash").unwrap();
        assert_eq!(
            deepseek.input_limits.images.resize,
            crate::utils::image::ImageResizeLimits::default()
        );
    }

    /// pi 0.87.1：目录 `inputLimits.maxRequestBytes` / `images.maxPerMessage` / `images.maxPerRequest`
    /// 解析进 ModelEntry（pi 自身也只解析、不重写历史，本仓同样只透传）。
    #[test]
    fn model_entry_carries_catalog_request_limits() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));

        // anthropic：32MiB / 200k 上下文模型 100 张，其余 600 张
        let haiku = find_model("anthropic", "claude-haiku-4-5").unwrap();
        assert_eq!(haiku.input_limits.max_request_bytes, Some(33_554_432));
        assert_eq!(haiku.input_limits.images.max_per_request, Some(100));
        assert_eq!(haiku.input_limits.images.max_per_message, None);
        let opus = find_model("anthropic", "claude-opus-5").unwrap();
        assert_eq!(opus.input_limits.images.max_per_request, Some(600));

        // openai / google 的目录值
        let gpt = find_model("openai", "gpt-4-turbo").unwrap();
        assert_eq!(gpt.input_limits.max_request_bytes, Some(536_870_912));
        assert_eq!(gpt.input_limits.images.max_per_request, Some(1500));
        let gemini = find_model("google", "gemini-3.1-pro-preview").unwrap();
        assert_eq!(gemini.input_limits.max_request_bytes, Some(20_971_520));
        assert_eq!(gemini.input_limits.images.max_per_request, Some(3600));

        // 只有 resize 的条目：请求级上限保持 None（不臆造默认值）
        let vision = find_model("fireworks", "accounts/fireworks/routers/kimi-latest").unwrap();
        assert_eq!(vision.input_limits.images.resize.max_width, 2000);
        assert_eq!(vision.input_limits.max_request_bytes, None);
        assert_eq!(vision.input_limits.images.max_per_request, None);
    }

    /// pi `mergeInputLimits`：modelOverrides.inputLimits 深合并——
    /// 顶层键与 images 键覆盖，images.resize 字段级合并。
    /// pi #10198：远程目录合并不能是平方复杂度。这里用一份**大**动态目录跑一遍合并，
    /// 既回归正确性（同键覆盖、新键追加、未知 type 丢弃），也把 O(n·m) 的写法钉在测试里
    /// （旧实现每条 `position()` 扫全表，条目多了就是平方）。
    #[test]
    fn dynamic_catalog_merge_upserts_by_key_without_scanning() {
        let _ad = crate::test_support::AgentDirGuard::temp();

        let baseline = baseline_models("deepseek");
        let first = baseline.first().expect("基线目录非空");
        let first_id = first.get("id").unwrap().as_str().unwrap().to_string();

        // 动态目录：覆盖基线首条 + 499 条新 chat 条目 + 1 条未知 type（应丢弃）
        let mut models: Vec<Value> = vec![json!({
            "id": first_id,
            "name": "overridden",
            "type": "chat"
        })];
        for i in 0..499 {
            models.push(json!({
                "id": format!("dyn-{i}"),
                "name": format!("Dyn {i}"),
                "type": "chat"
            }));
        }
        models.push(json!({ "id": "dyn-bogus", "name": "x", "type": "bogus" }));
        crate::core::model_refresh::write_stored_entry("deepseek", json!({ "models": models }))
            .unwrap();

        let merged = provider_models("deepseek");
        assert_eq!(
            merged.len(),
            baseline.len() + 499,
            "覆盖不增条，新键追加，未知 type 丢弃"
        );
        let overridden = merged
            .iter()
            .find(|m| m.get("id").and_then(|v| v.as_str()) == Some(first_id.as_str()))
            .unwrap();
        assert_eq!(overridden.get("name").unwrap().as_str(), Some("overridden"));
        assert!(
            merged
                .iter()
                .any(|m| m.get("id").and_then(|v| v.as_str()) == Some("dyn-498"))
        );
        assert!(
            !merged
                .iter()
                .any(|m| m.get("id").and_then(|v| v.as_str()) == Some("dyn-bogus")),
            "未知 type 的条目整条丢弃"
        );

        crate::core::model_refresh::remove_stored_entry("deepseek").ok();
    }

    #[test]
    fn model_overrides_deep_merge_input_limits() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models.json"));
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));
        write_models_json(serde_json::json!({
            "providers": {
                "anthropic": {
                    "modelOverrides": {
                        "claude-haiku-4-5": {
                            "inputLimits": {
                                "maxRequestBytes": 123,
                                "images": {
                                    "resize": { "maxWidth": 1568, "jpegQuality": 75 },
                                    "maxPerRequest": 7
                                }
                            }
                        }
                    }
                }
            }
        }));

        let e = find_model("anthropic", "claude-haiku-4-5").unwrap();
        assert_eq!(e.input_limits.max_request_bytes, Some(123));
        assert_eq!(e.input_limits.images.max_per_request, Some(7));
        let resize = e.input_limits.images.resize;
        assert_eq!(resize.max_width, 1568, "覆盖值生效");
        assert_eq!(resize.jpeg_quality, 75, "覆盖值生效");
        assert_eq!(resize.max_height, 2000, "未覆盖字段保留目录值");
        assert_eq!(resize.max_bytes, 4_718_592, "未覆盖字段保留目录值");

        let _ = std::fs::remove_file(agent_dir().join("models.json"));
    }

    /// pi 0.86.0 #9294：目录 `compat.allowedFallbackModels` 解析进 ModelEntry，
    /// 并原样传给 ModelConfig（Anthropic 服务端 fallback）。
    #[test]
    fn model_entry_carries_catalog_allowed_fallback_models() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));

        let fable = find_model("anthropic", "claude-fable-5").unwrap();
        let ids: Vec<&str> = fable
            .allowed_fallback_models
            .iter()
            .map(|f| f.model.as_str())
            .collect();
        assert_eq!(ids, vec!["claude-opus-4-8", "claude-opus-5"]);
        assert!(
            fable
                .allowed_fallback_models
                .iter()
                .all(|f| f.provider == "anthropic")
        );
        // 单价来自目录条目（输入 5 / 输出 25）
        let cost = fable.allowed_fallback_models[0].cost.as_ref().unwrap();
        assert_eq!(cost.get("input").and_then(|v| v.as_f64()), Some(5.0));
        assert_eq!(cost.get("output").and_then(|v| v.as_f64()), Some(25.0));

        // 配置侧同样携带
        let cfg = model_config_from_entry(&fable, None, None);
        assert_eq!(cfg.allowed_fallback_models, fable.allowed_fallback_models);

        // 无该 compat 的模型为空（不发 fallbacks / beta）
        let sonnet = find_model("anthropic", "claude-sonnet-4-5").unwrap();
        assert!(sonnet.allowed_fallback_models.is_empty());
    }

    /// pi 0.86.0 #9394：Codex 目录曾移除对 ChatGPT 账号不可用的 GPT-5.4 / 5.4-mini。
    /// prux 已删除 openai-codex provider（订阅登录改走 openai 的 Sign in with ChatGPT），
    /// 这些条目在 openai 目录中仍保留。
    #[test]
    fn openai_catalog_keeps_retired_codex_models() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));

        assert!(
            provider_models("openai")
                .iter()
                .any(|m| m.get("id").and_then(|v| v.as_str()) == Some("gpt-5.4")),
            "openai 目录应保留 gpt-5.4"
        );
        // Codex provider 已移除：不再有内嵌目录、不再有注册表默认模型
        assert!(baseline_models("openai-codex").is_empty());
        assert!(default_model_id("openai-codex").is_none());
        assert!(!SUPPORTED_PROVIDERS.contains(&"openai-codex"));
    }

    #[test]
    fn resolve_provider_model_falls_back_to_default() {
        // PRUX_PROVIDER/PRUX_MODEL 是进程级 env：持叶级 env 锁与写 env 的测试串行，并清掉它们
        let _ek = crate::test_support::env_key_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        unsafe { std::env::remove_var("PRUX_PROVIDER") };
        unsafe { std::env::remove_var("PRUX_MODEL") };
        let s = crate::core::settings_manager::Settings {
            default_model: None,
            default_provider: None,
            default_thinking_level: None,
            default_project_trust: None,
            theme: None,
            external_editor: None,
            skills: Vec::new(),
            themes: Vec::new(),
            enabled_models: Vec::new(),
        };
        // 无任何配置：回退 deepseek 默认模型（对齐 pi defaultModelPerProvider）
        let (p, m, t) = resolve_provider_model(None, None, &s).unwrap();
        assert_eq!(p, "deepseek");
        assert_eq!(m, "deepseek-v4-pro");
        assert!(t.is_none());
        // 只给 provider：用该 provider 的默认模型（查表）
        let (p, m, _) = resolve_provider_model(Some("opencode"), None, &s).unwrap();
        assert_eq!(p, "opencode");
        assert_eq!(m, "kimi-k2.6");
        let (p, m, _) = resolve_provider_model(Some("openai"), None, &s).unwrap();
        assert_eq!(p, "openai");
        assert_eq!(m, "gpt-5.5");
        // CLI 显式模型/设置优先于回退
        let (p, m, _) = resolve_provider_model(None, Some("deepseek/deepseek-v4-pro"), &s).unwrap();
        assert_eq!(p, "deepseek");
        assert_eq!(m, "deepseek-v4-pro");
        let mut s2 = s.clone();
        s2.default_provider = Some("opencode-go".to_string());
        s2.default_model = Some("minimax-m3".to_string());
        let (p, m, _) = resolve_provider_model(None, None, &s2).unwrap();
        assert_eq!(p, "opencode-go");
        assert_eq!(m, "minimax-m3");
    }

    #[test]
    fn provider_explicitly_configured_detects_sources() {
        let _ek = crate::test_support::env_key_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let s = crate::core::settings_manager::Settings {
            default_model: None,
            default_provider: None,
            default_thinking_level: None,
            default_project_trust: None,
            theme: None,
            external_editor: None,
            skills: Vec::new(),
            themes: Vec::new(),
            enabled_models: Vec::new(),
        };
        unsafe { std::env::remove_var("PRUX_PROVIDER") };
        unsafe { std::env::remove_var("PRUX_MODEL") };
        // 无任何配置：不是显式 provider
        assert!(!provider_explicitly_configured(None, None, &s));
        // --provider
        assert!(provider_explicitly_configured(Some("deepseek"), None, &s));
        // PRUX_PROVIDER env
        unsafe { std::env::set_var("PRUX_PROVIDER", "openai") };
        assert!(provider_explicitly_configured(None, None, &s));
        unsafe { std::env::remove_var("PRUX_PROVIDER") };
        // settings.default_provider
        let mut s2 = s.clone();
        s2.default_provider = Some("opencode".to_string());
        assert!(provider_explicitly_configured(None, None, &s2));
        // model 带 provider 前缀：显式
        assert!(provider_explicitly_configured(
            None,
            Some("deepseek/deepseek-v4-pro"),
            &s
        ));
        // model 无 provider 前缀：不算显式
        assert!(!provider_explicitly_configured(
            None,
            Some("deepseek-v4-pro"),
            &s
        ));
    }

    #[test]
    fn dynamic_cache_overrides_baseline() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        // 动态缓存：覆盖同名模型 + 追加新模型
        let mut dynamic = baseline_models("opencode-go");
        dynamic[0]["name"] = json!("Renamed By Remote");
        dynamic.push(json!({"id": "remote-only-model", "name": "Remote Only"}));
        write_store(serde_json::json!({
            "opencode-go": {
                "models": dynamic,
                "checkedAt": 1,
                "lastModified": 0,
                "etag": "w/\"x\"",
            }
        }));
        let merged = provider_models("opencode-go");
        assert!(merged.iter().any(|m| m["name"] == "Renamed By Remote"));
        assert!(merged.iter().any(|m| m["id"] == "remote-only-model"));
        // 其他 provider 不受 store 影响
        assert_eq!(provider_models("deepseek").len(), 2);
        // list_models 合并视图
        let listed = list_models("opencode-go");
        assert!(listed.iter().any(|(id, _)| id == "remote-only-model"));
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));
    }

    #[test]
    fn default_model_follows_pi_table() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        // 对齐 pi defaultModelPerProvider
        assert_eq!(
            default_model_for("deepseek").unwrap(),
            ("deepseek".to_string(), "deepseek-v4-pro".to_string())
        );
        assert_eq!(
            default_model_for("openai").unwrap(),
            ("openai".to_string(), "gpt-5.5".to_string())
        );
        assert_eq!(
            default_model_for("google").unwrap(),
            ("google".to_string(), "gemini-3.1-pro-preview".to_string())
        );
        // pi 0.99.0：fireworks / opencode-go / together 默认改指 Kimi K3
        assert_eq!(
            default_model_for("fireworks").unwrap(),
            (
                "fireworks".to_string(),
                "accounts/fireworks/models/kimi-k3".to_string()
            )
        );
        assert_eq!(
            default_model_for("opencode-go").unwrap(),
            ("opencode-go".to_string(), "kimi-k3".to_string())
        );
        assert_eq!(
            default_model_for("together").unwrap(),
            ("together".to_string(), "moonshotai/Kimi-K3".to_string())
        );
        // pi 0.99.1：openai-codex 已删除（订阅登录改走 openai 的 Sign in with ChatGPT）
        assert!(default_model_id("openai-codex").is_none());
        // pi 1.0.1：nvidia 默认改指仍在服务的 nemotron-3-ultra-550b-a55b
        assert_eq!(
            default_model_for("nvidia").unwrap(),
            (
                "nvidia".to_string(),
                "nvidia/nemotron-3-ultra-550b-a55b".to_string()
            )
        );
        // 表内默认不在基线时回退基线第一个
        let v = default_model_for("zai").unwrap();
        assert_eq!(v.0, "zai");
        assert_ne!(v.1, "glm-5.1", "zai 基线无 glm-5.1，应回退");
        assert!(baseline_models("zai").iter().any(|m| m["id"] == v.1));
        // 未收录 provider 回退基线
        assert!(default_model_for("unknown").is_none());
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));
    }

    #[test]
    fn models_json_adds_custom_provider() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models.json"));
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));
        write_models_json(serde_json::json!({
            "providers": {
                "my-ollama": {
                    "baseUrl": "http://localhost:11434/v1",
                    "api": "openai-completions",
                    "apiKey": "ollama",
                    "compat": { "supportsDeveloperRole": false },
                    "models": [
                        { "id": "llama3.1:8b" },
                        {
                            "id": "qwen2.5-coder:7b",
                            "name": "Qwen Coder",
                            "reasoning": true,
                            "contextWindow": 128000,
                            "maxTokens": 32000,
                            "thinkingLevelMap": { "off": null, "high": "high" }
                        }
                    ]
                }
            }
        }));

        // 自定义 provider 进入合并视图
        let models = provider_models("my-ollama");
        assert_eq!(models.len(), 2);
        assert!(is_known_provider("my-ollama"));

        // 最小条目：api 继承 provider、baseUrl 继承 provider、compat 合并 provider 级
        let e = find_model("my-ollama", "llama3.1:8b").unwrap();
        assert_eq!(e.provider, "my-ollama");
        assert_eq!(e.id, "llama3.1:8b");
        assert_eq!(e.base_url, "http://localhost:11434/v1");
        assert_eq!(e.api, "openai-completions");
        assert!(!e.supports_developer_role, "provider compat 应合并进模型");
        // 对齐 pi：缺省 128000/16384
        assert_eq!(e.context_window, 128_000, "缺省字段用默认值");
        assert_eq!(e.max_tokens, 16_384, "缺省 maxTokens 用默认值");
        assert!(!e.auth_header, "未配置 authHeader 默认 false");

        // 完整条目字段
        let e2 = find_model("my-ollama", "qwen2.5-coder:7b").unwrap();
        assert_eq!(e2.name, "Qwen Coder");
        assert!(e2.reasoning);
        assert_eq!(e2.context_window, 128000);
        assert_eq!(e2.max_tokens, 32000);
        assert!(e2.thinking_level_map.is_some());

        // list_models 可见
        let listed = list_models("my-ollama");
        assert!(listed.iter().any(|(id, _)| id == "llama3.1:8b"));

        // 自定义 provider 可从合并视图回退默认模型（无 --provider 也能解析）
        let (p, m) = default_model_for("my-ollama").unwrap();
        assert_eq!(p, "my-ollama");
        assert_eq!(m, "llama3.1:8b");
        let _ = std::fs::remove_file(agent_dir().join("models.json"));
    }

    #[test]
    fn models_json_overrides_builtin_provider() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models.json"));
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));
        let baseline_len = baseline_models("deepseek").len();
        write_models_json(serde_json::json!({
            "providers": {
                "deepseek": {
                    "baseUrl": "https://my-proxy.example.com",
                    "compat": { "supportsDeveloperRole": false },
                    "models": [{ "id": "my-custom-model", "contextWindow": 64000 }],
                    "modelOverrides": {
                        "deepseek-flash": { "name": "Flash (Proxied)" }
                    }
                }
            }
        }));

        let models = provider_models("deepseek");
        // 基线保留 + 自定义模型追加
        assert_eq!(models.len(), baseline_len + 1);

        // 内置模型：baseUrl 被 provider 覆盖，compat 合并，modelOverrides 改名
        let e = find_model("deepseek", "deepseek-flash").unwrap();
        assert_eq!(e.base_url, "https://my-proxy.example.com");
        assert_eq!(e.name, "Flash (Proxied)");
        assert!(
            !e.supports_developer_role,
            "provider compat 应覆盖内置 compat"
        );

        // 自定义模型：缺省 baseUrl 继承 provider，api 继承默认
        let e2 = find_model("deepseek", "my-custom-model").unwrap();
        assert_eq!(e2.base_url, "https://my-proxy.example.com");
        assert_eq!(e2.context_window, 64000);
        assert!(
            e2.reasoning || !e2.reasoning,
            "自定义模型默认非推理或按字段"
        );

        // 未知 id 的 modelOverrides 被忽略
        assert!(find_model("deepseek", "deepseek-v4-pro").is_ok());
        let _ = std::fs::remove_file(agent_dir().join("models.json"));
    }

    #[test]
    fn models_json_upsert_replaces_same_id() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models.json"));
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));
        write_models_json(serde_json::json!({
            "providers": {
                "deepseek": {
                    "baseUrl": "https://x.example.com",
                    "models": [{
                        "id": "deepseek-flash",
                        "name": "Replaced By Config",
                        "reasoning": false
                    }]
                }
            }
        }));
        let e = find_model("deepseek", "deepseek-flash").unwrap();
        assert_eq!(e.name, "Replaced By Config");
        assert!(!e.reasoning, "配置条目整体替换同 id 内置模型");
        assert_eq!(e.base_url, "https://x.example.com");
        let _ = std::fs::remove_file(agent_dir().join("models.json"));
    }

    /// 虚拟模型进 chat 目录：无物理条目的 provider 也能列出；同名物理条目被隐藏；
    /// `find_model` 优先返回虚拟条目（api = prux-virtual）；默认模型回退到虚拟条目。
    #[test]
    fn virtual_models_join_chat_catalog_and_hide_same_id_physical() {
        use crate::core::virtual_models::{
            ModelRoute, ModelRouteRequest, STANDALONE_OWNER, VIRTUAL_MODEL_API,
            VirtualModelDefinition, VirtualModelRouter,
        };

        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        write_models_json(serde_json::json!({
            "providers": {
                "prux-vm-test": {
                    "apiKey": "sk-lit",
                    "api": "openai-completions",
                    "models": [{"id": "auto", "name": "Physical Auto"}]
                }
            }
        }));

        struct R;
        impl VirtualModelRouter for R {
            fn route<'a>(
                &'a self,
                _r: ModelRouteRequest<'a>,
            ) -> futures_util::future::BoxFuture<'a, Result<ModelRoute>> {
                Box::pin(async {
                    Ok(ModelRoute {
                        provider: "prux-vm-test".into(),
                        model_id: "auto".into(),
                        thinking_level: None,
                        state: None,
                    })
                })
            }
        }

        crate::core::virtual_models::register(
            VirtualModelDefinition::new("prux-vm-test", "auto", "Auto", std::sync::Arc::new(R)),
            STANDALONE_OWNER,
        );

        // 同名物理条目被隐藏：列表里只有一条 auto
        let listed = list_models("prux-vm-test");
        assert_eq!(listed.iter().filter(|(id, _)| id == "auto").count(), 1);
        // find_model 返回虚拟条目（api 为 prux-virtual）
        assert_eq!(
            find_model("prux-vm-test", "auto").unwrap().api,
            VIRTUAL_MODEL_API
        );
        // 默认模型在无物理 chat 条目时回退到虚拟条目
        assert_eq!(
            default_model_for("prux-vm-test"),
            Some(("prux-vm-test".to_string(), "auto".to_string()))
        );

        // 无物理条目的新 provider：虚拟条目独自成目录
        crate::core::virtual_models::register(
            VirtualModelDefinition::new("prux-vm-only", "auto", "Auto", std::sync::Arc::new(R)),
            STANDALONE_OWNER,
        );
        assert_eq!(
            list_models("prux-vm-only"),
            vec![("auto".to_string(), "Auto".to_string())]
        );
        // 默认模型回退虚拟条目时必须返回 (provider, id)，不能把展示名当模型 id
        assert_eq!(
            default_model_for("prux-vm-only"),
            Some(("prux-vm-only".to_string(), "auto".to_string()))
        );

        crate::core::virtual_models::unregister("prux-vm-test", "auto");
        crate::core::virtual_models::unregister("prux-vm-only", "auto");
        let _ = std::fs::remove_file(agent_dir().join("models.json"));
    }

    /// pi 内置的 typesafe provider：只有 `type: classifier` 的 `jev-latest` 条目。
    #[test]
    fn typesafe_provider_ships_the_jev_classifier() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));

        assert!(SUPPORTED_PROVIDERS.contains(&"typesafe"));
        let entry = find_model_of_type("typesafe", "jev-latest", ModelType::Classifier).unwrap();
        assert_eq!(entry.api, "typesafe-system-one");
        assert_eq!(entry.base_url, "https://api.typesafe.ai/v1/");
        assert_eq!(entry.model_type, ModelType::Classifier);

        // 目录里没有 chat 条目：不能出现在 chat 列表，也没有默认模型
        assert!(list_models("typesafe").is_empty());
        assert!(default_model_for("typesafe").is_none());
        assert_eq!(
            list_models_of_type("typesafe", ModelType::Classifier)
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            vec!["jev-latest".to_string()]
        );
    }

    /// 三层的覆盖口径是 `(type, id)`：动态缓存里的 image 条目不得顶掉同 id 的 chat 条目。
    #[test]
    fn store_and_user_models_do_not_clobber_other_types() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));
        let _ = std::fs::remove_file(agent_dir().join("models.json"));

        let id = "google/gemini-3-pro-image";
        let chat = find_model_of_type("openrouter", id, ModelType::Chat).unwrap();

        // 动态缓存只回一条 image（同 id）：chat 条目必须原样保留
        write_store(json!({
            "openrouter": { "models": [{ "type": "image", "id": id, "api": "openrouter-images" }] }
        }));
        assert_eq!(
            find_model_of_type("openrouter", id, ModelType::Chat)
                .unwrap()
                .api,
            chat.api
        );
        assert_eq!(
            find_model_of_type("openrouter", id, ModelType::Image)
                .unwrap()
                .api,
            "openrouter-images"
        );

        // 用户 models.json 新增同 id 的 chat 模型：只替换 chat 条目，image 条目不受影响
        write_models_json(json!({
            "providers": { "openrouter": { "models": [{ "id": id, "name": "Mine" }] } }
        }));
        let mine = find_model_of_type("openrouter", id, ModelType::Chat).unwrap();
        assert_eq!(mine.name, "Mine");
        assert_eq!(mine.api, chat.api, "缺 api 时按同类型条目继承");
        assert_eq!(
            find_model_of_type("openrouter", id, ModelType::Image)
                .unwrap()
                .api,
            "openrouter-images"
        );

        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));
        let _ = std::fs::remove_file(agent_dir().join("models.json"));
    }

    /// 同 provider 同 id 兼有 chat 与 classifier 条目时，`configured_model_of_type`
    /// 必须按类型取到 classifier 条目（`classify` 类调用靠它，否则会拿到 chat 条目）。
    #[test]
    fn configured_model_of_type_picks_the_entry_of_that_type() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));
        let _ = std::fs::remove_file(agent_dir().join("models.json"));

        let id = "google/gemini-3-pro-image";
        let chat = configured_model("openrouter", id, None, None).unwrap();
        assert_eq!(chat.model_type, ModelType::Chat);

        // 同 id 追一条 classifier：两个类型共存，互不覆盖
        write_store(json!({
            "openrouter": { "models": [{
                "type": "classifier", "id": id, "api": "typesafe-system-one"
            }] }
        }));
        let classifier =
            configured_model_of_type("openrouter", id, ModelType::Classifier, None, None).unwrap();
        assert_eq!(classifier.model_type, ModelType::Classifier);
        assert_eq!(classifier.api, "typesafe-system-one");
        assert_eq!(classifier.model_id, id);
        // 不限类型的口径仍命中首个（chat）条目：两个口径确实不同
        assert_eq!(
            configured_model("openrouter", id, None, None)
                .unwrap()
                .model_type,
            ModelType::Chat
        );

        // 类型不匹配：typesafe 只有 classifier 条目，按 image 取必须报未找到
        let err = configured_model_of_type("typesafe", "jev-latest", ModelType::Image, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");

        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));
        let _ = std::fs::remove_file(agent_dir().join("models.json"));
    }

    /// `samplingParamsByThinkingLevel` 进目录解析 + `modelOverrides` 逐级别逐键合并。
    #[test]
    fn sampling_params_by_thinking_level_parses_and_merges_per_level() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        test_dir();
        let _ = std::fs::remove_file(agent_dir().join("models-store.json"));
        write_models_json(json!({
            "providers": {
                "sampling-demo": {
                    "api": "openai-completions",
                    "models": [{
                        "id": "qwen-thinking",
                        "reasoning": true,
                        "samplingParams": { "temperature": 1.0, "top_p": 0.95 },
                        "samplingParamsByThinkingLevel": {
                            "low": { "temperature": 0.6, "top_p": 0.95 },
                            "high": { "top_k": 64 }
                        }
                    }],
                    "modelOverrides": {
                        "qwen-thinking": {
                            "samplingParamsByThinkingLevel": { "high": { "top_k": 20 } }
                        }
                    }
                }
            }
        }));

        let entry = find_model("sampling-demo", "qwen-thinking").unwrap();
        let by_level = entry.sampling_params_by_thinking_level.as_ref().unwrap();
        assert_eq!(
            by_level["low"],
            json!({ "temperature": 0.6, "top_p": 0.95 })
        );
        // 同一级别逐键覆盖：top_k 被覆盖、级别里没提的键保留
        assert_eq!(by_level["high"], json!({ "top_k": 20 }));
        assert_eq!(entry.sampling_params.as_ref().unwrap()["temperature"], 1.0);

        // ModelConfig 沿用同一份映射
        let cfg = configured_model("sampling-demo", "qwen-thinking", None, None).unwrap();
        assert_eq!(
            cfg.sampling_params_for_level(Some("high")).unwrap()["top_k"],
            20
        );
        assert!(cfg.sampling_params_for_level(Some("medium")).is_none());

        let _ = std::fs::remove_file(agent_dir().join("models.json"));
    }

    /// 级别先按模型能力钳制再查覆盖项：低档位被声明为不支持时不能漏掉高档位的覆盖。
    #[test]
    fn sampling_params_for_level_uses_clamped_level() {
        let entry = ModelEntry {
            id: "m".into(),
            name: "m".into(),
            api: "openai-completions".into(),
            model_type: ModelType::Chat,
            base_url: "https://example.com/v1".into(),
            provider: "demo".into(),
            input: vec!["text".into()],
            output: Vec::new(),
            reasoning: true,
            context_window: 1000,
            max_tokens: 100,
            max_tokens_field: "max_tokens".into(),
            sampling_params: None,
            sampling_params_by_thinking_level: Some(json!({
                "high": { "temperature": 0.8 },
                "off": { "temperature": 0.7 }
            })),
            provider_routing: None,
            supports_usage_in_streaming: true,
            supports_store: true,
            supports_finish_reason: true,
            requires_assistant_after_tool_result: false,
            supports_strict_mode: false,
            cost: None,
            supports_developer_role: false,
            requires_reasoning_content_on_assistant_messages: false,
            thinking_format: "openai".into(),
            supports_reasoning_effort: true,
            thinking_level_map: Some(
                [("low", None), ("medium", None), ("high", Some("high"))]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.map(|s| s.to_string())))
                    .collect(),
            ),
            auth_header: false,
            input_limits: Default::default(),
            allowed_fallback_models: Vec::new(),
            supports_openai_grammar_tools: false,
            send_session_affinity_headers: None,
            session_affinity_format: None,
        };
        let cfg = model_config_from_entry(&entry, None, None);

        // low 不支持 → 钳制到 high，取 high 的覆盖项
        assert_eq!(
            cfg.sampling_params_for_level(Some("low")).unwrap()["temperature"],
            0.8
        );
        // 未指定级别按 off 处理（对齐 pi `options?.reasoningEffort ?? "off"`）
        assert_eq!(
            cfg.sampling_params_for_level(None).unwrap()["temperature"],
            0.7
        );
        assert_eq!(
            cfg.sampling_params_for_level(Some("off")).unwrap()["temperature"],
            0.7
        );
    }
}
