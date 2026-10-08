//! `codemode` 内置扩展：模型写 JavaScript，在 QuickJS 沙箱里以 `tools.<name>()` 编排其它工具。
//!
//! 配置（`mode` / `inlineBudget`）在 `agent_dir()/extensions/codemode.json`，见 [`config`]。

mod config;
mod declarations;
mod description;
mod sandbox;
mod source;
mod store;

use super::{
    EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_CODEMODE, command_arg,
    util::{notify_text, value_text},
};
use crate::{
    APP_NAME,
    core::{
        auth,
        extensions::{
            Extension, ExtensionCommand, ExtensionMode, ExtensionSetting, ExtensionTool,
            GrammarSampling, SubcommandDef, ToolExecCtx, ToolExposure, ToolLoadout,
            ToolLoadoutChanges, UiNotifyLevel,
        },
        model_resolver,
        model_resolver::ModelEntry,
        provider::{
            self, AgentMessage, ClassifierContext, ImageContent, ModelConfig, ModelType, Usage,
            usage::combine_usage,
        },
        tools::{ToolError, ToolResult, ToolResultAttachment},
    },
    extensions::tool_search,
    modes::interactive::{app::App, handlers::register_slash_command},
};
use description::CHARS_PER_TOKEN;
use futures_util::future::BoxFuture;
use sandbox::{SandboxRequest, ScriptError, ScriptGlobal, ScriptHost, ScriptOutput, ScriptTool};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex as StdMutex},
    time::Instant,
};
use tokio::sync::Semaphore;

/// 工具名（与扩展名一致）
pub const TOOL_NAME: &str = "codemode";

/// 扩展名
const EXT: &str = "codemode";

/// 斜杠命令名（`/codemode`）
const CMD: &str = "codemode";

/// `models.classify` / `models.generateImages` 共用的并发上限
const MAX_CONCURRENT_MODEL_CALLS: usize = 4;

/// `models.*` 记录里 `error` 文本的字符上限
const MODEL_CALL_ERROR_PREVIEW_CHARS: usize = 500;

/// `models.*` 记录里参数描述文本的字符上限
const MODEL_CALL_ARG_PREVIEW_CHARS: usize = 200;

/// `models.classify` 全局名（调用记录与错误文案共用）
const MODELS_CLASSIFY: &str = "models.classify";

/// `models.generateImages` 全局名（调用记录与错误文案共用）
const MODELS_GENERATE_IMAGES: &str = "models.generateImages";

/// `/codemode` 子命令（顺序 = 输入框候选顺序）。
///
/// 无参 = 开关设置面板（面板已展示全部配置），与 `settings` 同义，因此不声明 `settings` 之外的配置键。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "settings",
        description: "Open the codemode settings panel (same as `/codemode` with no arguments)",
    },
    SubcommandDef {
        name: "show",
        description: "Print the current mode and inlineBudget (config file path included)",
    },
];

/// 脚本输出的默认 token 预算
const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 10_000;

/// 自声明工厂：linkme 分布式切片
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static CODEMODE_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_CODEMODE,
    make: || -> Arc<dyn Extension> { Arc::new(Codemode) },
};

/// `codemode` 扩展本体（无状态：脚本状态全在沙箱与 `store` 里）。
pub struct Codemode;

impl Extension for Codemode {
    /// 扩展名 `codemode`。
    fn name(&self) -> &str {
        EXT
    }

    /// `/extension` 面板里的一句话说明。
    fn description(&self) -> &str {
        "Run JavaScript that orchestrates the other tools in a QuickJS sandbox"
    }

    /// 默认关闭：注册但不激活，在 `/extension` 面板开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 仅在 Dev / Creator 模式下可用（与 `tool-search` 等编排类扩展一致）。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 提供唯一的 `codemode` 工具；描述在装配工具表时由
    /// [`Extension::prepare_loadout`] 按当前可调工具动态改写。
    fn tools(&self) -> Vec<ExtensionTool> {
        vec![tool_definition()]
    }

    /// 本扩展执行脚本：它调起的嵌套工具算「脚本调用」（`codemode` 暴露方式的工具据此可见）。
    fn hosts_scripts(&self) -> bool {
        true
    }

    /// 按当前装载改写模型看到的声明：`codemode.mode = on` 时给可调工具补脚本调用样例，
    /// `only` 时把它们的声明从请求里摘掉（改由 `codemode` 的描述列全）。
    fn prepare_loadout(&self, loadout: &ToolLoadout) -> ToolLoadoutChanges {
        let cfg = config::load_config();
        let deferred: HashSet<String> = loadout
            .callable
            .iter()
            .filter(|t| t.exposure == ToolExposure::Deferred)
            .map(|t| t.name.clone())
            .collect();

        // 「direct」= 声明中带 `direct` 曝光的内置/扩展工具（`model-only` 不入，它不能被脚本调）。
        let is_direct = |name: &str| {
            loadout
                .declared
                .iter()
                .find(|t| t.name == name)
                .is_some_and(|t| t.exposure == ToolExposure::Direct)
        };

        let changes = description::build_loadout(
            TOOL_NAME,
            &loadout.declared,
            &loadout.callable,
            &is_direct,
            &deferred,
            cfg.mode(),
            Some(cfg.inline_budget()),
        );

        ToolLoadoutChanges {
            descriptions: changes.descriptions.into_iter().collect(),
            hidden_declarations: changes.hidden_declarations,
        }
    }

    /// 执行一次脚本（异步：脚本内并发调工具）。
    fn execute_tool_async(
        &self,
        name: String,
        args: Value,
        ctx: ToolExecCtx,
    ) -> BoxFuture<'static, std::result::Result<ToolResult, ToolError>> {
        Box::pin(async move {
            if name != TOOL_NAME {
                return Err(ToolError(format!("codemode: unknown tool: {name}")));
            }

            let Some(code) = args.get("code").and_then(|v| v.as_str()) else {
                return Err(ToolError(
                    "codemode: missing string argument `code`".to_string(),
                ));
            };
            run_script(code, ctx).await
        })
    }

    /// 会话切换：清掉上一个会话的 `store()` 快照（新会话在下次 `on_session_start` 重建）。
    fn on_session_switched(&self, _session_path: Option<&str>, _messages: &[AgentMessage]) {
        store::reset();
    }

    /// `/extension` 面板里的设置项：`mode` 与 `inlineBudget`（见 [`config`]）。
    fn settings(&self) -> Vec<ExtensionSetting> {
        config::panel_settings()
    }

    /// 应用面板选择并立即落盘（面板是「设置」语义）。
    fn apply_setting(&self, key: &str, value: &str) -> std::result::Result<(), String> {
        config::apply_panel_choice(key, value).map(|_| ())
    }

    /// 注册 `/codemode`：无参 / `settings` 开关设置面板，`show` 打印当前配置。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description:
                "Open the codemode settings panel; /codemode show prints the current config"
                    .to_string(),
            busy_safe: true, // handler 只读配置 / 开关设置面板，不锁 agent，忙碌安全
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 把 `/codemode` 的执行入口接线到 TUI 命令注册表。
    fn on_registered(&self) {
        register_slash_command(EXT, CMD, command_codemode);
    }
}

/// `/codemode` 命令处理：无参 / `settings` 开关设置面板，`show` 打印当前配置。
/// 返回值表示是否退出程序（此命令恒为 `false`）。
fn command_codemode(app: &mut App, raw: &str) -> bool {
    match command_arg(raw).trim() {
        "" | "settings" => {
            if app.ext_settings.is_open_for(EXT) {
                app.ext_settings.close();
            } else {
                app.ext_settings.open(EXT);
            }
            app.dirty = true;
        }
        "show" => {
            let cfg = config::load_config();
            let lines = [
                "codemode config".to_string(),
                format!("  mode:          {}", cfg.mode_label()),
                format!("  inlineBudget:  {}", cfg.inline_budget()),
                format!("  file:          {}", config::config_path().display()),
            ];
            notify_text(&lines.join("\n"), UiNotifyLevel::Info);
        }
        other => notify_text(
            &format!("codemode: unknown subcommand: {other}"),
            UiNotifyLevel::Error,
        ),
    }
    false
}

/// `codemode` 工具定义：`{code: string}` 参数、`model-only` 曝光、源码语法约束采样。
fn tool_definition() -> ExtensionTool {
    let mut tool = ExtensionTool::simple(
        TOOL_NAME,
        &description::create_description(&[], &HashSet::new(), None),
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Raw JavaScript source. Top-level await and return work. May start with a `// @options: {\"max_output_tokens\": 1000}` line."
                }
            },
            "required": ["code"]
        }),
        "Run JavaScript that calls other tools (chains, loops, Promise.all, filtering large results)",
    );
    tool.exposure = ToolExposure::ModelOnly; // 模型可见，但脚本不能再起脚本
    // 模型声明 `compat.supportsOpenAIGrammarTools` 时，`code` 以裸 JS 下发（不用 JSON 转义）
    tool.grammar_sampling = Some(GrammarSampling {
        openai_lark: Some(source::CODEMODE_SOURCE_GRAMMAR.to_string()),
        openai_regex: None,
    });
    tool.prompt_guidelines = vec![
        "Use codemode to batch or chain several tool calls, or to filter large tool output down to what you need, instead of issuing many individual tool calls. Batch independent calls in one codemode call using await Promise.allSettled([...])."
            .to_string(),
    ];
    tool
}

/// `models.*` 全局的副作用汇总：模型调用记录与用量合计（脚本结束后并入工具结果）。
#[derive(Default)]
struct ModelGlobalsState {
    /// 模型调用记录（分类器与图片生成共用一张表）
    calls: Vec<Value>,
    /// 已跑过的分类器调用数，用于生成记录 id 的序号。
    classify_count: usize,
    /// 已跑过的图片生成调用数，用于生成记录 id 的序号。
    image_count: usize,
    /// `models.generateImages()` 返回的图片张数：脚本一张都没展示时宿主补一条提示。
    generated_images: usize,
    /// 模型调用用量合计，并入 codemode 结果后计入会话成本；无调用或服务未报用量时为 None。
    usage: Option<Usage>,
}

/// 脚本可调工具/全局的宿主实现：工具走 [`ToolExecCtx::execute_tool`]，
/// 全局（`searchTools` / `describeTool` / `models.*`）在本模块内实现。
#[derive(Clone)]
struct Host {
    /// 本次工具执行的上下文（嵌套调用、取消标志）
    ctx: ToolExecCtx,
    /// 可调工具（查 `output_schema`）
    tools: Arc<HashMap<String, ExtensionTool>>,
    /// 工具名 → 声明样例（`searchTools` / `describeTool` 用）
    samples: Arc<HashMap<String, String>>,
    /// `models.*` 的调用记录与用量（脚本结束后由 [`run_script`] 取走）。
    model_globals: Arc<StdMutex<ModelGlobalsState>>,
    /// 模型调用并发闸门（分类器与图片生成共用）：最多 [`MAX_CONCURRENT_MODEL_CALLS`] 个同时在跑。
    model_call_gate: Arc<Semaphore>,
}

impl Host {
    /// 工具结果的脚本侧取值：声明了 `outputSchema` 且带结构化结果时用它（错误结果同样适用，
    /// 例如 MCP 的 `CallToolResult`）；否则失败结果抛错（`Err` 的消息即脚本收到的 Error），成功结果取文本。
    fn script_value(result: &ToolResult, tool: Option<&ExtensionTool>) -> Result<Value, String> {
        if let Some(tool) = tool
            && tool.output_schema.is_some()
            && let Some(structured) = &result.structured_content
        {
            return Ok(structured.clone());
        }

        if result.is_error {
            let err = if result.text.is_empty() {
                match tool {
                    Some(tool) => format!("Tool \"{}\" failed", tool.name),
                    None => "Tool failed".to_string(),
                }
            } else {
                result.text.clone()
            };
            return Err(err);
        }

        Ok(Value::String(result.text.clone()))
    }

    /// `searchTools(query, {limit, namespace})`：BM25 检索可调工具，返回 `{name, description}`。
    fn search_tools(&self, args: &[Value]) -> Result<Value, String> {
        let Some(query) = args.first().and_then(|v| v.as_str()) else {
            return Err("searchTools() expects a query string".to_string());
        };

        let opts = args.get(1).and_then(|v| v.as_object());
        let limit = match opts.and_then(|o| o.get("limit")) {
            Some(v) => match v.as_u64() {
                Some(n) if n > 0 => n as usize,
                _ => return Err("searchTools() limit must be a positive integer".to_string()),
            },
            None => tool_search::DEFAULT_LIMIT,
        };
        let namespace = match opts.and_then(|o| o.get("namespace")) {
            Some(Value::Null) | None => None,
            // 比对的是归一形式：`mcp__dev-radius` / `dev_radius` 等写法都指向同一命名空间
            Some(Value::String(s)) => Some(namespace_key(s)),
            Some(_) => return Err("searchTools() namespace must be a string".to_string()),
        };

        let documents: Vec<_> = self
            .tools
            .values()
            .filter(|t| {
                namespace
                    .as_ref()
                    .map(|ns| {
                        t.namespace
                            .as_ref()
                            .is_some_and(|own| namespace_key(own) == *ns)
                    })
                    .unwrap_or(true)
            })
            .map(tool_search::tool_document)
            .collect();

        let matches = tool_search::rank(query, &documents, limit);
        Ok(Value::Array(
            matches
                .into_iter()
                .map(|(name, _)| {
                    json!({
                        "name": declarations::to_codemode_identifier(&name),
                        "description": self.samples.get(&name).cloned().unwrap_or_default(),
                    })
                })
                .collect(),
        ))
    }

    /// `describeTool(name)`：按真名或脚本标识符查声明样例。
    fn describe_tool(&self, args: &[Value]) -> Result<Value, String> {
        let Some(name) = args.first().and_then(|v| v.as_str()) else {
            return Err("describeTool() expects a tool name".to_string());
        };

        let found = self
            .tools
            .values()
            .find(|t| t.name == name || declarations::to_codemode_identifier(&t.name) == name);

        Ok(match found.and_then(|t| self.samples.get(&t.name)) {
            Some(sample) => Value::String(sample.clone()),
            None => Value::Null,
        })
    }

    /// 某命名空间下的可调工具清单，供内联预算不足时按命名空间下钻。
    ///
    /// `name` 接受 `mcp__dev-radius` / `mcp__dev_radius` / `dev-radius` / `dev_radius` 等写法
    /// （见 [`namespace_key`]）；找不到该命名空间时返回 `null`。
    fn describe_namespace(&self, args: &[Value]) -> Result<Value, String> {
        let Some(name) = args.first().and_then(|v| v.as_str()) else {
            return Err("describe_namespace() expects a namespace name".to_string());
        };

        Ok(match namespace_tools(&self.tools, &self.samples, name) {
            Some((namespace, tools)) => json!({
                "namespace": namespace,
                "tools": tools
                    .into_iter()
                    .map(|(name, description)| json!({ "name": name, "description": description }))
                    .collect::<Vec<_>>(),
            }),
            None => Value::Null,
        })
    }

    /// 本次 `codemode` 调用自身的 id（`models.classify` 记录的 id 前缀）。
    fn codemode_call_id(&self) -> String {
        self.ctx
            .parent_tool_call_id
            .clone()
            .unwrap_or_else(|| TOOL_NAME.to_string())
    }

    /// `models.getModelsOfType(type, provider?)` / `models.getAvailableOfType(...)`：目录条目；`
    /// available_only` 时只保留有凭据的 provider
    fn models_of_type(&self, args: &[Value], available_only: bool) -> Result<Value, String> {
        let kind = model_type_arg(args.first())?;
        let provider = provider_arg(args.get(1))?;
        Ok(Value::Array(catalog_models(kind, provider, available_only)))
    }

    /// `models.getModelOfType(type, provider, id)`：单个目录条目；找不到时返回 `null`
    fn model_of_type(&self, args: &[Value]) -> Result<Value, String> {
        let kind = model_type_arg(args.first())?;
        let expected =
            || "models.getModelOfType() expects a type, a provider, and an id".to_string();
        let provider = args.get(1).and_then(|v| v.as_str()).ok_or_else(expected)?;
        let id = args.get(2).and_then(|v| v.as_str()).ok_or_else(expected)?;
        Ok(catalog_model(kind, provider, id))
    }

    /// `models.classify(model, context)`：跑一次分类器。
    ///
    /// 只用 `model` 里的 `provider` / `id`（脚本给的 baseUrl/headers 一律忽略，否则可能把凭据送给脚本）；
    /// 未知模型与参数错误抛错，provider 侧失败不抛（结果里 `stopReason == "error"`）。
    /// 用量并入 codemode 结果（计入会话成本），调用本身也记入 `details.modelCalls`。
    async fn classify(&self, args: &[Value]) -> Result<Value, String> {
        let model = resolve_model_call(MODELS_CLASSIFY, ModelType::Classifier, args.first())?;
        let context: ClassifierContext =
            serde_json::from_value(args.get(1).cloned().unwrap_or(Value::Null))
                .map_err(|e| format!("models.classify() expects a ClassifierContext: {e}"))?;

        let index = self.begin_model_call(MODELS_CLASSIFY, &model_ref(&model));
        let _permit = self.model_call_gate.acquire().await.ok();
        let started = Instant::now();
        let result = provider::classify(&model, &context).await;

        self.finish_model_call(
            index,
            started.elapsed(),
            &result.stop_reason,
            result.error_message.as_deref(),
            result.usage.as_ref(),
        );

        serde_json::to_value(&result).map_err(|e| e.to_string())
    }

    /// `models.generateImages(model, context)`：跑一次图片生成（一次性非流式，可能跑几分钟）。
    ///
    /// 与 `classify` 同口径：只认 `provider` / `id`，第二个实参形状不对就抛错，
    /// provider 侧失败不抛（`stopReason == "error"`）；用量并入 codemode 结果。
    /// 返回的图片张数记进 [`ModelGlobalsState::generated_images`]，供脚本未展示时补提示。
    async fn generate_images(&self, args: &[Value]) -> Result<Value, String> {
        let model = resolve_model_call(MODELS_GENERATE_IMAGES, ModelType::Image, args.first())?;
        let input = images_context_arg(args.get(1))?;

        let index = self.begin_model_call(MODELS_GENERATE_IMAGES, &model_ref(&model));
        let _permit = self.model_call_gate.acquire().await.ok();
        let started = Instant::now();
        let result = provider::generate_images(&model, &input).await;

        let images = result
            .output
            .iter()
            .filter(|block| matches!(block, ImageContent::Image { .. }))
            .count();
        if images > 0 {
            self.model_globals.lock().unwrap().generated_images += images;
        }

        self.finish_model_call(
            index,
            started.elapsed(),
            &result.stop_reason,
            result.error_message.as_deref(),
            result.usage.as_ref(),
        );

        serde_json::to_value(&result).map_err(|e| e.to_string())
    }

    /// 登记一条 `models.*` 调用记录，返回它在 `details.modelCalls` 里的下标。
    ///
    /// `ref_text` 写进记录的 `args`：只有 `provider/id`，不含提示词与图片数据。
    fn begin_model_call(&self, global: &str, ref_text: &str) -> usize {
        let mut state = self.model_globals.lock().unwrap();
        let count = if global == MODELS_GENERATE_IMAGES {
            state.image_count += 1;
            state.image_count
        } else {
            state.classify_count += 1;
            state.classify_count
        };

        state.calls.push(json!({
            "id": format!("{}/{global}/{count}", self.codemode_call_id()),
            "name": global,
            "args": ref_text,
            "status": "running",
            "durationMs": 0,
        }));
        state.calls.len() - 1
    }

    /// 回填调用记录的耗时/状态/错误/费用，并把用量并入本次 codemode 结果的合计。
    fn finish_model_call(
        &self,
        index: usize,
        elapsed: std::time::Duration,
        stop_reason: &str,
        error_message: Option<&str>,
        usage: Option<&Usage>,
    ) {
        let mut state = self.model_globals.lock().unwrap();
        if let Some(record) = state.calls.get_mut(index).and_then(|v| v.as_object_mut()) {
            record.insert("durationMs".to_string(), json!(elapsed.as_millis() as u64));
            record.insert(
                "status".to_string(),
                json!(if stop_reason == "stop" { "ok" } else { "error" }),
            );

            if let Some(message) = error_message {
                record.insert(
                    "error".to_string(),
                    json!(truncate_chars(message, MODEL_CALL_ERROR_PREVIEW_CHARS)),
                );
            }

            if let Some(usage) = usage {
                record.insert("cost".to_string(), json!(usage.cost.total));
            }
        }

        if let Some(usage) = usage {
            state.usage = Some(match state.usage.take() {
                Some(prev) => combine_usage(&prev, usage),
                None => usage.clone(),
            });
        }
    }
}

/// 解析 `type` 实参（`chat` / `image` / `classifier`）；其它值报错
fn model_type_arg(value: Option<&Value>) -> Result<ModelType, String> {
    match value.and_then(|v| v.as_str()) {
        Some("chat") => Ok(ModelType::Chat),
        Some("image") => Ok(ModelType::Image),
        Some("classifier") => Ok(ModelType::Classifier),
        other => Err(format!(
            "Unknown model type {}. Use \"chat\", \"image\", or \"classifier\".",
            match other {
                Some(value) => format!("\"{value}\""),
                None => "undefined".to_string(),
            }
        )),
    }
}

/// 解析可选 `provider` 实参：缺省 / `null` 为 None，非字符串报错。
fn provider_arg(value: Option<&Value>) -> Result<Option<String>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(provider)) => Ok(Some(provider.clone())),
        Some(_) => Err("provider must be a string".to_string()),
    }
}

/// `models.*` 记录里的模型引用文本：`provider/id`。
fn model_ref(model: &ModelConfig) -> String {
    format!("{}/{}", model.provider, model.model_id)
}

/// 脚本实参的文本形式（错误文案用）：缺失的实参表现为 `undefined`。
fn describe_script_value(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(value) => truncate_chars(&value.to_string(), MODEL_CALL_ARG_PREVIEW_CHARS),
    }
}

/// 解析 `models.classify` / `models.generateImages` 的模型实参。
///
/// 只取 `provider` / `id`（脚本自带的 baseUrl/headers 一律忽略，否则等于把凭据送给脚本），
/// 并按 `kind` 限定目录条目类型（同 provider 同 id 兼有 chat 条目时，
/// 不限类型的查找会拿到 chat，然后必然报类型不符）。
///
/// 参数不是模型对象、未知模型、类型不符都返回 `Err`（消息即脚本收到的 Error），文案里带上 `getAvailableOfType` 的指引。
fn resolve_model_call(
    global: &str,
    kind: ModelType,
    value: Option<&Value>,
) -> Result<ModelConfig, String> {
    let type_name = kind.as_str();
    let list_hint = format!(
        "List the {type_name} models you can use with models.getAvailableOfType(\"{type_name}\")."
    );

    let fields = value.and_then(|v| v.as_object());
    let provider = fields
        .and_then(|f| f.get("provider"))
        .and_then(|v| v.as_str());
    let id = fields.and_then(|f| f.get("id")).and_then(|v| v.as_str());

    let (Some(provider), Some(id)) = (provider, id) else {
        // 缺实参在脚本侧表现为 null（实参以 JSON 数组过沙箱）
        let undefined_hint = match value {
            None | Some(Value::Null) => {
                " models.getModelOfType() returns undefined for an unknown provider or id."
            }
            _ => "",
        };
        return Err(format!(
            "{global}() expects a model of type \"{type_name}\" as its first argument, got {}.{undefined_hint} {list_hint}",
            describe_script_value(value),
        ));
    };

    if let Ok(model) = model_resolver::configured_model_of_type(provider, id, kind, None, None) {
        return Ok(model);
    }

    // 类型不符：点名同 provider/id 下真实存在的类型
    let actual = [ModelType::Chat, ModelType::Image, ModelType::Classifier]
        .into_iter()
        .find(|other| {
            *other != kind && model_resolver::find_model_of_type(provider, id, *other).is_ok()
        });
    Err(match actual {
        Some(other) => format!(
            "\"{provider}/{id}\" is a {} model, not a {type_name} model. {list_hint}",
            other.as_str()
        ),
        None => format!("Unknown {type_name} model \"{provider}/{id}\". {list_hint}"),
    })
}

/// 校验 `models.generateImages()` 的上下文实参并转成协议层的图片输入。
///
/// 期望形状：`{ input: [{ type: "text", text }, ...可选 { type: "image", data, mimeType }] }`，
/// `input` 必须是非空数组；形状不对时返回 `Err`，文案给出期望形状（对齐 pi 的 `checkImagesContext`）。
fn images_context_arg(value: Option<&Value>) -> Result<Vec<ImageContent>, String> {
    const EXPECTED: &str = r#"{ input: [{ type: "text", text: <prompt> }, ...optional { type: "image", data: <base64>, mimeType } references] }"#;
    let fail = |problem: String| {
        format!("{MODELS_GENERATE_IMAGES}() {problem}. Expected context: {EXPECTED}.")
    };

    let Some(context) = value.and_then(|v| v.as_object()) else {
        return Err(fail(format!(
            "expects a context object as its second argument, got {}",
            describe_script_value(value)
        )));
    };

    let input = context.get("input").and_then(|v| v.as_array());
    let Some(input) = input.filter(|blocks| !blocks.is_empty()) else {
        return Err(fail(format!(
            "context.input must be a non-empty array of blocks, got {}",
            describe_script_value(context.get("input"))
        )));
    };

    input
        .iter()
        .enumerate()
        .map(|(index, block)| {
            serde_json::from_value::<ImageContent>(block.clone()).map_err(|_| {
                fail(format!(
                    "context.input[{index}] must be a text or image block, got {}",
                    describe_script_value(Some(block))
                ))
            })
        })
        .collect()
}

/// 目录条目 → 脚本可见的 `ModelInfo`（不含凭据：`ModelEntry` 本就不携带 headers）。
fn model_info(entry: &ModelEntry) -> Value {
    json!({
        "type": entry.model_type.as_str(),
        "provider": &entry.provider,
        "id": &entry.id,
        "name": &entry.name,
        "api": &entry.api,
        "input": &entry.input,
        "contextWindow": entry.context_window,
        "maxTokens": entry.max_tokens,
        "reasoning": entry.reasoning,
    })
}

/// 目录列表：`provider` 为 None 时遍历全部 provider；`available_only` 时只保留有凭据的 provider。
fn catalog_models(kind: ModelType, provider: Option<String>, available_only: bool) -> Vec<Value> {
    let configured: Option<HashSet<String>> =
        available_only.then(|| auth::list_configured_providers().into_iter().collect());

    let providers: Vec<String> = match provider {
        Some(provider) => vec![provider],
        None => model_resolver::SUPPORTED_PROVIDERS
            .iter()
            .map(|p| (*p).to_string())
            .collect(),
    };

    let mut out: Vec<Value> = Vec::new();
    for provider in providers {
        if let Some(configured) = &configured
            && !configured.contains(&provider)
        {
            continue;
        }

        for (id, _) in model_resolver::list_models_of_type(&provider, kind) {
            if let Ok(entry) = model_resolver::find_model_of_type(&provider, &id, kind) {
                out.push(model_info(&entry));
            }
        }
    }
    out
}

/// 命名空间比较用的归一形式：去掉 `mcp__` / `mcp_` 前缀，并把 `-` 视作 `_`。
///
/// 于是 `mcp__dev-radius` / `mcp__dev_radius` / `dev-radius` / `dev_radius` 指向同一个命名空间。
fn namespace_key(name: &str) -> String {
    let name = name
        .strip_prefix("mcp__")
        .or_else(|| name.strip_prefix("mcp_"))
        .unwrap_or(name);
    name.replace('-', "_")
}

/// 某个命名空间下的工具：返回 `(命名空间原名, 按脚本标识符排序的 (标识符, 声明样例))`；
/// 没有任何工具声明该命名空间时返回 `None`。
///
/// 排序是为了让同一命名空间的两次查询结果一致（工具表是 `HashMap`，遍历顺序不稳定）。
fn namespace_tools(
    tools: &HashMap<String, ExtensionTool>,
    samples: &HashMap<String, String>,
    name: &str,
) -> Option<(String, Vec<(String, String)>)> {
    let key = namespace_key(name);
    let mut namespace: Option<String> = None;
    let mut found: Vec<(String, String)> = Vec::new();

    for tool in tools.values() {
        let Some(own) = tool.namespace.as_deref() else {
            continue;
        };
        if namespace_key(own) != key {
            continue;
        }
        namespace = Some(own.to_string());
        found.push((
            declarations::to_codemode_identifier(&tool.name),
            samples.get(&tool.name).cloned().unwrap_or_default(),
        ));
    }

    found.sort();
    namespace.map(|namespace| (namespace, found))
}

/// 单个目录条目；找不到（或类型不匹配）时返回 `null`。
fn catalog_model(kind: ModelType, provider: &str, id: &str) -> Value {
    match model_resolver::find_model_of_type(provider, id, kind) {
        Ok(entry) => model_info(&entry),
        Err(_) => Value::Null,
    }
}

/// 截断到 `max_chars` 个字符，超出时以 `...` 结尾（记录里的预览文本用）。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    let head: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{head}...")
}

impl ScriptHost for Host {
    /// 嵌套调一个工具：`Err` 的消息成为脚本里抛出的 Error。
    fn call_tool(
        &self,
        name: String,
        args: Option<Value>,
    ) -> BoxFuture<'static, std::result::Result<Value, String>> {
        let ctx = self.ctx.clone();
        let tools = self.tools.clone();

        Box::pin(async move {
            let tool = match tools.get(&name) {
                Some(tool) => tool.clone(),
                None => return Err(format!("Unknown tool \"{name}\"")),
            };

            let args = args.unwrap_or(Value::Null);
            match ctx.execute_tool(name, args).await {
                Ok(result) => Host::script_value(&result, Some(&tool)),
                Err(e) => Err(e.0),
            }
        })
    }

    /// 调一个脚本全局（`searchTools` / `describeTool` / `models.*`）。
    fn call_global(
        &self,
        name: String,
        args: Vec<Value>,
    ) -> BoxFuture<'static, std::result::Result<Value, String>> {
        // 模型调用要 await（HTTP），单独走一条路
        if name == MODELS_CLASSIFY || name == MODELS_GENERATE_IMAGES {
            let host = self.clone();
            return Box::pin(async move {
                if name == MODELS_GENERATE_IMAGES {
                    host.generate_images(&args).await
                } else {
                    host.classify(&args).await
                }
            });
        }

        let result = match name.as_str() {
            "models.getModelsOfType" => self.models_of_type(&args, false),
            "models.getAvailableOfType" => self.models_of_type(&args, true),
            "models.getModelOfType" => self.model_of_type(&args),
            "searchTools" => self.search_tools(&args),
            "describeTool" => self.describe_tool(&args),
            "describeNamespace" => self.describe_namespace(&args),
            other => Err(format!("Unknown global \"{other}\"")),
        };
        Box::pin(async move { result })
    }
}

/// 跑一次脚本：解析源码 → 建沙箱 → 渲染结果（含输出截断与 `store` 落盘）。
///
/// 脚本开跑前先按**当前分支**重建 `store()` 快照（宿主把分支条目放进
/// [`ToolExecCtx::session_branch_entries`]）：分支切走后写入的数据不进本次 `load()`。
/// 拿不到分支数据（无会话 / 子代理上下文）时保持既有快照。
async fn run_script(code: &str, ctx: ToolExecCtx) -> std::result::Result<ToolResult, ToolError> {
    if let Some(entries) = &ctx.session_branch_entries {
        store::seed_from_entries(entries);
    }

    let parsed = source::parse_source(code).map_err(ToolError)?;
    let tools: Vec<ExtensionTool> = ctx.script_tools.iter().cloned().collect();
    let samples: HashMap<String, String> = tools
        .iter()
        .map(|t| {
            (
                t.name.clone(),
                declarations::render_tool_sample(
                    &t.name,
                    &t.description,
                    Some(&t.parameters),
                    t.output_schema.as_ref(),
                ),
            )
        })
        .collect();

    let by_name: HashMap<String, ExtensionTool> =
        tools.iter().map(|t| (t.name.clone(), t.clone())).collect();
    let script_tools: Vec<ScriptTool> = tools
        .iter()
        .map(|t| ScriptTool {
            name: t.name.clone(),
            js_name: declarations::to_codemode_identifier(&t.name),
            description: samples.get(&t.name).cloned().unwrap_or_default(),
        })
        .collect();

    let request = SandboxRequest {
        code: parsed.code,
        tools: script_tools,
        globals: vec![
            ScriptGlobal {
                name: "searchTools".to_string(),
                spread: true,
            },
            ScriptGlobal {
                name: "describeTool".to_string(),
                spread: true,
            },
            ScriptGlobal {
                name: "describeNamespace".to_string(),
                spread: true,
            },
            ScriptGlobal {
                name: "models.getModelsOfType".to_string(),
                spread: true,
            },
            ScriptGlobal {
                name: "models.getAvailableOfType".to_string(),
                spread: true,
            },
            ScriptGlobal {
                name: "models.getModelOfType".to_string(),
                spread: true,
            },
            ScriptGlobal {
                name: MODELS_CLASSIFY.to_string(),
                spread: true,
            },
            ScriptGlobal {
                name: MODELS_GENERATE_IMAGES.to_string(),
                spread: true,
            },
        ],
        store: store::snapshot(),
        timeout_ms: parsed.options.timeout_ms,
        cancel: ctx.parent_abort.clone(),
    };
    let model_globals = Arc::new(StdMutex::new(ModelGlobalsState::default()));
    let host = Arc::new(Host {
        ctx: ctx.clone(),
        tools: Arc::new(by_name),
        samples: Arc::new(samples),
        model_globals: model_globals.clone(),
        model_call_gate: Arc::new(Semaphore::new(MAX_CONCURRENT_MODEL_CALLS)),
    });

    let started = Instant::now();
    let outcome = sandbox::run(request, host).await;
    let wall_seconds = started.elapsed().as_secs_f64();

    // 输出条目：文本拼成一段；图片变附件
    let mut texts: Vec<String> = Vec::new();
    let mut attachments: Vec<ToolResultAttachment> = Vec::new();
    for item in &outcome.output {
        match item {
            ScriptOutput::Text(text) => texts.push(text.clone()),
            ScriptOutput::Image { data, mime_type } => {
                attachments.push(ToolResultAttachment {
                    data_base64: data.clone(),
                    mime_type: mime_type.clone(),
                    original_size: None,
                    converted_from: None,
                });
            }
        }
    }

    if outcome.ok {
        if let Some(value) = &outcome.value {
            texts.push(value_text(value));
        }

        store::apply_writes(&outcome.store_writes);

        // 生成了图片但一张都没展示：补一条提示
        let generated = model_globals.lock().unwrap().generated_images;
        if let Some(note) = unshown_images_note(generated, attachments.len()) {
            texts.push(note);
        }
    } else {
        texts.push(format!(
            "Script error:\n{}",
            format_error(
                outcome.error.as_ref(),
                outcome.error_kind,
                &nested_call_summary(&ctx)
            )
        ));
    }

    let max_tokens = parsed
        .options
        .max_output_tokens
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    let (body, full_output_path) = truncate_output(&texts, max_tokens);

    let header = format!(
        "Script {}\nWall time {wall_seconds:.1} seconds\nOutput:\n",
        if outcome.ok { "completed" } else { "failed" }
    );
    let text = format!("{header}{body}");

    let (model_calls, model_usage) = {
        let mut state = model_globals.lock().unwrap();
        (std::mem::take(&mut state.calls), state.usage.take())
    };

    // 失败同样走带 `is_error` 的结果：脚本已产生的输出、附件与分类器用量都要保留
    let mut result = if outcome.ok {
        ToolResult::text(text)
    } else {
        ToolResult::error(text)
    };
    result.attachments = attachments;
    result.usage = model_usage;

    let mut details = serde_json::Map::new();
    if let Some(path) = full_output_path {
        details.insert("fullOutputPath".to_string(), json!(path));
    }
    if !model_calls.is_empty() {
        details.insert("modelCalls".to_string(), Value::Array(model_calls));
    }
    if !details.is_empty() {
        result.details = Some(Value::Object(details));
    }

    Ok(result)
}

/// 脚本生成了图片却一张都没展示时的提示文本；没这种情况时返回 `None`。
///
/// `generated` 是 `models.generateImages()` 返回的图片张数，`shown` 是脚本经 `image()` 写进输出的张数（即结果附件数）。
fn unshown_images_note(generated: usize, shown: usize) -> Option<String> {
    (generated > 0 && shown == 0).then(|| {
        format!(
            "Note: models.generateImages() returned {generated} image{} that the script did not show. Show each image block of result.output with image(block).",
            if generated == 1 { "" } else { "s" }
        )
    })
}

/// 脚本侧追问「已发生的嵌套调用」摘要（失败文案用）。
fn nested_call_summary(ctx: &ToolExecCtx) -> Vec<(String, bool)> {
    let log = ctx.nested_calls.lock().unwrap();
    log.calls
        .iter()
        .filter_map(|call| {
            let name = call.get("toolName").and_then(|v| v.as_str())?.to_string();
            let is_error = call
                .get("isError")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Some((name, is_error))
        })
        .collect()
}

/// 让 `nested_call_summary` 的返回值直接参与格式化时保持可读。
fn format_call_summary(calls: &[(String, bool)]) -> String {
    if calls.is_empty() {
        return "No tool calls were made.".to_string();
    }

    let listed: Vec<String> = calls
        .iter()
        .map(|(name, is_error)| format!("{name} ({})", if *is_error { "error" } else { "ok" }))
        .collect();

    format!(
        "Tool calls made before the failure (they are not undone): {}",
        listed.join(", ")
    )
}

/// 失败文案：脚本错误用完整栈，其余按类型给前缀，再附上已发生的调用摘要。
fn format_error(
    error: Option<&ScriptError>,
    kind: sandbox::ErrorKind,
    calls: &[(String, bool)],
) -> String {
    let name = error.map(|e| e.name.as_str()).unwrap_or("Error");
    let message = error.map(|e| e.message.as_str()).unwrap_or("script failed");
    let head = match (kind, error) {
        (sandbox::ErrorKind::Script, Some(err)) => err
            .stack
            .clone()
            .unwrap_or_else(|| format!("{name}: {message}")),
        (sandbox::ErrorKind::Timeout, _) => format!("Script timed out: {message}"),
        (sandbox::ErrorKind::Aborted, _) => format!("Script aborted: {message}"),
        (sandbox::ErrorKind::Sandbox, _) => format!("Script sandbox failed: {message}"),
        (sandbox::ErrorKind::Script, None) => format!("{name}: {message}"),
    };
    format!("{head}\n\n{}", format_call_summary(calls))
}

/// 按 token 预算截断输出：超预算时保留首尾各一半，并把完整文本落到临时文件。
///
/// 返回（展示文本, 完整输出的临时文件路径）。（预算 = token 数 × 4 字符）。
fn truncate_output(texts: &[String], max_tokens: u64) -> (String, Option<String>) {
    let combined = texts.join("\n");
    let budget = (max_tokens as usize).saturating_mul(CHARS_PER_TOKEN);
    if texts.is_empty() || combined.chars().count() <= budget {
        return (combined, None);
    }

    let head_chars = budget / 2;
    let tail_chars = budget - head_chars;
    let total_chars = combined.chars().count();
    let head: String = combined.chars().take(head_chars).collect();
    let tail: String = combined
        .chars()
        .skip(total_chars.saturating_sub(tail_chars))
        .collect();
    let removed = total_chars.saturating_sub(head_chars + tail_chars);
    let lines = combined.lines().count();

    let mut text = format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: {lines}\n\n{head}…{} tokens truncated…{tail}",
        total_chars.div_ceil(CHARS_PER_TOKEN),
        removed.div_ceil(CHARS_PER_TOKEN)
    );

    match spill_output(&combined) {
        Ok(path) => {
            text.push_str(&format!(
                "\n\n[Full output: {path} (read with offset/limit)]"
            ));
            (text, Some(path))
        }
        Err(err) => {
            text.push_str(&format!("\n\n[Could not save the full output: {err}]"));
            (text, None)
        }
    }
}

/// 把完整输出写到临时文件（像 bash 的截断那样，留给模型 `read` 用）。
fn spill_output(text: &str) -> std::result::Result<String, String> {
    let mut file = tempfile::Builder::new()
        .prefix(&format!("{APP_NAME}-codemode-"))
        .suffix(".txt")
        .tempfile()
        .map_err(|e| e.to_string())?;
    std::io::Write::write_all(&mut file, text.as_bytes()).map_err(|e| e.to_string())?;
    let (_file, path) = file.keep().map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::ExtensionUiRequest;

    /// 取临时 agent_dir 并加全局锁（命令 handler 读写全局注册表与配置）。
    fn setup() -> (
        crate::test_support::AgentDirGuard,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let guard = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        (crate::test_support::AgentDirGuard::temp(), guard)
    }

    /// 取出并拼接所有待处理通知（`Notify` / `NotifyRich`）的纯文本。
    fn drain_notify_text() -> String {
        let mut out = String::new();
        while let Some(req) = crate::core::extensions::take_pending_ui() {
            match req {
                ExtensionUiRequest::Notify { text, .. } => out.push_str(&text),
                ExtensionUiRequest::NotifyRich { spans, .. } => {
                    for s in spans {
                        out.push_str(&s.text);
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// 子命令候选元数据与 `command_codemode` 的分支一一对应。
    #[test]
    fn subcommands_match_parser() {
        let names: Vec<&str> = SUBCOMMANDS.iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["settings", "show"]);
    }

    /// `/codemode`：无参 / `settings` 开关设置面板（面板里能看到两项配置），`show` 打印配置，
    /// 未知子命令报错。
    #[test]
    fn command_toggles_panel_and_shows_config() {
        let (_ad, _g) = setup();
        crate::core::extensions::register_extension(Codemode);
        crate::core::extensions::set_extension_enabled(EXT, true);
        let mut st = App::new();

        assert!(!command_codemode(&mut st, "codemode"));
        assert!(st.ext_settings.is_open_for(EXT), "无参应打开设置面板");
        let keys: Vec<&str> = st
            .ext_settings
            .items
            .iter()
            .map(|i| i.key.as_str())
            .collect();
        assert_eq!(keys, vec!["mode", "inlineBudget"], "面板应展示两项配置");

        assert!(!command_codemode(&mut st, "codemode"));
        assert!(!st.ext_settings.is_open_for(EXT), "再次无参应关闭面板");

        assert!(!command_codemode(&mut st, "codemode settings"));
        assert!(st.ext_settings.is_open_for(EXT), "settings 应打开设置面板");
        assert!(!command_codemode(&mut st, "codemode settings"));
        assert!(
            !st.ext_settings.is_open_for(EXT),
            "再次 settings 应关闭面板"
        );

        while crate::core::extensions::take_pending_ui().is_some() {}
        assert!(!command_codemode(&mut st, "codemode show"));
        let text = drain_notify_text();
        assert!(text.contains("mode:          on"), "got: {text}");
        assert!(text.contains("inlineBudget:  3000"), "got: {text}");
        assert!(text.contains("codemode.json"), "got: {text}");

        assert!(!command_codemode(&mut st, "codemode bogus"));
        let text = drain_notify_text();
        assert!(text.contains("unknown subcommand"), "got: {text}");

        crate::core::extensions::set_extension_enabled(EXT, false);
        _ = crate::core::extensions::unregister_extension(EXT);
    }

    /// 工具定义：名字/曝光/参数 schema。
    #[test]
    fn tool_definition_shape() {
        let tool = tool_definition();
        assert_eq!(tool.name, TOOL_NAME);
        assert_eq!(tool.exposure, ToolExposure::ModelOnly);
        assert_eq!(tool.parameters["required"][0], json!("code"));
        assert_eq!(
            tool.parameters["properties"]["code"]["type"],
            json!("string")
        );
        assert!(tool.description.contains("tools"));
        assert_eq!(tool.prompt_guidelines.len(), 1);
        // 语法约束采样：模型声明支持时 `code` 以裸 JS 下发
        let grammar = tool
            .grammar_sampling
            .as_ref()
            .expect("codemode 应声明语法约束采样");
        assert!(
            grammar
                .openai_lark
                .as_deref()
                .unwrap_or_default()
                .contains("options_source"),
            "lark 语法应约束首行选项行"
        );
        assert!(grammar.openai_regex.is_none());
    }

    /// `run_script` 开跑前用宿主给的**当前分支**条目重建 `store()`：脚本 `load()` 看得到
    /// 本分支的写入；拿不到分支数据（`None`）时保持既有快照。
    #[test]
    fn script_seeds_store_from_ctx_branch_entries() {
        let (_ad, _g) = setup();
        store::reset();

        let ctx_with = |entries: Option<Value>| ToolExecCtx {
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            session_branch_entries: entries.map(|e| std::sync::Arc::new(vec![e])),
            script_tools: Default::default(),
            cwd: "/tmp/x".to_string(),
            make_sub_agent: std::sync::Arc::new(|_| Err("unused".to_string())),
            parent_abort: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_id: None,
            depth: 0,
            parent_model: None,
            script_call: false,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let branch_entry = json!({
            "customType": store::ENTRY_TYPE,
            "data": { "set": { "branch": "from-branch" } },
        });
        let out = rt
            .block_on(Codemode.execute_tool_async(
                TOOL_NAME.to_string(),
                json!({ "code": "return load(\"branch\")" }),
                ctx_with(Some(branch_entry)),
            ))
            .expect("script runs");
        assert!(out.text.contains("from-branch"), "got: {}", out.text);

        // 非分支数据（None）：保持既有快照，不清空
        let out = rt
            .block_on(Codemode.execute_tool_async(
                TOOL_NAME.to_string(),
                json!({ "code": "return load(\"branch\")" }),
                ctx_with(None),
            ))
            .expect("script runs");
        assert!(out.text.contains("from-branch"), "got: {}", out.text);

        // 分支上没有写入 → 快照清空
        let out = rt
            .block_on(Codemode.execute_tool_async(
                TOOL_NAME.to_string(),
                json!({ "code": "return String(load(\"branch\"))" }),
                ctx_with(Some(json!({ "customType": "user", "data": {} }))),
            ))
            .expect("script runs");
        assert!(out.text.contains("undefined"), "got: {}", out.text);
        store::reset();
    }

    /// 2.4：脚本里 `models.generateImages` 是可用全局；参数不对时报错文案指向期望形状与
    /// `getAvailableOfType`（此处不发网络请求）。
    #[test]
    fn script_generate_images_validates_arguments() {
        let (_ad, _g) = setup();

        let run = |code: &str| {
            let ctx = ToolExecCtx {
                execute_tool: crate::core::extensions::unavailable_tool_exec(),
                parent_tool_call_id: None,
                nested_calls: Default::default(),
                session_branch_entries: None,
                script_tools: Default::default(),
                cwd: "/tmp/x".to_string(),
                make_sub_agent: std::sync::Arc::new(|_| Err("unused".to_string())),
                parent_abort: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                agent_id: None,
                depth: 0,
                parent_model: None,
                script_call: false,
            };
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(Codemode.execute_tool_async(
                    TOOL_NAME.to_string(),
                    json!({ "code": code }),
                    ctx,
                ))
                .expect("script runs")
        };

        // 全局存在（不是一个 undefined 调用错误）
        let out = run("return typeof models.generateImages");
        assert!(out.text.contains("function"), "got: {}", out.text);

        let out = run("return await models.generateImages({}, {})");
        assert!(out.text.contains("Script failed"), "got: {}", out.text);
        assert!(
            out.text
                .contains("models.generateImages() expects a model of type \"image\""),
            "got: {}",
            out.text
        );
        assert!(
            out.text.contains("models.getAvailableOfType(\"image\")"),
            "got: {}",
            out.text
        );
    }

    /// 脚本生成了图片却没用 `image()` 展示时的提示文案（无生成或无未展示时不提）。
    #[test]
    fn unshown_images_note_only_when_nothing_shown() {
        let note = unshown_images_note(1, 0).unwrap();
        assert!(note.contains("returned 1 image that"));
        assert!(note.contains("with image(block)"));
        assert!(
            unshown_images_note(3, 0)
                .unwrap()
                .contains("returned 3 images that")
        );
        assert!(unshown_images_note(3, 1).is_none());
        assert!(unshown_images_note(0, 0).is_none());
    }

    /// 2.5：脚本里的 `describeNamespace()` 端到端可用——按别名找到命名空间，
    /// 拿到其下工具（脚本标识符形式），未知命名空间得到 `null`。
    #[test]
    fn script_describe_namespace_lists_members() {
        let (_ad, _g) = setup();

        let namespaced = |name: &str, description: &str| {
            let mut tool = ExtensionTool::simple(
                name,
                description,
                json!({"type": "object", "properties": {}}),
                "",
            );
            tool.namespace = Some("dev-radius".to_string());
            tool
        };
        let ctx = ToolExecCtx {
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            session_branch_entries: None,
            script_tools: std::sync::Arc::new(vec![
                namespaced("mcp__dev-radius__put", "write a thing"),
                namespaced("mcp__dev-radius__get", "read a thing"),
            ]),
            cwd: "/tmp/x".to_string(),
            make_sub_agent: std::sync::Arc::new(|_| Err("unused".to_string())),
            parent_abort: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_id: None,
            depth: 0,
            parent_model: None,
            script_call: false,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt
            .block_on(Codemode.execute_tool_async(
                TOOL_NAME.to_string(),
                json!({
                    "code": "const ns = await describeNamespace('mcp__dev_radius'); \
                             return ns.namespace + '|' + ns.tools.map((t) => t.name).join(',')"
                }),
                ctx.clone(),
            ))
            .expect("script runs");
        assert!(
            out.text
                .contains("dev-radius|mcp__dev_radius__get,mcp__dev_radius__put"),
            "got: {}",
            out.text
        );

        // 未知命名空间 → `null`（宿主用 JSON `null` 表达「没有」）
        let out = rt
            .block_on(Codemode.execute_tool_async(
                TOOL_NAME.to_string(),
                json!({ "code": "return String(await describeNamespace('nope'))" }),
                ctx,
            ))
            .expect("script runs");
        assert!(out.text.contains("null"), "got: {}", out.text);
    }

    /// 未超预算时原样返回（不落盘）。
    #[test]
    fn truncation_passthrough() {
        let (text, path) = truncate_output(&["a".to_string(), "b".to_string()], 10);
        assert_eq!(text, "a\nb");
        assert!(path.is_none());
    }

    /// 超预算时保留首尾、附警告与落盘路径。
    #[test]
    fn truncation_keeps_head_and_tail() {
        let text = "x".repeat(1000);
        let (out, path) = truncate_output(std::slice::from_ref(&text), 10);
        assert!(out.starts_with("Warning: truncated output"), "{out}");
        assert!(out.contains("tokens truncated"), "{out}");
        assert!(out.contains("[Full output:"), "{out}");
        let path = path.expect("应落盘");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        let _ = std::fs::remove_file(&path);
    }

    /// `format_error`：脚本错误带栈，调用摘要列出已发生的调用。
    #[test]
    fn error_text_includes_stack_and_call_summary() {
        let err = sandbox::ScriptError {
            name: "TypeError".to_string(),
            message: "nope".to_string(),
            stack: Some("TypeError: nope\n    at x".to_string()),
        };
        let text = format_error(
            Some(&err),
            sandbox::ErrorKind::Script,
            &[("read".to_string(), false), ("bash".to_string(), true)],
        );
        assert!(text.starts_with("TypeError: nope"));
        assert!(text.contains("read (ok), bash (error)"));
        assert_eq!(
            format_error(None, sandbox::ErrorKind::Timeout, &[]),
            "Script timed out: script failed\n\nNo tool calls were made."
        );
        assert_eq!(
            format_error(None, sandbox::ErrorKind::Script, &[]),
            "Error: script failed\n\nNo tool calls were made."
        );
    }

    /// `value_text`：字符串原样，其余 JSON。
    #[test]
    fn value_text_shapes() {
        assert_eq!(value_text(&json!("hi")), "hi");
        assert_eq!(value_text(&json!({"a": 1})), "{\"a\":1}");
        assert_eq!(value_text(&json!(null)), "null");
    }

    /// `models.*` 的 type/provider 实参校验（文案对齐 pi）。
    #[test]
    fn model_type_and_provider_arguments_are_validated() {
        assert_eq!(
            model_type_arg(Some(&json!("chat"))).unwrap(),
            ModelType::Chat
        );
        assert_eq!(
            model_type_arg(Some(&json!("classifier"))).unwrap(),
            ModelType::Classifier
        );
        assert_eq!(
            model_type_arg(Some(&json!("vision"))).unwrap_err(),
            "Unknown model type \"vision\". Use \"chat\", \"image\", or \"classifier\"."
        );
        assert!(model_type_arg(None).unwrap_err().contains("undefined"));

        assert_eq!(provider_arg(None).unwrap(), None);
        assert_eq!(provider_arg(Some(&json!(null))).unwrap(), None);
        assert_eq!(
            provider_arg(Some(&json!("openai"))).unwrap().as_deref(),
            Some("openai")
        );
        assert_eq!(
            provider_arg(Some(&json!(7))).unwrap_err(),
            "provider must be a string"
        );
    }

    /// `models.*` 的 model 引用只取 provider/id；脚本自带的凭据字段一律忽略。
    #[test]
    fn resolve_model_call_uses_only_provider_and_id() {
        let _guard = crate::test_support::AgentDirGuard::temp();
        let model = resolve_model_call(
            MODELS_CLASSIFY,
            ModelType::Classifier,
            Some(&json!({
                "provider": "typesafe",
                "id": "jev-latest",
                "apiKey": "sk-secret",
                "headers": { "authorization": "Bearer sk-secret" },
            })),
        )
        .unwrap();
        assert_eq!(model.provider, "typesafe");
        assert_eq!(model.model_id, "jev-latest");
        assert!(
            !model.api_key.contains("sk-secret"),
            "脚本自带凭据不得透传：{}",
            model.api_key
        );

        // 形状不对 / 未知模型：都带期望形状与 getAvailableOfType 指引
        let err = resolve_model_call(
            MODELS_CLASSIFY,
            ModelType::Classifier,
            Some(&json!({ "provider": "typesafe" })),
        )
        .unwrap_err();
        assert!(
            err.starts_with("models.classify() expects a model of type \"classifier\"")
                && err.contains("getAvailableOfType(\"classifier\")"),
            "{err}"
        );

        let err = resolve_model_call(
            MODELS_CLASSIFY,
            ModelType::Classifier,
            Some(&json!({ "provider": "typesafe", "id": "nope" })),
        )
        .unwrap_err();
        assert!(
            err.starts_with("Unknown classifier model \"typesafe/nope\""),
            "{err}"
        );

        // 类型不符：点名实际类型
        let err = resolve_model_call(
            MODELS_GENERATE_IMAGES,
            ModelType::Image,
            Some(&json!({ "provider": "typesafe", "id": "jev-latest" })),
        )
        .unwrap_err();
        assert!(
            err.contains("is a classifier model, not a image model"),
            "{err}"
        );

        let err = resolve_model_call(MODELS_CLASSIFY, ModelType::Classifier, None).unwrap_err();
        assert!(err.contains("got undefined"), "{err}");
    }

    /// `models.generateImages` 的上下文校验：期望形状、空 input 与非块元素都报错。
    #[test]
    fn images_context_requires_non_empty_blocks() {
        let input = images_context_arg(Some(&json!({
            "input": [
                { "type": "text", "text": "a red fox" },
                { "type": "image", "data": "AA==", "mimeType": "image/png" },
            ]
        })))
        .unwrap();
        assert_eq!(input.len(), 2);
        assert!(matches!(&input[0], ImageContent::Text { text } if text == "a red fox"));
        assert!(
            matches!(&input[1], ImageContent::Image { data, mime_type } if data == "AA==" && mime_type == "image/png")
        );

        for bad in [
            json!(null),
            json!({ "prompt": "a red fox" }),
            json!({ "input": [] }),
            json!({ "input": [{ "type": "text" }] }),
            json!({ "input": ["a red fox"] }),
        ] {
            let err = images_context_arg(Some(&bad)).unwrap_err();
            assert!(
                err.starts_with("models.generateImages()")
                    && err.contains("Expected context: { input: ["),
                "{bad}: {err}"
            );
        }
    }

    /// 目录查询：按类型过滤、条目字段形状、类型不匹配或未知名返回 null。
    #[test]
    fn catalog_lookups_return_model_info() {
        let _guard = crate::test_support::AgentDirGuard::temp();
        let chat = catalog_models(ModelType::Chat, Some("openrouter".into()), false);
        assert!(!chat.is_empty());
        assert!(
            chat.iter()
                .all(|m| m["type"] == "chat" && m["provider"] == "openrouter")
        );
        assert!(
            chat.iter()
                .all(|m| m["api"].is_string() && m["input"].is_array() && m["id"].is_string())
        );

        let classifiers = catalog_models(ModelType::Classifier, Some("openrouter".into()), false);
        assert!(!classifiers.is_empty());
        assert!(classifiers.iter().all(|m| m["type"] == "classifier"));

        let one = catalog_model(ModelType::Classifier, "openrouter", "~typesafe/jev-latest");
        assert_eq!(one["type"], "classifier");
        assert_eq!(one["provider"], "openrouter");
        // 同 id 的条目在按 chat 查时不会命中
        assert_eq!(
            catalog_model(ModelType::Chat, "openrouter", "~typesafe/jev-latest"),
            Value::Null
        );
        assert_eq!(
            catalog_model(ModelType::Classifier, "openrouter", "nope"),
            Value::Null
        );
    }

    /// `getAvailableOfType`：只保留有凭据的 provider，且是全集的子集。
    #[test]
    fn available_models_require_credentials() {
        let _guard = crate::test_support::AgentDirGuard::temp();
        let configured: HashSet<String> = auth::list_configured_providers().into_iter().collect();
        let available = catalog_models(ModelType::Chat, None, true);
        assert!(
            available
                .iter()
                .all(|m| configured.contains(m["provider"].as_str().unwrap())),
            "{available:?}"
        );
        assert!(available.len() <= catalog_models(ModelType::Chat, None, false).len());
    }

    /// 命名空间别名归一：`mcp__dev-radius` / `mcp__dev_radius` / `dev-radius` / `dev_radius`
    /// 落到同一个 key，且不与相邻命名空间混淆。
    #[test]
    fn namespace_aliases_normalize() {
        let key = namespace_key("mcp__dev-radius");
        assert_eq!(key, namespace_key("mcp__dev_radius"));
        assert_eq!(key, namespace_key("dev-radius"));
        assert_eq!(key, namespace_key("dev_radius"));
        assert_ne!(key, namespace_key("dev"));
        assert_ne!(key, namespace_key("dev_radius2"));
        // 空命名空间与只有前缀的写法
        assert_eq!(namespace_key(""), "");
        assert_eq!(namespace_key("mcp__"), "");
    }

    /// `describeNamespace` 的取名空间部分：工具按脚本标识符排序、描述取自声明样例，
    /// 未声明命名空间的工具不进清单，不存在的命名空间返回 `None`。
    #[test]
    fn namespace_tools_lists_sorted_members() {
        let tool = |name: &str, namespace: Option<&str>| {
            let mut tool = ExtensionTool::simple(name, "d", json!({}), "");
            tool.namespace = namespace.map(|n| n.to_string());
            tool
        };
        let tools: HashMap<String, ExtensionTool> = [
            tool("mcp__dev-radius__b", Some("dev-radius")),
            tool("mcp__dev-radius__a", Some("dev-radius")),
            tool("plain_tool", None),
            tool("other_ns_tool", Some("other")),
        ]
        .into_iter()
        .map(|t| (t.name.clone(), t))
        .collect();
        let samples: HashMap<String, String> = tools
            .keys()
            .map(|k| (k.clone(), format!("sample of {k}")))
            .collect();

        let (namespace, found) = namespace_tools(&tools, &samples, "mcp__dev_radius").unwrap();
        assert_eq!(namespace, "dev-radius", "返回工具表里声明的原名");
        assert_eq!(
            found,
            vec![
                (
                    "mcp__dev_radius__a".to_string(),
                    "sample of mcp__dev-radius__a".to_string()
                ),
                (
                    "mcp__dev_radius__b".to_string(),
                    "sample of mcp__dev-radius__b".to_string()
                ),
            ],
            "按脚本标识符排序，且只含该命名空间的工具"
        );

        assert!(namespace_tools(&tools, &samples, "dev").is_none());
        assert!(namespace_tools(&tools, &samples, "mcp__plain_tool").is_none());
    }
}
